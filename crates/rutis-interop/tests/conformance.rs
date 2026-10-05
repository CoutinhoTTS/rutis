//! The session conformance suite against every implementation of the
//! session: Rust, Node and Python, each serving `conformance` as endpoint
//! of an endpoint-format session.
#![cfg(all(unix, feature = "conformance"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rutis_channel::PeerId;
use rutis_interop::conformance::{session, Fixture};
use rutis_interop::rpc::{Connection, Endpoint, Format};

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// No calls into `main`: the suite only calls the far end.
struct Nothing;
impl rutis_interop::rpc::Dispatch for Nothing {
    fn invoke(
        &self,
        _: &Connection,
        _: &str,
        _: &str,
        _: rutis_interop::rpc::Value,
    ) -> rutis_interop::rpc::Reply {
        Err(rutis_interop::Error::Value("main serves nothing".into()))
    }
}

fn main_endpoint(far: &str) -> Format {
    Format::Endpoint(Endpoint::rust(id("main")).expect(id(far)))
}

async fn run(far: Connection) {
    far.ready().await.unwrap();
    tokio::task::spawn_blocking(move || session(&far))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_meets_the_session_contract() {
    let (a, b) = rutis_transport_memory::pair();
    let _far = Connection::open_with(
        b,
        Arc::new(Fixture::default()),
        Format::Endpoint(Endpoint::rust(id("rust")).expect(id("main"))),
    )
    .unwrap();
    run(Connection::open_with(a, Arc::new(Nothing), main_endpoint("rust")).unwrap()).await;
}

/// Listen on a Unix socket, start `command` with it, open the session.
async fn far_end(
    mut command: tokio::process::Command,
    far: &str,
) -> (Connection, tokio::process::Child) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("conformance.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let child = command.arg(&socket).kill_on_drop(true).spawn().unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let stream = stream.into_std().unwrap();
    stream.set_nonblocking(false).unwrap();
    let reader = stream.try_clone().unwrap();
    struct Shut(std::os::unix::net::UnixStream);
    impl rutis_channel::Closer for Shut {
        fn close(&self, _: &str) {
            let _ = self.0.shutdown(std::net::Shutdown::Both);
        }
    }
    let closer = Arc::new(Shut(stream.try_clone().unwrap()));
    // The newline framing local runtimes speak, here from the transport crate.
    let channel = rutis_transport_local::framed(reader, stream, closer);
    std::mem::forget(directory);
    (
        Connection::open_with(channel, Arc::new(Nothing), main_endpoint(far)).unwrap(),
        child,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn node_meets_the_session_contract() {
    let mut command = tokio::process::Command::new("node");
    command
        .args(["--import", "tsx"])
        .arg(repo().join("interop/node/test/fixtures/conformance-session.mjs"))
        .current_dir(repo().join("interop/node"));
    let (far, _child) = far_end(command, "node").await;
    run(far).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_meets_the_session_contract() {
    let mut command = tokio::process::Command::new("python3");
    command
        .arg(repo().join("interop/python/tests/conformance_session.py"))
        .env("PYTHONPATH", repo().join("interop/python"));
    let (far, _child) = far_end(command, "python").await;
    run(far).await;
}
