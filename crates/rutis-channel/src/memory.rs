//! In-process channel pairs, for tests and for two peers in one process.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use crate::{Channel, ChannelError, ChannelInfo, Closer, Receiver, Sender};

/// Two connected channels. Each direction holds up to `capacity` messages
/// (at least one); a full direction blocks its sender. Closing either end
/// ends both, and both report its reason once the queued messages are read.
pub fn pair(capacity: usize) -> (Channel, Channel) {
    let capacity = capacity.max(1);
    let one = Arc::new(Pipe::new(capacity));
    let two = Arc::new(Pipe::new(capacity));
    (end(one.clone(), two.clone()), end(two, one))
}

fn end(outgoing: Arc<Pipe>, incoming: Arc<Pipe>) -> Channel {
    Channel {
        sender: Box::new(Out(outgoing.clone())),
        receiver: Box::new(In(incoming.clone())),
        closer: Arc::new(Both(outgoing, incoming)),
        info: ChannelInfo::new("memory"),
    }
}

/// One direction.
struct Pipe {
    capacity: usize,
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    queue: VecDeque<Vec<u8>>,
    closed: Option<String>,
}

impl Pipe {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(State {
                queue: VecDeque::new(),
                closed: None,
            }),
            changed: Condvar::new(),
        }
    }

    /// The first reason wins.
    fn close(&self, reason: &str) {
        self.state
            .lock()
            .unwrap()
            .closed
            .get_or_insert_with(|| reason.to_owned());
        self.changed.notify_all();
    }
}

struct Out(Arc<Pipe>);

impl Sender for Out {
    fn send(&mut self, message: Vec<u8>) -> Result<(), ChannelError> {
        let pipe = &self.0;
        let mut state = pipe.state.lock().unwrap();
        loop {
            if let Some(reason) = &state.closed {
                return Err(ChannelError::closed(reason.clone()));
            }
            if state.queue.len() < pipe.capacity {
                break;
            }
            state = pipe.changed.wait(state).unwrap();
        }
        state.queue.push_back(message);
        drop(state);
        pipe.changed.notify_all();
        Ok(())
    }
}

/// Without a sender the peer reads what is queued, then the end.
impl Drop for Out {
    fn drop(&mut self) {
        self.0.close("the peer dropped its channel");
    }
}

struct In(Arc<Pipe>);

impl Receiver for In {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let pipe = &self.0;
        let mut state = pipe.state.lock().unwrap();
        loop {
            if let Some(message) = state.queue.pop_front() {
                drop(state);
                pipe.changed.notify_all();
                return Ok(Some(message));
            }
            if let Some(reason) = &state.closed {
                return Err(ChannelError::closed(reason.clone()));
            }
            state = pipe.changed.wait(state).unwrap();
        }
    }
}

/// Without a receiver the peer's sends fail instead of filling up.
impl Drop for In {
    fn drop(&mut self) {
        self.0.close("the peer dropped its channel");
    }
}

struct Both(Arc<Pipe>, Arc<Pipe>);

impl Closer for Both {
    fn close(&self, reason: &str) {
        self.0.close(reason);
        self.1.close(reason);
    }
}
