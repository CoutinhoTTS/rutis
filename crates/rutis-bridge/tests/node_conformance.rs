//! The node conformance suite against a rutis node, in process.
#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::channel::PeerId;
use rutis_bridge::transport::memory::{MemoryPlugin, MemoryTransport};
use rutis_bridge::{Credential, IdentityPlugin, LinkConfig, LinkPlugin, Retry, StaticIdentity};

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rutis_node_meets_the_node_contract() {
    let quick = Retry {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
        ..Retry::default()
    };
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let main = Ctx::root().unwrap();
    (&main.plugin(MemoryPlugin::with_transport(transport.clone())))
        .await
        .unwrap();
    main.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).accept_token("node-token", id("node")),
    ));
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("node"), "memory", "main", "main-in")
            .require("node")
            .retry(quick.clone()),
    ));

    let node = Ctx::root().unwrap();
    (&node.plugin(MemoryPlugin::with_transport(transport)))
        .await
        .unwrap();
    node.plugin(IdentityPlugin::new(
        "node",
        StaticIdentity::new(id("node"))
            .present(id("main"), Credential::Bearer("node-token".into())),
    ));
    node.plugin(LinkPlugin::new(
        LinkConfig::dial(id("main"), "memory", "node", "main-in").retry(quick),
    ));
    rutis_bridge::testing::fixture(&node, id("main"));

    rutis_bridge::testing::node(&main, id("node")).await;
}
