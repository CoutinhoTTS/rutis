//! A ready session with one far end, as a link provides it (`Peer#<id>`).
//!
//! The session is the link's; what runs on it is registered here by
//! family. A call from the far end goes to the handler of its family:
//! the part of a control method before its first `.` (`rows.load` →
//! `rows`), the bare control method (`service`), or the part of a target
//! before its first `:` (`host:clock` → `host`). A family nobody registered
//! answers with an error, and the session goes on. The families registered
//! here are announced to the far end as `link.offers`; what the far end
//! offers is observable.
//!
//! Each family offered carries the version at which it was registered
//! (`since`): a family withdrawn and registered again between two looks is
//! a new offer, which a follower must not mistake for the old one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};

use rutis::TypeKey;
use rutis_channel::PeerId;
use rutis_interop::rpc::{Connection, Dispatch, Reply, Value};
use rutis_interop::Error;
use serde_json::json;
use tokio::sync::watch;

/// Serves one family of operations on a peer's session.
pub trait Handler: Send + Sync + 'static {
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: Value) -> Reply;
}

impl<F> Handler for F
where
    F: Fn(&Connection, &str, &str, Value) -> Reply + Send + Sync + 'static,
{
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: Value) -> Reply {
        self(peer, target, method, args)
    }
}

/// The families an end offers, ordered by `version`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Offers {
    pub families: BTreeSet<String>,
    pub version: u64,
    /// When each family was registered (a version). An end that does not
    /// say has every family at 0.
    pub since: BTreeMap<String, u64>,
}

impl Offers {
    /// Which offer of `family` this is, if it is offered: it changes
    /// whenever the family is registered anew, even if a follower never saw
    /// it withdrawn.
    pub fn epoch(&self, family: &str) -> Option<u64> {
        self.families
            .contains(family)
            .then(|| self.since.get(family).copied().unwrap_or(0))
    }
}

/// The family a call belongs to.
pub fn family<'a>(target: &'a str, method: &'a str) -> &'a str {
    match target {
        "" => method.split('.').next().unwrap_or(method),
        target => target.split(':').next().unwrap_or(target),
    }
}

/// The handlers of one session; its [`Dispatch`].
#[derive(Default)]
pub(crate) struct Operations {
    handlers: Mutex<HashMap<String, Arc<dyn Handler>>>,
    /// What the far end offers.
    remote: watch::Sender<Offers>,
    /// What this end offers, and how often that changed.
    local: Mutex<Offers>,
    session: Mutex<Option<Connection>>,
    announcing: Mutex<bool>,
    /// Versions of what features say about the session (services announced
    /// and withdrawn): one sequence for the session, so a feature that
    /// restarts keeps counting up and is never taken for an older message.
    versions: std::sync::atomic::AtomicU64,
}

impl Operations {
    /// Serve `session`; `announce` tells the far end what is offered (a
    /// compat far end, a local runtime, takes no `link.offers`).
    pub(crate) fn attach(&self, session: Connection, announce: bool) {
        *self.session.lock().unwrap() = Some(session);
        *self.announcing.lock().unwrap() = announce;
        self.announce();
    }

    /// Forget the session, which holds this as its dispatch.
    pub(crate) fn detach(&self) {
        self.session.lock().unwrap().take();
        self.handlers.lock().unwrap().clear();
    }

    /// Tell the far end what this end offers now. Fire and forget: a lost
    /// announcement is superseded by the next, and a closed session ends
    /// the question.
    fn announce(&self) {
        if !*self.announcing.lock().unwrap() {
            return;
        }
        let Some(session) = self.session.lock().unwrap().clone() else {
            return;
        };
        let offers = {
            let mut local = self.local.lock().unwrap();
            local.version += 1;
            local.families = self.handlers.lock().unwrap().keys().cloned().collect();
            local.clone()
        };
        let since: BTreeMap<&String, u64> = offers
            .families
            .iter()
            .map(|family| (family, offers.since.get(family).copied().unwrap_or(0)))
            .collect();
        let args =
            json!([{ "families": offers.families, "version": offers.version, "since": since }]);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = session.invoke_async("", "link.offers", args.into()).await;
            });
        }
    }
}

impl Dispatch for Operations {
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: Value) -> Reply {
        if target.is_empty() && method == "link.offers" {
            let announced = args
                .list()?
                .into_iter()
                .next()
                .ok_or_else(|| Error::Value("link.offers needs its offers".into()))?
                .json()?;
            let families: BTreeSet<String> = rutis_interop::decode(announced["families"].clone())?;
            let version: u64 = rutis_interop::decode(announced["version"].clone())?;
            let since: BTreeMap<String, u64> = match announced.get("since") {
                Some(since) => rutis_interop::decode(since.clone())?,
                None => BTreeMap::new(),
            };
            // An older announcement never overrides a newer one.
            self.remote.send_if_modified(|offers| {
                if version <= offers.version {
                    return false;
                }
                *offers = Offers {
                    families,
                    version,
                    since,
                };
                true
            });
            return Ok(Value::Undefined);
        }
        let family = family(target, method);
        let handler = self.handlers.lock().unwrap().get(family).cloned();
        match handler {
            Some(handler) => handler.invoke(peer, target, method, args),
            None => Err(Error::Value(format!(
                "{} is not offered here",
                if target.is_empty() { method } else { target }
            ))),
        }
    }
}

/// A session with one far end, ready: the far end greeted as `id`.
pub struct Peer {
    id: PeerId,
    connection: Connection,
    generation: u64,
    operations: Arc<Operations>,
}

/// The key the peer `id` is provided under (`Peer#mac`).
pub fn peer_key(id: &PeerId) -> TypeKey {
    TypeKey::keyed_dynamic::<Peer>(id.to_string())
}

/// A family registered on a peer; dropping it unregisters the family and
/// tells the far end.
#[must_use = "dropping the registration unregisters the family"]
pub struct Offered {
    operations: Weak<Operations>,
    family: String,
}

impl Drop for Offered {
    fn drop(&mut self) {
        if let Some(operations) = self.operations.upgrade() {
            if operations
                .handlers
                .lock()
                .unwrap()
                .remove(&self.family)
                .is_some()
            {
                operations.announce();
            }
        }
    }
}

impl Peer {
    pub(crate) fn new(
        id: PeerId,
        connection: Connection,
        generation: u64,
        operations: Arc<Operations>,
    ) -> Self {
        Self {
            id,
            connection,
            generation,
            operations,
        }
    }

    /// The far end's endpoint id.
    pub fn id(&self) -> &PeerId {
        &self.id
    }

    /// The session, to call the far end.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Which session of this link this is: each new session (a reconnect, a
    /// takeover) has a higher one. Resources of an older one never apply to
    /// a newer one.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Serve `family` on this session. A family has one handler at a time.
    pub fn register(&self, family: &str, handler: Arc<dyn Handler>) -> Result<Offered, Error> {
        if family.is_empty() || family == "link" || family.contains(['.', ':']) {
            return Err(Error::Value(format!("{family:?} cannot be registered")));
        }
        {
            let mut handlers = self.operations.handlers.lock().unwrap();
            if handlers.contains_key(family) {
                return Err(Error::Value(format!("{family} is already registered")));
            }
            handlers.insert(family.to_owned(), handler);
        }
        {
            // A new offer of the family, whatever was offered before. (The
            // locks are taken one at a time: `announce` takes them in the
            // other order.)
            let mut local = self.operations.local.lock().unwrap();
            local.version += 1;
            let version = local.version;
            local.since.insert(family.to_owned(), version);
        }
        self.operations.announce();
        Ok(Offered {
            operations: Arc::downgrade(&self.operations),
            family: family.to_owned(),
        })
    }

    /// The next version in this session's sequence: higher than any taken
    /// before on it, whichever feature took it.
    pub fn next_version(&self) -> u64 {
        self.operations
            .versions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// What the far end offers, as it announces it.
    pub fn offers(&self) -> watch::Receiver<Offers> {
        self.operations.remote.subscribe()
    }
}
