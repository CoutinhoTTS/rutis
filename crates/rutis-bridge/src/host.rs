//! `host`: loads plugins on a peer's behalf, from those installed here.
//!
//! ```text
//! plugins.describe(name)                           → { schema, version, integrity }
//! plugins.load(key, name, config, isolate, inject) → once the plugin settled
//! plugins.update(key, config)
//! plugins.unload(key)
//! ```
//!
//! `isolate` ([service name, label] pairs) and `inject` (service names) are
//! the row's, as the peer's loader has them: the plugin runs with each
//! named service isolated under its label (labels are the peer's, kept
//! apart from every other peer's), and waits for each injected service as
//! for its own dependencies. Names map to keys as the host was told
//! ([`HostPlugin::services`]; by default `host_key(name)`, rutis services
//! by name); a name it cannot map refuses the load.
//!
//! What it loads belongs to its own subtree: when the host goes (the link
//! ended, or it was unloaded), so do they. Opening a host to a peer gives
//! that peer the management of this node's installed plugins: open it only
//! to trusted peers. Names are looked up in a catalog of installed plugins;
//! files and absolute paths are refused.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::channel::PeerId;
use crate::session::rpc::{Connection, Reply, Value};
use crate::session::Error;
use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberView, Plugin, PluginFactory, TypeKey};
use serde_json::{json, Value as Json};

use crate::{peer_key, Offered, Peer};

/// What a hosted plugin declares.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Described {
    /// The JSON Schema of its configuration.
    pub schema: Option<Json>,
    pub version: Option<String>,
    /// A digest of what is installed, for the peer to notice changes.
    pub integrity: Option<String>,
}

/// The plugins installed here that a host may load, with what each
/// declares. Looking one up may take a while (a loader's resolvers).
pub trait PluginCatalog: Send + Sync + 'static {
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<Installed>>;
}

/// An installed plugin: what it declares, and how to build it.
#[derive(Clone)]
pub struct Installed {
    pub described: Described,
    pub factory: Arc<dyn PluginFactory<Json>>,
}

/// A catalog from a fixed list.
#[derive(Default)]
pub struct StaticCatalog {
    entries: HashMap<String, (Described, Arc<dyn PluginFactory<Json>>)>,
}

impl StaticCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(
        mut self,
        name: &str,
        described: Described,
        factory: impl PluginFactory<Json>,
    ) -> Self {
        self.entries
            .insert(name.to_owned(), (described, Arc::new(factory)));
        self
    }
}

impl PluginCatalog for StaticCatalog {
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Option<Installed>> {
        let found = self
            .entries
            .get(name)
            .map(|(described, factory)| Installed {
                described: described.clone(),
                factory: factory.clone(),
            });
        Box::pin(async move { found })
    }
}

/// A catalog's factory as a factory a fiber owns, with the row's injected
/// services added to its own dependencies.
struct Shared {
    factory: Arc<dyn PluginFactory<Json>>,
    injects: Vec<TypeKey>,
}

impl Shared {
    fn new(factory: Arc<dyn PluginFactory<Json>>, extra: Vec<TypeKey>) -> Self {
        let mut injects = factory.injects().to_vec();
        for key in extra {
            if !injects.contains(&key) {
                injects.push(key);
            }
        }
        Self { factory, injects }
    }
}

impl PluginFactory<Json> for Shared {
    fn name(&self) -> &str {
        self.factory.name()
    }
    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }
    fn validate_config(&self, config: &Json) -> Result<(), CordisError> {
        self.factory.validate_config(config)
    }
    fn build(&self, config: &Json) -> Result<Box<dyn Plugin>, CordisError> {
        self.factory.build(config)
    }
}

/// Maps a service name of a row's `isolate` or `inject` to its key.
pub type ServiceKeys = Arc<dyn Fn(&str) -> Option<TypeKey> + Send + Sync>;

/// Serves `plugins.*` to `Peer#<peer>` from `catalog`.
pub struct HostPlugin {
    label: String,
    peer: PeerId,
    injects: [TypeKey; 1],
    catalog: Arc<dyn PluginCatalog>,
    services: ServiceKeys,
}

impl HostPlugin {
    pub fn new(peer: PeerId, catalog: Arc<dyn PluginCatalog>) -> Self {
        Self {
            label: format!("rutis-bridge/host#{peer}"),
            injects: [peer_key(&peer)],
            peer,
            catalog,
            services: Arc::new(|name: &str| Some(crate::session::host_key(name))),
        }
    }

    /// How the service names of a row's `isolate` and `inject` map to keys
    /// here (a loader's service catalog); `None` refuses the load.
    pub fn services(mut self, services: ServiceKeys) -> Self {
        self.services = services;
        self
    }
}

struct Host {
    ctx: Ctx,
    peer: PeerId,
    catalog: Arc<dyn PluginCatalog>,
    services: ServiceKeys,
    loaded: Mutex<HashMap<String, FiberView>>,
}

fn not_found(name: &str) -> Error {
    Error::Remote {
        name: "NotFound".into(),
        message: format!("no plugin {name} is installed here"),
        graph: None,
    }
}

fn wait(result: rutis::BoxFuture<'static, Result<(), Arc<CordisError>>>) -> Reply {
    Ok(Value::future(async move {
        result
            .await
            .map_err(|error| Error::Value(error.to_string()))?;
        Ok(Value::Undefined)
    }))
}

impl Host {
    fn installed(&self, name: &str) -> Result<(), Error> {
        if name.starts_with("file:") || std::path::Path::new(name).is_absolute() {
            return Err(Error::Value(format!(
                "{name}: a host loads installed plugins only, not files"
            )));
        }
        Ok(())
    }

    fn describe(self: &Arc<Self>, args: Vec<Json>) -> Reply {
        let name: String = crate::session::decode(args.into_iter().next().unwrap_or_default())?;
        self.installed(&name)?;
        let host = self.clone();
        Ok(Value::future(async move {
            let found = host
                .catalog
                .find(&name)
                .await
                .ok_or_else(|| not_found(&name))?;
            let described = found.described;
            Ok(Value::Data(json!({
                "schema": described.schema,
                "version": described.version,
                "integrity": described.integrity,
            })))
        }))
    }

    fn load(self: &Arc<Self>, args: Vec<Json>) -> Reply {
        let mut args = args.into_iter();
        let key: String = crate::session::decode(args.next().unwrap_or_default())?;
        let name: String = crate::session::decode(args.next().unwrap_or_default())?;
        let config = args.next().unwrap_or(Json::Null);
        let isolate: Vec<(String, String)> = match args.next() {
            None | Some(Json::Null) => Vec::new(),
            Some(isolate) => crate::session::decode(isolate)?,
        };
        let inject: Vec<String> = match args.next() {
            None | Some(Json::Null) => Vec::new(),
            Some(inject) => crate::session::decode(inject)?,
        };
        self.installed(&name)?;
        let key_of = |service: &str| {
            (self.services)(service).ok_or_else(|| {
                Error::Value(format!("{service} is not a service {name} can use here"))
            })
        };
        // The row's scope: each isolated service under its label, kept
        // apart from other peers' labels.
        let mut scope = self.ctx.clone();
        for (service, label) in &isolate {
            scope = scope.isolate(key_of(service)?, &format!("peer:{}/{label}", self.peer));
        }
        let extra = inject
            .iter()
            .map(|service| key_of(service))
            .collect::<Result<Vec<_>, _>>()?;
        let host = self.clone();
        Ok(Value::future(async move {
            let found = host
                .catalog
                .find(&name)
                .await
                .ok_or_else(|| not_found(&name))?;
            let view = {
                let mut loaded = host.loaded.lock().unwrap();
                if loaded.contains_key(&key) {
                    return Err(Error::Value(format!("{key} is already loaded")));
                }
                let view = scope.plugin_with(Shared::new(found.factory, extra), config);
                loaded.insert(key, view.clone());
                view
            };
            (&view)
                .await
                .map_err(|error| Error::Value(error.to_string()))?;
            Ok(Value::Undefined)
        }))
    }

    fn update(&self, args: Vec<Json>) -> Reply {
        let mut args = args.into_iter();
        let key: String = crate::session::decode(args.next().unwrap_or_default())?;
        let config = args.next().unwrap_or(Json::Null);
        let view = self
            .loaded
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| Error::Value(format!("{key} is not loaded")))?;
        wait(view.update(config))
    }

    fn unload(&self, args: Vec<Json>) -> Reply {
        let key: String = crate::session::decode(args.into_iter().next().unwrap_or_default())?;
        match self.loaded.lock().unwrap().remove(&key) {
            Some(view) => wait(view.dispose()),
            // Already gone: unloading is idempotent.
            None => Ok(Value::Undefined),
        }
    }
}

/// The handler holds the host by `Arc`, so its replies can outlive the call.
struct Hosting(Arc<Host>);

impl crate::Handler for Hosting {
    fn invoke(&self, _: &Connection, _: &str, method: &str, args: Value) -> Reply {
        let host = &self.0;
        let args = match args.json()? {
            Json::Array(args) => args,
            other => vec![other],
        };
        match method {
            "plugins.describe" => host.describe(args),
            "plugins.load" => host.load(args),
            "plugins.update" => host.update(args),
            "plugins.unload" => host.unload(args),
            _ => Err(Error::Value(format!("{method} is not offered here"))),
        }
    }
}

impl Plugin for HostPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let peer = ctx.get_as::<Peer>(self.injects[0].clone()).ok_or_else(|| {
                CordisError::PluginFailed(format!("{}: the peer is gone", self.label).into())
            })?;
            let host = Arc::new(Host {
                ctx: ctx.clone(),
                peer: self.peer.clone(),
                catalog: self.catalog.clone(),
                services: self.services.clone(),
                loaded: Mutex::default(),
            });
            let offered: Offered = peer
                .register("plugins", Arc::new(Hosting(host.clone())))
                .map_err(|error| CordisError::PluginFailed(error.to_string().into()))?;
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    // The hosted plugins are this fiber's children and go
                    // with it.
                    drop(offered);
                    host.loaded.lock().unwrap().clear();
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}
