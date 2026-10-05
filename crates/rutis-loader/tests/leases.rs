//! Leases of remote runtimes: a runtime that listens serves one controller
//! at a time; whatever a session loaded is cleaned up when it ends; a newer
//! connection takes over only after the old lease is gone; a controller
//! that reconnects gets a new lease. Each plugin start and cleanup is
//! written to a log on the runtime's side. Python needs `websockets`
//! (RUTIS_INTEROP_PYTHON, else python3).
#![cfg(all(unix, feature = "interop"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rutis::{Ctx, FiberView};
use rutis_bridge::{
    peer_key, Credential, IdentityPlugin, LinkConfig, LinkPlugin, LinkState, Peer, Retry,
    RuntimeAccessPlugin, StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::RuntimePlugin;
use rutis_loader::{
    Chain, InteropResolver, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeRowsPlugin,
};
use rutis_transport_websocket::{Config, WebSocketPlugin};
use serde_json::json;
use tokio::io::AsyncBufReadExt;

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

async fn eventually<T>(mut check: impl FnMut() -> Option<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(found) = check() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

#[derive(Clone, Copy, Debug)]
enum Language {
    Python,
    Node,
}

/// A runtime listening for controllers, and the log its plugin writes.
struct Remote {
    _process: tokio::process::Child,
    address: String,
    log: PathBuf,
    language: Language,
    _project: tempfile::TempDir,
}

impl Remote {
    fn lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn wait_for(&self, line: &str) {
        eventually(
            || self.lines().iter().any(|l| l == line).then_some(()),
            &format!("{line:?} in {:?}", self.lines()),
        )
        .await;
    }

    fn row(&self) -> &'static str {
        match self.language {
            Language::Python => "py:logger",
            Language::Node => "logger",
        }
    }

    fn runtime(&self) -> &'static str {
        match self.language {
            Language::Python => "py",
            Language::Node => "node",
        }
    }
}

async fn remote(language: Language) -> Remote {
    let project = tempfile::tempdir().unwrap();
    let log = project.path().join("lease.log");
    let mut command = match language {
        Language::Python => {
            std::fs::write(
                project.path().join("logger.py"),
                format!(
                    "def apply(ctx, config):\n    log = open({log:?}, 'a')\n    log.write(f\"start {{config['who']}}\\n\"); log.flush()\n    def stop():\n        log.write(f\"stop {{config['who']}}\\n\"); log.close()\n    return stop\n",
                    log = log.display().to_string()
                ),
            )
            .unwrap();
            let python = std::env::var("RUTIS_INTEROP_PYTHON").unwrap_or_else(|_| "python3".into());
            let mut command = tokio::process::Command::new(python);
            command
                .args(["-m", "rutis_runtime", "listen:ws://127.0.0.1:0/rutis"])
                .args(["--id", "remote", "--peer", "main"])
                .arg(project.path())
                .env("PYTHONPATH", repo().join("interop/python"))
                .current_dir(project.path());
            command
        }
        Language::Node => {
            let anchor = project.path().join("package.json");
            std::fs::write(&anchor, "{}").unwrap();
            let package = project.path().join("node_modules/logger");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join("package.json"),
                json!({ "name": "logger", "version": "1.0.0", "type": "module", "main": "index.mjs" }).to_string(),
            )
            .unwrap();
            std::fs::write(
                package.join("index.mjs"),
                format!(
                    "import {{ appendFileSync }} from 'node:fs'\nexport function apply(ctx, config) {{\n  appendFileSync({log:?}, `start ${{config.who}}\\n`)\n  ctx.effect(() => () => appendFileSync({log:?}, `stop ${{config.who}}\\n`))\n}}\n",
                    log = log.display().to_string()
                ),
            )
            .unwrap();
            let runtime = repo().join("interop/node");
            let mut command = tokio::process::Command::new("node");
            command
                .args(["--import", "tsx"])
                .arg(runtime.join("src/runner.mjs"))
                .arg("listen:ws://127.0.0.1:0/rutis")
                .args(["--id", "remote", "--peer", "main"])
                .arg(&anchor)
                .current_dir(&runtime);
            command
        }
    };
    let mut process = command
        .env("RUTIS_INTEROP_TOKEN", "main-token")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(process.stderr.take().unwrap()).lines();
    let address = loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("the runtime's address");
        if let Some(address) = line.strip_prefix("rutis-interop: listening on ") {
            break address.to_owned();
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Remote {
        _process: process,
        address,
        log,
        language,
        _project: project,
    }
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A controller: an application named `main` whose loader runs one row of
/// the remote runtime, configured with `who`.
struct Controller {
    root: Ctx,
    link: FiberView,
    states: tokio::sync::watch::Receiver<LinkState>,
}

async fn controller(remote: &Remote, who: &str) -> Controller {
    let root = Ctx::root().unwrap();
    (&root.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
        .await
        .unwrap();
    root.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main"))
            .present(id("remote"), Credential::Bearer("main-token".into())),
    ));
    let link = LinkPlugin::new(
        LinkConfig::dial(id("remote"), "websocket", "main", &remote.address)
            .require("runtime")
            .retry(Retry {
                initial: Duration::from_millis(50),
                max: Duration::from_millis(300),
                ..Retry::default()
            }),
    );
    let states = link.state();
    let link = root.plugin(link);
    root.plugin(RuntimeAccessPlugin::new(id("remote"), remote.runtime()));
    let runtime = RuntimePlugin::remote(remote.runtime());
    let rows = Arc::new(match remote.language {
        Language::Python => InteropResolver::modules(runtime.handle()),
        Language::Node => InteropResolver::node(runtime.handle()),
    });
    root.plugin(runtime);
    let plugin = LoaderPlugin::new(
        Chain::new().with_shared(rows.clone()),
        LoaderOptions::default(),
    );
    let loader = plugin.handle();
    (&root.plugin(plugin)).await.unwrap();
    root.plugin(RuntimeRowsPlugin::new(rows));
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [
        { "id": "l", "name": remote.row(), "config": { "who": who } }
    ] }]))
    .unwrap();
    loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();
    Controller { root, link, states }
}

async fn successive_controllers_get_clean_leases(language: Language) {
    let remote = remote(language).await;
    let first = controller(&remote, "a").await;
    remote.wait_for("start a").await;
    // The controller leaves: its lease is cleaned up, the runtime stays.
    first.link.dispose().await.unwrap();
    remote.wait_for("stop a").await;
    let second = controller(&remote, "b").await;
    remote.wait_for("start b").await;
    drop(second);
    drop(first);
}

async fn a_newer_controller_takes_over_after_the_old_lease_is_gone(language: Language) {
    let remote = remote(language).await;
    let first = controller(&remote, "a").await;
    remote.wait_for("start a").await;
    let second = controller(&remote, "b").await;
    remote.wait_for("start b").await;
    let lines = remote.lines();
    let stop_a = lines
        .iter()
        .position(|l| l == "stop a")
        .expect("the old lease cleaned up");
    let start_b = lines.iter().position(|l| l == "start b").unwrap();
    assert!(
        stop_a < start_b,
        "the old lease goes before the new one starts: {lines:?}"
    );
    // The replaced controller stops trying, or it would take over back.
    first.link.dispose().await.unwrap();
    drop(second);
}

async fn a_reconnecting_controller_gets_a_new_lease(language: Language) {
    let remote = remote(language).await;
    let controller = controller(&remote, "a").await;
    remote.wait_for("start a").await;
    let peer = eventually(
        || controller.root.get_as::<Peer>(peer_key(&id("remote"))),
        "the peer",
    )
    .await;
    let generation = peer.generation();
    peer.connection()
        .close(rutis_interop::Error::Transport("cut".into()));
    drop(peer);
    remote.wait_for("stop a").await;
    eventually(
        || matches!(*controller.states.borrow(), LinkState::Ready { generation: g } if g > generation).then_some(()),
        "the link back",
    )
    .await;
    eventually(
        || (remote.lines().iter().filter(|l| *l == "start a").count() == 2).then_some(()),
        "the row loaded again",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_successive_controllers_get_clean_leases() {
    successive_controllers_get_clean_leases(Language::Python).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_takeover_after_the_old_lease_is_gone() {
    a_newer_controller_takes_over_after_the_old_lease_is_gone(Language::Python).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_reconnecting_controller_gets_a_new_lease() {
    a_reconnecting_controller_gets_a_new_lease(Language::Python).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_successive_controllers_get_clean_leases() {
    successive_controllers_get_clean_leases(Language::Node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_takeover_after_the_old_lease_is_gone() {
    a_newer_controller_takes_over_after_the_old_lease_is_gone(Language::Node).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_reconnecting_controller_gets_a_new_lease() {
    a_reconnecting_controller_gets_a_new_lease(Language::Node).await;
}
