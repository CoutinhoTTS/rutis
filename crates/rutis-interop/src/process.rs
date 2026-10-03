use crate::events::EventSink;
use crate::rpc::{Connection, Dispatch, Reply, Value as RpcValue};
use crate::Error;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

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
        if target.is_empty() && method == "event" {
            let mut args = args.list()?.into_iter();
            let name: String = crate::decode(args.next().unwrap_or(RpcValue::Undefined).json()?)?;
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
        let (name, handle, version): (String, Option<String>, u64) = crate::decode(args.json()?)?;
        self.update(name, handle, version);
        Ok(RpcValue::Undefined)
    }
}

/// The Node process, owned by a task that records how it ended. Dropping
/// `_kill` ends the process.
struct Child {
    exit: watch::Receiver<Option<String>>,
    /// The same status for the reader thread, which must not depend on the
    /// runtime: a current-thread runtime may be blocked in a synchronous call.
    ended: Arc<(Mutex<Option<String>>, Condvar)>,
    _kill: oneshot::Sender<()>,
}

impl Child {
    fn watch(mut child: tokio::process::Child) -> Self {
        let (kill, killed) = oneshot::channel::<()>();
        let (report, exit) = watch::channel(None);
        let ended = Arc::new((Mutex::new(None), Condvar::new()));
        if let Some(pid) = child.id() {
            let record = ended.clone();
            let _ = std::thread::Builder::new()
                .name("rutis-interop-exit".into())
                .spawn(move || {
                    if let Some(status) = peek_exit(pid) {
                        record_exit(&record, describe(Ok(status)));
                    }
                });
        }
        let record = ended.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                _ = killed => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let status = describe(status);
            record_exit(&record, status.clone());
            report.send_replace(Some(status));
        });
        Self {
            exit,
            ended,
            _kill: kill,
        }
    }

    /// How the process ended, once it has.
    fn status(&self) -> Option<String> {
        self.exit.borrow().clone()
    }

    async fn exited(&self) -> String {
        let mut exit = self.exit.clone();
        let status = exit.wait_for(Option::is_some).await;
        status.map_or_else(|_| "is gone".to_owned(), |status| status.clone().unwrap())
    }

    /// The error that ends a session whose peer went away: waits briefly for
    /// the process to end so the error can say how.
    fn disconnected(&self) -> impl FnOnce() -> Error + Send {
        let ended = self.ended.clone();
        move || {
            let (status, changed) = &*ended;
            let status = changed
                .wait_timeout_while(status.lock().unwrap(), Duration::from_secs(1), |status| {
                    status.is_none()
                })
                .unwrap()
                .0
                .clone();
            Error::Transport(match status {
                Some(status) => format!("Cordis process {status}"),
                None => "peer disconnected".to_owned(),
            })
        }
    }
}

fn describe(status: std::io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) if status.success() => "exited normally".to_owned(),
        Ok(status) => format!("exited with {status}"),
        Err(error) => format!("cannot be waited for: {error}"),
    }
}

fn record_exit(ended: &(Mutex<Option<String>>, Condvar), status: String) {
    ended.0.lock().unwrap().get_or_insert(status);
    ended.1.notify_all();
}

/// Waits until the process `pid` ends and reports its status without reaping
/// it (tokio still does), independently of any runtime.
fn peek_exit(pid: u32) -> Option<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `info` is a valid, writable siginfo_t.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None; // already reaped: the runtime reports it
        }
    }
    // SAFETY: waitid filled a SIGCHLD siginfo_t.
    let status = unsafe { info.si_status() };
    // Rebuild the raw wait status so it formats like `ExitStatus`.
    let raw = match info.si_code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_DUMPED => status | 0x80,
        _ => status,
    };
    Some(std::process::ExitStatus::from_raw(raw))
}

/// Owns one native Cordis process and its generated service bindings.
pub struct Process {
    peer: Connection,
    imports: Arc<Imports>,
    runtime: tokio::runtime::Handle,
    child: Child,
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
            },
        )
        .await
    }

    /// Launch a mount: see [`Mount`].
    pub async fn mount(node_package: &Path, mount: Mount<'_>) -> Result<Arc<Self>, Error> {
        let Mount {
            plugins,
            services,
            observer: events,
            hosts,
            events: forwarded,
            emits,
            anchor,
        } = mount;
        let (forwarded_names, forwarded) = match forwarded {
            Some((names, sink)) => (names, Some(sink)),
            None => (Vec::new(), None),
        };
        let provided: serde_json::Map<String, Value> = hosts
            .iter()
            .map(|host| (host.name.clone(), host.methods.clone()))
            .collect();
        let hosts = hosts
            .into_iter()
            .map(|host| (host.name, host.dispatch))
            .collect();
        let plugin = match (plugins.first(), anchor) {
            (Some((plugin, _)), _) => *plugin,
            (None, Some(anchor)) => anchor,
            (None, None) => {
                return Err(Error::Value(
                    "a mount needs at least one plugin or an anchor".into(),
                ))
            }
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
            status = child.wait() => return Err(Error::Transport(match status {
                Ok(status) => format!("Cordis process exited before connecting: {status}"),
                Err(error) => format!("Cordis process exited before connecting: {error}"),
            })),
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
            forwarded,
        });
        let child = Child::watch(child);
        let peer =
            Connection::connect_with(stream, imports.clone(), Box::new(child.disconnected()))?;
        peer.ready().await?;
        let process = Arc::new(Self {
            peer,
            imports,
            runtime: tokio::runtime::Handle::current(),
            child,
            _directory: directory,
        });
        let mounted = process
            .call_async(
                "",
                "mount",
                json!({ "plugins": plugins, "services": services, "provided": provided, "events": forwarded_names, "emits": emits }),
            )
            .await?;
        let slots: HashMap<String, (Option<String>, u64)> =
            crate::decode(mounted["services"].clone())?;
        for (name, (handle, version)) in slots {
            process.imports.update(name, handle, version);
        }
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
        self.call_async("", "rows.unload", json!([key]))
            .await
            .map(|_| ())
    }

    /// The JSON Schema of the plugin at `entry` (its schemastery `Config`
    /// converted), or `None` when it declares none.
    pub async fn row_schema(&self, entry: &Path) -> Result<Option<Value>, Error> {
        let schema = self.call_async("", "rows.schema", json!([entry])).await?;
        Ok((!schema.is_null()).then_some(schema))
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
        crate::rpc::settle(self.peer.invoke_async("", "emit", args).await?).await?;
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
        let status = self.child.exited().await;
        result?;
        if status != "exited normally" {
            return Err(Error::Transport(format!("Cordis process {status}")));
        }
        Ok(())
    }

    /// How the Node process ended (for example `exited with signal: 9
    /// (SIGKILL)`), or `None` while it runs.
    pub fn exit_status(&self) -> Option<String> {
        self.child.status()
    }

    /// Resolves once the session with the Node process has ended, whether
    /// by disposal or because the process went away.
    pub async fn closed(&self) {
        self.peer.closed().await
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.peer.close(Error::Transport("process dropped".into()));
    }
}
