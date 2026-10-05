//! The node conformance suite (feature `conformance`): what a full framework
//! node must do on a link, as checks run from a rutis node against it.
//!
//! The far node, linked to `main` as `peer`, must:
//!
//! - export `calendar`: `today()` → `"monday"` (sync), `later()` →
//!   `"tuesday"` (async);
//! - import `probe` (`record(line)`, sync), which `main` exports;
//! - host the installed plugin `conformance-greeter`, which records
//!   `greeter: <text>` through `probe` when it starts and
//!   `greeter gone: <text>` when it stops (`text` from its config);
//! - take the event `tick` from `main` and, while handling it, forward
//!   `tock` with `["pong", <number of tick arguments>]` back.
//!
//! [`fixture`] is a rutis node doing so; `interop/node/test/fixtures/
//! cordis-node.mjs` is a Cordis one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Listener, Plugin, PluginFactory};
use rutis_channel::PeerId;
use rutis_interop::rpc::{settle, Reply, Value};
use rutis_interop::{host_key, Error, HostDispatch};
use serde_json::{json, Value as Json};

use crate::{
    node_event, peer_key, Described, EventsPlugin, ExportPlugin, ImportPlugin, NodeEvent, Peer,
    StaticCatalog,
};

/// What `main` hears through `probe`.
#[derive(Clone, Default)]
struct Probe(Arc<Mutex<Vec<String>>>);
impl HostDispatch for Probe {
    fn invoke(&self, _method: &str, args: Value) -> Reply {
        let [line]: [String; 1] = rutis_interop::decode_value(args)?;
        self.0.lock().unwrap().push(line);
        Ok(Value::Undefined)
    }
    fn methods(&self) -> Option<Json> {
        Some(json!({ "record": "sync" }))
    }
}

struct Tocks(Arc<Mutex<Vec<Json>>>);
impl Listener<NodeEvent> for Tocks {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a NodeEvent,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(event.args.clone());
            Ok(None)
        })
    }
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

async fn call(peer: &Peer, method: &str, args: Json) -> Result<Json, Error> {
    let reply = peer
        .connection()
        .invoke_async("", method, args.into())
        .await?;
    settle(reply).await?.json()
}

/// Run the node checks against `peer`, a far node `main` (this
/// application, `root`) has a link to. Mounts what `main` needs for them.
pub async fn node(root: &Ctx, peer: PeerId) {
    let probe = Probe::default();
    root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe.clone()))
        .expect("probe provided");
    root.plugin(ImportPlugin::new(peer.clone(), ["calendar"]));
    root.plugin(ExportPlugin::new(peer.clone(), ["probe"]));
    root.plugin(EventsPlugin::new(peer.clone(), ["tick"], ["tock"]).expect("events"));
    let tocks = Arc::new(Mutex::new(Vec::new()));
    root.events()
        .on(root, &node_event("tock"), Tocks(tocks.clone()))
        .expect("tock listener");
    let heard = |line: String| {
        let probe = probe.clone();
        move || probe.0.lock().unwrap().contains(&line).then_some(())
    };

    // Its service, here: sync and async.
    let calendar = eventually(
        || root.get_as::<dyn HostDispatch>(host_key("calendar")),
        "calendar imported",
    )
    .await;
    let sync = calendar.clone();
    let today = tokio::task::spawn_blocking(move || sync.invoke("today", Value::List(vec![])))
        .await
        .unwrap()
        .expect("calendar.today");
    assert_eq!(
        today.json().unwrap(),
        json!("monday"),
        "a sync call into the node's service"
    );
    let later = settle(calendar.invoke("later", Value::List(vec![])).unwrap())
        .await
        .expect("calendar.later");
    assert_eq!(
        later.json().unwrap(),
        json!("tuesday"),
        "an async call into the node's service"
    );
    drop(calendar);

    // Its host: describe, load (which uses our probe there), update, unload.
    let at = eventually(|| root.get_as::<Peer>(peer_key(&peer)), "the peer").await;
    let mut offers = at.offers();
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
    let described = call(&at, "plugins.describe", json!(["conformance-greeter"]))
        .await
        .expect("plugins.describe");
    assert!(
        described.get("schema").is_some(),
        "describe answers with a schema field"
    );
    match call(&at, "plugins.describe", json!(["not-installed-anywhere"])).await {
        Err(Error::Remote { name, .. }) => {
            assert_eq!(name, "NotFound", "a missing plugin is NotFound")
        }
        other => panic!("describing a missing plugin must fail with NotFound, got {other:?}"),
    }
    assert!(
        call(&at, "plugins.load", json!(["f", "/etc/plugin", {}]))
            .await
            .is_err(),
        "a host refuses files"
    );
    call(
        &at,
        "plugins.load",
        json!(["g", "conformance-greeter", { "text": "one" }]),
    )
    .await
    .expect("plugins.load");
    eventually(heard("greeter: one".into()), "the hosted plugin started").await;
    call(&at, "plugins.update", json!(["g", { "text": "two" }]))
        .await
        .expect("plugins.update");
    eventually(heard("greeter: two".into()), "the hosted plugin restarted").await;
    call(&at, "plugins.unload", json!(["g"]))
        .await
        .expect("plugins.unload");
    eventually(
        heard("greeter gone: two".into()),
        "the hosted plugin stopped",
    )
    .await;

    // Events: tick there; tock back before tick's parallel ends.
    root.events()
        .parallel(
            root,
            &node_event("tick"),
            Arc::new(NodeEvent {
                args: json!(["ping"]),
            }),
        )
        .await
        .expect("tick forwarded");
    assert_eq!(
        *tocks.lock().unwrap(),
        vec![json!(["pong", 1])],
        "tock came back while tick was handled"
    );
}

/// `conformance-greeter`, for a rutis node.
struct Greeter([rutis::TypeKey; 1]);
impl PluginFactory<Json> for Greeter {
    // A factory's fiber is gated on what the factory declares.
    fn injects(&self) -> &[rutis::TypeKey] {
        &self.0
    }

    fn build(&self, config: &Json) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(Greeting(
            config["text"].as_str().unwrap_or("").to_owned(),
        )))
    }
}
struct Greeting(String);
impl Plugin for Greeting {
    fn name(&self) -> &str {
        "conformance-greeter"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let probe = ctx.require_as::<dyn HostDispatch>(host_key("probe"))?;
            record(probe.clone(), format!("greeter: {}", self.0)).await;
            let gone = format!("greeter gone: {}", self.0);
            Ok(Effect::AsyncDisposer(Box::new(move || {
                Box::pin(async move {
                    record(probe, gone).await;
                    Ok(())
                })
            })))
        })
    }
}

/// Record `line` through the (synchronous) probe, off the async threads.
async fn record(probe: Arc<dyn HostDispatch>, line: String) {
    let _ = tokio::task::spawn_blocking(move || {
        probe.invoke("record", Value::List(vec![Value::Data(json!(line))]))
    })
    .await;
}

/// On `tick`, forward `tock` back.
struct TickTock(Ctx);
impl Listener<NodeEvent> for TickTock {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a NodeEvent,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            let count = event.args.as_array().map_or(0, Vec::len);
            self.0
                .events()
                .parallel(
                    &self.0,
                    &node_event("tock"),
                    Arc::new(NodeEvent {
                        args: json!(["pong", count]),
                    }),
                )
                .await?;
            Ok(None)
        })
    }
}

struct Calendar;
impl HostDispatch for Calendar {
    fn invoke(&self, method: &str, _args: Value) -> Reply {
        match method {
            "today" => Ok(json!("monday").into()),
            "later" => Ok(Value::future(async { Ok(json!("tuesday").into()) })),
            _ => Err(Error::Value(method.into())),
        }
    }
    fn methods(&self) -> Option<Json> {
        Some(json!({ "today": "sync", "later": "async" }))
    }
}

/// Make `root`, linked to `main`, the conformance node: it provides and
/// exports `calendar`, imports `probe`, hosts `conformance-greeter` and
/// answers `tick` with `tock`.
pub fn fixture(root: &Ctx, main: PeerId) {
    root.provide_as::<dyn HostDispatch>(host_key("calendar"), Arc::new(Calendar))
        .expect("calendar provided");
    root.plugin(ExportPlugin::new(main.clone(), ["calendar"]));
    root.plugin(ImportPlugin::new(main.clone(), ["probe"]));
    let catalog = StaticCatalog::new().with(
        "conformance-greeter",
        Described {
            schema: Some(
                json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
            ),
            version: Some("1.0.0".into()),
            integrity: None,
        },
        Greeter([host_key("probe")]),
    );
    root.plugin(crate::HostPlugin::new(main.clone(), Arc::new(catalog)));
    root.plugin(EventsPlugin::new(main, ["tock"], ["tick"]).expect("events"));
    root.events()
        .on(root, &node_event("tick"), TickTock(root.clone()))
        .expect("tick listener");
}
