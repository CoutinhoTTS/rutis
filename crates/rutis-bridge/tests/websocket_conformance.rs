//! The node conformance suite against a Cordis node
//! (node/rutis-runtime/test/fixtures/cordis-conformance.mjs) over a loopback
//! WebSocket.
#![cfg(all(feature = "websocket", feature = "testing"))]

use std::path::Path;

use rutis::Ctx;
use rutis_bridge::channel::PeerId;
use rutis_bridge::transport::websocket::{Config, ListenerConfig, WebSocketPlugin};
use rutis_bridge::{IdentityPlugin, LinkConfig, LinkPlugin, StaticIdentity};

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cordis_node_meets_the_node_contract() {
    let node = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../node/rutis-runtime");
    // conformance-greeter, installed where the Cordis node resolves plugins.
    let anchor = tempfile::tempdir().unwrap();
    std::fs::write(anchor.path().join("package.json"), "{}").unwrap();
    let installed = anchor.path().join("node_modules/conformance-greeter");
    std::fs::create_dir_all(&installed).unwrap();
    for file in ["package.json", "index.mjs"] {
        std::fs::copy(
            node.join("test/fixtures/conformance-greeter").join(file),
            installed.join(file),
        )
        .unwrap();
    }

    let main = Ctx::root().unwrap();
    let websocket = WebSocketPlugin::new(Config::new().listener(ListenerConfig::new(
        "public",
        "127.0.0.1:0".parse().unwrap(),
        id("main"),
    )))
    .unwrap();
    let handle = websocket.clone();
    (&main.plugin(websocket)).await.unwrap();
    let address = format!(
        "ws://{}/rutis",
        handle.transport().unwrap().local_addr("public").unwrap()
    );
    main.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).accept_token("cordis-token", id("cordis")),
    ));
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("cordis"), "websocket", "main", "public").require("node"),
    ));

    let mut child = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(node.join("test/fixtures/cordis-conformance.mjs"))
        .arg(&address)
        .arg(anchor.path().join("package.json"))
        .current_dir(&node)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    rutis_bridge::testing::node(&main, id("cordis")).await;
    child.start_kill().unwrap();
}
