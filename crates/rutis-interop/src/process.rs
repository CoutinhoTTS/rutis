use crate::rpc::{Connection, Dispatch, Reply, Value as RpcValue};
use crate::Error;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

/// Receives changes of exported Cordis service slots. `handle` addresses the
/// object now in the slot (`None` when it is unavailable); `version` orders
/// changes, so an older notification must not override a newer one.
pub trait ServiceEvents: Send + Sync + 'static {
    fn changed(&self, name: &str, handle: Option<String>, version: u64);
}

/// A rutis service provided to the mounted Cordis plugins: the Node side
/// registers a proxy under `name` whose calls arrive here.
pub trait HostDispatch: Send + Sync + 'static {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply;
}

/// One host-provided service: its Cordis name, the bound methods as
/// `{ method: "sync" | "async" }`, and the dispatcher that serves them.
pub struct Host {
    pub name: String,
    pub methods: Value,
    pub dispatch: Arc<dyn HostDispatch>,
}

type Slots = Arc<Mutex<HashMap<String, (Option<String>, u64)>>>;

/// Records the newest handle per slot and forwards newer changes; serves
/// calls to host-provided services.
struct Imports {
    slots: Slots,
    events: Option<Arc<dyn ServiceEvents>>,
    hosts: HashMap<String, Arc<dyn HostDispatch>>,
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
            events.changed(&name, handle, version);
        }
    }
}
impl Dispatch for Imports {
    fn invoke(&self, _: &Connection, target: &str, method: &str, args: RpcValue) -> Reply {
        if let Some(name) = target.strip_prefix("host:") {
            let host = self
                .hosts
                .get(name)
                .ok_or_else(|| Error::Value(format!("no host service {name}")))?;
            return host.invoke(method, args);
        }
        if !(target.is_empty() && method == "service") {
            return Err(Error::Value(
                "application has no exported service target".into(),
            ));
        }
        let (name, handle, version): (String, Option<String>, u64) = crate::decode(args.json()?)?;
        self.update(name, handle, version);
        Ok(RpcValue::Undefined)
    }
}

/// Owns one native Cordis process and its generated service bindings.
pub struct Process {
    peer: Connection,
    imports: Arc<Imports>,
    runtime: tokio::runtime::Handle,
    child: tokio::sync::Mutex<tokio::process::Child>,
    _directory: tempfile::TempDir,
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
        let provided: serde_json::Map<String, Value> = hosts
            .iter()
            .map(|host| (host.name.clone(), host.methods.clone()))
            .collect();
        let hosts = hosts
            .into_iter()
            .map(|host| (host.name, host.dispatch))
            .collect();
        let Some((plugin, _)) = plugins.first() else {
            return Err(Error::Value("a mount needs at least one plugin".into()));
        };
        let plugins: Vec<Value> = plugins
            .iter()
            .map(|(entry, config)| json!({ "entry": entry, "config": config }))
            .collect();
        let directory = tempfile::Builder::new()
            .prefix("rutis-mount-")
            .tempdir()
            .map_err(|error| Error::Transport(error.to_string()))?;
        let socket = directory.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket)
            .map_err(|error| Error::Transport(error.to_string()))?;
        let mut child = tokio::process::Command::new("node")
            .arg("--import")
            .arg("tsx")
            .arg(node_package.join("src/runner.mjs"))
            .arg(&socket)
            .arg(plugin)
            .current_dir(node_package)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| Error::Transport(error.to_string()))?;
        let stream = tokio::select! {
            accepted = listener.accept() => accepted
                .map_err(|error| Error::Transport(error.to_string()))?.0,
            status = child.wait() => return Err(Error::Transport(format!("Cordis process exited before connecting: {status:?}"))),
        };
        let stream = stream
            .into_std()
            .map_err(|error| Error::Transport(error.to_string()))?;
        stream
            .set_nonblocking(false)
            .map_err(|error| Error::Transport(error.to_string()))?;
        let imports = Arc::new(Imports {
            slots: Slots::default(),
            events,
            hosts,
        });
        let peer = Connection::connect(stream, imports.clone())?;
        peer.ready().await?;
        let process = Arc::new(Self {
            peer,
            imports,
            runtime: tokio::runtime::Handle::current(),
            child: tokio::sync::Mutex::new(child),
            _directory: directory,
        });
        let mounted = process
            .call_async(
                "",
                "mount",
                json!({ "plugins": plugins, "services": services, "provided": provided }),
            )
            .await?;
        let slots: HashMap<String, (Option<String>, u64)> =
            crate::decode(mounted["services"].clone())?;
        for (name, (handle, version)) in slots {
            process.imports.update(name, handle, version);
        }
        Ok(process)
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
        crate::rpc::settle(
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
        crate::rpc::settle(self.peer.invoke_async(service, method, args.into()).await?)
            .await?
            .json()
    }

    pub async fn dispose(&self) -> Result<(), Error> {
        let result = self.call_async("", "dispose", Value::Null).await;
        self.peer
            .close(Error::Transport("plugin has been disposed".into()));
        let status = self
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;
        result?;
        if !status.success() {
            return Err(Error::Transport(format!("Cordis process exited: {status}")));
        }
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.peer.close(Error::Transport("process dropped".into()));
    }
}
