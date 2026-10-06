//! Rows hosted on other nodes: `peer:<id>/<plugin>` loads `<plugin>` on the
//! node `<id>`, through its `host` (rutis-bridge), as a row of this loader.
//!
//! As for language runtimes, readiness has two stages. The link provides
//! `Peer#<id>`; then [`PeerRowsPlugin`] waits for the peer to offer
//! `plugins`, resolves the rows that were resolved without it again, and
//! only then provides [`PeerRows`], which every such row depends on. A row
//! of a peer that is offline or offers no host waits for it, with no
//! schema; it is not unresolved. The plugin's own dependencies are the
//! target node's business: they are not copied into this loader.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Disposer, Effect, Plugin, PluginFactory, TypeKey};
use rutis_bridge::channel::PeerId;
use rutis_bridge::session::{settle, Value as RpcValue};
use rutis_bridge::{peer_key, Peer};
use serde_json::{json, Value};

use crate::{Loader, LoaderError, Resolved, Resolver};

const PREFIX: &str = "peer:";

/// The service rows of a peer depend on: the peer, once its host is
/// offered and the rows' declarations are complete.
pub struct PeerRows {
    peer: Arc<Peer>,
}

impl PeerRows {
    pub fn key(id: &PeerId) -> TypeKey {
        TypeKey::keyed_dynamic::<PeerRows>(id.to_string())
    }

    pub fn peer(&self) -> &Arc<Peer> {
        &self.peer
    }
}

/// The ready peers, for the resolver, which has no context of its own;
/// [`PeerRowsPlugin`]s keep it.
#[derive(Default)]
struct Directory {
    peers: Mutex<HashMap<PeerId, Arc<Peer>>>,
    /// Names resolved without their peer, to resolve again.
    stale: Mutex<HashSet<String>>,
}

/// Resolves `peer:<id>/<plugin>` rows. Share one between the loader's
/// chain (`Chain::with_shared`) and the [`PeerRowsPlugin`]s.
#[derive(Default)]
pub struct PeerResolver {
    directory: Arc<Directory>,
}

impl PeerResolver {
    pub fn new() -> Self {
        Self::default()
    }

    fn split(name: &str) -> Option<(PeerId, &str)> {
        let rest = name.strip_prefix(PREFIX)?;
        let (peer, plugin) = rest.split_once('/')?;
        let peer = PeerId::new(peer).ok()?;
        (!plugin.is_empty()).then_some((peer, plugin))
    }

    fn offline(&self, name: &str, peer: &PeerId, plugin: &str, why: &str) -> Arc<Resolved> {
        self.directory.stale.lock().unwrap().insert(name.to_owned());
        Arc::new(Resolved {
            factory: Arc::new(PeerFactory::new(name, peer.clone(), plugin)),
            schema: None,
            meta: json!({ "source": "peer", "peer": peer.to_string(), "plugin": plugin, "schema": format!("unavailable: {why}") }),
            foreign_scope: true,
        })
    }

    fn take_stale(&self, peer: &PeerId) -> HashSet<String> {
        let mut stale = self.directory.stale.lock().unwrap();
        let (mine, others): (HashSet<String>, HashSet<String>) = stale
            .drain()
            .partition(|name| Self::split(name).is_some_and(|(of, _)| &of == peer));
        *stale = others;
        mine
    }
}

impl Resolver for PeerResolver {
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoaderError>> {
        Box::pin(async move {
            let Some((peer_id, plugin)) = Self::split(name) else {
                return Err(LoaderError::NotFound {
                    name: name.to_owned(),
                });
            };
            let peer = self.directory.peers.lock().unwrap().get(&peer_id).cloned();
            let Some(peer) = peer else {
                return Ok(self.offline(name, &peer_id, plugin, "the peer is not connected"));
            };
            if !peer.offers().borrow().families.contains("plugins") {
                return Ok(self.offline(name, &peer_id, plugin, "the peer offers no host"));
            }
            let reply = peer
                .connection()
                .invoke_async("", "plugins.describe", json!([plugin]).into())
                .await;
            let described = match reply {
                Ok(described) => settle(described).await,
                Err(error) => Err(error),
            };
            let described = match described.and_then(RpcValue::json) {
                Ok(described) => described,
                Err(rutis_bridge::session::Error::Remote { name: kind, .. })
                    if kind == "NotFound" =>
                {
                    return Err(LoaderError::NotFound {
                        name: name.to_owned(),
                    })
                }
                // The session ended meanwhile: wait for the peer again.
                Err(rutis_bridge::session::Error::Transport(_)) => {
                    return Ok(self.offline(name, &peer_id, plugin, "the peer went away"))
                }
                Err(error) => {
                    return Err(LoaderError::Resolve {
                        name: name.to_owned(),
                        message: error.to_string(),
                    })
                }
            };
            Ok(Arc::new(Resolved {
                factory: Arc::new(PeerFactory::new(name, peer_id.clone(), plugin)),
                schema: described.get("schema").cloned().filter(|s| !s.is_null()),
                meta: json!({
                    "source": "peer",
                    "peer": peer_id.to_string(),
                    "plugin": plugin,
                    "version": described.get("version"),
                    "integrity": described.get("integrity"),
                }),
                foreign_scope: true,
            }))
        })
    }
}

/// Mount it after the link (or what provides `Peer#<id>`) and the loader,
/// with the resolver the loader uses.
pub struct PeerRowsPlugin {
    label: String,
    peer: PeerId,
    resolver: Arc<PeerResolver>,
    injects: [TypeKey; 2],
}

impl PeerRowsPlugin {
    pub fn new(peer: PeerId, resolver: Arc<PeerResolver>) -> Self {
        Self {
            label: format!("peer-rows#{peer}"),
            injects: [peer_key(&peer), TypeKey::of::<Loader>()],
            peer,
            resolver,
        }
    }
}

impl Plugin for PeerRowsPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let peer = ctx.require_as::<Peer>(self.injects[0].clone())?;
            let loader = ctx.require::<Loader>()?;
            let directory = self.resolver.directory.clone();
            directory
                .peers
                .lock()
                .unwrap()
                .insert(self.peer.clone(), peer.clone());
            // In a task: refreshing takes the loader's lock, which the
            // reconcile that started this plugin may hold.
            let task = tokio::spawn(follow(
                ctx.clone(),
                peer,
                loader,
                self.resolver.clone(),
                self.peer.clone(),
            ));
            let id = self.peer.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    task.abort();
                    directory.peers.lock().unwrap().remove(&id);
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}

/// Provide `PeerRows` while the peer offers a host: after resolving the
/// rows resolved without it, so no row starts on a placeholder.
async fn follow(
    ctx: Ctx,
    peer: Arc<Peer>,
    loader: Arc<Loader>,
    resolver: Arc<PeerResolver>,
    id: PeerId,
) {
    let mut offers = peer.offers();
    // What is provided, and for which offer of the host.
    let mut provided: Option<(Disposer, u64)> = None;
    loop {
        let hosting = offers.borrow_and_update().epoch("plugins");
        let current = provided.as_ref().map(|(_, epoch)| *epoch);
        if hosting != current {
            // The host went, or was opened anew (perhaps unseen in between,
            // with what it loaded gone): its rows stop; the peer and its
            // other features stay.
            if let Some((disposer, _)) = provided.take() {
                let _ = disposer.dispose().await;
            }
            if let Some(epoch) = hosting {
                let stale = resolver.take_stale(&id);
                for entry in loader.entries() {
                    let named = entry.options["name"].as_str().map(str::to_owned);
                    if named.is_some_and(|name| stale.contains(&name)) {
                        // A row that fails to resolve shows it in its status.
                        let _ = loader.refresh(&entry.id).await;
                    }
                }
                match ctx.provide_as(
                    PeerRows::key(&id),
                    Arc::new(PeerRows { peer: peer.clone() }),
                ) {
                    Ok(disposer) => provided = Some((disposer, epoch)),
                    Err(_) => return,
                }
            }
        }
        if offers.changed().await.is_err() {
            return;
        }
    }
}

struct PeerFactory {
    name: String,
    plugin: String,
    injects: [TypeKey; 1],
}

impl PeerFactory {
    fn new(name: &str, peer: PeerId, plugin: &str) -> Self {
        Self {
            name: name.to_owned(),
            injects: [PeerRows::key(&peer)],
            plugin: plugin.to_owned(),
        }
    }
}

impl PluginFactory<Value> for PeerFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn build(&self, config: &Value) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(PeerRow {
            name: self.name.clone(),
            plugin: self.plugin.clone(),
            config: config.clone(),
            injects: self.injects.clone(),
        }))
    }
}

/// One generation of a hosted row: loaded on apply, unloaded on cleanup.
struct PeerRow {
    name: String,
    plugin: String,
    config: Value,
    injects: [TypeKey; 1],
}

fn failed(error: impl std::fmt::Display) -> CordisError {
    CordisError::PluginFailed(error.to_string().into())
}

impl Plugin for PeerRow {
    fn name(&self) -> &str {
        &self.name
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let rows = ctx.require_as::<PeerRows>(self.injects[0].clone())?;
            let session = rows.peer().connection().clone();
            // Unique, and new for every generation.
            let key = ctx.instance().to_string();
            let row = ctx
                .get::<Loader>()
                .and_then(|loader| loader.row(ctx.instance()));
            let (isolate, inject) = row.map(|row| (row.isolate, row.inject)).unwrap_or_default();
            let isolate: Vec<(String, String)> = isolate;
            let loaded = session
                .invoke_async(
                    "",
                    "plugins.load",
                    json!([key, self.plugin, self.config, isolate, inject]).into(),
                )
                .await
                .map_err(failed)?;
            settle(loaded).await.map_err(failed)?;
            Ok(Effect::AsyncDisposer(Box::new(move || {
                Box::pin(async move {
                    let unloaded = session
                        .invoke_async("", "plugins.unload", json!([key]).into())
                        .await;
                    let unloaded = match unloaded {
                        Ok(unloaded) => settle(unloaded).await,
                        Err(error) => Err(error),
                    };
                    match unloaded {
                        // A link that ended took the hosted plugin with it.
                        Ok(_) | Err(rutis_bridge::session::Error::Transport(_)) => Ok(()),
                        Err(error) => Err(failed(error)),
                    }
                })
            })))
        })
    }
}

// ── composition ─────────────────────────────────────────────────────

/// The plugins this loader can resolve, as what a host serves its peer.
/// Rows of other peers (`peer:…`) are not hosted: that would make this
/// node relay them.
pub struct LoaderCatalog {
    loader: Loader,
}

impl LoaderCatalog {
    pub fn new(loader: Loader) -> Self {
        Self { loader }
    }
}

impl rutis_bridge::PluginCatalog for LoaderCatalog {
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<rutis_bridge::Installed>> {
        Box::pin(async move {
            if name.starts_with(PREFIX) {
                return None;
            }
            let resolved = self.loader.resolve(name).await.ok()?;
            Some(rutis_bridge::Installed {
                described: rutis_bridge::Described {
                    schema: resolved.schema.clone(),
                    version: resolved.meta["version"].as_str().map(str::to_owned),
                    integrity: resolved.meta["integrity"].as_str().map(str::to_owned),
                },
                factory: resolved.factory.clone(),
            })
        })
    }
}

/// The config of a `rutis-bridge/peer` row.
#[derive(serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
struct NodeConfig {
    peer: String,
    transport: String,
    identity: String,
    #[serde(default)]
    dial: Option<String>,
    #[serde(default)]
    listen: Option<String>,
    #[serde(default)]
    require: Vec<String>,
    #[serde(default)]
    declare: Vec<String>,
    #[serde(default)]
    export: Vec<String>,
    #[serde(default)]
    import: Vec<String>,
    #[serde(default)]
    events: NodeEvents,
    /// Host the plugins this loader resolves for the peer.
    #[serde(default)]
    host: bool,
    /// The peer is this runtime instance.
    #[serde(default)]
    runtime: Option<String>,
    /// Rows `peer:<peer>/…` of this loader run on the peer.
    #[serde(default)]
    rows: bool,
}

#[derive(serde::Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
struct NodeEvents {
    #[serde(default)]
    out: Vec<String>,
    #[serde(default, rename = "in")]
    inbound: Vec<String>,
}

impl NodeConfig {
    fn parse(config: &Value) -> Result<Self, CordisError> {
        let config: Self = serde_json::from_value(config.clone())
            .map_err(|error| failed(format!("invalid peer config: {error}")))?;
        let peer = PeerId::new(&config.peer).map_err(failed)?;
        match (&config.dial, &config.listen) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => {
                return Err(failed(format!(
                    "the link to {peer} needs one of dial and listen"
                )))
            }
        }
        if let Some(both) = config
            .events
            .out
            .iter()
            .find(|name| config.events.inbound.contains(name))
        {
            return Err(failed(format!(
                "event {both} cannot be forwarded both ways"
            )));
        }
        Ok(config)
    }

    fn link(&self) -> rutis_bridge::LinkConfig {
        let peer = PeerId::new(&self.peer).expect("validated");
        let mut link = match (&self.dial, &self.listen) {
            (Some(address), _) => {
                rutis_bridge::LinkConfig::dial(peer, &self.transport, &self.identity, address)
            }
            (_, Some(listener)) => {
                rutis_bridge::LinkConfig::listen(peer, &self.transport, &self.identity, listener)
            }
            _ => unreachable!("validated"),
        };
        link.require = self.require.clone();
        link.declare = self.declare.clone();
        link
    }

    fn features(&self, loader: Option<&Loader>) -> rutis_bridge::Features {
        rutis_bridge::Features {
            export: self.export.clone(),
            import: self.import.clone(),
            outbound: self.events.out.clone(),
            inbound: self.events.inbound.clone(),
            host: (self.host)
                .then(|| loader.cloned())
                .flatten()
                .map(|loader| {
                    Arc::new(LoaderCatalog::new(loader)) as Arc<dyn rutis_bridge::PluginCatalog>
                }),
            // The peer's rows name services as this loader's catalog does.
            host_services: (self.host)
                .then(|| loader.cloned())
                .flatten()
                .map(|loader| {
                    Arc::new(move |name: &str| loader.service_key(name))
                        as rutis_bridge::ServiceKeys
                }),
            runtime: self.runtime.clone(),
        }
    }
}

/// The schema of a `rutis-bridge/peer` row. What is exported, imported and
/// forwarded is volatile: changing it restarts only that feature and keeps
/// the session.
pub fn node_schema() -> Value {
    let names = json!({ "type": "array", "items": { "type": "string" }, "x-volatile": true });
    json!({
        "type": "object",
        "required": ["peer", "transport", "identity"],
        "properties": {
            "peer": { "type": "string", "description": "the far end's endpoint id" },
            "transport": { "type": "string", "description": "Transport#<kind>" },
            "identity": { "type": "string", "description": "Identity#<name>" },
            "dial": { "type": "string" },
            "listen": { "type": "string" },
            "require": { "type": "array", "items": { "type": "string" } },
            "declare": { "type": "array", "items": { "type": "string" } },
            "export": names,
            "import": names,
            "events": {
                "type": "object",
                "properties": { "out": names, "in": names }
            },
            "host": { "type": "boolean" },
            "runtime": { "type": "string" },
            "rows": { "type": "boolean" }
        }
    })
}

/// The `rutis-bridge/peer` row: a link and its features, composed; with
/// `rows`, this loader's `peer:<peer>/…` rows run on the peer. Register it
/// with [`register_peer_node`].
struct NodeFactory {
    resolver: Arc<PeerResolver>,
}

impl PluginFactory<Value> for NodeFactory {
    fn name(&self) -> &str {
        "rutis-bridge/peer"
    }

    fn validate_config(&self, config: &Value) -> Result<(), CordisError> {
        NodeConfig::parse(config).map(|_| ())
    }

    fn build(&self, config: &Value) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Node {
            config: NodeConfig::parse(config)?,
            resolver: self.resolver.clone(),
        }))
    }
}

/// Register the `rutis-bridge/peer` row type: share `resolver` with the
/// loader's chain for its `peer:` rows.
pub fn register_peer_node(builtins: &mut crate::Builtins, resolver: Arc<PeerResolver>) {
    builtins.register_raw(
        "rutis-bridge/peer",
        NodeFactory { resolver },
        Some(node_schema()),
    );
}

struct Node {
    config: NodeConfig,
    resolver: Arc<PeerResolver>,
}

struct Reconfigure {
    handle: rutis_bridge::PeerHandle,
    loader: Option<Loader>,
}

impl rutis::Listener<crate::VolatileUpdate> for Reconfigure {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        update: &'a crate::VolatileUpdate,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            let config = NodeConfig::parse(&update.config)?;
            self.handle
                .set(config.features(self.loader.as_ref()))
                .await
                .map_err(failed)?;
            Ok(None)
        })
    }
}

impl Plugin for Node {
    fn name(&self) -> &str {
        "rutis-bridge/peer"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let loader = ctx.get::<Loader>().map(|loader| (*loader).clone());
            let composed = rutis_bridge::PeerPlugin::new(
                self.config.link(),
                self.config.features(loader.as_ref()),
            )
            .map_err(failed)?;
            let handle = composed.handle();
            ctx.plugin(composed);
            if self.config.rows {
                let peer = PeerId::new(&self.config.peer).map_err(failed)?;
                ctx.plugin(PeerRowsPlugin::new(peer, self.resolver.clone()));
            }
            ctx.events().on(
                ctx,
                &crate::volatile_key(ctx),
                Reconfigure { handle, loader },
            )?;
            Ok(Effect::Done)
        })
    }
}
