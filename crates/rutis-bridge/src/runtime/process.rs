use crate::runtime::events::EventSink;
use crate::runtime::rpc::{Connection, Dispatch, Reply, Value as RpcValue};
use crate::runtime::Error;
use crate::runtime::{HostDispatch, RuntimeSession};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Receives changes of exported Cordis service slots. `handle` addresses the
/// object now in the slot (`None` when it is unavailable); `version` orders
/// changes, so an older notification must not override a newer one.
pub trait ServiceEvents: Send + Sync + 'static {
    fn changed(&self, name: &str, handle: Option<String>, version: u64);
}

/// One host-provided service: its Cordis name, the bound methods as
/// `{ method: "sync" | "async" }`, and the dispatcher that serves them.
pub struct Host {
    pub name: String,
    pub methods: Value,
    pub dispatch: Arc<dyn HostDispatch>,
}

type Slots = Arc<Mutex<HashMap<String, (Option<String>, u64)>>>;

/// A host service registered with the Node side, and how many leases use
/// it. Hosts of a mount stay for the process's lifetime.
struct HostEntry {
    dispatch: Arc<dyn HostDispatch>,
    leases: usize,
}

/// Records the newest handle per slot and forwards newer changes; serves
/// calls to host-provided services.
struct Imports {
    slots: Slots,
    events: Option<Arc<dyn ServiceEvents>>,
    /// Observers of the services rows export, by service name.
    exported: Mutex<HashMap<String, Arc<dyn ServiceEvents>>>,
    hosts: Mutex<HashMap<String, HostEntry>>,
    forwarded: Option<Arc<dyn EventSink>>,
}

/// Everything one mount needs: the plugins loaded in order into one Cordis
/// Context, the exported services, and what the rutis side contributes.
#[derive(Default)]
pub struct Mount<'a> {
    /// `(entry, config)` per plugin, in load order.
    pub plugins: Vec<(&'a Path, Value)>,
    /// Exported services: `{ name: [member, ...] }`.
    pub services: Value,
    /// Follows changes of the exported service slots.
    pub observer: Option<Arc<dyn ServiceEvents>>,
    /// rutis services the plugins may depend on.
    pub hosts: Vec<Host>,
    /// Cordis events forwarded to the rutis side, and where they go.
    pub events: Option<(Vec<String>, Arc<dyn EventSink>)>,
    /// Events the rutis side may emit into Cordis (see `Process::emit`).
    pub emits: Vec<String>,
    /// With no `plugins`: start an empty Context whose Cordis resolves from
    /// this file (a `package.json`), and load plugins later one by one with
    /// [`Process::load_row`].
    pub anchor: Option<&'a Path>,
    /// How to start the runtime process; `None` runs the Node runtime in
    /// the npm package (`node --import tsx <package>/src/runner.mjs`, feature
    /// `node`).
    pub launcher: Option<&'a Launcher>,
}

/// The command that starts a runtime process. It receives its channel and
/// then the first plugin (or the anchor) as its last two arguments: `fd:3`
/// when it takes an inherited socket ([`Launcher::inherit_fd`]), otherwise
/// a socket path to dial.
#[derive(Debug, Clone, Default)]
pub struct Launcher {
    pub program: std::ffi::OsString,
    pub args: Vec<std::ffi::OsString>,
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// The working directory; the application's by default.
    pub cwd: Option<std::path::PathBuf>,
    /// The process takes its channel as an inherited socket on fd 3
    /// (`fd:3`) instead of dialing a socket path.
    pub inherit_fd: bool,
}

impl Launcher {
    pub fn new(program: impl Into<std::ffi::OsString>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }

    pub fn arg(mut self, arg: impl Into<std::ffi::OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(
        mut self,
        name: impl Into<std::ffi::OsString>,
        value: impl Into<std::ffi::OsString>,
    ) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    pub fn cwd(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// The process takes `fd:3`, an inherited socket, as its channel.
    pub fn inherit_fd(mut self) -> Self {
        self.inherit_fd = true;
        self
    }

    /// The Node runtime of the npm package `node_package` (`node/rutis-runtime`,
    /// or a deployed `@arcships/rutis-runtime`): `node --import tsx
    /// src/runner.mjs` in the package, on an inherited socket when the
    /// package says it takes one (`rutisChannels` lists `"fd"`).
    #[cfg(feature = "node")]
    pub fn node(node_package: &std::path::Path) -> Self {
        let launcher = Launcher::new("node")
            .arg("--import")
            .arg("tsx")
            .arg(node_package.join("src/runner.mjs"))
            .cwd(node_package);
        match crate::runtime::spawn::node_inherits(node_package) {
            true => launcher.inherit_fd(),
            false => launcher,
        }
    }

    /// The Python runtime: `python3 -m rutis` (the `rutis` package installed
    /// where that interpreter finds it), with `project` ahead of the
    /// inherited `PYTHONPATH`, in `project`, on an inherited socket.
    #[cfg(feature = "python")]
    pub fn python(project: &std::path::Path) -> Self {
        let mut path = std::ffi::OsString::from(project);
        if let Some(inherited) = std::env::var_os("PYTHONPATH").filter(|p| !p.is_empty()) {
            path.push(":");
            path.push(inherited);
        }
        Launcher::new("python3")
            .arg("-m")
            .arg("rutis")
            .env("PYTHONPATH", path)
            .env("PYTHONUNBUFFERED", "1")
            // A plugin imported again after an edit must not come from a
            // bytecode file written in the same second as the old source.
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .cwd(project)
            .inherit_fd()
    }
}
impl Imports {
    fn update(&self, name: String, handle: Option<String>, version: u64) {
        {
            let mut slots = self.slots.lock().unwrap();
            let entry = slots.entry(name.clone()).or_insert((None, 0));
            if version <= entry.1 {
                return;
            }
            *entry = (handle.clone(), version);
        }
        if let Some(events) = &self.events {
            events.changed(&name, handle.clone(), version);
        }
        let exported = self.exported.lock().unwrap().get(&name).cloned();
        if let Some(observer) = exported {
            observer.changed(&name, handle, version);
        }
    }
}
impl Dispatch for Imports {
    fn invoke(&self, _: &Connection, target: &str, method: &str, args: RpcValue) -> Reply {
        if let Some(name) = target.strip_prefix("host:") {
            let host = self
                .hosts
                .lock()
                .unwrap()
                .get(name)
                .map(|entry| entry.dispatch.clone())
                .ok_or_else(|| Error::Value(format!("no host service {name}")))?;
            return host.invoke(method, args);
        }
        if target.is_empty() && method == "event" {
            let mut args = args.list()?.into_iter();
            let name: String =
                crate::runtime::decode(args.next().unwrap_or(RpcValue::Undefined).json()?)?;
            let values = args.next().unwrap_or(RpcValue::List(Vec::new())).list()?;
            return match &self.forwarded {
                Some(sink) => sink.event(&name, values),
                None => Ok(RpcValue::Undefined),
            };
        }
        if !(target.is_empty() && method == "service") {
            return Err(Error::Value(
                "application has no exported service target".into(),
            ));
        }
        let (name, handle, version): (String, Option<String>, u64) =
            crate::runtime::decode(args.json()?)?;
        self.update(name, handle, version);
        Ok(RpcValue::Undefined)
    }
}

/// What the `mount` call sends.
struct MountRest {
    plugins: Vec<Value>,
    services: Value,
    forwarded_names: Vec<String>,
    emits: Vec<String>,
    provided: serde_json::Map<String, Value>,
}

/// A mount's parts, once split from where the process comes from.
struct Started {
    plugins: Vec<(std::path::PathBuf, Value)>,
    services: Value,
    events: Option<Arc<dyn ServiceEvents>>,
    hosts: HashMap<String, HostEntry>,
    forwarded: Option<Arc<dyn EventSink>>,
    forwarded_names: Vec<String>,
    emits: Vec<String>,
    provided: serde_json::Map<String, Value>,
}

impl Started {
    /// The parts, and the plugins as given (the first names what to start).
    fn from(mount: Mount<'_>) -> (Self, Vec<(&Path, Value)>) {
        let Mount {
            plugins,
            services,
            observer: events,
            hosts,
            events: forwarded,
            emits,
            anchor: _,
            launcher: _,
        } = mount;
        let (forwarded_names, forwarded) = match forwarded {
            Some((names, sink)) => (names, Some(sink)),
            None => (Vec::new(), None),
        };
        let provided = hosts
            .iter()
            .map(|host| (host.name.clone(), host.methods.clone()))
            .collect();
        let hosts = hosts
            .into_iter()
            .map(|host| {
                let entry = HostEntry {
                    dispatch: host.dispatch,
                    leases: 1,
                };
                (host.name, entry)
            })
            .collect();
        let started = Self {
            plugins: plugins
                .iter()
                .map(|(entry, config)| (entry.to_path_buf(), config.clone()))
                .collect(),
            services,
            events,
            hosts,
            forwarded,
            forwarded_names,
            emits,
            provided,
        };
        (started, plugins)
    }
}

/// Owns one native Cordis process and its generated service bindings.
pub struct Process {
    peer: Connection,
    imports: Arc<Imports>,
    runtime: tokio::runtime::Handle,
    /// The process this side started; `None` for an attached runtime.
    child: Option<crate::runtime::spawn::Child>,
    /// Reported by the runner when mounting.
    features: std::sync::OnceLock<Vec<String>>,
    /// The services each loaded row exports, by row key.
    exports: Mutex<HashMap<String, Vec<String>>>,
    /// Serializes `hosts.provide` / `hosts.withdraw`, so a lease returns only
    /// once its service is registered on the Node side, and a withdrawal
    /// never overtakes the registration it undoes.
    host_changes: tokio::sync::Mutex<()>,
    _directory: Option<tempfile::TempDir>,
    /// For a runtime on a session something else owns (a link): what routes
    /// the runtime's calls here. The session is not this process's to close.
    routed: Option<Box<dyn std::any::Any + Send + Sync>>,
}

/// What a plugin module declares, for rutis-loader (see
/// [`Process::describe_row`]).
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct RowSchema {
    /// The JSON Schema of the config, if the plugin declares one.
    pub config: Option<Value>,
    /// The services the plugin injects; Cordis has only required ones.
    #[serde(default)]
    pub inject: Vec<String>,
    /// The services it provides to rutis: `{ name: { method: "sync" | "async" } }`.
    #[serde(default)]
    pub provides: serde_json::Map<String, Value>,
}

/// One row's use of a host service (see [`Process::lease_host`]). Give it
/// back with [`HostLease::release`]: dropping it releases nothing, and the
/// service stays registered in the runtime.
#[must_use = "a lease is given back only by HostLease::release"]
pub struct HostLease {
    process: Arc<Process>,
    name: String,
}

impl HostLease {
    /// Give the lease back; the last one withdraws the service from Cordis.
    pub async fn release(self) -> Result<(), Error> {
        self.process.release_host(&self.name).await
    }
}

impl Process {
    pub async fn launch(
        node_package: &Path,
        plugin: &Path,
        config: Value,
        services: Value,
    ) -> Result<Arc<Self>, Error> {
        Self::launch_observed(node_package, plugin, config, services, None).await
    }

    /// Launch and report every later change of the exported service slots.
    pub async fn launch_observed(
        node_package: &Path,
        plugin: &Path,
        config: Value,
        services: Value,
        events: Option<Arc<dyn ServiceEvents>>,
    ) -> Result<Arc<Self>, Error> {
        Self::launch_group(node_package, &[(plugin, config)], services, events).await
    }

    /// Launch a group of plugins in one Cordis Context, loaded in order, so
    /// dependencies between them resolve natively. `services` lists the
    /// exported service slots and their methods.
    pub async fn launch_group(
        node_package: &Path,
        plugins: &[(&Path, Value)],
        services: Value,
        events: Option<Arc<dyn ServiceEvents>>,
    ) -> Result<Arc<Self>, Error> {
        Self::launch_mount(node_package, plugins, services, events, Vec::new()).await
    }

    /// Launch a group and provide rutis services to it: each host is
    /// registered in the Cordis Context before the plugins load, so their
    /// dependencies on it resolve natively.
    pub async fn launch_mount(
        node_package: &Path,
        plugins: &[(&Path, Value)],
        services: Value,
        events: Option<Arc<dyn ServiceEvents>>,
        hosts: Vec<Host>,
    ) -> Result<Arc<Self>, Error> {
        Self::mount(
            node_package,
            Mount {
                plugins: plugins.to_vec(),
                services,
                observer: events,
                hosts,
                events: None,
                emits: Vec::new(),
                anchor: None,
                launcher: None,
            },
        )
        .await
    }

    /// Launch a mount: see [`Mount`].
    pub async fn mount(node_package: &Path, mount: Mount<'_>) -> Result<Arc<Self>, Error> {
        let (anchor, launcher) = (mount.anchor, mount.launcher);
        let (started, plugins) = Started::from(mount);
        let plugin = match (plugins.first(), anchor) {
            (Some((plugin, _)), _) => *plugin,
            (None, Some(anchor)) => anchor,
            (None, None) => {
                return Err(Error::Value(
                    "a mount needs at least one plugin or an anchor".into(),
                ))
            }
        };
        let (command, connect) = crate::runtime::spawn::command(launcher, node_package)?;
        let spawned = crate::runtime::spawn::spawn(command, plugin, connect).await?;
        Self::start(
            spawned.channel,
            Some(spawned.child),
            spawned.directory,
            started,
            crate::runtime::rpc::Format::Compat,
        )
        .await
    }

    /// Run a mount on a runtime that is already connected: `channel` leads
    /// to a runtime process this side did not start (for example one that
    /// listens on a WebSocket and was dialed). [`Mount::launcher`] and
    /// [`Mount::anchor`] do not apply: that process was started with its own.
    /// It has no exit status here, and its end is the session's end.
    ///
    /// Network sessions use the endpoint [`Format`](crate::runtime::rpc::Format).
    pub async fn attach(
        channel: crate::channel::Channel,
        mount: Mount<'_>,
        format: crate::runtime::rpc::Format,
    ) -> Result<Arc<Self>, Error> {
        let (started, _) = Started::from(mount);
        Self::start(channel, None, None, started, format).await
    }

    async fn start(
        channel: crate::channel::Channel,
        child: Option<crate::runtime::spawn::Child>,
        directory: Option<tempfile::TempDir>,
        started: Started,
        format: crate::runtime::rpc::Format,
    ) -> Result<Arc<Self>, Error> {
        let (imports, rest) = Self::imports(started);
        let peer = Connection::open_with(channel, imports.clone(), format)?;
        peer.ready().await?;
        Self::finish(peer, imports, child, directory, None, rest).await
    }

    /// What serves the runtime's calls into rutis, and the rest of the mount.
    fn imports(started: Started) -> (Arc<Imports>, MountRest) {
        let Started {
            plugins,
            services,
            events,
            hosts,
            forwarded,
            forwarded_names,
            emits,
            provided,
        } = started;
        let plugins: Vec<Value> = plugins
            .iter()
            .map(|(entry, config)| json!({ "entry": entry, "config": config }))
            .collect();
        let imports = Arc::new(Imports {
            slots: Slots::default(),
            events,
            exported: Mutex::default(),
            hosts: Mutex::new(hosts),
            forwarded,
        });
        let rest = MountRest {
            plugins,
            services,
            forwarded_names,
            emits,
            provided,
        };
        (imports, rest)
    }

    /// Run a mount on `session`, a runtime session something else owns:
    /// its calls into rutis are routed here while the process lives, and
    /// disposing or dropping the process leaves the session open.
    pub async fn over(
        session: Arc<dyn RuntimeSession>,
        mount: Mount<'_>,
    ) -> Result<Arc<Self>, Error> {
        let (started, _) = Started::from(mount);
        let (imports, rest) = Self::imports(started);
        let routed = session.route(imports.clone())?;
        Self::finish(
            session.connection(),
            imports,
            None,
            None,
            Some(routed),
            rest,
        )
        .await
    }

    async fn finish(
        peer: Connection,
        imports: Arc<Imports>,
        child: Option<crate::runtime::spawn::Child>,
        directory: Option<tempfile::TempDir>,
        routed: Option<Box<dyn std::any::Any + Send + Sync>>,
        rest: MountRest,
    ) -> Result<Arc<Self>, Error> {
        let MountRest {
            plugins,
            services,
            forwarded_names,
            emits,
            provided,
        } = rest;
        let process = Arc::new(Self {
            peer,
            imports,
            runtime: tokio::runtime::Handle::current(),
            child,
            features: std::sync::OnceLock::new(),
            exports: Mutex::default(),
            host_changes: tokio::sync::Mutex::new(()),
            _directory: directory,
            routed,
        });
        let mounted = process
            .call_async(
                "",
                "mount",
                json!({ "plugins": plugins, "services": services, "provided": provided, "events": forwarded_names, "emits": emits }),
            )
            .await?;
        let slots: HashMap<String, (Option<String>, u64)> =
            crate::runtime::decode(mounted["services"].clone())?;
        for (name, (handle, version)) in slots {
            process.imports.update(name, handle, version);
        }
        // A runner older than 0.3.0 reports no features.
        let features: Vec<String> =
            crate::runtime::decode(mounted.get("features").cloned().unwrap_or(json!([])))?;
        let _ = process.features.set(features);
        Ok(process)
    }

    /// Load one plugin into the Context as row `key` (rows mode, see
    /// [`Mount::anchor`]). `isolate` gives (service, label) pairs: rows
    /// naming the same label share that service's scope; `inject` lists
    /// extra services the row waits for. Resolves once the plugin settled.
    pub async fn load_row(
        &self,
        key: &str,
        entry: &Path,
        config: Value,
        isolate: &[(String, String)],
        inject: &[String],
    ) -> Result<(), Error> {
        self.call_async(
            "",
            "rows.load",
            json!([key, entry, config, isolate, inject]),
        )
        .await
        .map(|_| ())
    }

    /// Load row `key` like [`Process::load_row`], exporting the services in
    /// `exports` (`{ name: { method: "sync" | "async" } }`) to `observer`:
    /// it hears every change of each service, read from the row's scope,
    /// until the row is unloaded. A name another row exports is refused.
    #[allow(clippy::too_many_arguments)]
    pub async fn load_row_exporting(
        &self,
        key: &str,
        entry: &Path,
        config: Value,
        isolate: &[(String, String)],
        inject: &[String],
        exports: &serde_json::Map<String, Value>,
        observer: Arc<dyn ServiceEvents>,
    ) -> Result<(), Error> {
        if !exports.is_empty() {
            self.require("rows.v2")?;
        }
        let names: Vec<String> = exports.keys().cloned().collect();
        {
            // Registered before loading: the row's services may appear
            // while it starts.
            let mut exported = self.imports.exported.lock().unwrap();
            if let Some(name) = names.iter().find(|name| exported.contains_key(*name)) {
                return Err(Error::Value(format!("service {name} is already exported")));
            }
            for name in &names {
                exported.insert(name.clone(), observer.clone());
            }
        }
        self.exports.lock().unwrap().insert(key.to_owned(), names);
        let loaded = self
            .call_async(
                "",
                "rows.load",
                json!([key, entry, config, isolate, inject, exports]),
            )
            .await;
        if loaded.is_err() {
            self.forget_exports(key);
        }
        loaded.map(|_| ())
    }

    /// Stop following the services row `key` exported, withdrawing them
    /// first. The runtime withdraws them before it answers `rows.unload`,
    /// but its notification runs as a task of its own and may reach the
    /// observer only after this: without the withdrawal here, a late one
    /// finds no observer and the service stays projected.
    fn forget_exports(&self, key: &str) {
        let names = self.exports.lock().unwrap().remove(key);
        let observers: Vec<_> = {
            let mut exported = self.imports.exported.lock().unwrap();
            names
                .into_iter()
                .flatten()
                .filter_map(|name| exported.remove(&name).map(|observer| (name, observer)))
                .collect()
        };
        for (name, observer) in observers {
            let version = self
                .imports
                .slots
                .lock()
                .unwrap()
                .get(&name)
                .map_or(0, |(_, version)| *version);
            observer.changed(&name, None, version);
        }
    }

    /// Give row `key` a new config. Cordis commits volatile values in place
    /// (`loader/volatile-update`, as dsh's loader does); any other change
    /// restarts the row with the config. An inactive row keeps it for its
    /// next activation.
    pub async fn update_row(&self, key: &str, config: Value) -> Result<(), Error> {
        self.call_async("", "rows.update", json!([key, config]))
            .await
            .map(|_| ())
    }

    /// Dispose row `key`.
    pub async fn unload_row(&self, key: &str) -> Result<(), Error> {
        let unloaded = self.call_async("", "rows.unload", json!([key])).await;
        // After the call: the withdrawals of the row's services arrive
        // before its reply.
        self.forget_exports(key);
        unloaded.map(|_| ())
    }

    /// What the plugin at `entry` declares: its config schema (schemastery
    /// `Config` converted), the services it injects, and the services it
    /// provides to rutis.
    pub async fn describe_row(&self, entry: &Path) -> Result<RowSchema, Error> {
        self.require("rows.v2")?;
        crate::runtime::decode(self.call_async("", "rows.schema", json!([entry])).await?)
    }

    /// Whether the Node runtime supports `feature` (reported when mounting).
    pub fn supports(&self, feature: &str) -> bool {
        self.features
            .get()
            .is_some_and(|features| features.iter().any(|f| f == feature))
    }

    fn require(&self, feature: &str) -> Result<(), Error> {
        match self.supports(feature) {
            true => Ok(()),
            false => Err(Error::Value(format!(
                "the Node runtime lacks `{feature}`: @arcships/rutis-runtime 0.3.0 or later is required"
            ))),
        }
    }

    /// Lease the host service `name` for a row: the first lease registers
    /// it with the Node side (methods from `methods`, else from
    /// [`HostDispatch::methods`]); the last one released withdraws it. A
    /// newer lease's dispatch replaces the older one's, since rows restart
    /// when their provider changes.
    pub async fn lease_host(
        self: &Arc<Self>,
        name: &str,
        dispatch: Arc<dyn HostDispatch>,
        methods: Option<Value>,
    ) -> Result<HostLease, Error> {
        self.require("hosts")?;
        let _changing = self.host_changes.lock().await;
        let first = {
            let mut hosts = self.imports.hosts.lock().unwrap();
            match hosts.get_mut(name) {
                Some(entry) => {
                    entry.leases = entry
                        .leases
                        .checked_add(1)
                        .ok_or_else(|| Error::Value(format!("host lease overflow for {name}")))?;
                    entry.dispatch = dispatch.clone();
                    false
                }
                None => {
                    hosts.insert(
                        name.to_owned(),
                        HostEntry {
                            dispatch: dispatch.clone(),
                            leases: 1,
                        },
                    );
                    true
                }
            }
        };
        if first {
            let methods = methods
                .or_else(|| dispatch.methods())
                .unwrap_or_else(|| json!({}));
            if let Err(error) = self
                .call_async("", "hosts.provide", json!([name, methods]))
                .await
            {
                self.imports.hosts.lock().unwrap().remove(name);
                return Err(error);
            }
        }
        Ok(HostLease {
            process: self.clone(),
            name: name.to_owned(),
        })
    }

    async fn release_host(&self, name: &str) -> Result<(), Error> {
        let _changing = self.host_changes.lock().await;
        let last = {
            let mut hosts = self.imports.hosts.lock().unwrap();
            let Some(entry) = hosts.get_mut(name) else {
                return Ok(());
            };
            entry.leases -= 1;
            let last = entry.leases == 0;
            if last {
                hosts.remove(name);
            }
            last
        };
        match last {
            true => self
                .call_async("", "hosts.withdraw", json!([name]))
                .await
                .map(|_| ()),
            false => Ok(()),
        }
    }

    /// The handle of the object currently in an exported slot, if available.
    pub fn service(&self, name: &str) -> Option<String> {
        self.imports
            .slots
            .lock()
            .unwrap()
            .get(name)
            .and_then(|(handle, _)| handle.clone())
    }

    /// Tell the Cordis side that no Rust proxy uses `handle` any longer.
    /// Best effort: a closed session has already released everything.
    pub fn release(&self, handle: &str) {
        let peer = self.peer.clone();
        let handle = handle.to_owned();
        self.runtime.spawn(async move {
            let _ = peer
                .invoke_async("", "release", json!([handle]).into())
                .await;
        });
    }

    pub fn connection(&self) -> &Connection {
        &self.peer
    }

    pub fn call(&self, service: &str, method: &str, args: Value) -> Result<Value, Error> {
        self.peer.invoke(service, method, args.into())?.json()
    }

    /// Call a method with protocol arguments (keeps `undefined` distinct);
    /// the result may contain object references (see `decode_value`).
    pub fn invoke(
        &self,
        handle: &str,
        method: &str,
        args: Vec<RpcValue>,
    ) -> Result<RpcValue, Error> {
        self.peer.invoke(handle, method, RpcValue::List(args))
    }

    /// Emit a declared event in the Cordis Context with `parallel`; resolves
    /// once the Cordis listeners are done.
    pub async fn emit(&self, event: &str, args: Vec<RpcValue>) -> Result<(), Error> {
        let args = RpcValue::List(vec![json!(event).into(), RpcValue::List(args)]);
        crate::runtime::rpc::settle(self.peer.invoke_async("", "emit", args).await?).await?;
        Ok(())
    }

    /// Read a declared property of the service object `handle` (live).
    pub fn get(&self, handle: &str, property: &str) -> Result<RpcValue, Error> {
        self.peer
            .invoke("", "get", json!([handle, property]).into())
    }

    /// Asynchronous form of [`Process::invoke`]: awaits a returned Promise.
    pub async fn invoke_async(
        &self,
        handle: &str,
        method: &str,
        args: Vec<RpcValue>,
    ) -> Result<RpcValue, Error> {
        crate::runtime::rpc::settle(
            self.peer
                .invoke_async(handle, method, RpcValue::List(args))
                .await?,
        )
        .await
    }

    pub async fn call_async(
        &self,
        service: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, Error> {
        crate::runtime::rpc::settle(self.peer.invoke_async(service, method, args.into()).await?)
            .await?
            .json()
    }

    pub async fn dispose(&self) -> Result<(), Error> {
        // A session something else owns stays, and so does its runtime; its
        // rows were unloaded one by one.
        if self.routed.is_some() {
            return Ok(());
        }
        let result = self.call_async("", "dispose", Value::Null).await;
        self.peer
            .close(Error::Transport("plugin has been disposed".into()));
        let Some(child) = &self.child else {
            return result.map(|_| ());
        };
        let status = child.exited().await;
        result?;
        if status != "exited normally" {
            return Err(Error::Transport(format!("Cordis process {status}")));
        }
        Ok(())
    }

    /// How the Node process ended (for example `exited with signal: 9
    /// (SIGKILL)`), or `None` while it runs.
    pub fn exit_status(&self) -> Option<String> {
        self.child.as_ref().and_then(|child| child.status())
    }

    /// Resolves once the session with the Node process has ended, whether
    /// by disposal or because the process went away.
    pub async fn closed(&self) {
        self.peer.closed().await
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // A session something else owns stays; only the routing goes.
        if self.routed.is_none() {
            self.peer.close(Error::Transport("process dropped".into()));
        }
    }
}
