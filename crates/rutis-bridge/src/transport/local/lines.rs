//! Newline framing for byte-stream transports (Unix sockets, inherited
//! fds): the channel appends `\n` to each message it sends and strips it
//! from each one it receives. Messages must not contain a raw newline; the
//! session codec escapes newlines inside strings and emits none between
//! tokens.

use std::io::{BufRead, BufReader, IoSlice, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::channel::{Channel, ChannelError, ChannelInfo, Closer, Receiver, Sender};

/// Frame a byte stream as a [`Channel`]. `read` and `write` are the two
/// directions of one stream (for a socket, two handles of it); `closer`
/// must wake a thread blocked on either.
pub fn channel(
    read: impl Read + Send + 'static,
    write: impl Write + Send + 'static,
    closer: Arc<dyn Closer>,
    info: ChannelInfo,
) -> Channel {
    let ended = Arc::new(AtomicBool::new(false));
    Channel {
        sender: Box::new(LineSender(write, ended.clone())),
        receiver: Box::new(LineReceiver(BufReader::new(read), ended)),
        closer,
        info,
    }
}

/// The channel has no half-close: once its receiver saw the end, sending
/// fails too. A stream does not say so by itself everywhere: on macOS a
/// socket whose far end shut it down still takes writes, and drops them.
struct LineSender<W>(W, Arc<AtomicBool>);

impl<W: Write + Send> Sender for LineSender<W> {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        if self.1.load(Ordering::SeqCst) {
            return Err(closed("the channel ended"));
        }
        if message.contains(&b'\n') {
            return Err(closed("message contains a raw newline"));
        }
        // The message and its newline in one write where the stream takes
        // both, without copying the message.
        let mut parts = [IoSlice::new(message), IoSlice::new(b"\n")];
        let mut parts = &mut parts[..];
        while !parts.is_empty() {
            match self.0.write_vectored(parts) {
                Ok(0) => return Err(closed("stream closed while sending")),
                Ok(written) => IoSlice::advance_slices(&mut parts, written),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(closed(error)),
            }
        }
        self.0.flush().map_err(closed)
    }
}

struct LineReceiver<R>(BufReader<R>, Arc<AtomicBool>);

impl<R: Read + Send> Receiver for LineReceiver<R> {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut line = Vec::new();
        let read = self.0.read_until(b'\n', &mut line).map_err(closed);
        let received = match read.map(|_| line.pop()) {
            Ok(Some(b'\n')) => return Ok(Some(line)),
            Ok(None) => Ok(None),
            Ok(Some(_)) => Err(closed("stream ended inside a message")),
            Err(error) => Err(error),
        };
        self.1.store(true, Ordering::SeqCst);
        received
    }
}

fn closed(reason: impl std::fmt::Display) -> ChannelError {
    ChannelError::Closed {
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Nothing;
    impl Closer for Nothing {
        fn close(&self, _: &str) {}
    }

    fn receive(bytes: &[u8]) -> Channel {
        channel(
            Cursor::new(bytes.to_vec()),
            Vec::new(),
            Arc::new(Nothing),
            ChannelInfo::default(),
        )
    }

    #[test]
    fn strips_one_newline_per_message() {
        let mut channel = receive(b"{\"a\":1}\n\n{}\n");
        assert_eq!(channel.receiver.recv().unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(channel.receiver.recv().unwrap().unwrap(), b"");
        assert_eq!(channel.receiver.recv().unwrap().unwrap(), b"{}");
        assert_eq!(channel.receiver.recv().unwrap(), None);
    }

    #[test]
    fn a_truncated_message_is_an_error_not_an_end() {
        let mut channel = receive(b"{\"a\"");
        assert!(matches!(
            channel.receiver.recv(),
            Err(ChannelError::Closed { .. })
        ));
    }

    #[test]
    fn appends_the_newline_and_refuses_raw_ones() {
        let mut sender = LineSender(Vec::new(), Arc::default());
        sender.send(b"{}").unwrap();
        assert_eq!(sender.0, b"{}\n");
        assert!(sender.send(b"a\nb").is_err());
        assert_eq!(sender.0, b"{}\n");
    }

    /// A stream that takes every write after its far end went (as a macOS
    /// socket does): the channel still refuses to send once it saw the end.
    #[test]
    fn sending_fails_once_the_end_was_received() {
        let mut channel = receive(b"{}\n");
        assert_eq!(channel.receiver.recv().unwrap().unwrap(), b"{}");
        channel.sender.send(b"{}").unwrap();
        assert_eq!(channel.receiver.recv().unwrap(), None);
        assert!(channel.sender.send(b"{}").is_err());
    }
}
