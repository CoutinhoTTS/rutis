//! A multiplexing adapter, for tests only: many logical channels over one
//! physical connection (itself a memory channel pair), each with its own
//! credit-based flow control, authentication and registration. No
//! production transport multiplexes yet; this checks that nothing above
//! the channel assumes one connection per channel
//! (`docs/design-protocol-channel-decoupling-2026-10-03.md`, logical
//! channels and physical connections).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use rutis::{BoxFuture, Ctx};
use rutis_bridge::{
    peer_key, transport_key, Credential, Dial, Failure, IdentityPlugin, LinkConfig, LinkPlugin,
    Peer, Presented, Refusal, Registered, Registration, RegistrationError, Registrations, Retry,
    StaticIdentity, Transport,
};
use rutis_channel::{
    Channel, ChannelError, ChannelInfo, Closer, ConnectError, PeerId, Receiver, Sender,
};

/// Messages a logical channel may have in flight before the far end
/// consumed them.
const WINDOW: usize = 8;

enum Frame {
    Open {
        channel: u32,
        listener: String,
        token: String,
        protocol: String,
    },
    Accepted {
        channel: u32,
        peer: String,
    },
    Refused {
        channel: u32,
        failure: Failure,
        reason: String,
    },
    Data {
        channel: u32,
        payload: Vec<u8>,
    },
    Credit {
        channel: u32,
    },
    Close {
        channel: u32,
    },
}

impl Frame {
    fn encode(&self) -> Vec<u8> {
        let (kind, channel, rest): (u8, u32, Vec<u8>) = match self {
            Frame::Open {
                channel,
                listener,
                token,
                protocol,
            } => (
                0,
                *channel,
                format!("{listener}\n{token}\n{protocol}").into_bytes(),
            ),
            Frame::Accepted { channel, peer } => (1, *channel, peer.clone().into_bytes()),
            Frame::Refused {
                channel,
                failure,
                reason,
            } => {
                let mut rest = vec![match failure {
                    Failure::Retryable => 0,
                    Failure::AuthRejected => 1,
                    Failure::Incompatible => 2,
                }];
                rest.extend_from_slice(reason.as_bytes());
                (2, *channel, rest)
            }
            Frame::Data { channel, payload } => (3, *channel, payload.clone()),
            Frame::Credit { channel } => (4, *channel, Vec::new()),
            Frame::Close { channel } => (5, *channel, Vec::new()),
        };
        let mut bytes = vec![kind];
        bytes.extend_from_slice(&channel.to_be_bytes());
        bytes.extend_from_slice(&rest);
        bytes
    }

    fn decode(bytes: &[u8]) -> Frame {
        let channel = u32::from_be_bytes(bytes[1..5].try_into().unwrap());
        let rest = bytes[5..].to_vec();
        let text = || String::from_utf8(rest.clone()).unwrap();
        match bytes[0] {
            0 => {
                let text = text();
                let mut parts = text.splitn(3, '\n');
                Frame::Open {
                    channel,
                    listener: parts.next().unwrap().into(),
                    token: parts.next().unwrap().into(),
                    protocol: parts.next().unwrap().into(),
                }
            }
            1 => Frame::Accepted {
                channel,
                peer: text(),
            },
            2 => Frame::Refused {
                channel,
                failure: match rest[0] {
                    0 => Failure::Retryable,
                    1 => Failure::AuthRejected,
                    2 => Failure::Incompatible,
                    _ => panic!("invalid refusal failure"),
                },
                reason: String::from_utf8(rest[1..].to_vec()).unwrap(),
            },
            3 => Frame::Data {
                channel,
                payload: rest,
            },
            4 => Frame::Credit { channel },
            _ => Frame::Close { channel },
        }
    }
}

#[test]
fn refusal_classification_is_independent_of_diagnostics() {
    for failure in [
        Failure::Retryable,
        Failure::AuthRejected,
        Failure::Incompatible,
    ] {
        for reason in [
            "protocol",
            "not listening",
            "not accepted",
            "",
            "诊断\nchanged",
        ] {
            let encoded = Frame::Refused {
                channel: 42,
                failure,
                reason: reason.into(),
            }
            .encode();
            let Frame::Refused {
                channel,
                failure: decoded_failure,
                reason: decoded_reason,
            } = Frame::decode(&encoded)
            else {
                panic!("expected refusal");
            };
            assert_eq!(channel, 42);
            assert_eq!(decoded_failure, failure);
            assert_eq!(decoded_reason, reason);
        }
    }
}

#[derive(Default)]
struct LogicalState {
    inbox: VecDeque<Vec<u8>>,
    credits: usize,
    ended: Option<Result<(), String>>,
}

/// One logical channel's state, shared by its ends and the demultiplexer.
#[derive(Default)]
struct Logical {
    state: Mutex<LogicalState>,
    changed: Condvar,
}

impl Logical {
    fn end(&self, how: Result<(), String>) {
        self.state.lock().unwrap().ended.get_or_insert(how);
        self.changed.notify_all();
    }
}

/// One physical connection and the logical channels on it.
struct Physical {
    sender: Mutex<Box<dyn Sender>>,
    closer: Arc<dyn Closer>,
    channels: Mutex<HashMap<u32, Arc<Logical>>>,
    pending: Mutex<HashMap<u32, std::sync::mpsc::Sender<Result<String, (Failure, String)>>>>,
    next: AtomicU32,
    alive: Mutex<bool>,
}

impl Physical {
    fn send(&self, frame: Frame) -> Result<(), ChannelError> {
        self.sender.lock().unwrap().send(&frame.encode())
    }

    /// The physical connection failed: every logical channel on it hears.
    fn fail(&self, reason: &str) {
        *self.alive.lock().unwrap() = false;
        for (_, logical) in self.channels.lock().unwrap().drain() {
            logical.end(Err(reason.to_owned()));
        }
        for (_, waiting) in self.pending.lock().unwrap().drain() {
            let _ = waiting.send(Err((Failure::AuthRejected, reason.to_owned())));
        }
    }

    fn logical(self: &Arc<Self>, id: u32, peer: Option<PeerId>) -> Channel {
        let logical = Arc::new(Logical::default());
        logical.state.lock().unwrap().credits = WINDOW;
        self.channels.lock().unwrap().insert(id, logical.clone());
        Channel {
            sender: Box::new(LogicalSender {
                id,
                logical: logical.clone(),
                physical: Arc::downgrade(self),
            }),
            receiver: Box::new(LogicalReceiver {
                id,
                logical: logical.clone(),
                physical: Arc::downgrade(self),
            }),
            closer: Arc::new(LogicalCloser {
                id,
                logical,
                physical: Arc::downgrade(self),
            }),
            info: ChannelInfo {
                transport: "mux",
                peer,
                label: format!("mux #{id}"),
            },
        }
    }
}

struct LogicalSender {
    id: u32,
    logical: Arc<Logical>,
    physical: Weak<Physical>,
}
impl Sender for LogicalSender {
    fn send(&mut self, message: &[u8]) -> Result<(), ChannelError> {
        let mut state = self.logical.state.lock().unwrap();
        loop {
            if let Some(ended) = &state.ended {
                return Err(ChannelError::Closed {
                    reason: ended.clone().err().unwrap_or_else(|| "closed".into()),
                });
            }
            if state.credits > 0 {
                state.credits -= 1;
                break;
            }
            // Out of credit: this channel waits; the others go on.
            state = self.logical.changed.wait(state).unwrap();
        }
        drop(state);
        let physical = self.physical.upgrade().ok_or(ChannelError::Closed {
            reason: "gone".into(),
        })?;
        physical.send(Frame::Data {
            channel: self.id,
            payload: message.to_vec(),
        })
    }
}

struct LogicalReceiver {
    id: u32,
    logical: Arc<Logical>,
    physical: Weak<Physical>,
}
impl Receiver for LogicalReceiver {
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut state = self.logical.state.lock().unwrap();
        loop {
            if let Some(message) = state.inbox.pop_front() {
                drop(state);
                // Consumed: the far end may send one more.
                if let Some(physical) = self.physical.upgrade() {
                    let _ = physical.send(Frame::Credit { channel: self.id });
                }
                return Ok(Some(message));
            }
            match &state.ended {
                Some(Ok(())) => return Ok(None),
                Some(Err(reason)) => {
                    return Err(ChannelError::Closed {
                        reason: reason.clone(),
                    })
                }
                None => state = self.logical.changed.wait(state).unwrap(),
            }
        }
    }
}

struct LogicalCloser {
    id: u32,
    logical: Arc<Logical>,
    physical: Weak<Physical>,
}
impl Closer for LogicalCloser {
    fn close(&self, reason: &str) {
        self.logical.end(Err(reason.to_owned()));
        // Only this logical channel: the physical connection stays.
        if let Some(physical) = self.physical.upgrade() {
            physical.channels.lock().unwrap().remove(&self.id);
            let _ = physical.send(Frame::Close { channel: self.id });
        }
    }
}

/// One end of the adapter: dials over a shared physical connection to the
/// other end, which routes each logical channel to its registration.
#[derive(Default)]
struct Mux {
    /// The physical connection to the far end (this end dials it).
    physical: Mutex<Option<Arc<Physical>>>,
    /// The far end, to make a new physical connection with when needed.
    far: Mutex<Weak<Mux>>,
    registrations: Registrations,
    local: Mutex<Option<PeerId>>,
    physicals_made: AtomicUsize,
}

impl Mux {
    /// Join `a` and `b` with physical connections made on demand.
    fn join(a: &Arc<Mux>, b: &Arc<Mux>) {
        *a.far.lock().unwrap() = Arc::downgrade(b);
        *b.far.lock().unwrap() = Arc::downgrade(a);
    }

    /// The physical connection `dial` uses: the current one, or a new one
    /// (only when the old one died; a dead channel is never revived).
    fn connection(self: &Arc<Self>) -> Arc<Physical> {
        let mut physical = self.physical.lock().unwrap();
        if let Some(live) = physical.as_ref().filter(|p| *p.alive.lock().unwrap()) {
            return live.clone();
        }
        let far = self.far.lock().unwrap().upgrade().expect("the far end");
        let (ours, theirs) = rutis_transport_memory::pair();
        self.physicals_made.fetch_add(1, Ordering::SeqCst);
        let made = Self::run(ours, None);
        Self::run(theirs, Some(far));
        *physical = Some(made.clone());
        made
    }

    /// Start a physical connection's demultiplexer; `router` answers opens.
    fn run(channel: Channel, router: Option<Arc<Mux>>) -> Arc<Physical> {
        let Channel {
            sender,
            mut receiver,
            closer,
            ..
        } = channel;
        let physical = Arc::new(Physical {
            sender: Mutex::new(sender),
            closer,
            channels: Mutex::default(),
            pending: Mutex::default(),
            next: AtomicU32::new(1),
            alive: Mutex::new(true),
        });
        let reading = physical.clone();
        std::thread::spawn(move || {
            while let Ok(Some(message)) = receiver.recv() {
                let channel_of = |id: u32| reading.channels.lock().unwrap().get(&id).cloned();
                match Frame::decode(&message) {
                    Frame::Data { channel, payload } => {
                        if let Some(logical) = channel_of(channel) {
                            let mut state = logical.state.lock().unwrap();
                            assert!(state.inbox.len() < WINDOW, "the far end overran its credit");
                            state.inbox.push_back(payload);
                            logical.changed.notify_all();
                        }
                    }
                    Frame::Credit { channel } => {
                        if let Some(logical) = channel_of(channel) {
                            logical.state.lock().unwrap().credits += 1;
                            logical.changed.notify_all();
                        }
                    }
                    Frame::Close { channel } => {
                        if let Some(logical) = reading.channels.lock().unwrap().remove(&channel) {
                            logical.end(Ok(()));
                        }
                    }
                    Frame::Accepted { channel, peer } => {
                        if let Some(waiting) = reading.pending.lock().unwrap().remove(&channel) {
                            let _ = waiting.send(Ok(peer));
                        }
                    }
                    Frame::Refused {
                        channel,
                        failure,
                        reason,
                    } => {
                        if let Some(waiting) = reading.pending.lock().unwrap().remove(&channel) {
                            let _ = waiting.send(Err((failure, reason)));
                        }
                    }
                    Frame::Open {
                        channel,
                        listener,
                        token,
                        protocol,
                    } => {
                        let Some(router) = router.as_ref() else {
                            continue;
                        };
                        // Each logical channel authenticates and routes on
                        // its own: sharing a connection shares no rights.
                        let ticket = router
                            .registrations
                            .route(&listener, Presented::Bearer(&token));
                        match ticket {
                            Ok(ticket) if ticket.protocol == protocol => {
                                let logical = reading.logical(channel, Some(ticket.peer.clone()));
                                let _ = reading.send(Frame::Accepted {
                                    channel,
                                    peer: ticket.peer.to_string(),
                                });
                                if let Err(logical) =
                                    router.registrations.hand_over(&ticket, logical)
                                {
                                    logical.closer.close("registration revoked");
                                }
                            }
                            Ok(_) => {
                                let _ = reading.send(Frame::Refused {
                                    channel,
                                    failure: Failure::Incompatible,
                                    reason: "protocol".into(),
                                });
                            }
                            // As the real transports: good credentials
                            // nobody listens for yet are worth retrying.
                            Err(Refusal::NotListening) => {
                                let _ = reading.send(Frame::Refused {
                                    channel,
                                    failure: Failure::Retryable,
                                    reason: "not listening".into(),
                                });
                            }
                            Err(_) => {
                                let _ = reading.send(Frame::Refused {
                                    channel,
                                    failure: Failure::AuthRejected,
                                    reason: "not accepted".into(),
                                });
                            }
                        }
                    }
                }
            }
            reading.fail("the physical connection ended");
        });
        physical
    }
}

impl Transport for Mux {
    fn kind(&self) -> &str {
        "mux"
    }

    fn dial<'a>(&'a self, dial: &'a Dial) -> BoxFuture<'a, Result<Channel, ConnectError>> {
        Box::pin(async move {
            let this = self
                .far
                .lock()
                .unwrap()
                .upgrade()
                .and_then(|far| far.far.lock().unwrap().upgrade());
            let this = this.expect("joined");
            let physical = this.connection();
            let token = match (&dial.identity, &dial.peer) {
                (Some(identity), Some(peer)) => match identity.credential(peer) {
                    Some(Credential::Bearer(token)) => token,
                    _ => String::new(),
                },
                _ => String::new(),
            };
            let id = physical.next.fetch_add(1, Ordering::SeqCst);
            let (answered, answer) = std::sync::mpsc::channel();
            physical.pending.lock().unwrap().insert(id, answered);
            // The logical channel exists before the answer, so no data
            // sent right after acceptance is lost.
            let channel = physical.logical(id, dial.peer.clone());
            physical
                .send(Frame::Open {
                    channel: id,
                    listener: dial.address.clone(),
                    token,
                    protocol: dial.protocol.clone(),
                })
                .map_err(|error| ConnectError::Retryable {
                    reason: error.to_string(),
                })?;
            let answer =
                tokio::task::spawn_blocking(move || answer.recv_timeout(Duration::from_secs(5)))
                    .await
                    .unwrap();
            match answer {
                Ok(Ok(_peer)) => Ok(channel),
                Ok(Err((failure, reason))) => {
                    physical.channels.lock().unwrap().remove(&id);
                    Err(match failure {
                        Failure::Retryable => ConnectError::Retryable { reason },
                        Failure::AuthRejected => ConnectError::AuthRejected { reason },
                        Failure::Incompatible => ConnectError::Incompatible { reason },
                    })
                }
                Err(_) => Err(ConnectError::Retryable {
                    reason: "no answer".into(),
                }),
            }
        })
    }

    fn register(&self, registration: Registration) -> Result<Registered, RegistrationError> {
        let local = self.local.lock().unwrap().clone().expect("a listening end");
        self.registrations.register(&local, registration)
    }
}

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

fn ends() -> (Arc<Mux>, Arc<Mux>) {
    let (a, b) = (Arc::new(Mux::default()), Arc::new(Mux::default()));
    *b.local.lock().unwrap() = Some(id("main"));
    Mux::join(&a, &b);
    (a, b)
}

#[test]
fn logical_channels_meet_the_channel_contract() {
    let (dialer, listener) = ends();
    let count = AtomicU32::new(0);
    let kept = Mutex::new(Vec::new());
    rutis_channel::testing::contract(|| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        let peer = format!("p{n}");
        let (deliver, delivered) = std::sync::mpsc::channel();
        let deliver = Mutex::new(deliver);
        kept.lock().unwrap().push(
            listener
                .register(Registration {
                    listener: "in".into(),
                    peer: id(&peer),
                    identity: Arc::new(
                        StaticIdentity::new(id("main")).accept_token(peer.clone(), id(&peer)),
                    ),
                    protocol: "p".into(),
                    deliver: Box::new(move |channel| {
                        let _ = deliver.lock().unwrap().send(channel);
                    }),
                })
                .unwrap(),
        );
        let identity: Arc<dyn rutis_bridge::Identity> = Arc::new(
            StaticIdentity::new(id(&peer)).present(id("main"), Credential::Bearer(peer.clone())),
        );
        let dialed = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                dialer.dial(
                    &Dial::address("in")
                        .peer(id("main"))
                        .identity(identity)
                        .protocol("p"),
                ),
            )
            .unwrap();
        (
            dialed,
            delivered.recv_timeout(Duration::from_secs(5)).unwrap(),
        )
    });
    // Every pair shared one physical connection.
    assert_eq!(dialer.physicals_made.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn links_share_a_connection_but_not_their_sessions_or_rights() {
    let (mac_side, main_side) = ends();
    let quick = Retry {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
        ..Retry::default()
    };
    // main: one listener, two far ends.
    let main = Ctx::root().unwrap();
    main.provide_as::<dyn Transport>(transport_key("mux"), main_side.clone())
        .unwrap();
    main.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main"))
            .accept_token("mac-token", id("mac"))
            .accept_token("pi-token", id("pi")),
    ));
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("mac"), "mux", "main", "in").retry(quick.clone()),
    ));
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("pi"), "mux", "main", "in").retry(quick.clone()),
    ));
    // mac and pi: two applications on the other end, sharing its adapter,
    // so their links go over the same physical connection.
    let mut far = Vec::new();
    for (name, token) in [("mac", "mac-token"), ("pi", "pi-token")] {
        let root = Ctx::root().unwrap();
        root.provide_as::<dyn Transport>(transport_key("mux"), mac_side.clone())
            .unwrap();
        root.plugin(IdentityPlugin::new(
            name,
            StaticIdentity::new(id(name)).present(id("main"), Credential::Bearer(token.into())),
        ));
        root.plugin(LinkPlugin::new(
            LinkConfig::dial(id("main"), "mux", name, "in").retry(quick.clone()),
        ));
        far.push(root);
    }
    let wait_for = |root: &Ctx, peer: &str| {
        let (root, peer) = (root.clone(), id(peer));
        async move {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(found) = root.get_as::<Peer>(peer_key(&peer)) {
                        return found;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap()
        }
    };
    let mac = wait_for(&main, "mac").await;
    let pi = wait_for(&main, "pi").await;
    assert_eq!(
        mac_side.physicals_made.load(Ordering::SeqCst),
        1,
        "one physical connection"
    );
    assert_eq!(mac.connection().greeting().unwrap().endpoint, id("mac"));
    assert_eq!(pi.connection().greeting().unwrap().endpoint, id("pi"));
    assert_ne!(
        mac.connection().tag(),
        pi.connection().tag(),
        "two sessions"
    );

    // Closing one session's channel leaves the other.
    mac.connection()
        .close(rutis_interop::Error::Transport("closed".into()));
    let mac_again = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(again) = main.get_as::<Peer>(peer_key(&id("mac"))) {
                if again.generation() > mac.generation() {
                    return again;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pi_now = main.get_as::<Peer>(peer_key(&id("pi"))).unwrap();
    assert!(Arc::ptr_eq(&pi, &pi_now), "pi kept its session");
    assert_eq!(
        mac_side.physicals_made.load(Ordering::SeqCst),
        1,
        "the link reconnected over the same connection"
    );
    drop(mac_again);

    // The physical connection fails: both sessions end, neither channel
    // comes back to life; the links redial and the adapter makes a new
    // physical connection for them.
    let physical = mac_side.physical.lock().unwrap().clone().unwrap();
    physical.closer.close("cable cut");
    let (pi_before, mac_before) = (pi.generation(), mac.generation());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pi_now = main
                .get_as::<Peer>(peer_key(&id("pi")))
                .map(|p| p.generation());
            let mac_now = main
                .get_as::<Peer>(peer_key(&id("mac")))
                .map(|p| p.generation());
            if pi_now.is_some_and(|g| g > pi_before) && mac_now.is_some_and(|g| g > mac_before + 1)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        mac_side.physicals_made.load(Ordering::SeqCst),
        2,
        "one new physical connection"
    );
    let old = pi
        .connection()
        .invoke_async("", "anything", rutis_interop::rpc::Value::Undefined)
        .await;
    assert!(
        matches!(old, Err(rutis_interop::Error::Transport(_))),
        "the old session stays ended"
    );
}
