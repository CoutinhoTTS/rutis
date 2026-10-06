//! A running host: the kernel, the local language runtimes the
//! configuration names, the transports and identity links use, and the
//! loader that runs the rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rutis::Ctx;
use rutis_bridge::runtime::{LocalRuntime, RuntimeHandle, RuntimePlugin, RuntimeState};
use rutis_bridge::transport::local::LocalPlugin;
use rutis_bridge::transport::websocket::{
    Config, ListenerConfig, ServerTls, Trust, WebSocketPlugin,
};
use rutis_bridge::{Credential, IdentityPlugin, PeerId, StaticIdentity};
use rutis_loader::{
    register_peer_node, Builtins, Chain, Layer, Loader, LoaderOptions, LoaderPlugin, Patch,
    PeerResolver, RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
};

use crate::config::{token, HostConfig, NodeRuntime, PythonRuntime};

pub struct Host {
    /// The host runs while this lives.
    _root: Ctx,
    pub loader: Loader,
    pub runtimes: Vec<(String, RuntimeHandle)>,
    /// The resolvers of the runtimes' rows, to invalidate after a change.
    pub resolvers: Vec<Arc<RuntimeResolver>>,
}

pub type Error = String;

impl Host {
    /// Start everything `config` names; the rows are not loaded yet.
    pub async fn start(config: &HostConfig) -> Result<Self, Error> {
        let root = Ctx::root().map_err(|error| error.to_string())?;
        let mut catalog = ServiceCatalog::new();
        catalog.share_by_name();

        // Transports and this node's identity, for the rows that link.
        (&root.plugin(LocalPlugin::new()))
            .await
            .map_err(|error| format!("the local transport: {error}"))?;
        let websocket = WebSocketPlugin::new(websocket(config)?)?;
        (&root.plugin(websocket))
            .await
            .map_err(|error| format!("the WebSocket transport: {error}"))?;
        root.plugin(IdentityPlugin::new("host", identity(config)?));

        let mut runtimes = Vec::new();
        let mut resolvers = Vec::new();
        let peers = Arc::new(PeerResolver::new());
        let mut builtins = Builtins::new();
        register_peer_node(&mut builtins, peers.clone());
        let mut chain = Chain::new().with(builtins).with_shared(peers);
        if let Some(py) = &config.runtimes.py {
            let runtime = python(py)?;
            let handle = runtime.handle();
            let rows = Arc::new(RuntimeResolver::modules(handle.clone()).with_catalog(&catalog));
            root.plugin(runtime);
            chain = chain.with_shared(rows.clone());
            runtimes.push(("py".to_owned(), handle));
            resolvers.push(rows);
        }
        if let Some(node) = &config.runtimes.node {
            let runtime = node_runtime(node)?;
            let handle = runtime.handle();
            let rows = Arc::new(RuntimeResolver::node(handle.clone()).with_catalog(&catalog));
            root.plugin(runtime);
            chain = chain.with_shared(rows.clone());
            runtimes.push(("node".to_owned(), handle));
            resolvers.push(rows);
        }

        for remote in &config.runtimes.remote {
            let runtime = RuntimePlugin::remote(&remote.name);
            let handle = runtime.handle();
            let rows = match remote.language.as_str() {
                "python" | "py" => RuntimeResolver::modules(handle.clone()),
                "node" => RuntimeResolver::node(handle.clone()),
                other => {
                    return Err(format!(
                        "remote runtime {}: the language is python or node, not {other}",
                        remote.name
                    ))
                }
            };
            let rows = Arc::new(rows.with_catalog(&catalog));
            root.plugin(runtime);
            chain = chain.with_shared(rows.clone());
            resolvers.push(rows);
            // Not waited for at start: it runs when its link does.
        }

        let plugin = LoaderPlugin::new(
            chain,
            LoaderOptions {
                catalog,
                ..LoaderOptions::default()
            },
        );
        let loader = plugin.handle();
        (&root.plugin(plugin))
            .await
            .map_err(|error| format!("the loader: {error}"))?;
        for rows in &resolvers {
            root.plugin(RuntimeRowsPlugin::new(rows.clone()));
        }
        Ok(Self {
            _root: root,
            loader,
            runtimes,
            resolvers,
        })
    }

    /// Wait until every runtime runs, or say which one could not start.
    pub async fn runtimes_ready(&self) -> Result<(), Error> {
        for (name, handle) in &self.runtimes {
            loop {
                match handle.state() {
                    RuntimeState::Ready(_) => break,
                    RuntimeState::Down(reason) => {
                        return Err(format!("the {name} runtime did not start: {reason}"))
                    }
                    _ => handle.changed().await,
                }
            }
        }
        Ok(())
    }

    /// Load `rows` (replacing what was loaded before).
    pub async fn load(&self, rows: Vec<serde_json::Value>) -> Result<(), Error> {
        let patches: Vec<Patch> = serde_json::from_value(serde_json::json!([{ "insert": rows }]))
            .map_err(|error| format!("rows: {error}"))?;
        self.loader
            .reconcile(vec![Layer::new("rutis.json", patches)], None)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// The service `name` as rutis sees it (what a row provides).
    #[allow(dead_code)]
    pub fn service(&self, name: &str) -> Option<Arc<dyn rutis_bridge::session::HostDispatch>> {
        self._root
            .get_as::<dyn rutis_bridge::session::HostDispatch>(rutis_bridge::session::host_key(
                name,
            ))
    }

    /// Forget what the runtimes' resolvers cached, so changed plugins are
    /// described again.
    pub fn invalidate(&self) {
        for rows in &self.resolvers {
            rows.invalidate_all();
        }
    }
}

fn websocket(config: &HostConfig) -> Result<Config, Error> {
    let read =
        |path: &Path| std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()));
    let env_path = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let mut websocket = Config::new();
    if let Some(ca) = env_path("RUTIS_CA") {
        websocket = websocket.trust(Trust {
            system: true,
            ca_pem: vec![read(&ca)?],
        });
    }
    let local = PeerId::new(config.id.clone()).map_err(|error| format!("id: {error}"))?;
    for listener in &config.listen {
        let mut listening = ListenerConfig::new(&listener.name, listener.address, local.clone());
        let cert = listener.cert.clone().or_else(|| env_path("RUTIS_CERT"));
        let key = listener.key.clone().or_else(|| env_path("RUTIS_KEY"));
        if let (Some(cert), Some(key)) = (cert, key) {
            listening = listening.tls(ServerTls {
                certificate_pem: read(&cert)?,
                key_pem: read(&key)?,
                client_ca_pem: None,
            });
        }
        websocket = websocket.listener(listening);
    }
    websocket.validate()?;
    Ok(websocket)
}

fn identity(config: &HostConfig) -> Result<StaticIdentity, Error> {
    let local = PeerId::new(config.id.clone()).map_err(|error| format!("id: {error}"))?;
    let mut identity = StaticIdentity::new(local);
    for (peer, dials) in config.peers() {
        let Some(token) = token(&peer) else { continue };
        let peer = PeerId::new(peer.clone()).map_err(|error| format!("peer {peer}: {error}"))?;
        identity = match dials {
            true => identity.present(peer, Credential::Bearer(token)),
            false => identity.accept_token(token, peer),
        };
    }
    Ok(identity)
}

/// The Node runtime of `node.project`, with the `@arcships/rutis-runtime`
/// package installed there (or given, or next to this program).
fn node_runtime(node: &NodeRuntime) -> Result<LocalRuntime, Error> {
    let anchor = node.project.join("package.json");
    if !anchor.exists() {
        return Err(format!(
            "the Node runtime needs a package.json in {}: run `npm init -y` there",
            node.project.display()
        ));
    }
    let candidates = [
        node.runtime.clone(),
        Some(node.project.join("node_modules/@arcships/rutis-runtime")),
        std::env::var_os("RUTIS_NODE_RUNTIME").map(PathBuf::from),
    ];
    let package = candidates
        .into_iter()
        .flatten()
        .find(|package| package.join("package.json").exists())
        .ok_or_else(|| {
            format!(
                "the Node runtime is not installed in {}: run `npm install @arcships/rutis-runtime` there",
                node.project.display()
            )
        })?;
    Ok(LocalRuntime::node(package, anchor))
}

/// The Python runtime of `py.project`, with an interpreter that has the
/// `rutis` package.
fn python(py: &PythonRuntime) -> Result<LocalRuntime, Error> {
    let interpreter = py
        .python
        .clone()
        .or_else(|| {
            std::env::var_os("VIRTUAL_ENV").map(|venv| PathBuf::from(venv).join("bin/python"))
        })
        .or_else(|| Some(py.project.join(".venv/bin/python")).filter(|venv| venv.exists()))
        .unwrap_or_else(|| PathBuf::from("python3"));
    // A source checkout of the rutis package (tests, development of rutis).
    let source = std::env::var_os("RUTIS_PYTHON_PATH");
    let mut check = std::process::Command::new(&interpreter);
    check.args(["-c", "import rutis"]);
    if let Some(source) = &source {
        check.env("PYTHONPATH", source);
    }
    let found = check
        .output()
        .map_err(|error| format!("{}: {error}", interpreter.display()))?;
    if !found.status.success() {
        return Err(format!(
            "the Python runtime needs the rutis package in {}: run `{} -m pip install rutis` (or `uv add rutis`)",
            interpreter.display(),
            interpreter.display()
        ));
    }
    let mut runtime = LocalRuntime::python(&py.project).interpreter(interpreter);
    if let Some(source) = source {
        runtime = runtime.python_path(source);
    }
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NodeRuntime, PythonRuntime, Runtimes};
    use rutis_bridge::session::{settle, Value};
    use serde_json::json;
    use std::time::Duration;

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    async fn greeting(host: &Host) -> String {
        let service = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(service) = host.service("greeter") {
                    return service;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the greeter");
        let reply = service
            .invoke("hello", Value::List(vec![json!("Ada").into()]))
            .unwrap();
        settle(reply)
            .await
            .unwrap()
            .json()
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Reloading a row runs the plugin's edited code, in Node and Python.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reloaded_row_runs_the_edited_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let sdk = repo()
            .join("node/rutis/src/index.mjs")
            .canonicalize()
            .unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "try", "type": "module" }"#,
        )
        .unwrap();
        let node_plugin = |word: &str| {
            format!(
                "import {{ definePlugin }} from 'file://{}'\n\
                 export default definePlugin({{ provides: {{ greeter: {{ hello: 'sync' }} }}, apply(ctx) {{\n\
                 ctx.provide('greeter', {{ hello: name => '{word}, ' + name }}) }} }})\n",
                sdk.display()
            )
        };
        let entry = dir.path().join("greeter.mjs");
        std::fs::write(&entry, node_plugin("Hello")).unwrap();
        let config = HostConfig {
            id: "test".into(),
            runtimes: Runtimes {
                node: Some(NodeRuntime {
                    project: dir.path().to_owned(),
                    runtime: Some(repo().join("node/rutis-runtime")),
                }),
                ..Runtimes::default()
            },
            listen: Vec::new(),
            rows: vec![json!({ "id": "greeter", "name": format!("file://{}", entry.display()) })],
        };
        let host = Host::start(&config).await.unwrap();
        host.runtimes_ready().await.unwrap();
        host.load(config.rows()).await.unwrap();
        assert_eq!(greeting(&host).await, "Hello, Ada");
        // A different size, so the edit shows even within the same instant.
        std::fs::write(&entry, node_plugin("Welcome")).unwrap();
        host.invalidate();
        host.loader.reload("greeter").await.unwrap();
        assert_eq!(greeting(&host).await, "Welcome, Ada");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reloaded_python_row_runs_the_edited_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let python_plugin = |word: &str| {
            format!(
                "provides = {{'greeter': {{'hello': 'sync'}}}}\n\
                 class Greeter:\n    def hello(self, name):\n        return '{word}, ' + name\n\
                 def apply(ctx, config):\n    ctx.provide('greeter', Greeter())\n"
            )
        };
        std::fs::write(dir.path().join("greeter.py"), python_plugin("Hello")).unwrap();
        // The repository's rutis package, for an interpreter without it.
        std::env::set_var("RUTIS_PYTHON_PATH", repo().join("python/rutis"));
        let config = HostConfig {
            id: "test".into(),
            runtimes: Runtimes {
                py: Some(PythonRuntime {
                    project: dir.path().to_owned(),
                    python: std::env::var_os("RUTIS_PYTHON").map(PathBuf::from),
                }),
                ..Runtimes::default()
            },
            listen: Vec::new(),
            rows: vec![json!({ "id": "greeter", "name": "py:greeter" })],
        };
        let host = Host::start(&config).await.unwrap();
        host.runtimes_ready().await.unwrap();
        host.load(config.rows()).await.unwrap();
        assert_eq!(greeting(&host).await, "Hello, Ada");
        std::fs::write(dir.path().join("greeter.py"), python_plugin("Welcome")).unwrap();
        host.loader.reload("greeter").await.unwrap();
        assert_eq!(greeting(&host).await, "Welcome, Ada");
    }

    /// A runtime on another machine (here, a process listening on loopback):
    /// a peer row links to it, and its rows are `<runtime>:<module>`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_remote_runtime_runs_rows_named_after_it() {
        use tokio::io::AsyncBufReadExt;
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("greeter.py"),
            "provides = {'greeter': {'hello': 'sync'}}\n\
             class Greeter:\n    def hello(self, name):\n        return 'Remote, ' + name\n\
             def apply(ctx, config):\n    ctx.provide('greeter', Greeter())\n",
        )
        .unwrap();
        let python = std::env::var("RUTIS_PYTHON").unwrap_or_else(|_| "python3".into());
        let mut child = tokio::process::Command::new(python)
            .args([
                "-m",
                "rutis",
                "listen:ws://127.0.0.1:0/rutis",
                "--id",
                "gpu",
                "--peer",
                "remote-test",
            ])
            .arg(project.path())
            .env("PYTHONPATH", repo().join("python/rutis"))
            .env("RUTIS_TOKEN", "remote-token")
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
        let address = loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("the runtime's address");
            if let Some(address) = line.strip_prefix("rutis: listening on ") {
                break address.to_owned();
            }
        };
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

        std::env::set_var("RUTIS_TOKEN_GPU", "remote-token");
        let config: HostConfig = serde_json::from_value(json!({
            "id": "remote-test",
            "runtimes": { "remote": [{ "name": "gpu", "language": "python" }] },
            "rows": [
                { "id": "gpu", "name": "rutis-bridge/peer", "config": { "peer": "gpu", "dial": address, "runtime": "gpu" } },
                { "id": "greeter", "name": "gpu:greeter" }
            ]
        }))
        .unwrap();
        let host = Host::start(&config).await.unwrap();
        host.load(config.rows()).await.unwrap();
        assert_eq!(greeting(&host).await, "Remote, Ada");
        child.start_kill().unwrap();
    }
}
