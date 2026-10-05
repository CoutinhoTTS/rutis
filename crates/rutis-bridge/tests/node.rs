//! Node feature plugins between two applications in one process: services
//! exported and imported, plugins hosted for a peer, events forwarded.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberView, Listener, Plugin, PluginFactory};
use rutis_bridge::{
    node_event, peer_key, Credential, Described, EventsPlugin, ExportPlugin, HostPlugin,
    IdentityPlugin, ImportPlugin, LinkConfig, LinkPlugin, NodeEvent, Peer, Retry, StaticCatalog,
    StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{settle, Reply, Value};
use rutis_interop::{host_key, HostDispatch};
use rutis_transport_memory::{MemoryPlugin, MemoryTransport};
use serde_json::{json, Value as Json};

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
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

fn quick() -> Retry {
    Retry {
        initial: Duration::from_millis(20),
        max: Duration::from_millis(200),
        rejected: Duration::from_millis(100),
        ..Retry::default()
    }
}

/// `main` listens for `mac`, `mac` dials it; both get their peer.
async fn linked() -> (Ctx, Ctx, FiberView) {
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let main = Ctx::root().unwrap();
    (&main.plugin(MemoryPlugin::with_transport(transport.clone())))
        .await
        .unwrap();
    main.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
    ));
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("mac"), "memory", "main", "main-in").retry(quick()),
    ));
    let mac = Ctx::root().unwrap();
    (&mac.plugin(MemoryPlugin::with_transport(transport)))
        .await
        .unwrap();
    mac.plugin(IdentityPlugin::new(
        "mac",
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
    ));
    let mac_link = mac.plugin(LinkPlugin::new(
        LinkConfig::dial(id("main"), "memory", "mac", "main-in").retry(quick()),
    ));
    eventually(|| main.get_as::<Peer>(peer_key(&id("mac"))), "main's peer").await;
    eventually(|| mac.get_as::<Peer>(peer_key(&id("main"))), "mac's peer").await;
    (main, mac, mac_link)
}

/// A clock whose readings start at `base`.
struct Clock(AtomicU64);
impl HostDispatch for Clock {
    fn invoke(&self, method: &str, _args: Value) -> Reply {
        let now = self.0.fetch_add(1, Ordering::SeqCst);
        match method {
            "now" => Ok(json!(now).into()),
            "later" => Ok(Value::future(async move { Ok(json!(now + 1000).into()) })),
            _ => Err(rutis_interop::Error::Value(method.into())),
        }
    }
    fn methods(&self) -> Option<Json> {
        Some(json!({ "now": "sync", "later": "async" }))
    }
}

fn clock(base: u64) -> Arc<dyn HostDispatch> {
    Arc::new(Clock(AtomicU64::new(base)))
}

#[tokio::test(flavor = "multi_thread")]
async fn exported_services_are_imported_replaced_and_withdrawn() {
    let (main, mac, _) = linked().await;
    let provided = main
        .provide_as::<dyn HostDispatch>(host_key("clock"), clock(0))
        .unwrap();
    main.plugin(ExportPlugin::new(id("mac"), ["clock"]));
    mac.plugin(ImportPlugin::new(id("main"), ["clock"]));

    let imported = eventually(
        || mac.get_as::<dyn HostDispatch>(host_key("clock")),
        "the import",
    )
    .await;
    assert_eq!(
        imported.methods(),
        Some(json!({ "now": "sync", "later": "async" }))
    );
    let now = tokio::task::spawn_blocking({
        let imported = imported.clone();
        move || imported.invoke("now", Value::List(vec![]))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(now.json().unwrap(), json!(0));
    let later = settle(imported.invoke("later", Value::List(vec![])).unwrap())
        .await
        .unwrap();
    assert_eq!(later.json().unwrap(), json!(1001));
    drop(imported);

    // Replaced over there: replaced here.
    provided.dispose().await.unwrap();
    let provided = main
        .provide_as::<dyn HostDispatch>(host_key("clock"), clock(500))
        .unwrap();
    let replaced = eventually(
        || {
            let service = mac.get_as::<dyn HostDispatch>(host_key("clock"))?;
            let now = settle(service.invoke("later", Value::List(vec![])).ok()?);
            let now = futures_now(now)?;
            (now >= 1500).then_some(now)
        },
        "the replacement",
    )
    .await;
    assert!(replaced >= 1500);

    // Withdrawn over there: withdrawn here.
    provided.dispose().await.unwrap();
    eventually(
        || {
            mac.get_as::<dyn HostDispatch>(host_key("clock"))
                .is_none()
                .then_some(())
        },
        "the withdrawal",
    )
    .await;
}

/// Poll a settle future once on a fresh runtime thread (tests only).
fn futures_now(future: impl std::future::Future<Output = Reply> + Send + 'static) -> Option<u64> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
            .ok()?
            .json()
            .ok()?
            .as_u64()
    })
    .join()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_never_replaces_a_service_provided_here() {
    let (main, mac, _) = linked().await;
    main.provide_as::<dyn HostDispatch>(host_key("clock"), clock(0))
        .unwrap();
    mac.provide_as::<dyn HostDispatch>(host_key("clock"), clock(7))
        .unwrap();
    main.plugin(ExportPlugin::new(id("mac"), ["clock"]));
    mac.plugin(ImportPlugin::new(id("main"), ["clock"]));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let local = mac.get_as::<dyn HostDispatch>(host_key("clock")).unwrap();
    let now = tokio::task::spawn_blocking(move || local.invoke("now", Value::List(vec![])))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(now.json().unwrap(), json!(7), "the local clock stays");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_export_waits_for_the_far_end_to_offer_services() {
    let (main, mac, _) = linked().await;
    main.provide_as::<dyn HostDispatch>(host_key("clock"), clock(0))
        .unwrap();
    main.plugin(ExportPlugin::new(id("mac"), ["clock"]));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(mac.get_as::<dyn HostDispatch>(host_key("clock")).is_none());
    // Importing later still gets it: the exporter announces on the offer.
    mac.plugin(ImportPlugin::new(id("main"), ["clock"]));
    eventually(
        || mac.get_as::<dyn HostDispatch>(host_key("clock")),
        "the late import",
    )
    .await;
}

/// A Rust plugin installed on mac, providing `greeting` with its config.
struct Greeter;
impl PluginFactory<Json> for Greeter {
    fn build(&self, config: &Json) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Greeting(
            config["text"].as_str().unwrap_or("hello").to_owned(),
        )))
    }
}
struct Greeting(String);
impl Plugin for Greeting {
    fn name(&self) -> &str {
        "greeter"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(GreetingService(self.0.clone()))?;
            Ok(Effect::Done)
        })
    }
}
struct GreetingService(String);

#[tokio::test(flavor = "multi_thread")]
async fn a_host_loads_installed_plugins_for_its_peer() {
    let (main, mac, mac_link) = linked().await;
    let catalog = StaticCatalog::new().with(
        "greeter",
        Described {
            schema: Some(json!({ "type": "object" })),
            version: Some("1.0.0".into()),
            integrity: None,
        },
        Greeter,
    );
    mac.plugin(HostPlugin::new(id("main"), Arc::new(catalog)));
    let at_main = main.get_as::<Peer>(peer_key(&id("mac"))).unwrap();
    let mut offers = at_main.offers();
    eventually(
        || {
            offers
                .borrow_and_update()
                .families
                .contains("plugins")
                .then_some(())
        },
        "the host offered",
    )
    .await;
    let session = at_main.connection();

    let described = settle(
        session
            .invoke_async("", "plugins.describe", json!(["greeter"]).into())
            .await
            .unwrap(),
    )
    .await
    .unwrap()
    .json()
    .unwrap();
    assert_eq!(described["version"], "1.0.0");
    let missing = settle(
        session
            .invoke_async("", "plugins.describe", json!(["unknown"]).into())
            .await
            .unwrap(),
    )
    .await
    .unwrap_err();
    assert!(matches!(missing, rutis_interop::Error::Remote { ref name, .. } if name == "NotFound"));
    assert!(session
        .invoke_async(
            "",
            "plugins.load",
            json!(["k", "/etc/plugin.so", {}]).into()
        )
        .await
        .is_err());

    settle(
        session
            .invoke_async(
                "",
                "plugins.load",
                json!(["g1", "greeter", { "text": "hi" }]).into(),
            )
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(mac.get::<GreetingService>().unwrap().0, "hi");
    settle(
        session
            .invoke_async(
                "",
                "plugins.update",
                json!(["g1", { "text": "hey" }]).into(),
            )
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    eventually(
        || (mac.get::<GreetingService>()?.0 == "hey").then_some(()),
        "the update",
    )
    .await;
    settle(
        session
            .invoke_async("", "plugins.unload", json!(["g1"]).into())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(mac.get::<GreetingService>().is_none());

    // What a host loaded goes with the link.
    settle(
        session
            .invoke_async("", "plugins.load", json!(["g2", "greeter", {}]).into())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(mac.get::<GreetingService>().is_some());
    mac_link.dispose().await.unwrap();
    eventually(
        || mac.get::<GreetingService>().is_none().then_some(()),
        "the hosted plugin unloaded",
    )
    .await;
}

struct Record(Arc<Mutex<Vec<Json>>>);
impl Listener<NodeEvent> for Record {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a NodeEvent,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            self.0.lock().unwrap().push(event.args.clone());
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn events_go_one_way_and_the_sender_waits_for_the_listeners() {
    let (main, mac, _) = linked().await;
    let heard = Arc::new(Mutex::new(Vec::new()));
    mac.events()
        .on(&mac, &node_event("tick"), Record(heard.clone()))
        .unwrap();
    (&mac.plugin(EventsPlugin::new(id("main"), Vec::<String>::new(), ["tick"]).unwrap()))
        .await
        .unwrap();
    (&main.plugin(EventsPlugin::new(id("mac"), ["tick"], Vec::<String>::new()).unwrap()))
        .await
        .unwrap();
    let at_main = main.get_as::<Peer>(peer_key(&id("mac"))).unwrap();
    let mut offers = at_main.offers();
    eventually(
        || {
            offers
                .borrow_and_update()
                .families
                .contains("events")
                .then_some(())
        },
        "events offered",
    )
    .await;

    main.events()
        .parallel(
            &main,
            &node_event("tick"),
            Arc::new(NodeEvent {
                args: json!([1, "a"]),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        *heard.lock().unwrap(),
        vec![json!([1, "a"])],
        "parallel waited for mac"
    );

    assert!(EventsPlugin::new(id("mac"), ["tick"], ["tick"]).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_composition_changes_one_feature_and_keeps_its_session() {
    use rutis_bridge::{Features, PeerPlugin};
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("main-in", id("main"));
    let main = Ctx::root().unwrap();
    (&main.plugin(MemoryPlugin::with_transport(transport.clone())))
        .await
        .unwrap();
    main.plugin(IdentityPlugin::new(
        "main",
        StaticIdentity::new(id("main")).accept_token("mac-token", id("mac")),
    ));
    main.provide_as::<dyn HostDispatch>(host_key("clock"), clock(0))
        .unwrap();
    main.provide_as::<dyn HostDispatch>(host_key("calendar"), clock(100))
        .unwrap();
    let composed = PeerPlugin::new(
        LinkConfig::listen(id("mac"), "memory", "main", "main-in").retry(quick()),
        Features {
            export: vec!["clock".into()],
            ..Features::default()
        },
    )
    .unwrap();
    let handle = composed.handle();
    main.plugin(composed);

    let mac = Ctx::root().unwrap();
    (&mac.plugin(MemoryPlugin::with_transport(transport)))
        .await
        .unwrap();
    mac.plugin(IdentityPlugin::new(
        "mac",
        StaticIdentity::new(id("mac")).present(id("main"), Credential::Bearer("mac-token".into())),
    ));
    mac.plugin(
        PeerPlugin::new(
            LinkConfig::dial(id("main"), "memory", "mac", "main-in").retry(quick()),
            Features {
                import: vec!["clock".into(), "calendar".into()],
                ..Features::default()
            },
        )
        .unwrap(),
    );
    eventually(
        || mac.get_as::<dyn HostDispatch>(host_key("clock")),
        "clock imported",
    )
    .await;
    let session = main.get_as::<Peer>(peer_key(&id("mac"))).unwrap();
    assert!(mac
        .get_as::<dyn HostDispatch>(host_key("calendar"))
        .is_none());

    // Export calendar as well: it arrives, and the session is the same.
    handle
        .set(Features {
            export: vec!["clock".into(), "calendar".into()],
            ..Features::default()
        })
        .await
        .unwrap();
    eventually(
        || mac.get_as::<dyn HostDispatch>(host_key("calendar")),
        "calendar imported",
    )
    .await;
    let still = main.get_as::<Peer>(peer_key(&id("mac"))).unwrap();
    assert_eq!(still.generation(), session.generation());
    assert!(Arc::ptr_eq(&still, &session), "the link kept its session");
    assert!(handle
        .set(Features {
            outbound: vec!["x".into()],
            inbound: vec!["x".into()],
            ..Features::default()
        })
        .await
        .is_err());
}
