//! The local connector: starts a runtime process and connects it, either
//! on an inherited socket (fd 3) or, for runtimes that cannot take one, on
//! a Unix socket in a private directory that the process dials back. It
//! watches how the process ends.
//!
//! This serves the `Process` compatibility facade; it moves into
//! `rutis-transport-local` once runtimes get sessions through local + link.
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rutis_channel::Channel;
use tokio::sync::{oneshot, watch};

use crate::Error;

/// The fd a runtime process finds its channel on (`fd:3`).
pub(crate) const CHANNEL_FD: i32 = 3;

/// How the process gets its channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connect {
    /// One end of a socket pair, as fd 3: `fd:3 <first>`.
    Inherit,
    /// A socket path the process dials: `<path> <first>`.
    DialBack,
}

/// A started runtime process and the channel it connected.
pub(crate) struct Spawned {
    pub channel: Channel,
    pub child: Child,
    /// Holds the socket when the process dials back; removed when the
    /// process is dropped.
    pub directory: Option<tempfile::TempDir>,
}

fn transport(error: std::io::Error) -> Error {
    Error::Transport(error.to_string())
}

/// Start `command` with the channel and `first` (the first plugin or the
/// anchor) as its last two arguments, and connect it. The channel ends with
/// how the process ended (`Cordis process exited …`).
pub(crate) async fn spawn(
    command: tokio::process::Command,
    first: &Path,
    connect: Connect,
) -> Result<Spawned, Error> {
    match connect {
        Connect::Inherit => inherit(command, first),
        Connect::DialBack => dial_back(command, first).await,
    }
}

fn inherit(mut command: tokio::process::Command, first: &Path) -> Result<Spawned, Error> {
    let (ours, theirs) = UnixStream::pair().map_err(transport)?;
    let fd = theirs.as_raw_fd();
    // SAFETY: between fork and exec, only async-signal-safe calls: dup2 and
    // fcntl. Our end and every other descriptor keep CLOEXEC; only fd 3
    // crosses exec.
    unsafe {
        command.pre_exec(move || {
            if fd == CHANNEL_FD {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, CHANNEL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .arg(format!("fd:{CHANNEL_FD}"))
        .arg(first)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(transport)?;
    // The child has its copy; ours would keep the channel open after it exits.
    drop(theirs);
    let child = Child::watch(child);
    let mut channel = crate::unix::channel(ours, "")?;
    channel.info.transport = "fd";
    let channel = crate::unix::on_disconnect(channel, Box::new(child.disconnected()));
    Ok(Spawned {
        channel: traced(channel),
        child,
        directory: None,
    })
}

async fn dial_back(mut command: tokio::process::Command, first: &Path) -> Result<Spawned, Error> {
    let directory = tempfile::Builder::new()
        .prefix("rutis-mount-")
        .tempdir()
        .map_err(transport)?;
    let socket = directory.path().join("peer.sock");
    let listener = tokio::net::UnixListener::bind(&socket).map_err(transport)?;
    let mut child = command
        .arg(&socket)
        .arg(first)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(transport)?;
    let stream = tokio::select! {
        accepted = listener.accept() => accepted.map_err(transport)?.0,
        status = child.wait() => return Err(Error::Transport(match status {
            Ok(status) => format!("Cordis process exited before connecting: {status}"),
            Err(error) => format!("Cordis process exited before connecting: {error}"),
        })),
    };
    let stream = stream.into_std().map_err(transport)?;
    let child = Child::watch(child);
    let channel = crate::unix::on_disconnect(
        crate::unix::channel(stream, "")?,
        Box::new(child.disconnected()),
    );
    Ok(Spawned {
        channel: traced(channel),
        child,
        directory: Some(directory),
    })
}

/// Set to report every message crossing a runtime channel (direction and
/// length, never content) on stderr.
pub(crate) const TRACE_VARIABLE: &str = "RUTIS_INTEROP_TRACE";

fn traced(channel: Channel) -> Channel {
    match std::env::var_os(TRACE_VARIABLE) {
        Some(_) => rutis_channel::trace::trace(
            channel,
            Arc::new(|line: &str| eprintln!("rutis-interop trace: {line}")),
        ),
        None => channel,
    }
}

/// Whether the Node runtime package at `package` takes an inherited channel:
/// its `package.json` lists `"fd"` in `rutisChannels`. Older packages do
/// not, and dial back.
#[cfg(feature = "node")]
pub(crate) fn node_connect(package: &Path) -> Connect {
    let channels = std::fs::read(package.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|manifest| manifest.get("rutisChannels").cloned());
    match channels {
        Some(serde_json::Value::Array(channels)) if channels.iter().any(|c| c == "fd") => {
            Connect::Inherit
        }
        _ => Connect::DialBack,
    }
}

/// The Node process, owned by a task that records how it ended. Dropping
/// `_kill` ends the process.
pub(crate) struct Child {
    exit: watch::Receiver<Option<String>>,
    /// The same status for the reader thread, which must not depend on the
    /// runtime: a current-thread runtime may be blocked in a synchronous call.
    ended: Arc<(Mutex<Option<String>>, Condvar)>,
    _kill: oneshot::Sender<()>,
}

impl Child {
    pub(crate) fn watch(mut child: tokio::process::Child) -> Self {
        let (kill, killed) = oneshot::channel::<()>();
        let (report, exit) = watch::channel(None);
        let ended = Arc::new((Mutex::new(None), Condvar::new()));
        if let Some(pid) = child.id() {
            let record = ended.clone();
            let _ = std::thread::Builder::new()
                .name("rutis-interop-exit".into())
                .spawn(move || {
                    if let Some(status) = peek_exit(pid) {
                        record_exit(&record, describe(Ok(status)));
                    }
                });
        }
        let record = ended.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = killed => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let status = describe(status);
            record_exit(&record, status.clone());
            report.send_replace(Some(status));
        });
        Self {
            exit,
            ended,
            _kill: kill,
        }
    }

    /// How the process ended, once it has: as soon as either the exit
    /// watcher or the runtime records it, so it agrees with the error that
    /// ended the session.
    pub(crate) fn status(&self) -> Option<String> {
        self.ended.0.lock().unwrap().clone()
    }

    pub(crate) async fn exited(&self) -> String {
        let mut exit = self.exit.clone();
        let status = exit.wait_for(Option::is_some).await;
        status.map_or_else(|_| "is gone".to_owned(), |status| status.clone().unwrap())
    }

    /// The error that ends a session whose peer went away: waits briefly for
    /// the process to end so the error can say how.
    pub(crate) fn disconnected(&self) -> impl FnOnce() -> Error + Send {
        let ended = self.ended.clone();
        move || {
            let (status, changed) = &*ended;
            let status = changed
                .wait_timeout_while(status.lock().unwrap(), Duration::from_secs(1), |status| {
                    status.is_none()
                })
                .unwrap()
                .0
                .clone();
            Error::Transport(match status {
                Some(status) => format!("Cordis process {status}"),
                None => "peer disconnected".to_owned(),
            })
        }
    }
}

fn describe(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) if status.success() => "exited normally".to_owned(),
        Ok(status) => format!("exited with {status}"),
        Err(error) => format!("cannot be waited for: {error}"),
    }
}

fn record_exit(ended: &(Mutex<Option<String>>, Condvar), status: String) {
    ended.0.lock().unwrap().get_or_insert(status);
    ended.1.notify_all();
}

/// Waits until the process `pid` ends and reports its status without reaping
/// it (tokio still does), independently of any runtime.
fn peek_exit(pid: u32) -> Option<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `info` is a valid, writable siginfo_t.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None; // already reaped: the runtime reports it
        }
    }
    // SAFETY: waitid filled a SIGCHLD siginfo_t.
    let status = unsafe { info.si_status() };
    // Rebuild the raw wait status so it formats like `ExitStatus`.
    let raw = match info.si_code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_DUMPED => status | 0x80,
        _ => status,
    };
    Some(std::process::ExitStatus::from_raw(raw))
}
