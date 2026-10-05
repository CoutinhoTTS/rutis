//! Relays: references of one session forwarded to another.
//!
//! A reference imported on session A and sent to session B is exported on
//! B as a relay of the same kind; B's calls, reads and awaits on it are
//! forwarded to A with the call chain rebased (`rebase`). The relay holds
//! the import, so releasing the relay releases the original on A. A relay
//! coming back to Rust is the import itself, and an import sent back to its
//! own session goes home as before.

use super::*;

/// A call forwarded through a relay.
pub(super) enum Relayed {
    Call(Option<String>, Value),
    Get(String),
    Await,
}

impl Connection {
    /// An import goes back to its own session as a home reference; to any
    /// other session as a relay of the same kind exported here. One relay
    /// per import, so the peer sees one identity however often it is sent.
    pub(super) fn encode_import(
        &self,
        import: &Arc<Import>,
        grants: &mut Vec<u64>,
    ) -> Result<WireValue, Error> {
        if Weak::ptr_eq(&import.peer, &Arc::downgrade(&self.0)) {
            return Ok(WireValue::Reference {
                id: import.id,
                kind: import.kind,
                home: true,
                origin: import.origin.clone(),
            });
        }
        let identity = Arc::as_ptr(import) as usize;
        {
            let mut exports = self.0.exports.lock().unwrap();
            if let Some(id) = exports.relays.get(&identity).copied() {
                return grant(&mut exports, id, grants);
            }
        }
        let source = import.connection()?;
        // Forwarded calls run on the background executor: they only wait for
        // the other session, and must progress while this connection's own
        // runtime is blocked by a synchronous call.
        let executor = self.background()?;
        // Peers accept only their own native ids in an origin. The source
        // session's entries are added back when an await is forwarded.
        let origin = rebase(&import.origin, source.tag(), self.tag())
            .into_iter()
            .filter(|entry| !entry.contains('/'))
            .collect();
        let relay = Arc::new(Object {
            executor,
            business: true,
            origin,
            body: Body::Relay(import.clone()),
        });
        let mut exports = self.0.exports.lock().unwrap();
        // Another thread may have created the relay meanwhile.
        let id = match exports.relays.get(&identity) {
            Some(id) => *id,
            None => {
                let id = allocate(&mut exports, relay)?;
                exports.relays.insert(identity, id);
                id
            }
        };
        grant(&mut exports, id, grants)
    }
    pub(super) fn export_object(&self, id: u64) -> Result<Arc<Object>, Error> {
        let object = self.export(id)?;
        if object.kind() != Kind::Object {
            return Err(transport(
                "method call or property read on a non-object reference",
            ));
        }
        Ok(object)
    }
    /// Forward a call on a relay to the reference it stands in for, with the
    /// chain rebased to that reference's session. A call pumped by a
    /// synchronous waiter is forwarded synchronously on the same thread, so
    /// reverse calls keep reaching that waiter. Any other call is forwarded
    /// asynchronously and cancelled with the incoming call.
    pub(super) fn relay(
        &self,
        id: String,
        path: Vec<String>,
        object: Arc<Object>,
        call: Relayed,
        sync: bool,
        flight: Option<Flight>,
    ) {
        let Body::Relay(import) = &object.body else {
            unreachable!("relayed calls target relays")
        };
        let expected = match &call {
            Relayed::Call(None, _) => Kind::Function,
            Relayed::Call(Some(_), _) | Relayed::Get(_) => Kind::Object,
            Relayed::Await => Kind::Future,
        };
        if import.kind != expected {
            // The owner would fail its whole session on a mismatched kind.
            return self.respond(id, Err(Error::Value("reference kind mismatch".into())));
        }
        let target = match import.connection() {
            Ok(target) => target,
            Err(error) => return self.respond(id, Err(error)),
        };
        let operation = match call {
            Relayed::Call(method, args) => Operation::Call(import.id, method, args),
            Relayed::Get(property) => Operation::Get(import.id, property),
            Relayed::Await => Operation::Await(import.id, import.origin.clone()),
        };
        let forwarded = rebase(&path, self.tag(), target.tag());
        if sync {
            // Why blocking here cannot deadlock across the two sessions:
            // - this thread is a synchronous waiter pumping its own chain
            //   (`sync` is set only there; checked below), so a reverse call
            //   from `target` that belongs to this chain is delivered to this
            //   very thread by path, and runs nested instead of waiting;
            // - no lock of this session is held here: `execute` runs after
            //   `receive` released `calls` and `exports`, and `request_sync`
            //   takes `target`'s locks only briefly, never while waiting.
            // Forwarding synchronously from anywhere else (an ordinary task,
            // or while holding a lock) would break both; such callers must
            // take the asynchronous path below.
            debug_assert!(
                Pumping::active(),
                "synchronous relay forwarding outside a synchronous waiter"
            );
            let result = {
                let _path = PathGuard::enter(forwarded);
                target.request_sync(operation)
            };
            self.respond(id, result);
            drop(flight);
            return;
        }
        let (cancel, cancelled) = oneshot::channel();
        {
            let mut calls = self.0.calls.lock().unwrap();
            if calls.closed.is_some() {
                return;
            }
            calls.awaiting.insert(
                id.clone(),
                Awaiting {
                    object: object.clone(),
                    path,
                    _cancel: cancel,
                },
            );
        }
        let peer = self.clone();
        object.executor.handle.spawn(async move {
            tokio::select! {
                // Dropping the forwarded request cancels it on the target.
                result = with_path(forwarded, target.request_async(operation)) => {
                    peer.finish_await(id, result)
                }
                _ = cancelled => {},
            }
            drop(flight);
        });
    }
}
