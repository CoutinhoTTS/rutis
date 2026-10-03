//! Windows: `LoadLibraryExW` by full path and `GetProcAddress`; the loaded
//! SDK is found by its module name.
//!
//! Verified on windows-2025 (SDK design §十一, Windows feasibility record):
//! a plugin loaded with `LOAD_LIBRARY_SEARCH_APPLICATION_DIR |
//! LOAD_LIBRARY_SEARCH_SYSTEM32` reuses the `rutis_sdk.dll` and `std-*.dll`
//! the host already loaded from its own directory; copies planted in the
//! working directory, on `PATH` or next to the plugin are not used.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use windows_sys::Win32::Foundation::{GetLastError, HMODULE};
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetProcAddress, LoadLibraryExW,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
    LOAD_LIBRARY_SEARCH_SYSTEM32,
};

/// An `HMODULE`. Never freed.
pub(crate) type Handle = NonNull<c_void>;

/// The `LoadError::step` for a failed library open.
pub(crate) const OPEN_STEP: &str = "LoadLibraryExW";

fn last_error() -> String {
    std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32).to_string()
}

fn module_path(module: HMODULE) -> Result<PathBuf, String> {
    let mut buf = vec![0u16; 512];
    loop {
        let len = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) };
        if len == 0 {
            return Err(format!("GetModuleFileNameW failed: {}", last_error()));
        }
        if (len as usize) < buf.len() {
            return Ok(PathBuf::from(std::ffi::OsString::from_wide(
                &buf[..len as usize],
            )));
        }
        // Truncated: retry with a larger buffer (paths may exceed MAX_PATH).
        buf.resize(buf.len() * 2, 0);
        if buf.len() > 1 << 16 {
            return Err("module path is too long".into());
        }
    }
}

/// The file of the SDK module the host runs with.
///
/// The address of `rutis_sdk::rutis_sdk_boot_id` as seen from the host is a
/// jump stub the linker placed in the host executable, not the function in
/// the DLL, so it cannot identify the module. The host imports the SDK by
/// its file name, and Windows binds imports to the module of that base name
/// loaded first (the host's, at startup). Look that module up by name, then
/// call its own `rutis_sdk_boot_id` (found with `GetProcAddress`) to confirm
/// that it is the SDK this host was built with.
pub(crate) fn loaded_sdk_path() -> Result<PathBuf, String> {
    let name = rutis_dylib_meta::sdk_reference(rutis_sdk::SDK_TARGET)?;
    let wide: Vec<u16> = std::ffi::OsStr::new(&name)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut module: HMODULE = std::ptr::null_mut();
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            wide.as_ptr(),
            &mut module,
        )
    };
    let Some(module) = NonNull::new(module).filter(|_| ok != 0) else {
        return Err(format!("{name} is not loaded: {}", last_error()));
    };
    let boot_id: unsafe extern "C" fn(*mut u8, usize) -> usize =
        unsafe { std::mem::transmute(find_symbol(module, b"rutis_sdk_boot_id\0")?) };
    let mut buf = [0u8; 128];
    let len = unsafe { boot_id(buf.as_mut_ptr(), buf.len()) };
    if len > buf.len() || &buf[..len] != rutis_sdk::SDK_ID.as_bytes() {
        return Err(format!(
            "the loaded {name} reports a different SDK identity"
        ));
    }
    module_path(module.as_ptr())
}

/// Opens a cached library so that, while the handle is open, nobody can
/// write, delete or rename it. `LoadLibraryExW` still opens it: it asks
/// only for read and execute access, which this share mode allows.
pub(crate) fn open_pinned(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

pub(crate) unsafe fn open_library(path: &Path) -> Result<Handle, String> {
    // With LOAD_LIBRARY_SEARCH_* a relative name would be searched for, so
    // pass a full path. `absolute` also turns `/` into `\`.
    let path = std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if wide[..wide.len() - 1].contains(&0) {
        return Err("library path contains a NUL character".into());
    }
    let module = LoadLibraryExW(
        wide.as_ptr(),
        std::ptr::null_mut(),
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
    );
    NonNull::new(module).ok_or_else(|| format!("{}: {}", path.display(), last_error()))
}

/// `name` must end with a NUL byte.
pub(crate) unsafe fn find_symbol(handle: Handle, name: &[u8]) -> Result<*mut c_void, String> {
    debug_assert_eq!(name.last(), Some(&0));
    match GetProcAddress(handle.as_ptr(), name.as_ptr()) {
        Some(function) => Ok(function as *mut c_void),
        None => Err(format!(
            "{}: {}",
            String::from_utf8_lossy(&name[..name.len() - 1]),
            last_error()
        )),
    }
}
