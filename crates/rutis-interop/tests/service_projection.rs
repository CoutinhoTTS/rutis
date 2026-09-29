#![cfg(unix)]
//! A Cordis service slot projected into rutis follows replacement and
//! withdrawal through public rutis API, while earlier snapshots keep
//! addressing their original object.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin};
use rutis_interop::{decode, Error, Process, Projection};
use serde_json::json;

// What the generator emits for one exported service.
struct Counter {
    process: Arc<Process>,
    handle: String,
}
impl Counter {
    fn current(&self) -> Result<f64, Error> {
        decode(self.process.call(&self.handle, "current", json!([]))?)
    }
    fn swap(&self, value: f64) -> Result<(), Error> {
        self.process.call(&self.handle, "swap", json!([value]))?;
        Ok(())
    }
    fn assign(&self, value: f64) -> Result<(), Error> {
        self.process.call(&self.handle, "assign", json!([value]))?;
        Ok(())
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.process.release(&self.handle);
    }
}

const PLUGIN: &[u8] = br#"
export function apply(ctx) {
  const make = value => ({
    current() { return value },
    swap(next) { ctx.set('counter', make(next)) },
    assign(next) { ctx.counter = make(next) },
  })
  let withdraw = ctx.provide('counter', make(1))
  ctx.provide('control', {
    withdraw() { withdraw(); withdraw = undefined },
    restore(value) { withdraw = ctx.provide('counter', make(value)) },
  })
}
"#;

struct Mount {
    plugin: std::path::PathBuf,
    process: Arc<std::sync::Mutex<Option<Arc<Process>>>>,
}

impl Plugin for Mount {
    fn name(&self) -> &str {
        "projection-test"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let projection = Projection::new();
            projection.service::<Counter>("counter", |process, handle| Counter { process, handle });
            let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node");
            let process = Process::launch_observed(
                &package,
                &self.plugin,
                json!({}),
                json!({ "counter": ["current", "swap", "assign"], "control": ["withdraw", "restore"] }),
                Some(projection.clone()),
            )
            .await?;
            let owner = process.clone();
            let followed = projection.clone();
            ctx.effect(move || {
                Effect::AsyncDisposer(Box::new(move || {
                    Box::pin(async move {
                        followed.close();
                        owner.dispose().await.map_err(Into::into)
                    })
                }))
            })?;
            *self.process.lock().unwrap() = Some(process.clone());
            projection.attach(ctx, process)?;
            Ok(Effect::Done)
        })
    }
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_service_follows_replacement_and_withdrawal() {
    let mut file = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    file.write_all(PLUGIN).unwrap();
    let ctx = Ctx::root().unwrap();
    let slot = Arc::default();
    let view = ctx.plugin(Mount {
        plugin: file.path().to_owned(),
        process: Arc::clone(&slot),
    });
    (&view).await.unwrap();
    let process = slot.lock().unwrap().clone().unwrap();

    let first = ctx.get::<Counter>().unwrap();
    assert_eq!(first.current().unwrap(), 1.0);

    // ctx.set emits nothing in Cordis; the slot is re-read after the call.
    first.swap(2.0).unwrap();
    let second = ctx.get::<Counter>().unwrap();
    assert_eq!(second.current().unwrap(), 2.0);
    assert_eq!(first.current().unwrap(), 1.0, "a snapshot keeps its object");

    // Property assignment goes through internal/set.
    second.assign(3.0).unwrap();
    assert_eq!(ctx.get::<Counter>().unwrap().current().unwrap(), 3.0);

    // A released, replaced object is dropped on the Cordis side.
    let released = first.handle.clone();
    drop(first);
    settle().await;
    assert!(process.call(&released, "current", json!([])).is_err());

    // Withdrawal removes the native binding; a new provide publishes again.
    process.call("control", "withdraw", json!([])).unwrap();
    settle().await;
    assert!(ctx.get::<Counter>().is_none());
    process.call("control", "restore", json!([5])).unwrap();
    settle().await;
    assert_eq!(ctx.get::<Counter>().unwrap().current().unwrap(), 5.0);
    assert_eq!(second.current().unwrap(), 2.0);

    drop(second);
    view.dispose().await.unwrap();
    assert!(ctx.get::<Counter>().is_none());
    ctx.shutdown().await.unwrap();
}
