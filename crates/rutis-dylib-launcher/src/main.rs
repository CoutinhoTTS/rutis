//! Standalone launcher. It must not depend on rutis-sdk or dynamic libstd.
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use sha2::{Digest, Sha256};
    use std::env;
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{self, Command};

    /// Variables that make the platform loader pick libraries from elsewhere.
    /// They are saved as RUTIS_ORIG_* and removed; the host restores them.
    /// Code injected into this launcher itself (DYLD_INSERT_LIBRARIES) runs
    /// before main and is outside the launcher's protection (SDK design §5.4).
    #[cfg(target_os = "linux")]
    const LOADER_PREFIX: &str = "LD_";
    #[cfg(target_os = "macos")]
    const LOADER_PREFIX: &str = "DYLD_";

    pub(super) fn main() {
        if let Err(error) = run() {
            eprintln!("rutis dylib bundle rejected: {error}");
            process::exit(1);
        }
    }

    fn run() -> Result<(), String> {
        let host_name = option_env!("RUTIS_BUNDLE_HOST_FILE")
            .ok_or("launcher was not bound to a host artifact")?;
        let host_hash = option_env!("RUTIS_BUNDLE_HOST_SHA256")
            .ok_or("launcher was not bound to a host hash")?;
        let sdk_name = option_env!("RUTIS_BUNDLE_SDK_FILE")
            .ok_or("launcher was not bound to an SDK artifact")?;
        let sdk_hash = option_env!("RUTIS_BUNDLE_SDK_SHA256")
            .ok_or("launcher was not bound to an SDK hash")?;
        let std_name =
            option_env!("RUTIS_BUNDLE_STD_FILE").ok_or("launcher was not bound to libstd")?;
        let std_hash = option_env!("RUTIS_BUNDLE_STD_SHA256")
            .ok_or("launcher was not bound to a libstd hash")?;
        let dir = env::current_exe()
            .map_err(|e| e.to_string())?
            .canonicalize()
            .map_err(|e| e.to_string())?
            .parent()
            .ok_or("launcher has no parent directory")?
            .to_path_buf();
        let host = verify(&dir, host_name, host_hash)?;
        verify(&dir, sdk_name, sdk_hash)?;
        verify(&dir, std_name, std_hash)?;
        // Only the checked bundle directory may supply Rust dynamic libraries.
        // The installation must remain immutable until the host exits.
        let mut command = Command::new(&host);
        command.args(env::args_os().skip(1));
        let saved_prefix = format!("RUTIS_ORIG_{LOADER_PREFIX}");
        let original_environment = env::vars_os().collect::<Vec<_>>();
        for (name, _) in &original_environment {
            if name.to_string_lossy().starts_with(&saved_prefix) {
                command.env_remove(name);
            }
        }
        for (name, value) in original_environment {
            let name_text = name.to_string_lossy();
            if name_text.starts_with(LOADER_PREFIX) {
                command.env(format!("RUTIS_ORIG_{name_text}"), value);
                command.env_remove(name);
            }
        }
        command.env("RUTIS_DYLIB_LAUNCHER", "1");
        // macOS resolves the bundle through the host's @loader_path run path.
        #[cfg(target_os = "linux")]
        command.env("LD_LIBRARY_PATH", &dir);
        Err(command.exec().to_string())
    }

    fn verify(dir: &Path, name: &str, expected: &str) -> Result<PathBuf, String> {
        if !is_hash(expected) {
            return Err(format!("invalid embedded hash for {name}"));
        }
        if Path::new(name).components().count() != 1 {
            return Err(format!("invalid bundled filename: {name}"));
        }
        let path = dir.join(name);
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let actual = format!("{:x}", Sha256::digest(bytes));
        if actual != expected {
            return Err(format!(
                "{} SHA-256 mismatch: expected {expected}, got {actual}",
                path.display()
            ));
        }
        Ok(path)
    }

    fn is_hash(hash: &str) -> bool {
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }
}

/// Windows has no `exec`: the launcher starts the host as a child and waits.
///
/// - The host, SDK and std files are opened with read-only sharing, hashed
///   through those handles, and kept open until the host exits, so nobody
///   can write, delete or rename them while it runs. The loader opens them
///   again for read and execute, which this share mode allows.
/// - No environment variable is removed. The host's static imports resolve
///   from the executable's directory first, and all three files are present
///   there (checked above); `PATH` and the working directory are searched
///   only for a DLL that is missing, and `.local` redirection was not
///   honoured in the feasibility test (SDK design §十一).
/// - A Job Object with `KILL_ON_JOB_CLOSE` ends the host and its children
///   when the launcher exits for any reason.
/// - Ctrl+C reaches every process on the console; the launcher ignores it
///   and lets the host decide, then forwards the host's exit code.
#[cfg(windows)]
mod windows {
    use sha2::{Digest, Sha256};
    use std::env;
    use std::fs::{self, File};
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::process::{self, Command};
    use windows_sys::Win32::Foundation::{HANDLE, TRUE};
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn main() {
        match run() {
            Ok(code) => process::exit(code),
            Err(error) => {
                eprintln!("rutis dylib bundle rejected: {error}");
                process::exit(1);
            }
        }
    }

    fn run() -> Result<i32, String> {
        let host_name = option_env!("RUTIS_BUNDLE_HOST_FILE")
            .ok_or("launcher was not bound to a host artifact")?;
        let host_hash = option_env!("RUTIS_BUNDLE_HOST_SHA256")
            .ok_or("launcher was not bound to a host hash")?;
        let sdk_name = option_env!("RUTIS_BUNDLE_SDK_FILE")
            .ok_or("launcher was not bound to an SDK artifact")?;
        let sdk_hash = option_env!("RUTIS_BUNDLE_SDK_SHA256")
            .ok_or("launcher was not bound to an SDK hash")?;
        let std_name =
            option_env!("RUTIS_BUNDLE_STD_FILE").ok_or("launcher was not bound to libstd")?;
        let std_hash = option_env!("RUTIS_BUNDLE_STD_SHA256")
            .ok_or("launcher was not bound to a libstd hash")?;
        let dir = env::current_exe()
            .map_err(|e| e.to_string())?
            .parent()
            .ok_or("launcher has no parent directory")?
            .to_path_buf();
        // Held until this function returns, i.e. after the host has exited.
        let (host, _host_file) = verify(&dir, host_name, host_hash)?;
        let (_, _sdk_file) = verify(&dir, sdk_name, sdk_hash)?;
        let (_, _std_file) = verify(&dir, std_name, std_hash)?;

        let job = kill_on_close_job()?;
        // Joining the job before the host starts puts the host (and anything
        // it starts) in it from its first instruction.
        let joined = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } != 0;
        unsafe { SetConsoleCtrlHandler(Some(ignore_interrupt), TRUE) };
        let mut child = Command::new(&host)
            .args(env::args_os().skip(1))
            .env("RUTIS_DYLIB_LAUNCHER", "1")
            .spawn()
            .map_err(|e| format!("{}: {e}", host.display()))?;
        if !joined && unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) } == 0
        {
            let _ = child.kill();
            return Err(format!(
                "could not place the host in a job object: {}",
                std::io::Error::last_os_error()
            ));
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        Ok(status.code().unwrap_or(1))
    }

    unsafe extern "system" fn ignore_interrupt(ctrl_type: u32) -> windows_sys::core::BOOL {
        // Handled (ignored) here; other events (console close, logoff,
        // shutdown) keep their default handling, which ends the launcher
        // and, through the job, the host.
        (ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT) as windows_sys::core::BOOL
    }

    fn kill_on_close_job() -> Result<HANDLE, String> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(format!(
                "CreateJobObjectW: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        // BREAKAWAY_OK: a host may still start a process that outlives it,
        // by asking for CREATE_BREAKAWAY_FROM_JOB explicitly.
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(format!(
                "SetInformationJobObject: {}",
                std::io::Error::last_os_error()
            ));
        }
        // The handle is never closed: the system closes it when the
        // launcher exits, which is what ends the job.
        Ok(job)
    }

    fn verify(dir: &Path, name: &str, expected: &str) -> Result<(PathBuf, File), String> {
        if !super::is_hash(expected) {
            return Err(format!("invalid embedded hash for {name}"));
        }
        if Path::new(name).components().count() != 1 {
            return Err(format!("invalid bundled filename: {name}"));
        }
        let path = dir.join(name);
        let metadata =
            fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let actual = format!("{:x}", Sha256::digest(bytes));
        if actual != expected {
            return Err(format!(
                "{} SHA-256 mismatch: expected {expected}, got {actual}",
                path.display()
            ));
        }
        Ok((path, file))
    }
}

#[cfg(windows)]
fn is_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    unix::main();
}

#[cfg(windows)]
fn main() {
    windows::main();
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn main() {
    eprintln!("rutis dylib launcher is available only on Linux, macOS and Windows");
    std::process::exit(1);
}
