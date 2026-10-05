//! The in-memory transport: bounded channels within one process, for tests
//! and in-process links. Applications use it only when configured to.
//!
//! [`pair`] makes two connected channels. [`MemoryPlugin`] provides
//! `Transport#memory`, whose `dial(name)` reaches a [`Listener`] opened
//! with [`MemoryTransport::listen`]. Unloading the plugin closes every
//! channel and listener it made.

use std::collections::{HashMap, VecDeque};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};
use rutis_bridge::{
    transport_key, Credential, Dial, Presented, Refusal, Registered, Registration,
    RegistrationError, Registrations, Transport,
};
use rutis_channel::{Channel, ChannelError, ChannelInfo, Closer, ConnectError, Receiver, Sender};

/// Messages buffered per direction before `send` blocks.
pub const CAPACITY: usize = 64;

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<u8>>,
    /// The sending end finished; the receiver drains, then sees the end.
    finished: bool,
    /// Closed by either end: no more sends, receivers stop.
    closed: Option<String>,
}

#[derive(Default)]
struct Direction {
    state: Mutex<State>,
    changed: Condvar,
}

impl Direction {
    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_all();
    }
}

struct MemorySender(Arc<Direction>);
impl Sender for MemorySender {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(reason) = &state.closed {
                return Err(ChannelError::Closed {
                    reason: reason.clone(),
                });
            }
            if state.queue.len() < CAPACITY {
                state.queue.push_back(message.to_vec());
                self.0.changed.notify_all();
                return Ok(());
            }
            state = self.0.changed.wait(state).unwrap();
        }
    }
}
impl Drop for MemorySender {
    fn drop(&mut self) {
        self.0.update(|state| state.finished = true);
    }
}

struct MemoryReceiver {
    incoming: Arc<Direction>,
    /// Set when this end closed itself: its own receive reports the close.
    own: Arc<Mutex<Option<String>>>,
}
impl Receiver for MemoryReceiver {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut state = self.incoming.state.lock().unwrap();
        loop {
            if let Some(reason) = self.own.lock().unwrap().clone() {
                return Err(ChannelError::Closed { reason });
            }
            if let Some(message) = state.queue.pop_front() {
                self.incoming.changed.notify_all();
                return Ok(Some(message));
            }
            if state.finished || state.closed.is_some() {
                return Ok(None);
            }
            state = self.incoming.changed.wait(state).unwrap();
        }
    }
}
impl Drop for MemoryReceiver {
    fn drop(&mut self) {
        self.incoming
            .update(|state| state.closed = Some("receiver dropped".into()));
    }
}

struct MemoryCloser {
    incoming: Arc<Direction>,
    outgoing: Arc<Direction>,
    own: Arc<Mutex<Option<String>>>,
}
impl Closer for MemoryCloser {
    fn close(&self, reason: &str) {
        self.own
            .lock()
            .unwrap()
            .get_or_insert_with(|| reason.to_owned());
        for direction in [&self.incoming, &self.outgoing] {
            direction.update(|state| {
                state.closed.get_or_insert_with(|| reason.to_owned());
            });
        }
    }
}

fn end(incoming: &Arc<Direction>, outgoing: &Arc<Direction>, label: &str) -> Channel {
    let own = Arc::new(Mutex::new(None));
    Channel {
        sender: Box::new(MemorySender(outgoing.clone())),
        receiver: Box::new(MemoryReceiver {
            incoming: incoming.clone(),
            own: own.clone(),
        }),
        closer: Arc::new(MemoryCloser {
            incoming: incoming.clone(),
            outgoing: outgoing.clone(),
            own,
        }),
        info: ChannelInfo {
            transport: "memory",
            peer: None,
            label: label.to_owned(),
        },
    }
}

/// Two channels joined end to end: what one sends, the other receives.
pub fn pair() -> (Channel, Channel) {
    let (a, b) = (Arc::default(), Arc::default());
    (end(&a, &b, ""), end(&b, &a, ""))
}

/// Provides `Transport#memory`. Clones share one transport, so a test can
/// listen on the plugin it mounts, or mount it in two applications that
/// talk in-process.
#[derive(Clone, Default)]
pub struct MemoryPlugin {
    transport: Arc<MemoryTransport>,
}

impl MemoryPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    /// A plugin providing an existing transport.
    pub fn with_transport(transport: Arc<MemoryTransport>) -> Self {
        Self { transport }
    }

    pub fn transport(&self) -> &Arc<MemoryTransport> {
        &self.transport
    }
}

impl Plugin for MemoryPlugin {
    fn name(&self) -> &str {
        "rutis-bridge/memory"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let transport = self.transport.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    transport.close_all();
                    Ok(())
                }))
            })?;
            ctx.provide_as::<dyn Transport>(transport_key("memory"), self.transport.clone())?;
            Ok(Effect::Done)
        })
    }
}

/// Named in-process listeners, and the channels dialed to them.
///
/// Besides plain listeners ([`MemoryTransport::listen`]), links register on
/// named endpoints ([`MemoryTransport::endpoint`]): a dial to one presents
/// the dialing identity's bearer token, and is routed like a network
/// connection, to the registration whose identity verifies it.
#[derive(Default)]
pub struct MemoryTransport {
    listeners: Mutex<HashMap<String, mpsc::Sender<Channel>>>,
    /// Endpoints links may register on: name -> the endpoint they serve.
    endpoints: Mutex<HashMap<String, rutis_channel::PeerId>>,
    registrations: Registrations,
    open: Mutex<Vec<Weak<dyn Closer>>>,
}

/// Accepts the channels dialed to one name; dropping it stops listening.
pub struct Listener {
    name: String,
    transport: Weak<MemoryTransport>,
    accepted: mpsc::Receiver<Channel>,
}

impl Listener {
    /// Block until the next channel arrives; `None` once the transport is
    /// closed.
    pub fn accept(&self) -> Option<Channel> {
        self.accepted.recv().ok()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(transport) = self.transport.upgrade() {
            transport.listeners.lock().unwrap().remove(&self.name);
        }
    }
}

impl MemoryTransport {
    /// Listen on `name`; a name has one listener at a time.
    pub fn listen(self: &Arc<Self>, name: &str) -> Result<Listener, ConnectError> {
        let mut listeners = self.listeners.lock().unwrap();
        if listeners.contains_key(name) {
            return Err(ConnectError::Incompatible {
                reason: format!("memory:{name} already has a listener"),
            });
        }
        let (sender, accepted) = mpsc::channel();
        listeners.insert(name.to_owned(), sender);
        Ok(Listener {
            name: name.to_owned(),
            transport: Arc::downgrade(self),
            accepted,
        })
    }

    /// Accept registrations on `name` for endpoint `local`, as a network
    /// transport's configured listener.
    pub fn endpoint(&self, name: &str, local: rutis_channel::PeerId) {
        self.endpoints
            .lock()
            .unwrap()
            .insert(name.to_owned(), local);
    }

    fn track(&self, channels: [&Channel; 2]) {
        let mut open = self.open.lock().unwrap();
        open.retain(|closer| closer.strong_count() > 0);
        for channel in channels {
            open.push(Arc::downgrade(&channel.closer));
        }
    }

    fn close_all(&self) {
        self.listeners.lock().unwrap().clear();
        self.registrations.clear();
        for closer in std::mem::take(&mut *self.open.lock().unwrap()) {
            if let Some(closer) = closer.upgrade() {
                closer.close("transport unloaded");
            }
        }
    }
}

impl MemoryTransport {
    fn dial_endpoint(&self, dial: &Dial) -> Result<Channel, ConnectError> {
        let address = dial.address.as_str();
        let token = match (&dial.identity, &dial.peer) {
            (Some(identity), Some(peer)) => match identity.credential(peer) {
                Some(Credential::Bearer(token)) => Some(token),
                _ => None,
            },
            _ => None,
        };
        let Some(token) = token else {
            return Err(ConnectError::AuthRejected {
                reason: format!("memory:{address}: credentials required"),
            });
        };
        let ticket = self
            .registrations
            .route(address, Presented::Bearer(&token))
            .map_err(|refusal| match refusal {
                Refusal::NotListening => ConnectError::Retryable {
                    reason: format!("memory:{address}: not listening for this peer yet"),
                },
                Refusal::Unauthenticated => ConnectError::AuthRejected {
                    reason: format!("memory:{address}: not accepted here"),
                },
                Refusal::Ambiguous => ConnectError::AuthRejected {
                    reason: format!("memory:{address}: ambiguous credentials"),
                },
            })?;
        if !dial.protocol.is_empty() && dial.protocol != ticket.protocol {
            return Err(ConnectError::Incompatible {
                reason: format!("memory:{address} speaks {}", ticket.protocol),
            });
        }
        let (a, b) = (Arc::default(), Arc::default());
        let (mut dialed, mut accepted) = (end(&a, &b, address), end(&b, &a, address));
        dialed.info.peer = dial.peer.clone();
        accepted.info.peer = Some(ticket.peer.clone());
        self.track([&dialed, &accepted]);
        match self.registrations.hand_over(&ticket, accepted) {
            Ok(()) => Ok(dialed),
            Err(_) => Err(ConnectError::AuthRejected {
                reason: format!("memory:{address}: registration revoked"),
            }),
        }
    }
}

impl Transport for MemoryTransport {
    fn kind(&self) -> &str {
        "memory"
    }

    fn dial<'a>(&'a self, dial: &'a Dial) -> BoxFuture<'a, Result<Channel, ConnectError>> {
        Box::pin(async move {
            let address = dial.address.as_str();
            if self.endpoints.lock().unwrap().contains_key(address) {
                return self.dial_endpoint(dial);
            }
            let listener = self.listeners.lock().unwrap().get(address).cloned();
            let listener = listener.ok_or_else(|| ConnectError::Retryable {
                reason: format!("nothing listens on memory:{address}"),
            })?;
            let (a, b) = (Arc::default(), Arc::default());
            let (mut dialed, accepted) = (end(&a, &b, address), end(&b, &a, address));
            dialed.info.peer = dial.peer.clone();
            self.track([&dialed, &accepted]);
            listener
                .send(accepted)
                .map_err(|_| ConnectError::Retryable {
                    reason: format!("memory:{address} stopped listening"),
                })?;
            Ok(dialed)
        })
    }

    fn register(&self, registration: Registration) -> Result<Registered, RegistrationError> {
        let local = self
            .endpoints
            .lock()
            .unwrap()
            .get(&registration.listener)
            .cloned()
            .ok_or_else(|| {
                RegistrationError::NoListener(format!(
                    "no memory endpoint named {}",
                    registration.listener
                ))
            })?;
        self.registrations.register(&local, registration)
    }
}

#[cfg(test)]
mod tests;
