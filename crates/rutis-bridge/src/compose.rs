//! `peer`: a link and the features it runs, as one plugin. It composes the
//! existing plugins and adds no protocol, authorization or reconnection of
//! its own. The link and its features are its subtree; the transport and
//! the identity it names are shared and are not.
//!
//! Features change one by one ([`PeerHandle::set`]): changing what is
//! exported, imported or forwarded restarts only that feature, and the link
//! keeps its session.

use std::sync::{Arc, Mutex};

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberView, Plugin};

use crate::{
    EventsPlugin, ExportPlugin, HostPlugin, ImportPlugin, LinkConfig, LinkPlugin, LinkState,
    PluginCatalog, RuntimeAccessPlugin,
};

/// What a peer composition runs on its link.
#[derive(Clone, Default)]
pub struct Features {
    /// Local services announced to the peer.
    pub export: Vec<String>,
    /// The peer's services registered here.
    pub import: Vec<String>,
    /// Events forwarded to the peer.
    pub outbound: Vec<String>,
    /// Events the peer forwards here.
    pub inbound: Vec<String>,
    /// Plugins this node hosts for the peer: opening a host gives the peer
    /// the management of what the catalog holds.
    pub host: Option<Arc<dyn PluginCatalog>>,
    /// The peer is the runtime instance of this name
    /// (`RuntimeSession#<name>`).
    pub runtime: Option<String>,
}

impl Features {
    fn events(&self) -> (&[String], &[String]) {
        (&self.outbound, &self.inbound)
    }
}

#[derive(Default)]
struct Children {
    ctx: Option<Ctx>,
    export: Option<FiberView>,
    import: Option<FiberView>,
    events: Option<FiberView>,
    host: Option<FiberView>,
    runtime: Option<FiberView>,
}

/// The composition plugin.
pub struct PeerPlugin {
    label: String,
    link: LinkConfig,
    features: Arc<Mutex<Features>>,
    children: Arc<Mutex<Children>>,
    state: Arc<Mutex<Option<tokio::sync::watch::Receiver<LinkState>>>>,
}

/// Changes a running composition's features.
#[derive(Clone)]
pub struct PeerHandle {
    link: LinkConfig,
    features: Arc<Mutex<Features>>,
    children: Arc<Mutex<Children>>,
    state: Arc<Mutex<Option<tokio::sync::watch::Receiver<LinkState>>>>,
}

impl PeerPlugin {
    pub fn new(link: LinkConfig, features: Features) -> Result<Self, String> {
        // Checked now, not when the link is up.
        if let Some(both) = features
            .outbound
            .iter()
            .find(|name| features.inbound.contains(name))
        {
            return Err(format!("event {both} cannot be forwarded both ways"));
        }
        Ok(Self {
            label: format!("rutis-bridge/peer#{}", link.peer),
            link,
            features: Arc::new(Mutex::new(features)),
            children: Arc::default(),
            state: Arc::default(),
        })
    }

    pub fn handle(&self) -> PeerHandle {
        PeerHandle {
            link: self.link.clone(),
            features: self.features.clone(),
            children: self.children.clone(),
            state: self.state.clone(),
        }
    }
}

/// One feature's child plugin, or none when the feature is off.
fn spawn(ctx: &Ctx, link: &LinkConfig, features: &Features, part: Part) -> Option<FiberView> {
    let peer = link.peer.clone();
    match part {
        Part::Export => (!features.export.is_empty())
            .then(|| ctx.plugin(ExportPlugin::new(peer, features.export.clone()))),
        Part::Import => (!features.import.is_empty())
            .then(|| ctx.plugin(ImportPlugin::new(peer, features.import.clone()))),
        Part::Events => {
            let (outbound, inbound) = features.events();
            if outbound.is_empty() && inbound.is_empty() {
                return None;
            }
            // Validated when the features were set.
            EventsPlugin::new(peer, outbound.to_vec(), inbound.to_vec())
                .ok()
                .map(|plugin| ctx.plugin(plugin))
        }
        Part::Host => features
            .host
            .clone()
            .map(|catalog| ctx.plugin(HostPlugin::new(peer, catalog))),
        Part::Runtime => features
            .runtime
            .as_deref()
            .map(|name| ctx.plugin(RuntimeAccessPlugin::new(peer, name))),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Export,
    Import,
    Events,
    Host,
    Runtime,
}

const PARTS: [Part; 5] = [
    Part::Export,
    Part::Import,
    Part::Events,
    Part::Host,
    Part::Runtime,
];

impl Children {
    fn slot(&mut self, part: Part) -> &mut Option<FiberView> {
        match part {
            Part::Export => &mut self.export,
            Part::Import => &mut self.import,
            Part::Events => &mut self.events,
            Part::Host => &mut self.host,
            Part::Runtime => &mut self.runtime,
        }
    }
}

fn changed(old: &Features, new: &Features, part: Part) -> bool {
    match part {
        Part::Export => old.export != new.export,
        Part::Import => old.import != new.import,
        Part::Events => old.events() != new.events(),
        Part::Host => match (&old.host, &new.host) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (None, None) => false,
            _ => true,
        },
        Part::Runtime => old.runtime != new.runtime,
    }
}

impl PeerHandle {
    /// Replace the features: only those that changed restart; the link and
    /// its session stay.
    pub async fn set(&self, features: Features) -> Result<(), String> {
        if let Some(both) = features
            .outbound
            .iter()
            .find(|name| features.inbound.contains(name))
        {
            return Err(format!("event {both} cannot be forwarded both ways"));
        }
        let old = std::mem::replace(&mut *self.features.lock().unwrap(), features.clone());
        let ctx = self.children.lock().unwrap().ctx.clone();
        let Some(ctx) = ctx else {
            // Not running: the new features apply when it starts.
            return Ok(());
        };
        for part in PARTS {
            if !changed(&old, &features, part) {
                continue;
            }
            let previous = self.children.lock().unwrap().slot(part).take();
            if let Some(previous) = previous {
                previous
                    .dispose()
                    .await
                    .map_err(|error| error.to_string())?;
            }
            let next = spawn(&ctx, &self.link, &features, part);
            *self.children.lock().unwrap().slot(part) = next;
        }
        Ok(())
    }

    /// What the composition's link is doing, once it runs.
    pub fn link_state(&self) -> Option<tokio::sync::watch::Receiver<LinkState>> {
        self.state.lock().unwrap().clone()
    }
}

impl Plugin for PeerPlugin {
    fn name(&self) -> &str {
        &self.label
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let link = LinkPlugin::new(self.link.clone());
            *self.state.lock().unwrap() = Some(link.state());
            ctx.plugin(link);
            let features = self.features.lock().unwrap().clone();
            {
                let mut children = self.children.lock().unwrap();
                children.ctx = Some(ctx.clone());
                for part in PARTS {
                    *children.slot(part) = spawn(ctx, &self.link, &features, part);
                }
            }
            let children = self.children.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    // The children are this fiber's and go with it.
                    *children.lock().unwrap() = Children::default();
                    Ok(())
                }))
            })?;
            Ok(Effect::Done)
        })
    }
}
