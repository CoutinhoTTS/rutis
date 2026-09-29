#![cfg(unix)]
//! Cordis events forwarded to rutis listeners: `emit` is fire and forget,
//! `parallel` waits until the rutis listeners are done.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{Ctx, Event, EventKey};
use rutis_interop::rpc::Value;
use rutis_interop::{decode_value, Error, Events, Mount, Process};
use serde_json::json;

const PLUGIN: &str = r#"
export function apply(ctx) {
  ctx.provide('bus', {
    fire(n) { ctx.emit('demo/tick', n, 'emit') },
    async wait(n) { await ctx.parallel('demo/tick', n, 'parallel'); return 'done' },
  })
}
"#;

struct Tick {
    n: f64,
    tag: String,
}
impl Event for Tick {
    const NAME: &'static str = "demo/tick";
    type Value = ();
}
fn tick(args: Vec<Value>) -> Result<Tick, Error> {
    let mut args = args.into_iter();
    Ok(Tick {
        n: decode_value(args.next().unwrap_or(Value::Undefined))?,
        tag: decode_value(args.next().unwrap_or(Value::Undefined))?,
    })
}

struct Recorder(Arc<Mutex<Vec<(f64, String)>>>);
impl rutis::Listener<Tick> for Recorder {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        event: &'a Tick,
    ) -> rutis::BoxFuture<'a, Result<Option<()>, rutis::CordisError>> {
        Box::pin(async move {
            // A slow listener: parallel must still wait for it.
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.0.lock().unwrap().push((event.n, event.tag.clone()));
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cordis_events_reach_rutis_listeners() {
    let mut plugin = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    plugin.write_all(PLUGIN.as_bytes()).unwrap();
    let ctx = Ctx::root().unwrap();
    let seen: Arc<Mutex<Vec<(f64, String)>>> = Arc::default();
    ctx.events()
        .on(&ctx, &EventKey::<Tick>::of(), Recorder(seen.clone()))
        .unwrap();

    let events = Events::new();
    events.forward::<Tick>("demo/tick", tick);
    let process = Process::mount(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node"),
        Mount {
            plugins: vec![(plugin.path(), json!({}))],
            services: json!({ "bus": ["fire", "wait"] }),
            events: Some((events.names(), events.clone())),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    events.attach(&ctx);
    let bus = process.service("bus").unwrap();

    // parallel returns only after the rutis listener finished.
    assert_eq!(
        process.call_async(&bus, "wait", json!([2])).await.unwrap(),
        json!("done")
    );
    assert_eq!(*seen.lock().unwrap(), vec![(2.0, "parallel".to_string())]);

    // emit returns at once; the event still arrives.
    process.call(&bus, "fire", json!([1])).unwrap();
    for _ in 0..100 {
        if seen.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(seen.lock().unwrap()[1], (1.0, "emit".to_string()));

    events.close();
    process.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}
