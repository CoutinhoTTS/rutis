//! The Unix socket channel of the `Process` compatibility facade, and the
//! session constructors that took a socket before sessions ran on any
//! [`Channel`](rutis_channel::Channel).
//!
//! Transports belong to the transport crates (`rutis-transport-local`);
//! this copy exists only because the facade still starts its own processes.
//! It goes once runtimes get their sessions through local + link (N2).
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use rutis_channel::{Channel, ChannelError, ChannelInfo, Closer, Receiver, Sender};

use crate::rpc::{Connection, Dispatch};
use crate::Error;

struct Shut(UnixStream);
impl Closer for Shut {
    fn close(&self, _reason: &str) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

/// One message per line, as the runtimes have always spoken.
struct Lines(UnixStream);
impl Sender for Lines {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        let mut line = Vec::with_capacity(message.len() + 1);
        line.extend_from_slice(message);
        line.push(b'\n');
        self.0
            .write_all(&line)
            .map_err(|error| ChannelError::Closed {
                reason: error.to_string(),
            })
    }
}
struct LinesIn(BufReader<UnixStream>);
impl Receiver for LinesIn {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut line = Vec::new();
        match self.0.read_until(b'\n', &mut line) {
            Ok(0) => Ok(None),
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                Ok(Some(line))
            }
            Err(error) => Err(ChannelError::Closed {
                reason: error.to_string(),
            }),
        }
    }
}

/// A newline-framed channel on a connected Unix socket.
pub(crate) fn channel(stream: UnixStream, label: &str) -> Result<Channel, Error> {
    let transport = |error: std::io::Error| Error::Transport(error.to_string());
    stream.set_nonblocking(false).map_err(transport)?;
    let reader = stream.try_clone().map_err(transport)?;
    let closer = Arc::new(Shut(stream.try_clone().map_err(transport)?));
    Ok(Channel {
        sender: Box::new(Lines(stream)),
        receiver: Box::new(LinesIn(BufReader::new(reader))),
        closer,
        info: ChannelInfo {
            transport: "unix",
            peer: None,
            label: label.to_owned(),
        },
    })
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
