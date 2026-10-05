//! A channel that misbehaves on command, for tests of what sits on top:
//! slow delivery, a far end that vanishes, a connection that stays open but
//! carries nothing (half-open).

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{Channel, ChannelError, Closer, Receiver, Sender};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    /// Every message waits this long before it is sent or delivered.
    Delay(Duration),
    /// Messages go nowhere and none arrive; nothing closes.
    HalfOpen,
}

#[derive(Default)]
struct State {
    /// The mode, and whether the channel was dropped.
    mode: Mutex<(Mode, bool)>,
    changed: Condvar,
}

/// Switches the faults of a channel made by [`fault`].
#[derive(Clone)]
pub struct Faults {
    state: Arc<State>,
    closer: Arc<dyn Closer>,
}

impl Faults {
    fn set(&self, mode: Mode) {
        self.state.mode.lock().unwrap().0 = mode;
        self.state.changed.notify_all();
    }

    /// Deliver normally again.
    pub fn heal(&self) {
        self.set(Mode::Normal);
    }

    /// Delay every message by `delay`, in both directions.
    pub fn delay(&self, delay: Duration) {
        self.set(Mode::Delay(delay));
    }

    /// Stop carrying messages without closing: sends succeed and vanish,
    /// receives wait. Messages swallowed meanwhile are lost.
    pub fn half_open(&self) {
        self.set(Mode::HalfOpen);
    }

    /// Drop whatever is in flight and close, as a far end that disappears.
    pub fn drop_and_close(&self, reason: &str) {
        self.state.mode.lock().unwrap().1 = true;
        self.state.changed.notify_all();
        self.closer.close(reason);
    }
}

/// Wrap `channel`; the returned [`Faults`] switches its behaviour.
pub fn fault(channel: Channel) -> (Channel, Faults) {
    let state = Arc::new(State::default());
    let faults = Faults {
        state: state.clone(),
        closer: channel.closer.clone(),
    };
    let closer: Arc<dyn Closer> = Arc::new(FaultCloser {
        inner: channel.closer,
        state: state.clone(),
    });
    let channel = Channel {
        sender: Box::new(FaultSender {
            inner: channel.sender,
            state: state.clone(),
        }),
        receiver: Box::new(FaultReceiver {
            inner: channel.receiver,
            state,
        }),
        closer,
        info: channel.info,
    };
    (channel, faults)
}

fn dropped() -> ChannelError {
    ChannelError::Closed {
        reason: "dropped by fault injection".into(),
    }
}

struct FaultSender {
    inner: Box<dyn Sender>,
    state: Arc<State>,
}
impl Sender for FaultSender {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        let (mode, dropping) = *self.state.mode.lock().unwrap();
        if dropping {
            return Err(dropped());
        }
        match mode {
            Mode::Normal => self.inner.send(message),
            Mode::Delay(delay) => {
                std::thread::sleep(delay);
                self.inner.send(message)
            }
            Mode::HalfOpen => Ok(()),
        }
    }
}

struct FaultReceiver {
    inner: Box<dyn Receiver>,
    state: Arc<State>,
}
impl Receiver for FaultReceiver {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        loop {
            let message = self.inner.recv()?;
            let mut guard = self.state.mode.lock().unwrap();
            if guard.1 {
                return Err(dropped());
            }
            match guard.0 {
                Mode::Normal => return Ok(message),
                Mode::Delay(delay) => {
                    drop(guard);
                    std::thread::sleep(delay);
                    return Ok(message);
                }
                Mode::HalfOpen => {
                    // Swallowed. Wait until healed or dropped; then read on.
                    while guard.0 == Mode::HalfOpen && !guard.1 {
                        guard = self.state.changed.wait(guard).unwrap();
                    }
                    if guard.1 {
                        return Err(dropped());
                    }
                    if message.is_none() {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

struct FaultCloser {
    inner: Arc<dyn Closer>,
    state: Arc<State>,
}
impl Closer for FaultCloser {
    fn close(&self, reason: &str) {
        self.state.mode.lock().unwrap().1 = true;
        self.state.changed.notify_all();
        self.inner.close(reason);
    }
}
