//! Links between two applications in one process, over the memory
//! transport: sessions, peers, operations and offers, reconnection,
//! takeover, failure categories and stopping.

use std::sync::Arc;
use std::time::Duration;

use rutis::{Ctx, FiberView};
use rutis_bridge::channel::PeerId;
use rutis_bridge::session::Error;
use rutis_bridge::session::{Connection, Endpoint, Format, Value};
use rutis_bridge::transport::memory::{MemoryPlugin, MemoryTransport};
use rutis_bridge::{
    peer_key, protocol, Credential, Dial, Failure, Identity, IdentityPlugin, LinkConfig,
    LinkPlugin, LinkState, Peer, Retry, StaticIdentity, Transport,
};
use serde_json::json;
use tokio::sync::watch;

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

fn quick() -> Retry {
    Retry {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
        rejected: Duration::from_millis(100),
        handshake: Duration::from_secs(2),
        ..Retry::default()
    }
}

async fn eventually<T>(mut check: impl FnMut() -> Option<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(found) = check() {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn state(
    states: &watch::Receiver<LinkState>,
    wanted: impl Fn(&LinkState) -> bool,
    what: &str,
) -> LinkState {
    eventually(
        || {
            let state = states.borrow().clone();
            wanted(&state).then_some(state)
        },
        what,
    )
    .await
}

fn peer(root: &Ctx, of: &str) -> Option<Arc<Peer>> {
    root.get_as::<Peer>(peer_key(&id(of)))
}

/// One application: the shared transport, an identity and a link.
struct App {
    root: Ctx,
    link: FiberView,
    identity: FiberView,
    states: watch::Receiver<LinkState>,
}

async fn app(transport: &Arc<MemoryTransport>, identity: StaticIdentity, link: LinkConfig) -> App {
    let root = Ctx::root().unwrap();
    let memory = root.plugin(MemoryPlugin::with_transport(transport.clone()));
    (&memory).await.unwrap();
    let name = identity.local().to_string();
    let identity = root.plugin(IdentityPlugin::new(&name, identity));
    let plugin = LinkPlugin::new(link.retry(quick()));
    let states = plugin.state();
    let link = root.plugin(plugin);
    App {
        root,
        link,
        identity,
        states,
    }
}

/// `main` listens for `mac`; `mac` dials it with its token.
async fn linked() -> (Arc<MemoryTransport>, App, App) {
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let main = app(
        &transport,
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
        LinkConfig::listen(id("mac"), "memory", "main", "main-in"),
    )
    .await;
    let mac = app(
        &transport,
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
        LinkConfig::dial(id("main"), "memory", "mac", "main-in"),
    )
    .await;
    (transport, main, mac)
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_serve_registered_families_and_announce_them() {
    let (_transport, main, mac) = linked().await;
    let at_main = eventually(|| peer(&main.root, "mac"), "Peer#mac at main").await;
    let at_mac = eventually(|| peer(&mac.root, "main"), "Peer#main at mac").await;
    assert_eq!(at_main.generation(), 1);
    assert_eq!(at_main.connection().greeting().unwrap().endpoint, id("mac"));

    let offered = at_main
        .register(
            "echo",
            Arc::new(|_: &Connection, _: &str, method: &str, args: Value| {
                Ok(Value::Data(
                    json!({ "method": method, "args": args.json()? }),
                ))
            }),
        )
        .unwrap();
    assert!(at_main
        .register(
            "echo",
            Arc::new(|_: &Connection, _: &str, _: &str, _| Ok(Value::Undefined))
        )
        .is_err());
    let reply = at_mac
        .connection()
        .invoke_async("", "echo.ping", Value::Data(json!([1])))
        .await
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(reply, json!({ "method": "echo.ping", "args": [1] }));

    // Unregistered: an error, and the session goes on.
    let refused = at_mac
        .connection()
        .invoke_async("", "plugins.load", Value::Undefined)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("not offered"), "{refused}");

    let mut offers = at_mac.offers();
    eventually(
        || {
            offers
                .borrow_and_update()
                .families
                .contains("echo")
                .then_some(())
        },
        "echo offered",
    )
    .await;
    drop(offered);
    eventually(
        || offers.borrow_and_update().families.is_empty().then_some(()),
        "echo withdrawn",
    )
    .await;
    assert!(at_mac
        .connection()
        .invoke_async("", "echo.ping", Value::Undefined)
        .await
        .is_err());
}

/// A family withdrawn and registered again before the far end looks is a
/// new offer there: its epoch changed though the families did not.
#[tokio::test(flavor = "multi_thread")]
async fn a_family_registered_anew_is_a_new_offer() {
    let (_transport, main, mac) = linked().await;
    let at_main = eventually(|| peer(&main.root, "mac"), "main's peer").await;
    let at_mac = eventually(|| peer(&mac.root, "main"), "mac's peer").await;
    let handler = || Arc::new(|_: &Connection, _: &str, _: &str, _: Value| Ok(Value::Undefined));

    let offered = at_main.register("plugins", handler()).unwrap();
    let offers = at_mac.offers();
    let first = eventually(|| offers.borrow().epoch("plugins"), "plugins offered").await;
    // Withdrawn and registered again at once: one look may see neither change.
    drop(offered);
    let _again = at_main.register("plugins", handler()).unwrap();
    let second = eventually(
        || {
            offers
                .borrow()
                .epoch("plugins")
                .filter(|epoch| *epoch != first)
        },
        "the new offer",
    )
    .await;
    assert!(second > first);
    assert_eq!(
        offers.borrow().families,
        ["plugins".to_owned()].into_iter().collect()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dialing_link_reconnects_and_peers_get_a_new_generation() {
    let (_transport, main, mac) = linked().await;
    let first = eventually(|| peer(&mac.root, "main"), "the first session").await;
    first.connection().close(Error::Transport("cut".into()));
    drop(first);
    let second = eventually(
        || peer(&mac.root, "main").filter(|peer| peer.generation() == 2),
        "the second session at mac",
    )
    .await;
    eventually(
        || peer(&main.root, "mac").filter(|peer| peer.generation() == 2),
        "the second session at main",
    )
    .await;
    assert!(second.connection().greeting().is_some());
    state(
        &mac.states,
        |s| matches!(s, LinkState::Ready { generation: 2 }),
        "ready again",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_connection_takes_over_and_the_old_one_is_told() {
    let (transport, main, mac) = linked().await;
    let old = eventually(|| peer(&main.root, "mac"), "the first session").await;
    let old_at_mac = eventually(|| peer(&mac.root, "main"), "mac's session").await;
    // Another connection authenticating as mac, completing its handshake.
    let identity: Arc<dyn Identity> = Arc::new(
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
    );
    let channel = transport
        .dial(
            &Dial::address("main-in")
                .peer(id("main"))
                .identity(identity)
                .protocol(protocol()),
        )
        .await
        .unwrap();
    struct Nothing;
    impl rutis_bridge::session::Dispatch for Nothing {
        fn invoke(
            &self,
            _: &Connection,
            _: &str,
            _: &str,
            _: Value,
        ) -> rutis_bridge::session::Reply {
            Ok(Value::Undefined)
        }
    }
    let newer = Connection::open_with(
        channel,
        Arc::new(Nothing),
        Format::Endpoint(Endpoint::rust(id("mac")).expect(id("main"))),
    )
    .unwrap();
    newer.ready().await.unwrap();
    // The old session ended, told it was replaced.
    old_at_mac.connection().closed().await;
    old.connection().closed().await;
    let current = eventually(
        || peer(&main.root, "mac").filter(|peer| peer.generation() > old.generation()),
        "the newer session",
    )
    .await;
    assert!(!current.connection().greeting().is_none());
    drop(newer);
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_are_retried_by_category() {
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let _main = app(
        &transport,
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
        LinkConfig::listen(id("mac"), "memory", "main", "main-in"),
    )
    .await;

    // app() installs plugins but does not wait for the listener to register.
    // Before registration a dial is Retryable, regardless of its credentials.
    state(
        &_main.states,
        |s| matches!(s, LinkState::Connecting),
        "the listener registration",
    )
    .await;

    // A wrong token: slow retries, reported as an authentication failure.
    let guessing = app(
        &transport,
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("guess".into())),
        LinkConfig::dial(id("main"), "memory", "mac", "main-in"),
    )
    .await;
    let waiting = state(
        &guessing.states,
        |s| matches!(s, LinkState::Waiting { .. }),
        "a refusal",
    )
    .await;
    assert!(matches!(
        waiting,
        LinkState::Waiting { failure: Failure::AuthRejected, retry_in, .. } if retry_in == quick().rejected
    ));

    // An endpoint speaking another protocol: no more attempts.
    let foreign = transport.clone();
    foreign.endpoint("old-in", id("old"));
    let _old = foreign
        .register(rutis_bridge::Registration {
            listener: "old-in".into(),
            peer: id("mac"),
            identity: Arc::new(StaticIdentity::new(id("old")).accept_token("mac-token", id("mac"))),
            protocol: "rutis.2".into(),
            deliver: Box::new(|_| {}),
        })
        .unwrap();
    let dialing_old = app(
        &transport,
        StaticIdentity::new(id("mac")).present(id("old"), Credential::Bearer("mac-token".into())),
        LinkConfig::dial(id("old"), "memory", "mac", "old-in"),
    )
    .await;
    state(
        &dialing_old.states,
        |s| matches!(s, LinkState::Stopped { .. }),
        "a stop",
    )
    .await;

    // Nothing listening yet: retryable, with backoff.
    let early = app(
        &transport,
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
        LinkConfig::dial(id("main"), "memory", "mac", "nowhere"),
    )
    .await;
    state(
        &early.states,
        |s| {
            matches!(
                s,
                LinkState::Waiting {
                    failure: Failure::Retryable,
                    ..
                }
            )
        },
        "a retryable failure",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_far_end_greeting_as_someone_else_is_an_identity_failure() {
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let main = app(
        &transport,
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
        LinkConfig::listen(id("mac"), "memory", "main", "main-in"),
    )
    .await;
    // Holds mac's token, but greets as pi.
    let _impostor = app(
        &transport,
        StaticIdentity::new(id("pi")).present(id("main"), Credential::Bearer("mac-token".into())),
        LinkConfig::dial(id("main"), "memory", "pi", "main-in"),
    )
    .await;
    state(
        &main.states,
        |s| {
            matches!(
                s,
                LinkState::Waiting {
                    failure: Failure::AuthRejected,
                    ..
                }
            )
        },
        "the mismatch",
    )
    .await;
    assert!(peer(&main.root, "mac").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_link_withdraws_its_peer_and_revokes_its_registration() {
    let (transport, main, mac) = linked().await;
    eventually(|| peer(&main.root, "mac"), "the session").await;
    let at_mac = eventually(|| peer(&mac.root, "main"), "mac's session").await;

    main.link.dispose().await.unwrap();
    assert!(peer(&main.root, "mac").is_none());
    at_mac.connection().closed().await;
    drop(at_mac);
    // With main's registration revoked, mac is refused until main relinks:
    // its credentials are good, so it retries as for a listener not up yet.
    state(
        &mac.states,
        |s| {
            matches!(
                s,
                LinkState::Waiting {
                    failure: Failure::Retryable,
                    ..
                }
            )
        },
        "the refusal",
    )
    .await;
    let identity: Arc<dyn Identity> = Arc::new(
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
    );
    assert!(matches!(
        transport
            .dial(
                &Dial::address("main-in")
                    .peer(id("main"))
                    .identity(identity)
                    .protocol(protocol())
            )
            .await,
        Err(rutis_bridge::channel::ConnectError::Retryable { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_stops_with_its_identity() {
    let (_transport, main, mac) = linked().await;
    eventually(|| peer(&mac.root, "main"), "the session").await;
    mac.identity.dispose().await.unwrap();
    eventually(
        || peer(&mac.root, "main").is_none().then_some(()),
        "the peer withdrawn",
    )
    .await;
    eventually(
        || peer(&main.root, "mac").is_none().then_some(()),
        "main's peer withdrawn",
    )
    .await;
    assert_eq!(*mac.states.borrow(), LinkState::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_far_end_without_the_required_contract_stops_the_link() {
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let main = app(
        &transport,
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
        LinkConfig::listen(id("mac"), "memory", "main", "main-in"),
    )
    .await;
    // mac requires a runtime; main is a node.
    let mac = app(
        &transport,
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
        LinkConfig::dial(id("main"), "memory", "mac", "main-in").require("runtime"),
    )
    .await;
    let stopped = state(
        &mac.states,
        |s| matches!(s, LinkState::Stopped { .. }),
        "the stop",
    )
    .await;
    assert!(matches!(stopped, LinkState::Stopped { error } if error.contains("runtime")));
    assert!(peer(&mac.root, "main").is_none());

    // Requiring what it declares works.
    let fine = app(
        &transport,
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
        LinkConfig::dial(id("main"), "memory", "mac", "main-in").require("node"),
    )
    .await;
    eventually(|| peer(&fine.root, "main"), "the node peer").await;
    drop(main);
}
