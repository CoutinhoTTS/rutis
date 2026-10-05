//! Two Rust endpoints linked over a loopback WebSocket: one listens, one
//! dials; both get the other's peer and call its operations.

use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::{
    peer_key, Credential, IdentityPlugin, LinkConfig, LinkPlugin, Peer, StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{Connection, Value};
use rutis_transport_websocket::{Config, ListenerConfig, WebSocketPlugin};
use serde_json::json;

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

async fn eventually<T>(mut check: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(found) = check() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out")
}

#[tokio::test(flavor = "multi_thread")]
async fn two_rust_endpoints_link_over_websocket() {
    // main: listens for mac.
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
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
    ));
    main.plugin(LinkPlugin::new(LinkConfig::listen(
        id("mac"),
        "websocket",
        "main",
        "public",
    )));

    // mac: dials main.
    let mac = Ctx::root().unwrap();
    (&mac.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
        .await
        .unwrap();
    mac.plugin(IdentityPlugin::new(
        "mac",
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
    ));
    mac.plugin(LinkPlugin::new(LinkConfig::dial(
        id("main"),
        "websocket",
        "mac",
        &address,
    )));

    let at_main = eventually(|| main.get_as::<Peer>(peer_key(&id("mac")))).await;
    let at_mac = eventually(|| mac.get_as::<Peer>(peer_key(&id("main")))).await;
    let _offered = at_mac
        .register(
            "clock",
            Arc::new(|_: &Connection, _: &str, _: &str, _: Value| Ok(Value::Data(json!(42)))),
        )
        .unwrap();
    let now = at_main
        .connection()
        .invoke_async("", "clock.now", Value::Undefined)
        .await
        .unwrap();
    assert_eq!(now.json().unwrap(), json!(42));
    assert_eq!(at_main.connection().greeting().unwrap().endpoint, id("mac"));
}
