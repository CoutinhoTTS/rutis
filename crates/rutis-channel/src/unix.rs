//! Unix stream sockets, one message per line.

use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use crate::{Channel, ChannelError, ChannelInfo, Closer, Receiver, Sender};

impl Channel {
    /// A Unix stream socket carrying one message per line. Messages must not
    /// contain `\n`; compact JSON never does.
    pub fn unix(stream: UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(false)?;
        let reader = stream.try_clone()?;
        let closer = stream.try_clone()?;
        Ok(Self {
            sender: Box::new(Lines(stream)),
            receiver: Box::new(LineReader(BufReader::new(reader))),
            closer: Arc::new(Stop(closer)),
            info: ChannelInfo::new("unix"),
        })
    }
}

struct Lines(UnixStream);

impl Sender for Lines {
    fn send(&mut self, mut message: Vec<u8>) -> Result<(), ChannelError> {
        debug_assert!(
            !message.contains(&b'\n'),
            "a line-framed message contains a newline"
        );
        message.push(b'\n');
        self.0
            .write_all(&message)
            .map_err(|error| ChannelError::closed(error.to_string()))
    }
}

struct LineReader(BufReader<UnixStream>);

impl Receiver for LineReader {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut line = Vec::new();
        match self.0.read_until(b'\n', &mut line) {
            // A failed read ends the stream, as end of file does.
            Ok(0) | Err(_) => Ok(None),
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                }
                Ok(Some(line))
            }
        }
    }
}

/// Shuts the socket down, which wakes a thread blocked reading or writing
/// it. A stream cannot carry the reason.
struct Stop(UnixStream);

impl Closer for Stop {
    fn close(&self, _reason: &str) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}
