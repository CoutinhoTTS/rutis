//! A link: the connection and session with one far end, kept up.
//!
//! The link dials the far end (and redials, with backoff by failure
//! category) or registers on a listener for it (and lets a newer connection
//! take over). Once the far end greets as the expected endpoint, the link
//! provides `Peer#<id>`; when the session ends it withdraws it, so what
//! depends on the peer stops natively. It knows nothing of rows, schemas
//! or what the families registered on the peer do.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, Plugin, TypeKey};
use rutis_channel::{Channel, Closer, ConnectError, PeerId};
use rutis_interop::rpc::{Connection, Endpoint, Format};
use rutis_interop::{Error, Handshake};
use tokio::sync::{mpsc, watch};

use crate::peer::{Operations, Peer};
use crate::{identity_key, peer_key, transport_key, Dial, Identity, Registration, Transport};

/// The session protocol links speak, as a transport subprotocol.
pub fn protocol() -> String {
    format!("rutis.{}", rutis_interop::ENDPOINT_PROTOCOL)
}

/// How the link reaches its far end.
#[derive(Clone, Debug)]
pub enum Connect {
    /// Dial this address on the transport.
    Dial(String),
    /// Accept the far end on this listener of the transport.
    Listen(String),
}

/// Reconnection, for a dialing link
/// (`docs/design-protocol-channel-decoupling-2026-10-03.md`).
#[derive(Clone, Debug)]
pub struct Retry {
    /// The first wait after a retryable failure; doubled each time.
    pub initial: Duration,
    pub max: Duration,
    /// ± this fraction of each wait, at random.
    pub jitter: f64,
    /// A session lasting this long resets the backoff.
    pub stable: Duration,
    /// The wait after an authentication failure.
    pub rejected: Duration,
    /// How long a far end may take to greet.
    pub handshake: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            jitter: 0.2,
            stable: Duration::from_secs(60),
            rejected: Duration::from_secs(30),
            handshake: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkConfig {
    /// The far end.
    pub peer: PeerId,
    /// The transport's kind (`Transport#<kind>`).
    pub transport: String,
    /// The identity's name (`Identity#<name>`): this endpoint, its
    /// credentials and the rules that verify the far end.
    pub identity: String,
    pub connect: Connect,
    pub retry: Retry,
    /// Capabilities the far end must declare, such as its contract
    /// (`runtime`, `node`); without them the link stops, as for an
    /// incompatible far end.
    pub require: Vec<String>,
    /// Capabilities this end declares besides the session's own and
    /// `node`, which every rutis link declares.
    pub declare: Vec<String>,
}

impl LinkConfig {
    pub fn dial(peer: PeerId, transport: &str, identity: &str, address: &str) -> Self {
        Self {
            peer,
            transport: transport.into(),
            identity: identity.into(),
            connect: Connect::Dial(address.into()),
            retry: Retry::default(),
            require: Vec::new(),
            declare: Vec::new(),
        }
    }

    pub fn listen(peer: PeerId, transport: &str, identity: &str, listener: &str) -> Self {
        Self {
            connect: Connect::Listen(listener.into()),
            ..Self::dial(peer, transport, identity, "")
        }
    }

    pub fn retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    /// Require the far end to declare `capability`.
    pub fn require(mut self, capability: &str) -> Self {
        self.require.push(capability.into());
        self
    }

    /// Declare `capability` to the far end.
    pub fn declare(mut self, capability: &str) -> Self {
        self.declare.push(capability.into());
        self
    }
}

/// Why the link is not ready, by what it means for retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Retryable,
    AuthRejected,
    Incompatible,
}

/// What a link is doing, for diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkState {
    /// Not applied.
    Idle,
    /// Dialing, or waiting for the far end to connect.
    Connecting,
    /// Session `generation` is ready; `Peer#<id>` is provided.
    Ready { generation: u64 },
    /// The last attempt failed; the next comes after `retry_in`.
    Waiting {
        failure: Failure,
        error: String,
        retry_in: Duration,
    },
    /// The far end is incompatible: no more attempts until the link is
    /// restarted or reconfigured.
    Stopped { error: String },
}

/// The link plugin: depends on `Transport#<kind>` and `Identity#<name>`,
/// provides `Peer#<id>` while a session is ready.
pub struct LinkPlugin {
    name: String,
    config: LinkConfig,
    injects: [TypeKey; 2],
    state: Arc<watch::Sender<LinkState>>,
}

impl LinkPlugin {
    pub fn new(config: LinkConfig) -> Self {
        Self {
            name: format!("rutis-bridge/link#{}", config.peer),
            injects: [
                transport_key(&config.transport),
                identity_key(&config.identity),
            ],
            state: Arc::new(watch::channel(LinkState::Idle).0),
            config,
        }
    }

    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.state.subscribe()
    }
}

impl Plugin for LinkPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let missing = |what: &str| {
                CordisError::PluginFailed(format!("{}: {what} is not provided", self.name).into())
            };
            let transport = ctx
                .get_as::<dyn Transport>(self.injects[0].clone())
                .ok_or_else(|| missing("the transport"))?;
            let identity = ctx
                .get_as::<dyn Identity>(self.injects[1].clone())
                .ok_or_else(|| missing("the identity"))?;
            let link = Link {
                ctx: ctx.clone(),
                config: self.config.clone(),
                transport,
                identity,
                state: self.state.clone(),
                generation: 0,
            };
            let task = tokio::spawn(link.run());
            let state = self.state.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    // Dropping the task's state closes the session, revokes
                    // the registration and cancels any retry.
                    task.abort();
                    state.send_replace(LinkState::Idle);
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}

struct Link {
    ctx: Ctx,
    config: LinkConfig,
    transport: Arc<dyn Transport>,
    identity: Arc<dyn Identity>,
    state: Arc<watch::Sender<LinkState>>,
    generation: u64,
}

/// A ready session and what the link holds for it. Dropping it closes the
/// session.
struct Live {
    session: Connection,
    closer: Arc<dyn Closer>,
    operations: Arc<Operations>,
    /// In a mutex only to make `Live` shareable while the link awaits.
    provided: Mutex<Option<Disposer>>,
    since: Instant,
}

impl Live {
    /// Withdraw the peer, then close the session.
    async fn end(self, reason: &str) {
        let provided = self.provided.lock().unwrap().take();
        if let Some(provided) = provided {
            let _ = provided.dispose().await;
        }
        self.session.close(Error::Transport(reason.into()));
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.session.close(Error::Transport("link stopped".into()));
        self.operations.detach();
    }
}

fn classify(error: &Error) -> Failure {
    match error {
        Error::Handshake(Handshake::Incompatible(_)) => Failure::Incompatible,
        Error::Handshake(Handshake::IdentityMismatch(_)) => Failure::AuthRejected,
        _ => Failure::Retryable,
    }
}

impl Link {
    async fn run(mut self) {
        match self.config.connect.clone() {
            Connect::Dial(address) => self.dialing(address).await,
            Connect::Listen(listener) => self.listening(listener).await,
        }
    }

    /// Open a session on `channel` and, once the far end greeted as the
    /// expected endpoint, provide the peer.
    async fn start(&mut self, channel: Channel) -> Result<Live, Error> {
        let closer = channel.closer.clone();
        let operations = Arc::new(Operations::default());
        let mut endpoint =
            Endpoint::rust(self.identity.local().clone()).expect(self.config.peer.clone());
        // A rutis endpoint is a full framework node.
        endpoint.capabilities.push("node".into());
        endpoint
            .capabilities
            .extend(self.config.declare.iter().cloned());
        let format = Format::Endpoint(endpoint);
        let session = Connection::open_with(channel, operations.clone(), format)?;
        let ready = tokio::time::timeout(self.config.retry.handshake, session.ready()).await;
        match ready {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                session.close(error.clone());
                return Err(error);
            }
            Err(_) => {
                let error = Error::Transport("the far end did not greet in time".into());
                session.close(error.clone());
                return Err(error);
            }
        }
        // The contract is checked before use, not guessed from failing calls.
        if let Some(missing) = self
            .config
            .require
            .iter()
            .find(|capability| !session.supports(capability))
        {
            let error = Error::Handshake(Handshake::Incompatible(format!(
                "{} does not declare {missing}",
                self.config.peer
            )));
            session.close(error.clone());
            return Err(error);
        }
        operations.attach(session.clone());
        self.generation += 1;
        let peer = Peer::new(
            self.config.peer.clone(),
            session.clone(),
            self.generation,
            operations.clone(),
        );
        let provided = self
            .ctx
            .provide_as(peer_key(&self.config.peer), Arc::new(peer))
            .map_err(|error| Error::Transport(format!("cannot provide the peer: {error}")))?;
        self.state.send_replace(LinkState::Ready {
            generation: self.generation,
        });
        Ok(Live {
            session,
            closer,
            operations,
            provided: Mutex::new(Some(provided)),
            since: Instant::now(),
        })
    }

    async fn wait(&self, failure: Failure, error: String, retry_in: Duration) {
        self.state.send_replace(LinkState::Waiting {
            failure,
            error,
            retry_in,
        });
        tokio::time::sleep(retry_in).await;
    }

    async fn dialing(&mut self, address: String) {
        let dial = Dial::address(address)
            .peer(self.config.peer.clone())
            .identity(self.identity.clone())
            .protocol(protocol());
        let retry = self.config.retry.clone();
        let mut backoff = Backoff::new(&retry);
        loop {
            self.state.send_replace(LinkState::Connecting);
            let (failure, error) = match self.transport.dial(&dial).await {
                Err(error) => (
                    match error {
                        ConnectError::Retryable { .. } => Failure::Retryable,
                        ConnectError::AuthRejected { .. } => Failure::AuthRejected,
                        ConnectError::Incompatible { .. } => Failure::Incompatible,
                    },
                    error.to_string(),
                ),
                Ok(channel) => match self.start(channel).await {
                    Err(error) => (classify(&error), error.to_string()),
                    Ok(live) => {
                        live.session.closed().await;
                        if live.since.elapsed() >= retry.stable {
                            backoff.reset();
                        }
                        live.end("session ended").await;
                        (Failure::Retryable, "the session ended".into())
                    }
                },
            };
            match failure {
                Failure::Incompatible => {
                    self.state.send_replace(LinkState::Stopped { error });
                    return;
                }
                Failure::AuthRejected => self.wait(failure, error, retry.rejected).await,
                Failure::Retryable => self.wait(failure, error, backoff.next()).await,
            }
        }
    }

    async fn listening(&mut self, listener: String) {
        let (sender, mut accepted) = mpsc::unbounded_channel::<Channel>();
        let registered = self.transport.register(Registration {
            listener,
            peer: self.config.peer.clone(),
            identity: self.identity.clone(),
            protocol: protocol(),
            deliver: Box::new(move |channel| {
                if let Err(refused) = sender.send(channel) {
                    refused.0.closer.close("the link stopped");
                }
            }),
        });
        let _registered = match registered {
            Ok(registered) => registered,
            Err(error) => {
                self.state.send_replace(LinkState::Stopped {
                    error: error.to_string(),
                });
                return;
            }
        };
        self.state.send_replace(LinkState::Connecting);
        let mut current: Option<Live> = None;
        loop {
            let ended = async {
                match &current {
                    Some(live) => live.session.closed().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                channel = accepted.recv() => {
                    let Some(channel) = channel else { return };
                    // A newer connection of the far end takes over: the old
                    // session ends, and its peer is withdrawn, first.
                    if let Some(old) = current.take() {
                        old.closer.replaced();
                        old.end("replaced by a new connection").await;
                    }
                    match self.start(channel).await {
                        Ok(live) => current = Some(live),
                        Err(error) => {
                            self.state.send_replace(LinkState::Waiting {
                                failure: classify(&error),
                                error: error.to_string(),
                                retry_in: Duration::ZERO,
                            });
                        }
                    }
                }
                () = ended => {
                    if let Some(old) = current.take() {
                        old.end("session ended").await;
                    }
                    self.state.send_replace(LinkState::Connecting);
                }
            }
        }
    }
}

/// Exponential backoff with jitter.
struct Backoff {
    initial: Duration,
    max: Duration,
    jitter: f64,
    next: Duration,
    seed: Mutex<u64>,
}

impl Backoff {
    fn new(retry: &Retry) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9e37_79b9_7f4a_7c15, |elapsed| elapsed.as_nanos() as u64)
            | 1;
        Self {
            initial: retry.initial,
            max: retry.max,
            jitter: retry.jitter,
            next: retry.initial,
            seed: Mutex::new(seed),
        }
    }

    fn reset(&mut self) {
        self.next = self.initial;
    }

    /// The next wait: the current step ± jitter; the step doubles up to max.
    fn next(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        let unit = {
            // xorshift64: no cryptographic need, only spreading reconnects.
            let mut seed = self.seed.lock().unwrap();
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            (*seed >> 11) as f64 / (1u64 << 53) as f64
        };
        base.mul_f64(1.0 + self.jitter * (2.0 * unit - 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_its_cap_within_jitter_and_resets() {
        let retry = Retry::default();
        let mut backoff = Backoff::new(&retry);
        let steps: Vec<f64> = (0..10).map(|_| backoff.next().as_secs_f64()).collect();
        let bases = [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0, 30.0];
        for (step, base) in steps.iter().zip(bases) {
            assert!(
                (base * 0.8..=base * 1.2).contains(step),
                "{step} is not within 20% of {base}"
            );
        }
        backoff.reset();
        assert!(backoff.next() <= Duration::from_millis(600));
    }
}
