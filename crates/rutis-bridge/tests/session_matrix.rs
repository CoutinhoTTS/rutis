#![cfg(all(unix, feature = "testing", feature = "websocket"))]
//! The runtime conformance suite (`rutis_bridge::runtime::testing::runtime`) over
//! every channel: the Node and the Python runtime, each connected on an
//! inherited socket (`fd:3`), on a socket path it dials back, and over a
//! loopback WebSocket it listens on (dialed by Rust and attached).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rutis_bridge::runtime::{Launcher, Mount, Process};
use serde_json::{json, Value};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[derive(Clone, Copy, Debug)]
enum Runtime {
    Node,
    Python,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Via {
    Inherit,
    DialBack,
    WebSocket,
}

/// The Python interpreter: one with `websockets` for WebSocket channels
/// (`RUTIS_PYTHON`), else `python3`.
fn python() -> String {
    std::env::var("RUTIS_PYTHON").unwrap_or_else(|_| "python3".into())
}

/// Start `runtime` on a fresh project, connected as `via` says.
async fn start(runtime: Runtime, via: Via, dir: &Path) -> (Arc<Process>, PathBuf) {
    let (launcher, anchor, entry) = match runtime {
        Runtime::Node => {
            let package = repo().join("node/rutis-runtime");
            let anchor = dir.join("package.json");
            std::fs::write(&anchor, "{}").unwrap();
            let entry = dir.join("weather.mjs");
            std::fs::copy(
                repo().join("node/rutis-runtime/test/fixtures/conformance-weather.mjs"),
                &entry,
            )
            .unwrap();
            let launcher = Launcher::new("node")
                .arg("--import")
                .arg("tsx")
                .arg(package.join("src/runner.mjs"))
                .cwd(&package);
            (launcher, anchor, entry)
        }
        Runtime::Python => {
            std::fs::copy(
                repo().join("python/rutis/tests/conformance_weather.py"),
                dir.join("weather_plugin.py"),
            )
            .unwrap();
            let mut path = repo().join("python/rutis").into_os_string();
            path.push(":");
            path.push(dir);
            let launcher = Launcher::new(python())
                .arg("-m")
                .arg("rutis")
                .env("PYTHONPATH", path);
            (launcher, dir.to_owned(), PathBuf::from("weather_plugin"))
        }
    };
    if via == Via::WebSocket {
        return (listening(launcher, &anchor).await, entry);
    }
    let launcher = match via {
        Via::Inherit => launcher.inherit_fd(),
        _ => launcher,
    };
    let process = Process::mount(
        &repo().join("node/rutis-runtime"),
        Mount {
            anchor: Some(&anchor),
            launcher: Some(&launcher),
            ..Mount::default()
        },
    )
    .await
    .unwrap_or_else(|error| panic!("{runtime:?} ({via:?}) failed to start: {error}"));
    (process, entry)
}

/// Start the runtime listening on a loopback WebSocket, dial it, attach.
async fn listening(launcher: Launcher, anchor: &Path) -> Arc<Process> {
    use rutis_bridge::channel::PeerId;
    use rutis_bridge::{Credential, Dial, Identity, StaticIdentity, Transport};
    use tokio::io::AsyncBufReadExt;

    let mut command = tokio::process::Command::new(&launcher.program);
    command
        .args(&launcher.args)
        .envs(launcher.env.iter().map(|(name, value)| (name, value)))
        .arg("listen:ws://127.0.0.1:0/rutis")
        .args(["--id", "runtime", "--peer", "main"])
        .arg(anchor)
        .env("RUTIS_TOKEN", "controller-token")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &launcher.cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().unwrap();
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
    // Keep reading its stderr, and keep it running for the test's length.
    tokio::spawn(async move {
        while let Ok(Some(_)) = lines.next_line().await {}
        let _ = child.wait().await;
    });
    let transport = rutis_bridge::transport::websocket::WebSocketTransport::start(
        rutis_bridge::transport::websocket::Config::new(),
    )
    .unwrap();
    let runtime = PeerId::new("runtime").unwrap();
    let identity: Arc<dyn Identity> =
        Arc::new(StaticIdentity::new(PeerId::new("main").unwrap()).present(
            runtime.clone(),
            Credential::Bearer("controller-token".into()),
        ));
    let channel = transport
        .dial(
            &Dial::address(address)
                .peer(runtime)
                .identity(identity)
                .protocol(format!(
                    "rutis.{}",
                    rutis_bridge::session::ENDPOINT_PROTOCOL
                )),
        )
        .await
        .unwrap();
    // The transport's threads must outlive the channel: leak it for the test.
    std::mem::forget(transport);
    let format = rutis_bridge::session::Format::Endpoint(
        rutis_bridge::session::Endpoint::rust(PeerId::new("main").unwrap())
            .expect(PeerId::new("runtime").unwrap()),
    );
    let process = Process::attach(channel, Mount::default(), format)
        .await
        .unwrap();
    assert!(
        process.connection().supports("runtime"),
        "a runner declares its contract"
    );
    process
}

async fn semantics(runtime: Runtime, via: Via) {
    let dir = tempfile::tempdir().unwrap();
    let (process, entry) = start(runtime, via, dir.path()).await;
    rutis_bridge::runtime::testing::runtime(process, &entry, via != Via::WebSocket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_on_an_inherited_socket() {
    semantics(Runtime::Node, Via::Inherit).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_dialing_a_socket_path() {
    semantics(Runtime::Node, Via::DialBack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_listening_on_a_websocket() {
    semantics(Runtime::Node, Via::WebSocket).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_on_an_inherited_socket() {
    semantics(Runtime::Python, Via::Inherit).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_dialing_a_socket_path() {
    semantics(Runtime::Python, Via::DialBack).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_listening_on_a_websocket() {
    semantics(Runtime::Python, Via::WebSocket).await;
}

/// A runtime package that does not list `fd` in `rutisChannels` is started
/// the old way, dialing a socket path.
#[tokio::test(flavor = "multi_thread")]
async fn a_runtime_without_fd_support_dials_back() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("runtime");
    std::fs::create_dir_all(package.join("src")).unwrap();
    let real = repo().join("node/rutis-runtime");
    for file in std::fs::read_dir(real.join("src")).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_dir() {
            continue;
        }
        std::fs::copy(file.path(), package.join("src").join(file.file_name())).unwrap();
    }
    copy_dir(&real.join("src/channel"), &package.join("src/channel"));
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(real.join("package.json")).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("rutisChannels");
    std::fs::write(package.join("package.json"), manifest.to_string()).unwrap();
    std::os::unix::fs::symlink(real.join("node_modules"), package.join("node_modules")).unwrap();
    // The runner sees its channel argument: record it.
    let anchor = dir.path().join("package.json");
    std::fs::write(&anchor, "{}").unwrap();
    let probe = dir.path().join("argv.mjs");
    std::fs::write(
        &probe,
        "export function apply(ctx) { ctx.provide('argv', { channel() { return process.argv[2] } }) }",
    )
    .unwrap();
    let process = Process::launch(&package, &probe, json!({}), json!({ "argv": ["channel"] }))
        .await
        .unwrap();
    let channel = process.call("argv", "channel", json!([])).unwrap();
    let channel = channel.as_str().unwrap();
    assert!(!channel.starts_with("fd:"), "{channel}");
    assert!(channel.ends_with("peer.sock"), "{channel}");
    process.dispose().await.unwrap();

    // The real package takes fd 3.
    let process = Process::launch(&real, &probe, json!({}), json!({ "argv": ["channel"] }))
        .await
        .unwrap();
    assert_eq!(
        process.call("argv", "channel", json!([])).unwrap(),
        json!("fd:3")
    );
    process.dispose().await.unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for file in std::fs::read_dir(from).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), to.join(file.file_name())).unwrap();
    }
}
