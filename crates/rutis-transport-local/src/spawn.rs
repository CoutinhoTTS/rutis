//! Starting processes on a channel: the process gets one end of a socket
//! pair as fd 3 (`fd:3`), or, when it cannot take one, a socket path in a
//! private directory that it dials back. The channel owns the process:
//! closing it, or dropping all of it, ends the process, and its end says how
//! the process ended.
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rutis_channel::{Channel, ChannelError, ChannelInfo, Closer, ConnectError, PeerId, Receiver};
use tokio::sync::oneshot;

use crate::lines;

/// The fd a process finds its channel on (`fd:3`).
pub const CHANNEL_FD: i32 = 3;

/// A process `spawn:<name>` starts: `program args… <channel> trailing…`,
/// where `<channel>` is `fd:3` or the socket path to dial.
#[derive(Clone, Debug)]
pub struct Spawn {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    /// The working directory; the application's by default.
    pub cwd: Option<PathBuf>,
    pub handover: Handover,
    /// Arguments after the channel.
    pub trailing: Vec<OsString>,
    /// The endpoint the process is: whoever starts a process names it.
    pub peer: PeerId,
}

/// How the process gets its channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handover {
    /// One end of a socket pair, as fd 3.
    Inherit,
    /// A socket path the process dials.
    DialBack,
}

impl Spawn {
    pub fn new(program: impl Into<OsString>, peer: PeerId) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            handover: Handover::Inherit,
            trailing: Vec::new(),
            peer,
        }
    }

    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }
}

fn retryable(error: std::io::Error) -> ConnectError {
    ConnectError::Retryable {
        reason: error.to_string(),
    }
}

/// Start `spawn` and connect it.
pub(crate) async fn start(spawn: &Spawn) -> Result<Channel, ConnectError> {
    let (channel, child, directory) = match spawn.handover {
        Handover::Inherit => inherit(spawn)?,
        Handover::DialBack => dial_back(spawn).await?,
    };
    let Channel {
        sender,
        receiver,
        closer,
        mut info,
    } = channel;
    info.peer = Some(spawn.peer.clone());
    Ok(Channel {
        sender,
        receiver: Box::new(Ended {
            receiver,
            ended: Some(child.ended.clone()),
        }),
        closer: Arc::new(Owning {
            closer,
            child,
            _directory: directory,
        }),
        info,
    })
}

type Started = (Channel, Child, Option<tempfile::TempDir>);

fn inherit(spawn: &Spawn) -> Result<Started, ConnectError> {
    let (ours, theirs) = UnixStream::pair().map_err(retryable)?;
    let fd = theirs.as_raw_fd();
    let mut command = spawn.command();
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
        .args(&spawn.trailing)
        .spawn()
        .map_err(|error| cannot_start(spawn, error))?;
    // The child has its copy; ours would keep the channel open after it exits.
    drop(theirs);
    Ok((socket(ours, "fd")?, Child::watch(child), None))
}

async fn dial_back(spawn: &Spawn) -> Result<Started, ConnectError> {
    let directory = tempfile::Builder::new()
        .prefix("rutis-spawn-")
        .tempdir()
        .map_err(retryable)?;
    let path = directory.path().join("peer.sock");
    let listener = tokio::net::UnixListener::bind(&path).map_err(retryable)?;
    let mut child = spawn
        .command()
        .arg(&path)
        .args(&spawn.trailing)
        .spawn()
        .map_err(|error| cannot_start(spawn, error))?;
    let stream = tokio::select! {
        accepted = listener.accept() => accepted.map_err(retryable)?.0,
        status = child.wait() => return Err(ConnectError::Retryable {
            reason: format!("the process exited before connecting: {}", describe(status)),
        }),
    };
    let stream = stream.into_std().map_err(retryable)?;
    Ok((
        socket(stream, "unix")?,
        Child::watch(child),
        Some(directory),
    ))
}

/// A program that cannot be started is configuration, not a passing fault.
fn cannot_start(spawn: &Spawn, error: std::io::Error) -> ConnectError {
    let reason = format!("cannot start {}: {error}", spawn.program.to_string_lossy());
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
            ConnectError::Incompatible { reason }
        }
        _ => ConnectError::Retryable { reason },
    }
}

fn socket(stream: UnixStream, transport: &'static str) -> Result<Channel, ConnectError> {
    stream.set_nonblocking(false).map_err(retryable)?;
    let reader = stream.try_clone().map_err(retryable)?;
    let closer = Arc::new(crate::unix::Shut(stream.try_clone().map_err(retryable)?));
    Ok(lines::channel(
        reader,
        stream,
        closer,
        ChannelInfo {
            transport,
            peer: None,
            label: String::new(),
        },
    ))
}

/// A closer that owns its process: dropped, it ends the process.
struct Owning {
    closer: Arc<dyn Closer>,
    child: Child,
    _directory: Option<tempfile::TempDir>,
}

impl Closer for Owning {
    fn close(&self, reason: &str) {
        self.closer.close(reason);
        self.child.end();
    }
}

/// Ends the channel, however it ends, with how the process ended.
struct Ended {
    receiver: Box<dyn Receiver>,
    ended: Option<Arc<Exit>>,
}

impl Receiver for Ended {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        match self.receiver.recv() {
            Ok(Some(message)) => Ok(Some(message)),
            _ => Err(ChannelError::Closed {
                reason: match self.ended.take().and_then(|ended| ended.wait()) {
                    Some(status) => format!("the process {status}"),
                    None => "the process disconnected".into(),
                },
            }),
        }
    }
}

/// How the process ended, once it has. Readable from the reader thread,
/// which must not depend on the async runtime: a current-thread runtime may
/// be blocked in a synchronous call.
#[derive(Default)]
struct Exit(Mutex<Option<String>>, Condvar);

impl Exit {
    fn record(&self, status: String) {
        self.0.lock().unwrap().get_or_insert(status);
        self.1.notify_all();
    }

    /// Waits for the process to end, so the channel's end can say how: a
    /// process that closed its channel ends soon, one whose channel was
    /// closed within the grace period.
    fn wait(&self) -> Option<String> {
        let wait = GRACE + Duration::from_secs(1);
        self.1
            .wait_timeout_while(self.0.lock().unwrap(), wait, |status| status.is_none())
            .unwrap()
            .0
            .clone()
    }
}

/// How long a process whose channel closed may take to end by itself.
const GRACE: Duration = Duration::from_secs(2);

/// The process, owned by a task that records how it ended. Dropped, it
/// kills the process; [`Child::end`] gives it [`GRACE`] first.
struct Child {
    ended: Arc<Exit>,
    end: Mutex<Option<oneshot::Sender<()>>>,
}

impl Child {
    fn watch(mut child: tokio::process::Child) -> Self {
        let (kill, killed) = oneshot::channel::<()>();
        let ended = Arc::new(Exit::default());
        if let Some(pid) = child.id() {
            let record = ended.clone();
            let _ = std::thread::Builder::new()
                .name("rutis-spawn-exit".into())
                .spawn(move || {
                    if let Some(status) = peek_exit(pid) {
                        record.record(describe(Ok(status)));
                    }
                });
        }
        let record = ended.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                ended = killed => {
                    // Its channel closed: it may end by itself.
                    if ended.is_ok() {
                        if let Ok(status) = tokio::time::timeout(GRACE, child.wait()).await {
                            record.record(describe(status));
                            return;
                        }
                    }
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            record.record(describe(status));
        });
        Self {
            ended,
            end: Mutex::new(Some(kill)),
        }
    }

    /// End the process: its channel closed, so it should end by itself;
    /// it is killed if it has not after [`GRACE`].
    fn end(&self) {
        if let Some(end) = self.end.lock().unwrap().take() {
            let _ = end.send(());
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
