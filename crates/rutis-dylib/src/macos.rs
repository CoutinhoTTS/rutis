//! macOS checks that run on a plugin file before `dlopen`.

use std::ffi::{c_void, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const QUARANTINE: &[u8] = b"com.apple.quarantine\0";

/// Whether `path` carries `com.apple.quarantine`. `dlopen` of such a file
/// asks Gatekeeper and, without a user to answer, never returns.
pub(crate) fn quarantined(path: &Path) -> Result<bool, String> {
    let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // XATTR_NOFOLLOW is not passed: dlopen follows symlinks too.
    let size = unsafe {
        libc::getxattr(
            c_path.as_ptr(),
            QUARANTINE.as_ptr().cast(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    if size >= 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOATTR) {
        Ok(false)
    } else {
        Err(format!("{}: {error}", path.display()))
    }
}

type CFTypeRef = *const c_void;
type OSStatus = i32;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFTypeRef,
        buffer: *const u8,
        length: isize,
        is_directory: u8,
    ) -> CFTypeRef;
    fn CFStringCreateWithBytes(
        allocator: CFTypeRef,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
    fn CFRelease(value: CFTypeRef);
}

#[link(name = "Security", kind = "framework")]
extern "C" {
    fn SecStaticCodeCreateWithPath(path: CFTypeRef, flags: u32, code: *mut CFTypeRef) -> OSStatus;
    fn SecRequirementCreateWithString(
        text: CFTypeRef,
        flags: u32,
        requirement: *mut CFTypeRef,
    ) -> OSStatus;
    fn SecStaticCodeCheckValidity(
        code: CFTypeRef,
        flags: u32,
        requirement: CFTypeRef,
    ) -> OSStatus;
}

struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}

/// Checks that `path` has a valid signature from an Apple-issued certificate
/// whose Team ID is one of `team_ids`. Ad-hoc signatures never match.
pub(crate) fn check_team_id(path: &Path, team_ids: &[String]) -> Result<(), String> {
    if team_ids.is_empty() {
        return Err("the Team ID allowlist is empty".into());
    }
    for id in team_ids {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(format!("invalid Team ID {id:?}"));
        }
    }
    let teams = team_ids
        .iter()
        .map(|id| format!("certificate leaf[subject.OU] = \"{id}\""))
        .collect::<Vec<_>>()
        .join(" or ");
    let text = format!("anchor apple generic and ({teams})");
    let bytes = path.as_os_str().as_bytes();
    unsafe {
        let url = Owned(CFURLCreateFromFileSystemRepresentation(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as isize,
            0,
        ));
        let text = Owned(CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as isize,
            CF_STRING_ENCODING_UTF8,
            0,
        ));
        if url.0.is_null() || text.0.is_null() {
            return Err("could not build the code signing query".into());
        }
        let mut code = std::ptr::null();
        let status = SecStaticCodeCreateWithPath(url.0, 0, &mut code);
        let code = Owned(code);
        if status != 0 {
            return Err(format!("reading the code signature failed (OSStatus {status})"));
        }
        let mut requirement = std::ptr::null();
        let status = SecRequirementCreateWithString(text.0, 0, &mut requirement);
        let requirement = Owned(requirement);
        if status != 0 {
            return Err(format!("invalid code requirement (OSStatus {status})"));
        }
        match SecStaticCodeCheckValidity(code.0, 0, requirement.0) {
            0 => Ok(()),
            // errSecCSUnsigned
            -67062 => Err("plugin is not signed".into()),
            // errSecCSReqFailed
            -67050 => Err(format!(
                "plugin is not signed by an allowed Team ID ({})",
                team_ids.join(", ")
            )),
            status => Err(format!("code signature is not valid (OSStatus {status})")),
        }
    }
}
