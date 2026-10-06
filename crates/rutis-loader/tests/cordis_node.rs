//! A rutis node and a Cordis application linked as full nodes over a
//! loopback WebSocket (node/rutis-runtime/test/fixtures/cordis-node.mjs): each
//! imports the other's service, rutis rows are hosted in Cordis, events
//! cross both ways. The Cordis side reports on stdout.
#![cfg(all(unix, feature = "peer"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Listener};
use rutis_bridge::channel::PeerId;
use rutis_bridge::session::{host_key, HostDispatch};
use rutis_bridge::session::{settle, Reply, Value as RpcValue};
use rutis_bridge::transport::websocket::{Config, ListenerConfig, WebSocketPlugin};
use rutis_bridge::{
    node_event, EventsPlugin, ExportPlugin, IdentityPlugin, ImportPlugin, LinkConfig, LinkPlugin,
    NodeEvent, StaticIdentity,
};
use rutis_loader::{
    Chain, Layer, LoaderOptions, LoaderPlugin, Patch, PeerResolver, PeerRowsPlugin,
};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

fn id(s: &str) -> PeerId {
    PeerId::new(s).unwrap()
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
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

struct Clock;
impl HostDispatch for Clock {
    fn invoke(&self, _: &str, _: RpcValue) -> Reply {
        Ok(json!(42).into())
    }
    fn methods(&self) -> Option<Value> {
        Some(json!({ "now": "sync" }))
    }
}

struct Record(Arc<Mutex<Vec<Value>>>);
impl Listener<NodeEvent> for Record {
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

#[tokio::test(flavor = "multi_thread")]
async fn a_rutis_node_and_a_cordis_node_share_services_rows_and_events() {
    // An npm plugin installed on the Cordis side only.
    let project = tempfile::tempdir().unwrap();
    let anchor = project.path().join("package.json");
    std::fs::write(&anchor, "{}").unwrap();
    let package = project.path().join("node_modules/greeter-js");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.json"),
        json!({ "name": "greeter-js", "version": "2.1.0", "type": "module", "main": "index.mjs" })
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        package.join("index.mjs"),
        "export function apply(ctx, config) {\n  process.stdout.write(`greeter: ${config.text}\\n`)\n  ctx.effect(() => () => process.stdout.write(`greeter gone: ${config.text}\\n`))\n}\n",
    )
    .unwrap();

    // The rutis node: listens for mac.
    let main = Ctx::root().unwrap();
    main.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock))
        .unwrap();
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
    main.plugin(LinkPlugin::new(
        LinkConfig::listen(id("mac"), "websocket", "main", "public").require("node"),
    ));
    main.plugin(ImportPlugin::new(id("mac"), ["calendar"]));
    main.plugin(ExportPlugin::new(id("mac"), ["clock"]));
    main.plugin(EventsPlugin::new(id("mac"), ["tick"], ["tock"]).unwrap());
    let tocks = Arc::new(Mutex::new(Vec::new()));
    main.events()
        .on(&main, &node_event("tock"), Record(tocks.clone()))
        .unwrap();
    let rows = Arc::new(PeerResolver::new());
    let plugin = LoaderPlugin::new(
        Chain::new().with_shared(rows.clone()),
        LoaderOptions::default(),
    );
    let loader = plugin.handle();
    (&main.plugin(plugin)).await.unwrap();
    main.plugin(PeerRowsPlugin::new(id("mac"), rows));
    let patches: Vec<Patch> = serde_json::from_value(json!([{ "insert": [
        { "id": "g", "name": "peer:mac/greeter-js", "config": { "text": "from rutis" } },
        // Its scope crosses: it waits for what it injects, and runs with
        // what it isolates in a scope of its own.
        { "id": "gated", "name": "peer:mac/greeter-js", "config": { "text": "gated" }, "inject": ["absent"] },
        { "id": "isolated", "name": "peer:mac/greeter-js", "config": { "text": "isolated" }, "isolate": { "calendar": true } }
    ] }]))
    .unwrap();
    loader
        .reconcile(vec![Layer::new("rows", patches)], None)
        .await
        .unwrap();

    // The Cordis node: dials main.
    let mut child = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(repo().join("node/rutis-runtime/test/fixtures/cordis-node.mjs"))
        .arg(&address)
        .arg(&anchor)
        .current_dir(repo().join("node/rutis-runtime"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let said = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let record = said.clone();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            record.lock().unwrap().push(line);
        }
    });
    let saw = |line: &'static str| {
        let said = said.clone();
        move || said.lock().unwrap().iter().any(|l| l == line).then_some(())
    };

    // Cordis's calendar, imported here; sync and async.
    let calendar = eventually(
        || main.get_as::<dyn HostDispatch>(host_key("calendar")),
        "calendar imported",
    )
    .await;
    let today = tokio::task::spawn_blocking({
        let calendar = calendar.clone();
        move || calendar.invoke("today", RpcValue::List(vec![]))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(today.json().unwrap(), json!("monday"));
    let later = settle(calendar.invoke("later", RpcValue::List(vec![])).unwrap())
        .await
        .unwrap();
    assert_eq!(later.json().unwrap(), json!("tuesday"));
    drop(calendar);

    // rutis's clock, imported there and called synchronously.
    eventually(saw("clock: 42"), "Cordis calling the clock").await;

    // A rutis row hosted in Cordis.
    eventually(saw("greeter: from rutis"), "the hosted row").await;
    eventually(saw("greeter: isolated"), "the isolated row").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        saw("greeter: gated")().is_none(),
        "a hosted row waits for what it injects"
    );

    // tick goes there, tock comes back.
    main.events()
        .parallel(
            &main,
            &node_event("tick"),
            Arc::new(NodeEvent {
                args: json!(["ping"]),
            }),
        )
        .await
        .unwrap();
    eventually(saw("tick: [\"ping\"]"), "tick in Cordis").await;
    assert_eq!(
        *tocks.lock().unwrap(),
        vec![json!(["pong", 1])],
        "tock back before tick's parallel ended"
    );

    // Removing the row unloads it there.
    loader.reconcile(vec![], None).await.unwrap();
    eventually(saw("greeter gone: from rutis"), "the hosted row unloaded").await;

    // Cordis remembers a withdrawal of the clock: an older announcement
    // arriving after it is stale, a newer one provides it again (its user
    // starts once more each time).
    let clock_uses = || {
        said.lock()
            .unwrap()
            .iter()
            .filter(|line| *line == "clock: 42")
            .count()
    };
    let used = clock_uses();
    let session = main
        .get_as::<rutis_bridge::Peer>(rutis_bridge::peer_key(&id("mac")))
        .unwrap()
        .connection()
        .clone();
    let send = |method: &'static str, fields: RpcValue| {
        let session = session.clone();
        async move {
            let reply = session
                .invoke_async("", method, RpcValue::List(vec![fields]))
                .await
                .unwrap();
            settle(reply).await.unwrap();
        }
    };
    let announcement = |version: u64| {
        RpcValue::Record(
            [
                ("name".to_owned(), RpcValue::Data(json!("clock"))),
                (
                    "service".to_owned(),
                    RpcValue::Record(
                        [(
                            "now".to_owned(),
                            RpcValue::callback(|_| Ok(json!(42).into())),
                        )]
                        .into(),
                    ),
                ),
                ("shape".to_owned(), RpcValue::Data(json!({ "now": "sync" }))),
                ("version".to_owned(), RpcValue::Data(json!(version))),
            ]
            .into(),
        )
    };
    send(
        "services.withdraw",
        RpcValue::Data(json!({ "name": "clock", "version": 1000 })),
    )
    .await;
    send("services.announce", announcement(999)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        clock_uses(),
        used,
        "a stale announcement brings nothing back"
    );
    send("services.announce", announcement(1001)).await;
    eventually(
        || (clock_uses() == used + 1).then_some(()),
        "the clock again",
    )
    .await;

    // Cordis's export restarted with another list: what it still exports is
    // back here (its versions go on from the last export's).
    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"export agenda\n").await.unwrap();
    stdin.flush().await.unwrap();
    eventually(saw("export: calendar,agenda"), "the export restarted").await;
    eventually(
        || main.get_as::<dyn HostDispatch>(host_key("calendar")),
        "calendar imported again",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        main.get_as::<dyn HostDispatch>(host_key("calendar"))
            .is_some(),
        "calendar stays"
    );
    child.start_kill().unwrap();
}

/// A Cordis node that listens: rutis dials it; a second rutis controller
/// with the same identity takes the session over.
#[tokio::test(flavor = "multi_thread")]
async fn a_listening_cordis_node_lets_a_newer_connection_take_over() {
    let anchor = tempfile::tempdir().unwrap();
    std::fs::write(anchor.path().join("package.json"), "{}").unwrap();
    let mut child = tokio::process::Command::new("node")
        .args(["--import", "tsx"])
        .arg(repo().join("node/rutis-runtime/test/fixtures/cordis-node.mjs"))
        .arg("listen:ws://127.0.0.1:0/rutis")
        .arg(anchor.path().join("package.json"))
        .current_dir(repo().join("node/rutis-runtime"))
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let address = loop {
        let line = lines
            .next_line()
            .await
            .unwrap()
            .expect("the node's address");
        if let Some(address) = line.strip_prefix("rutis: listening on ") {
            break address.to_owned();
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });

    async fn controller(address: &str) -> (Ctx, rutis::FiberView) {
        let root = Ctx::root().unwrap();
        root.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock))
            .unwrap();
        (&root.plugin(WebSocketPlugin::new(Config::new()).unwrap()))
            .await
            .unwrap();
        root.plugin(IdentityPlugin::new(
            "main",
            StaticIdentity::new(id("main")).present(
                id("mac"),
                rutis_bridge::Credential::Bearer("mac-token".into()),
            ),
        ));
        let link = root.plugin(LinkPlugin::new(
            LinkConfig::dial(id("mac"), "websocket", "main", address).require("node"),
        ));
        root.plugin(ImportPlugin::new(id("mac"), ["calendar"]));
        (root, link)
    }

    let (first, first_link) = controller(&address).await;
    eventually(
        || first.get_as::<dyn HostDispatch>(host_key("calendar")),
        "the first controller's import",
    )
    .await;
    let (second, _second_link) = controller(&address).await;
    eventually(
        || second.get_as::<dyn HostDispatch>(host_key("calendar")),
        "the second controller's import",
    )
    .await;
    // The first was replaced: its session and what it imported went.
    eventually(
        || {
            first
                .get_as::<dyn HostDispatch>(host_key("calendar"))
                .is_none()
                .then_some(())
        },
        "the first controller's import withdrawn",
    )
    .await;
    // Stop it before it takes the session back.
    first_link.dispose().await.unwrap();
    let calendar = second
        .get_as::<dyn HostDispatch>(host_key("calendar"))
        .unwrap();
    let today =
        tokio::task::spawn_blocking(move || calendar.invoke("today", RpcValue::List(vec![])))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(today.json().unwrap(), json!("monday"));
    child.start_kill().unwrap();
}
