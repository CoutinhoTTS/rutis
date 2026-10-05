//! The local connector: starts a runtime process that dials back over a
//! Unix socket in a private directory, and watches how the process ends.
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rutis_channel::Channel;
use tokio::sync::{oneshot, watch};

use crate::Error;

/// A started runtime process and the channel it connected.
pub(crate) struct Spawned {
    pub channel: Channel,
    pub child: Child,
    /// Holds the socket; removed when the process is dropped.
    pub directory: tempfile::TempDir,
}

/// Start `command` with the socket path and `first` (the first plugin or
/// the anchor) as its last two arguments, and wait for it to connect. The
/// channel ends with how the process ended (`Cordis process exited …`).
pub(crate) async fn spawn(
    mut command: tokio::process::Command,
    first: &Path,
) -> Result<Spawned, Error> {
    let transport = |error: std::io::Error| Error::Transport(error.to_string());
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
        channel,
        child,
        directory,
    })
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

    /// How the process ended, once it has.
    pub(crate) fn status(&self) -> Option<String> {
        self.exit.borrow().clone()
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
