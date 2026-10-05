//! Local language runtimes: a process this machine starts, a link to it,
//! and the runtime plugin running its rows, composed. Its session comes the
//! way a remote runtime's does (`RuntimeSession#<name>` from a link), only
//! started here: the local transport (`rutis-transport-local`) spawns it on
//! an inherited socket, the
//! link speaks the compat protocol with it and does not reconnect, since a
//! local runtime process is not restarted.
//!
//! When the process ends, its link stops, its session and then the runtime
//! plugin go, and the runtime's state says how it ended. Restarting the
//! composition (`FiberView::restart`) starts a new process: that is the
//! application's decision.
//!
//! What a language's process is (its program, arguments, how it takes its
//! channel) comes from `rutis-interop` ([`Launcher`]); the transport only
//! starts it.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};
use rutis_bridge::{
    identity_key, transport_key, Identity, LinkConfig, LinkPlugin, LinkState, RuntimeAccessPlugin,
    StaticIdentity, Transport,
};
use rutis_channel::PeerId;
use rutis_interop::{Launcher, RuntimeHandle, RuntimePlugin, RuntimeState};
use serde_json::Value;

use rutis_transport_local::{Handover, LocalTransport, Spawn};

/// Set to report every message crossing a runtime channel (direction and
/// length, never content) on stderr, as for runtimes `Process` starts.
const TRACE_VARIABLE: &str = "RUTIS_INTEROP_TRACE";

/// A local runtime: mount it before the plugins (and the loader rows) that
/// load into it. It provides what [`RuntimePlugin`] does (`Runtime#<name>`).
pub struct LocalRuntime {
    label: String,
    /// The runtime's name (`Runtime#<name>`).
    runtime_name: String,
    launcher: Launcher,
    anchor: PathBuf,
    runtime: RuntimePlugin,
}

impl LocalRuntime {
    /// The Node runtime, named `"node"`. `node_package`: the rutis-interop
    /// npm runtime (`interop/node`, or a deployed `@arcships/rutis-interop`).
    /// `anchor`: the `package.json` plugins and Cordis resolve from.
    #[cfg(feature = "node")]
    pub fn node(node_package: impl Into<PathBuf>, anchor: impl Into<PathBuf>) -> Self {
        let launcher = Launcher::node(&node_package.into());
        Self::with("node", launcher, anchor.into())
    }

    /// A Python runtime named `"py"`: `python3 -m rutis_runtime`, with the
    /// SDK directory `sdk` (`interop/python`), importing plugin modules from
    /// `project`. Python 3.12 or later.
    #[cfg(feature = "python")]
    pub fn python(sdk: impl Into<PathBuf>, project: impl Into<PathBuf>) -> Self {
        let (sdk, project) = (sdk.into(), project.into());
        let launcher = Launcher::python(&sdk, &project);
        Self::with("py", launcher, project)
    }

    /// A runtime started with `launcher`; it receives its channel and the
    /// anchor as its last two arguments.
    pub fn launcher(
        name: impl Into<String>,
        launcher: Launcher,
        anchor: impl Into<PathBuf>,
    ) -> Self {
        Self::with(&name.into(), launcher, anchor.into())
    }

    fn with(name: &str, launcher: Launcher, anchor: PathBuf) -> Self {
        Self {
            label: format!("{name}-runtime (local)"),
            runtime_name: name.to_owned(),
            launcher,
            runtime: RuntimePlugin::session(name, anchor.clone()),
            anchor,
        }
    }

    /// The process to start: the launcher, given its channel and the
    /// anchor as its last two arguments; the endpoint is the runtime's name.
    fn spawn(&self) -> Spawn {
        let peer = PeerId::new(self.runtime_name.clone())
            .unwrap_or_else(|_| PeerId::new("runtime").expect("a valid id"));
        let mut spawn = Spawn::new(self.launcher.program.clone(), peer);
        spawn.args = self.launcher.args.clone();
        spawn.env = self.launcher.env.clone();
        spawn.cwd = self.launcher.cwd.clone();
        spawn.handover = match self.launcher.inherit_fd {
            true => Handover::Inherit,
            false => Handover::DialBack,
        };
        spawn.trailing = vec![OsString::from(&self.anchor)];
        spawn
    }

    /// Declare the methods of the rutis service at `host_key(name)`, as
    /// [`RuntimePlugin::host`].
    pub fn host(mut self, name: &str, methods: Value) -> Self {
        self.runtime = self.runtime.host(name, methods);
        self
    }

    /// Name the runtime: its services are keyed by the name.
    pub fn named(mut self, name: impl Into<String>) -> Self {
        let name: String = name.into();
        self.runtime = self.runtime.named(name.clone());
        self.label = format!("{name}-runtime (local)");
        self.runtime_name = name;
        self
    }

    /// Run the Python runtime with this interpreter instead of `python3`.
    pub fn interpreter(mut self, program: impl Into<OsString>) -> Self {
        self.launcher.program = program.into();
        self
    }

    /// Observe the runtime from code that is not a plugin (a loader resolver).
    pub fn handle(&self) -> RuntimeHandle {
        self.runtime.handle()
    }
}

impl Plugin for LocalRuntime {
    fn name(&self) -> &str {
        &self.label
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            // Its own transport and identity, under keys of its own, from a
            // child: what the link waits for must come from a provider that
            // is running, and this one is still starting.
            let kind = format!("local:{}", self.runtime_name);
            let spawn = self.spawn();
            let peer = spawn.peer.clone();
            ctx.plugin(Carrier {
                label: format!("{kind} carrier"),
                kind: kind.clone(),
                spawn,
            });
            let link = LinkPlugin::new(
                LinkConfig::dial(peer.clone(), &kind, &kind, "spawn:runtime").local_runtime(),
            );
            let mut link_state = link.state();
            self.runtime.starting();
            ctx.plugin(link);
            ctx.plugin(RuntimeAccessPlugin::new(peer, &self.runtime_name));
            ctx.plugin(self.runtime.clone());
            // Started once the runtime runs, as a runtime plugin starting its
            // own process is: while it starts this is Loading, and a restart
            // or dispose abandons the start (its process ends with the link).
            let handle = self.runtime.handle();
            let failure = loop {
                if let RuntimeState::Ready(_) = handle.state() {
                    return Ok(Effect::Done);
                }
                if let RuntimeState::Down(reason) = handle.state() {
                    break reason;
                }
                if let LinkState::Stopped { error } = &*link_state.borrow() {
                    break error.clone();
                }
                tokio::select! {
                    _ = handle.changed() => {}
                    _ = link_state.changed() => {}
                    _ = ctx.cancelled() => {
                        self.runtime.stopped();
                        return Ok(Effect::Done);
                    }
                }
            };
            self.runtime.failed(failure.clone());
            Err(CordisError::PluginFailed(failure.into()))
        })
    }
}

/// The transport that starts the runtime process, and the identity of this
/// side, for the link to it. Unloaded, it closes the channel, ending the
/// process.
struct Carrier {
    label: String,
    kind: String,
    spawn: Spawn,
}

impl Plugin for Carrier {
    fn name(&self) -> &str {
        &self.label
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let transport = Arc::new(LocalTransport::default());
            transport.spawner("runtime", self.spawn.clone());
            if std::env::var_os(TRACE_VARIABLE).is_some() {
                transport.trace(Arc::new(|line: &str| {
                    eprintln!("rutis-interop trace: {line}")
                }));
            }
            let closing = transport.clone();
            ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    closing.close_all();
                    Ok(())
                }))
            })?;
            ctx.provide_as::<dyn Transport>(transport_key(&self.kind), transport)?;
            let main = PeerId::new("main").expect("a valid id");
            ctx.provide_as::<dyn Identity>(
                identity_key(&self.kind),
                Arc::new(StaticIdentity::new(main)),
            )?;
            Ok(Effect::Done)
        })
    }
}
