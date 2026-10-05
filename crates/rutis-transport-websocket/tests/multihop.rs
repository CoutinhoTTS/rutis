//! A service two links away, used from Node: b exports `clock` to a, a
//! re-exports it to a Cordis node c. c calls it synchronously with a
//! callback b calls back, which calls the clock again. Node runs only calls
//! of the chain it waits for, so this passes only if every hop rebases the
//! chain for the next session.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::{
    Credential, ExportPlugin, IdentityPlugin, ImportPlugin, LinkConfig, LinkPlugin, StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Reply, Value};
use rutis_interop::{host_key, Error, HostDispatch};
use rutis_transport_memory::{MemoryPlugin, MemoryTransport};
use rutis_transport_websocket::{Config, ListenerConfig, WebSocketPlugin};
use serde_json::{json, Value as Json};
use tokio::io::AsyncBufReadExt;

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

struct Clock(std::sync::atomic::AtomicU64);
impl HostDispatch for Clock {
    fn invoke(&self, method: &str, args: Value) -> Reply {
        match method {
            "now" => Ok(json!(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst)).into()),
            "each" => {
                let callback = args.list()?.remove(0).reference()?;
                let days = ["mon", "tue"]
                    .into_iter()
                    .map(|day| {
                        callback
                            .call(Value::List(vec![Value::Data(json!(day))]))?
                            .json()
                    })
                    .collect::<Result<Vec<Json>, Error>>()?;
                Ok(Value::Data(Json::Array(days)))
            }
            "later" => Ok(Value::future(async { Ok(json!("later").into()) })),
            _ => Err(Error::Value(method.into())),
        }
    }
    fn methods(&self) -> Option<Json> {
        Some(json!({ "now": "sync", "each": "sync", "later": "async" }))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reexported_service_calls_back_into_a_waiting_node_runtime() {
    // b and a talk in memory; c (Node) reaches a over WebSocket.
    let memory = Arc::new(MemoryTransport::default());
    memory.endpoint("a-in", id("a"));

    let a = Ctx::root().unwrap();
    (&a.plugin(MemoryPlugin::with_transport(memory.clone())))
        .await
        .unwrap();
    let websocket = WebSocketPlugin::new(Config::new().listener(ListenerConfig::new(
        "public",
        "127.0.0.1:0".parse().unwrap(),
        id("a"),
    )))
    .unwrap();
    let handle = websocket.clone();
    (&a.plugin(websocket)).await.unwrap();
    let address = format!(
        "ws://{}/rutis",
        handle.transport().unwrap().local_addr("public").unwrap()
    );
    a.plugin(IdentityPlugin::new(
        "a",
        StaticIdentity::new(id("a"))
            .accept_token("b-token", id("b"))
            .accept_token("c-token", id("c")),
    ));
    a.plugin(LinkPlugin::new(LinkConfig::listen(
        id("b"),
        "memory",
        "a",
        "a-in",
    )));
    a.plugin(LinkPlugin::new(LinkConfig::listen(
        id("c"),
        "websocket",
        "a",
        "public",
    )));
    a.plugin(ImportPlugin::new(id("b"), ["clock"]));
    a.plugin(ExportPlugin::new(id("c"), ["clock"]));

    let b = Ctx::root().unwrap();
    (&b.plugin(MemoryPlugin::with_transport(memory)))
        .await
        .unwrap();
    b.plugin(IdentityPlugin::new(
        "b",
        StaticIdentity::new(id("b")).present(id("a"), Credential::Bearer("b-token".into())),
    ));
    b.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock(Default::default())))
        .unwrap();
    b.plugin(LinkPlugin::new(LinkConfig::dial(
        id("a"),
        "memory",
        "b",
        "a-in",
    )));
    b.plugin(ExportPlugin::new(id("a"), ["clock"]));

    let node = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
    let mut child = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(node.join("test/fixtures/cordis-multihop.mjs"))
        .arg(&address)
        .current_dir(&node)
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let calls = lines.next_line().await.unwrap().expect("the calls' result");
        let later = lines
            .next_line()
            .await
            .unwrap()
            .expect("the async call's result");
        (calls, later)
    })
    .await
    .expect("the Node side finished its calls");
    let calls: Json = serde_json::from_str(&result.0).unwrap();
    assert_eq!(calls, json!({ "first": 0, "days": ["MON@1", "TUE@2"] }));
    assert_eq!(result.1, "later: later");
    let status = child.wait().await.unwrap();
    assert!(status.success());
}
