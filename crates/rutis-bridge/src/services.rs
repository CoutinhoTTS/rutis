//! Services between nodes: `export` announces local services to a peer,
//! `import` registers a peer's services as local native ones.
//!
//! A service crosses as what rutis services of other languages already are:
//! `dyn HostDispatch` at `host_key(name)`. On the wire it is a record of one
//! function reference per method, with the methods' shape (`sync` or
//! `async`); `version` orders announcements, so a replacement or withdrawal
//! never loses to an older one.
//!
//! ```text
//! services.announce { name, service: { method: fn, … }, shape, version }
//! services.withdraw { name, version }
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, Plugin, TypeKey};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Connection, Reference, Reply, Value};
use rutis_interop::{host_key, Error, HostDispatch};
use serde_json::{json, Value as Json};

use crate::{peer_key, Offered, Peer};

fn failed(label: &str, error: impl std::fmt::Display) -> CordisError {
    CordisError::PluginFailed(format!("{label}: {error}").into())
}

// ── export ──────────────────────────────────────────────────────────

/// Announces local services (`host_key(name)`) to `Peer#<peer>`, following
/// their replacement and withdrawal. Each name is a child plugin injecting
/// its service, so rutis gating drives the announcements.
pub struct ExportPlugin {
    label: String,
    names: Vec<String>,
    injects: [TypeKey; 1],
}

impl ExportPlugin {
    pub fn new(peer: PeerId, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            label: format!("rutis-bridge/export#{peer}"),
            names: names.into_iter().map(Into::into).collect(),
            injects: [peer_key(&peer)],
        }
    }
}

impl Plugin for ExportPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let peer = ctx
                .get_as::<Peer>(self.injects[0].clone())
                .ok_or_else(|| failed(&self.label, "the peer is gone"))?;
            let versions = Arc::new(AtomicU64::new(0));
            for name in &self.names {
                ctx.plugin(ExportOne {
                    label: format!("{}/{name}", self.label),
                    service: name.clone(),
                    injects: [host_key(name)],
                    peer: peer.clone(),
                    versions: versions.clone(),
                });
            }
            Ok(Effect::Done)
        })
    }
}

/// Announces one service while it is provided.
struct ExportOne {
    label: String,
    /// The exported service's name.
    service: String,
    injects: [TypeKey; 1],
    peer: Arc<Peer>,
    /// Shared by an export's names: every announcement has a new version.
    versions: Arc<AtomicU64>,
}

impl Plugin for ExportOne {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let service = ctx
                .get_as::<dyn HostDispatch>(self.injects[0].clone())
                .ok_or_else(|| failed(&self.label, "the service is gone"))?;
            let shape = service.methods().ok_or_else(|| {
                failed(
                    &self.label,
                    "the service does not report its methods, so it cannot cross",
                )
            })?;
            let methods: BTreeMap<String, Json> =
                rutis_interop::decode(shape.clone()).map_err(|error| failed(&self.label, error))?;
            let record: BTreeMap<String, Value> = methods
                .keys()
                .map(|method| {
                    let service = service.clone();
                    let method_name = method.clone();
                    let call = Value::callback(move |args| service.invoke(&method_name, args));
                    (method.clone(), call)
                })
                .collect();
            let version = self.versions.fetch_add(1, Ordering::SeqCst) + 1;
            let announcer = tokio::spawn(announce(
                self.peer.clone(),
                self.service.clone(),
                Value::Record(record),
                shape,
                version,
            ));
            let peer = self.peer.clone();
            let name = self.service.clone();
            let versions = self.versions.clone();
            ctx.effect(move || {
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        announcer.abort();
                        let version = versions.fetch_add(1, Ordering::SeqCst) + 1;
                        // Withdrawn there before this fiber ends. A peer that
                        // went away has nothing to withdraw.
                        let withdrawn = peer
                            .connection()
                            .invoke_async(
                                "",
                                "services.withdraw",
                                json!([{ "name": name, "version": version }]).into(),
                            )
                            .await;
                        if let Ok(withdrawn) = withdrawn {
                            let _ = rutis_interop::rpc::settle(withdrawn).await;
                        }
                        Ok(())
                    })
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}

/// Announce whenever the far end offers `services`: at once if it does,
/// again for every new offer of them (an importer that started again).
async fn announce(peer: Arc<Peer>, name: String, service: Value, shape: Json, version: u64) {
    let mut offers = peer.offers();
    let mut announced_to = None;
    loop {
        let offered = offers.borrow_and_update().epoch("services");
        if offered.is_some() && offered != announced_to {
            let announced = peer
                .connection()
                .invoke_async(
                    "",
                    "services.announce",
                    Value::List(vec![Value::Record(BTreeMap::from([
                        ("name".to_owned(), Value::Data(json!(name))),
                        ("service".to_owned(), service.clone()),
                        ("shape".to_owned(), Value::Data(shape.clone())),
                        ("version".to_owned(), Value::Data(json!(version))),
                    ]))]),
                )
                .await;
            if let Err(error) = announced {
                eprintln!(
                    "rutis-bridge: cannot announce {name} to {}: {error}",
                    peer.id()
                );
            }
        }
        announced_to = offered;
        if offers.changed().await.is_err() {
            return;
        }
    }
}

// ── import ──────────────────────────────────────────────────────────

/// Registers the services `names` that `Peer#<peer>` announces as local
/// services (`host_key(name)`), replaced and withdrawn as it says. A name
/// already provided here is refused, naming both sides.
pub struct ImportPlugin {
    label: String,
    peer: PeerId,
    names: HashSet<String>,
    injects: [TypeKey; 1],
}

impl ImportPlugin {
    pub fn new(peer: PeerId, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            label: format!("rutis-bridge/import#{peer}"),
            injects: [peer_key(&peer)],
            names: names.into_iter().map(Into::into).collect(),
            peer,
        }
    }
}

/// One imported service: calls go to the far end, through one function
/// reference per method, or one object reference (as Cordis sends a
/// service object).
struct Remote {
    /// The session the service is on.
    session: Connection,
    target: Target,
    asynchronous: HashSet<String>,
    shape: Json,
}

enum Target {
    Functions(BTreeMap<String, Reference>),
    Object(Reference),
}

enum Call {
    Function(Reference),
    Method(Reference, String),
}

impl HostDispatch for Remote {
    fn invoke(&self, method: &str, args: Value) -> Reply {
        let unknown = || Error::Value(format!("unknown method {method}"));
        let call = match &self.target {
            Target::Functions(methods) => {
                Call::Function(methods.get(method).ok_or_else(unknown)?.clone())
            }
            Target::Object(object) => {
                self.shape.get(method).ok_or_else(unknown)?;
                Call::Method(object.clone(), method.to_owned())
            }
        };
        // Called on behalf of another session (re-exported to a third
        // node, say): that session's call chain is rebased for this one, so
        // a call back reaches the caller that waits for it.
        let source =
            rutis_interop::rpc::caller().filter(|source| source.tag() != self.session.tag());
        if self.asynchronous.contains(method) {
            // The far end answers with its own future: wait for that too.
            let call = async move {
                let reply = match call {
                    Call::Function(reference) => reference.call_async(args).await?,
                    Call::Method(object, method) => object.call_method_async(&method, args).await?,
                };
                rutis_interop::rpc::settle(reply).await
            };
            return Ok(match source {
                Some(source) => Value::future(self.session.forward_async(&source, call)),
                None => Value::future(call),
            });
        }
        let call = || match call {
            Call::Function(reference) => reference.call(args),
            Call::Method(object, method) => object.call_method(&method, args),
        };
        match source {
            Some(source) => self.session.forward(&source, call),
            None => call(),
        }
    }

    fn methods(&self) -> Option<Json> {
        Some(self.shape.clone())
    }
}

/// The newest the peer said of a name: provided at `version`, or, with no
/// `provided`, withdrawn at it. A withdrawal is kept so an older
/// announcement arriving after it (calls are dispatched concurrently) does
/// not bring the service back.
struct Imported {
    version: u64,
    provided: Option<Disposer>,
}

struct Importer {
    ctx: Ctx,
    session: Connection,
    peer: PeerId,
    names: HashSet<String>,
    services: Mutex<HashMap<String, Imported>>,
}

impl Importer {
    fn announce(&self, args: Value) -> Reply {
        let mut fields = match args.list()?.into_iter().next() {
            Some(Value::Record(fields)) => fields,
            _ => return Err(Error::Value("services.announce needs a record".into())),
        };
        let take = |fields: &mut BTreeMap<String, Value>, key: &str| {
            fields
                .remove(key)
                .ok_or_else(|| Error::Value(format!("services.announce lacks {key}")))
        };
        let name: String = rutis_interop::decode(take(&mut fields, "name")?.json()?)?;
        let version: u64 = rutis_interop::decode(take(&mut fields, "version")?.json()?)?;
        let shape = take(&mut fields, "shape")?.json()?;
        if !self.names.contains(&name) {
            // Not imported here: the reference is dropped, which releases it.
            return Ok(Value::Undefined);
        }
        let kinds: BTreeMap<String, String> = rutis_interop::decode(shape.clone())?;
        let asynchronous = kinds
            .iter()
            .filter(|(_, kind)| *kind == "async")
            .map(|(method, _)| method.clone())
            .collect();
        let target = match take(&mut fields, "service")? {
            Value::Record(record) => Target::Functions(
                record
                    .into_iter()
                    .map(|(method, value)| value.reference().map(|reference| (method, reference)))
                    .collect::<Result<BTreeMap<_, _>, _>>()?,
            ),
            Value::Reference(object) if object.is_object() => Target::Object(object),
            _ => {
                return Err(Error::Value(
                    "a service crosses as a record of functions or an object".into(),
                ))
            }
        };
        let mut services = self.services.lock().unwrap();
        if services
            .get(&name)
            .is_some_and(|current| current.version >= version)
        {
            return Ok(Value::Undefined);
        }
        let previous = services.remove(&name);
        let key = host_key(&name);
        let replacing = previous
            .as_ref()
            .is_some_and(|previous| previous.provided.is_some());
        if !replacing && self.ctx.get_as::<dyn HostDispatch>(key.clone()).is_some() {
            return Err(Error::Value(format!(
                "{name} is already provided here: the import from {} is refused",
                self.peer
            )));
        }
        let provided = self
            .ctx
            .provide_as::<dyn HostDispatch>(
                key,
                Arc::new(Remote {
                    session: self.session.clone(),
                    target,
                    asynchronous,
                    shape,
                }),
            )
            .map_err(|error| Error::Value(error.to_string()))?;
        services.insert(
            name,
            Imported {
                version,
                provided: Some(provided),
            },
        );
        drop(services);
        if let Some(mut previous) = previous {
            if let Some(provided) = previous.provided.take() {
                tokio::spawn(async move {
                    let _ = provided.dispose().await;
                });
            }
        }
        Ok(Value::Undefined)
    }

    fn withdraw(&self, args: Value) -> Reply {
        let fields = args
            .list()?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Value("services.withdraw needs its name".into()))?
            .json()?;
        let name: String = rutis_interop::decode(fields["name"].clone())?;
        let version: u64 = rutis_interop::decode(fields["version"].clone())?;
        if !self.names.contains(&name) {
            return Ok(Value::Undefined);
        }
        let removed = {
            let mut services = self.services.lock().unwrap();
            match services.get(&name) {
                Some(current) if current.version >= version => None,
                // Withdrawn at `version`, even if not (yet) announced: an
                // older announcement is stale.
                _ => services.insert(
                    name,
                    Imported {
                        version,
                        provided: None,
                    },
                ),
            }
        };
        if let Some(mut removed) = removed {
            if let Some(provided) = removed.provided.take() {
                return Ok(Value::future(async move {
                    provided
                        .dispose()
                        .await
                        .map_err(|error| Error::Value(error.to_string()))?;
                    Ok(Value::Undefined)
                }));
            }
        }
        Ok(Value::Undefined)
    }
}

impl crate::Handler for Importer {
    fn invoke(&self, _: &Connection, _: &str, method: &str, args: Value) -> Reply {
        match method {
            "services.announce" => self.announce(args),
            "services.withdraw" => self.withdraw(args),
            _ => Err(Error::Value(format!("{method} is not offered here"))),
        }
    }
}

impl Plugin for ImportPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let peer = ctx
                .get_as::<Peer>(self.injects[0].clone())
                .ok_or_else(|| failed(&self.label, "the peer is gone"))?;
            let importer = Arc::new(Importer {
                ctx: ctx.clone(),
                session: peer.connection().clone(),
                peer: self.peer.clone(),
                names: self.names.clone(),
                services: Mutex::default(),
            });
            let offered: Offered = peer
                .register("services", importer.clone())
                .map_err(|error| failed(&self.label, error))?;
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    // Unregistering stops announcements; the imported
                    // services were provided from this fiber and go with it.
                    drop(offered);
                    importer.services.lock().unwrap().clear();
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}
