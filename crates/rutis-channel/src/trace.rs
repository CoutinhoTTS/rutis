//! A channel that reports what crosses it, for debugging: direction,
//! length and how it ended, never the content (messages carry evaluated
//! configuration, which may hold secrets).

use std::sync::Arc;

use crate::{Channel, ChannelError, Closer, Receiver, Sender};

/// Where trace lines go.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

/// Wrap `channel` so every send, receive and close is reported to `sink`
/// under the channel's label.
pub fn trace(channel: Channel, sink: Sink) -> Channel {
    let label: Arc<str> = match channel.info.label.as_str() {
        "" => channel.info.transport.into(),
        label => format!("{} {label}", channel.info.transport).into(),
    };
    Channel {
        sender: Box::new(Traced {
            inner: channel.sender,
            label: label.clone(),
            sink: sink.clone(),
        }),
        receiver: Box::new(Traced {
            inner: channel.receiver,
            label: label.clone(),
            sink: sink.clone(),
        }),
        closer: Arc::new(Traced {
            inner: channel.closer,
            label,
            sink,
        }),
        info: channel.info,
    }
}

struct Traced<T> {
    inner: T,
    label: Arc<str>,
    sink: Sink,
}

impl Sender for Traced<Box<dyn Sender>> {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        let result = self.inner.send(message);
        match &result {
            Ok(()) => (self.sink)(&format!("{}: sent {} bytes", self.label, message.len())),
            Err(error) => (self.sink)(&format!("{}: send failed: {error}", self.label)),
        }
        result
    }
}

impl Receiver for Traced<Box<dyn Receiver>> {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let result = self.inner.recv();
        match &result {
            Ok(Some(message)) => {
                (self.sink)(&format!("{}: received {} bytes", self.label, message.len()))
            }
            Ok(None) => (self.sink)(&format!("{}: ended", self.label)),
            Err(error) => (self.sink)(&format!("{}: receive failed: {error}", self.label)),
        }
        result
    }
}

impl Closer for Traced<Arc<dyn Closer>> {
    fn close(&self, reason: &str) {
        (self.sink)(&format!("{}: closed: {reason}", self.label));
        self.inner.close(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Loop(Arc<Mutex<Vec<Vec<u8>>>>);
    impl Sender for Loop {
        fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
            self.0.lock().unwrap().push(message.to_vec());
            Ok(())
        }
    }
    impl Receiver for Loop {
        fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
            Ok(self.0.lock().unwrap().pop())
        }
    }
    struct Nothing;
    impl Closer for Nothing {
        fn close(&self, _: &str) {}
    }

    #[test]
    fn reports_sizes_and_ends_but_never_content() {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let record = lines.clone();
        let mut channel = trace(
            Channel {
                sender: Box::new(Loop(queue.clone())),
                receiver: Box::new(Loop(queue)),
                closer: Arc::new(Nothing),
                info: crate::ChannelInfo {
                    transport: "memory",
                    peer: None,
                    label: "peer mac".into(),
                },
            },
            Arc::new(move |line: &str| record.lock().unwrap().push(line.to_owned())),
        );
        channel.sender.send(b"{\"token\":\"secret\"}").unwrap();
        channel.receiver.recv().unwrap();
        channel.receiver.recv().unwrap();
        channel.closer.close("done");
        let lines = lines.lock().unwrap();
        assert_eq!(
            *lines,
            [
                "memory peer mac: sent 18 bytes",
                "memory peer mac: received 18 bytes",
                "memory peer mac: ended",
                "memory peer mac: closed: done",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains("secret")));
    }
}
