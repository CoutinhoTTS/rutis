//! A bounded queue between a blocking side (the channel's caller) and an
//! async side (the connection task). Either end may close it, waking both.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};

use rutis_channel::ChannelError;
use tokio::sync::Notify;

/// How a pipe ended, as its consumer sees it once drained.
#[derive(Clone, Debug)]
pub(crate) enum Ending {
    /// The far end finished normally.
    Finished,
    Failed(String),
}

struct State {
    queue: VecDeque<Vec<u8>>,
    bytes: usize,
    ending: Option<Ending>,
}

pub(crate) struct Pipe {
    state: Mutex<State>,
    capacity: usize,
    /// Wakes blocking waiters.
    changed: Condvar,
    /// Wakes the async side.
    notify: Notify,
}

impl Pipe {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                bytes: 0,
                ending: None,
            }),
            capacity,
            changed: Condvar::new(),
            notify: Notify::new(),
        }
    }

    fn has_room(&self, state: &State, size: usize) -> bool {
        // One message always fits, however large.
        state.queue.is_empty() || state.bytes + size <= self.capacity
    }

    fn admit(&self, state: &mut State, message: Vec<u8>) {
        state.bytes += message.len();
        state.queue.push_back(message);
        self.changed.notify_all();
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    fn take(&self, state: &mut State) -> Option<Vec<u8>> {
        let message = state.queue.pop_front()?;
        state.bytes -= message.len();
        self.changed.notify_all();
        self.notify.notify_waiters();
        self.notify.notify_one();
        Some(message)
    }

    /// Queue `message`, blocking while the pipe is full.
    pub(crate) fn push_blocking(&self, message: &[u8]) -> Result<(), ChannelError> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(ending) = &state.ending {
                return Err(closed(ending));
            }
            if self.has_room(&state, message.len()) {
                self.admit(&mut state, message.to_vec());
                return Ok(());
            }
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Queue `message`, waiting while the pipe is full. `false` once closed.
    pub(crate) async fn push(&self, message: Vec<u8>) -> bool {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().unwrap();
                if state.ending.is_some() {
                    return false;
                }
                if self.has_room(&state, message.len()) {
                    self.admit(&mut state, message);
                    return true;
                }
            }
            notified.await;
        }
    }

    /// The next message, blocking while there is none. `Ok(None)` once
    /// drained after a normal end.
    pub(crate) fn pop_blocking(&self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(message) = self.take(&mut state) {
                return Ok(Some(message));
            }
            match &state.ending {
                Some(Ending::Finished) => return Ok(None),
                Some(ending) => return Err(closed(ending)),
                None => state = self.changed.wait(state).unwrap(),
            }
        }
    }

    /// The next message, waiting while there is none; `None` once closed.
    /// Messages still queued at the close are dropped.
    pub(crate) async fn pop(&self) -> Option<Vec<u8>> {
        loop {
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().unwrap();
                if state.ending.is_some() {
                    return None;
                }
                if let Some(message) = self.take(&mut state) {
                    return Some(message);
                }
            }
            notified.await;
        }
    }

    /// End the pipe; the first ending wins.
    pub(crate) fn close(&self, ending: Ending) {
        let mut state = self.state.lock().unwrap();
        state.ending.get_or_insert(ending);
        self.changed.notify_all();
        self.notify.notify_waiters();
        self.notify.notify_one();
    }
}

fn closed(ending: &Ending) -> ChannelError {
    ChannelError::Closed {
        reason: match ending {
            Ending::Finished => "closed".into(),
            Ending::Failed(reason) => reason.clone(),
        },
    }
}
