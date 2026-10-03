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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    unix::main();
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("rutis dylib launcher is available only on Linux and macOS");
    std::process::exit(1);
}
