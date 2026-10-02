//! The cross-process writer lock of `@deepseek-ai/dsh-atomic-write`
//! (`withFileLock`), so rutis and dsh processes exclude each other:
//!
//! - the lock is `<file>.lock`, created exclusively, holding `<pid>\n`;
//! - a contender takes over a lock whose holder process no longer exists,
//!   serialized on `<lock>.takeover-<sha256(record)[..16]>`;
//! - contention backs off from 20 ms to 200 ms until the deadline.
//!
//! Writes go through a temporary sibling renamed over the file, so readers
//! never take the lock.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub const DEFAULT_WAIT: Duration = Duration::from_millis(2000);

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("timed out waiting for the writer lock at {0}")]
    Timeout(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn create(path: &Path, content: &str) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(content.as_bytes())
}

/// Whether the holder a `<pid>\n` record names is proven gone.
fn holder_exited(record: &str) -> bool {
    let Some(digits) = record.strip_suffix('\n') else {
        return false;
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(pid) = digits.parse::<i64>() else {
        return false;
    };
    if pid == 0 || pid > i64::from(i32::MAX) || pid == i64::from(std::process::id()) {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 only probes for the process.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn take_over_exited(lock: &Path) -> bool {
    let Ok(record) = std::fs::read_to_string(lock) else {
        return false;
    };
    if !holder_exited(&record) {
        return false;
    }
    let digest = Sha256::digest(record.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let claim = PathBuf::from(format!("{}.takeover-{}", lock.display(), &hex[..16]));
    if create(&claim, &format!("{}\n", std::process::id())).is_err() {
        return false;
    }
    let removed = std::fs::read_to_string(lock).ok().as_deref() == Some(record.as_str())
        && holder_exited(&record)
        && std::fs::remove_file(lock).is_ok();
    let _ = std::fs::remove_file(&claim);
    removed
}

/// Hold the writer lock of `file` around `operation` (blocking).
pub fn with_file_lock<T>(
    file: &Path,
    wait: Duration,
    operation: impl FnOnce() -> T,
) -> Result<T, LockError> {
    let lock = PathBuf::from(format!("{}.lock", file.display()));
    let deadline = Instant::now() + wait;
    let mut delay = Duration::from_millis(20);
    loop {
        match create(&lock, &format!("{}\n", std::process::id())) {
            Ok(()) => break,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if take_over_exited(&lock) {
                    continue;
                }
            }
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= deadline {
            return Err(LockError::Timeout(lock));
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(200));
    }
    let result = operation();
    let _ = std::fs::remove_file(&lock);
    Ok(result)
}

/// Replace `file` with `content` atomically: a fresh sibling, renamed over.
pub fn write_atomic(file: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = PathBuf::from(format!(
        "{}.{:012x}.tmp",
        file.display(),
        nonce & 0xffff_ffff_ffff
    ));
    let result = create(&temp, content).and_then(|()| std::fs::rename(&temp, file));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_and_takes_over_dead_holders() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.json");
        let lock = dir.path().join("package.json.lock");

        // A live holder (ourselves, recorded as another pid that exists: 1).
        std::fs::write(&lock, "1\n").unwrap();
        let err = with_file_lock(&file, Duration::from_millis(100), || ()).unwrap_err();
        assert!(matches!(err, LockError::Timeout(_)), "{err:?}");

        // A dead holder is taken over.
        std::fs::write(&lock, "2147483000\n").unwrap();
        let ran = with_file_lock(&file, Duration::from_millis(500), || true).unwrap();
        assert!(ran);
        assert!(!lock.exists(), "released after the operation");

        write_atomic(&file, "{}\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{}\n");
    }
}
