//! A service two links away: b exports `clock` to a, a imports it and
//! exports it on to c. Calls from c reach b; a callback c passes is called
//! back by b during c's synchronous call, and that callback calls the clock
//! again: a re-entrant chain c → a → b → a → c → a → b. A withdrawal at b
//! reaches c.

use std::sync::Arc;
use std::time::Duration;

use rutis::Ctx;
use rutis_bridge::{
    Credential, ExportPlugin, Identity, IdentityPlugin, ImportPlugin, LinkConfig, LinkPlugin,
    Retry, StaticIdentity,
};
use rutis_channel::PeerId;
use rutis_interop::rpc::{settle, Reply, Value};
use rutis_interop::{host_key, Error, HostDispatch};
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
        ..Retry::default()
    }
}

/// b's clock: `now` counts, `each` calls back for every day, `later` is async.
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

async fn node(transport: &Arc<MemoryTransport>, identity: StaticIdentity) -> Ctx {
    let root = Ctx::root().unwrap();
    (&root.plugin(MemoryPlugin::with_transport(transport.clone())))
        .await
        .unwrap();
    let name = identity.local().to_string();
    root.plugin(IdentityPlugin::new(&name, identity));
    root
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_reexported_across_two_links_keeps_calls_callbacks_and_withdrawals() {
    let transport = Arc::new(MemoryTransport::default());
    transport.endpoint("a-in", id("a"));

    // a: accepts b and c on one listener; imports from b, exports to c.
    let a = node(
        &transport,
        StaticIdentity::new(id("a"))
            .accept_token("b-token", id("b"))
            .accept_token("c-token", id("c")),
    )
    .await;
    a.plugin(LinkPlugin::new(
        LinkConfig::listen(id("b"), "memory", "a", "a-in").retry(quick()),
    ));
    a.plugin(LinkPlugin::new(
        LinkConfig::listen(id("c"), "memory", "a", "a-in").retry(quick()),
    ));
    a.plugin(ImportPlugin::new(id("b"), ["clock"]));
    a.plugin(ExportPlugin::new(id("c"), ["clock"]));

    // b: owns the clock.
    let b = node(
        &transport,
        StaticIdentity::new(id("b")).present(id("a"), Credential::Bearer("b-token".into())),
    )
    .await;
    let provided = b
        .provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock(Default::default())))
        .unwrap();
    b.plugin(LinkPlugin::new(
        LinkConfig::dial(id("a"), "memory", "b", "a-in").retry(quick()),
    ));
    b.plugin(ExportPlugin::new(id("a"), ["clock"]));

    // c: uses it, two links away.
    let c = node(
        &transport,
        StaticIdentity::new(id("c")).present(id("a"), Credential::Bearer("c-token".into())),
    )
    .await;
    c.plugin(LinkPlugin::new(
        LinkConfig::dial(id("a"), "memory", "c", "a-in").retry(quick()),
    ));
    c.plugin(ImportPlugin::new(id("a"), ["clock"]));

    let clock = eventually(
        || c.get_as::<dyn HostDispatch>(host_key("clock")),
        "the clock at c",
    )
    .await;
    assert_eq!(
        clock.methods(),
        Some(json!({ "now": "sync", "each": "sync", "later": "async" }))
    );

    let calls = tokio::task::spawn_blocking({
        let clock = clock.clone();
        move || -> Result<(Json, Json), Error> {
            let first = clock.invoke("now", Value::List(vec![]))?.json()?;
            // b calls this back while c waits; it calls the clock again.
            let again = clock.clone();
            let callback = Value::callback(move |args| {
                let [day]: [String; 1] = rutis_interop::decode_value(args)?;
                let now = again.invoke("now", Value::List(vec![]))?.json()?;
                Ok(json!(format!("{}@{now}", day.to_uppercase())).into())
            });
            let days = clock.invoke("each", Value::List(vec![callback]))?.json()?;
            Ok((first, days))
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(calls.0, json!(0));
    assert_eq!(calls.1, json!(["MON@1", "TUE@2"]));
    let later = settle(clock.invoke("later", Value::List(vec![])).unwrap())
        .await
        .unwrap();
    assert_eq!(later.json().unwrap(), json!("later"));
    drop(clock);

    // Withdrawn at b: gone at a, then at c.
    provided.dispose().await.unwrap();
    eventually(
        || {
            a.get_as::<dyn HostDispatch>(host_key("clock"))
                .is_none()
                .then_some(())
        },
        "the withdrawal at a",
    )
    .await;
    eventually(
        || {
            c.get_as::<dyn HostDispatch>(host_key("clock"))
                .is_none()
                .then_some(())
        },
        "the withdrawal at c",
    )
    .await;
}
