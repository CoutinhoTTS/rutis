//! Unix socket channels for the local runtime processes, and the session
//! constructors that took a socket before sessions ran on any
//! [`Channel`](rutis_channel::Channel).
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use rutis_channel::{lines, Channel, ChannelError, ChannelInfo, Closer, Receiver};

use crate::rpc::{Connection, Dispatch};
use crate::Error;

struct Shut(UnixStream);
impl Closer for Shut {
    fn close(&self, _reason: &str) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// A newline-framed channel on a connected Unix socket.
pub(crate) fn channel(stream: UnixStream, label: &str) -> Result<Channel, Error> {
    let transport = |error: std::io::Error| Error::Transport(error.to_string());
    stream.set_nonblocking(false).map_err(transport)?;
    let reader = stream.try_clone().map_err(transport)?;
    let closer = Arc::new(Shut(stream.try_clone().map_err(transport)?));
    Ok(lines::channel(
        reader,
        stream,
        closer,
        ChannelInfo {
            transport: "unix",
            peer: None,
            label: label.to_owned(),
        },
    ))
}

/// Ends the channel, however it ends, with the error `disconnected` builds.
struct Disconnected {
    receiver: Box<dyn Receiver>,
    disconnected: Option<Box<dyn FnOnce() -> Error + Send>>,
}
impl Receiver for Disconnected {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        match self.receiver.recv() {
            Ok(Some(message)) => Ok(Some(message)),
            _ => Err(ChannelError::Closed {
                reason: match self.disconnected.take() {
                    Some(disconnected) => disconnected().to_string(),
                    None => "peer disconnected".into(),
                },
            }),
        }
    }
}

/// Replace how the channel reports its end: `disconnected` runs on the
/// reader thread once the far end is gone, and may block.
pub(crate) fn on_disconnect(
    mut channel: Channel,
    disconnected: Box<dyn FnOnce() -> Error + Send>,
) -> Channel {
    channel.receiver = Box::new(Disconnected {
        receiver: channel.receiver,
        disconnected: Some(disconnected),
    });
    channel
}

impl Connection {
    /// A session on a connected Unix socket: [`Connection::open`] on its
    /// newline-framed channel.
    pub fn connect(stream: UnixStream, dispatch: Arc<dyn Dispatch>) -> Result<Self, Error> {
        Self::connect_with(
            stream,
            dispatch,
            Box::new(|| Error::Transport("peer disconnected".into())),
        )
    }

    /// Like [`Connection::connect`]; `disconnected` builds the error that
    /// ends the session when the peer goes away, for example with the exit
    /// status of its process. It runs on the reader thread and may block.
    pub fn connect_with(
        stream: UnixStream,
        dispatch: Arc<dyn Dispatch>,
        disconnected: Box<dyn FnOnce() -> Error + Send>,
    ) -> Result<Self, Error> {
        Self::open(on_disconnect(channel(stream, "")?, disconnected), dispatch)
    }
}
