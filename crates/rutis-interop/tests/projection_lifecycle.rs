#![cfg(unix)]
//! Regressions for service projection and export lifecycle (PR #73 review of
//! 908da2c): withdrawal/re-registration ordering, handles held across
//! reentrant replacement, cycles after close, stable identity of Service
//! instances, Service.check gating, effects created by service methods, and
//! slot refresh after a throwing method.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, ServiceIntercept, TypeKey};
use rutis_interop::{decode, Error, Process, Projection};
use serde_json::{json, Value};

fn package() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/node")
}

// What the generator emits for one exported service.
struct Counter {
    process: Arc<Process>,
    handle: String,
}
impl Counter {
    fn call(&self, method: &str, args: Value) -> Result<Value, Error> {
        self.process.call(&self.handle, method, args)
    }
    fn current(&self) -> Result<f64, Error> {
        decode(self.call("current", json!([]))?)
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        self.process.release(&self.handle);
    }
}

const COUNTER: &str = r#"
export function apply(ctx) {
  const make = value => ({
    current() { return value },
    swap(next) { ctx.set('counter', make(next)) },
    swapThenFail(next) { ctx.set('counter', make(next)); throw new Error('after swap') },
  })
  let withdraw = ctx.provide('counter', make(1))
  ctx.provide('control', {
    cycle(value) { withdraw(); withdraw = ctx.provide('counter', make(value)) },
  })
}
"#;

type Shared = Arc<Mutex<Option<Arc<Process>>>>;

struct Mount {
    plugin: PathBuf,
    process: Shared,
}

impl Plugin for Mount {
    fn name(&self) -> &str {
        "projection-lifecycle"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let projection = Projection::new();
            projection.service::<Counter>("counter", |process, handle| Counter { process, handle });
            let process = Process::launch_observed(
                &package(),
                &self.plugin,
                json!({}),
                json!({ "counter": ["current", "swap", "swapThenFail"], "control": ["cycle"] }),
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
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn mounted() -> (tempfile::NamedTempFile, Ctx, rutis::FiberView, Shared) {
    let mut file = tempfile::Builder::new().suffix(".mjs").tempfile().unwrap();
    file.write_all(COUNTER.as_bytes()).unwrap();
    let ctx = Ctx::root().unwrap();
    let shared = Shared::default();
    let view = ctx.plugin(Mount {
        plugin: file.path().to_owned(),
        process: shared.clone(),
    });
    (&view).await.unwrap();
    (file, ctx, view, shared)
}

// Review #1: the new registration waits for the withdrawal to complete, and a
// failed publication is retried instead of being recorded as applied.
#[tokio::test(flavor = "current_thread")]
async fn withdrawal_and_immediate_reregistration_keep_the_service() {
    let (_file, ctx, view, shared) = mounted().await;
    let process = shared.lock().unwrap().clone().unwrap();
    process.call("control", "cycle", json!([9])).unwrap();
    settle().await;
    assert_eq!(ctx.get::<Counter>().unwrap().current().unwrap(), 9.0);
    drop(process);
    view.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}

// Review #3: a handle whose proxy is being published is not released when a
// reentrant change replaces it before publication finishes.
#[tokio::test(flavor = "current_thread")]
async fn a_proxy_held_during_reentrant_replacement_stays_callable() {
    let (_file, ctx, view, _shared) = mounted().await;
    let held: Arc<Mutex<Option<Arc<Counter>>>> = Arc::default();
    let hook = held.clone();
    ctx.intercept_set_as::<Counter>(TypeKey::of::<Counter>(), move |candidate| {
        let mut held = hook.lock().unwrap();
        if held.is_none() {
            *held = Some(candidate.clone());
            drop(held);
            // Replace again while the candidate is still being published.
            candidate.call("swap", json!([3])).unwrap();
        }
        ServiceIntercept::Continue
    })
    .unwrap();
    ctx.get::<Counter>()
        .unwrap()
        .call("swap", json!([2]))
        .unwrap();
    settle().await;
    let held = held.lock().unwrap().take().unwrap();
    assert_eq!(held.current().unwrap(), 2.0);
    assert_eq!(ctx.get::<Counter>().unwrap().current().unwrap(), 3.0);
    drop(held);
    view.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}

// Review #5: closing breaks the Process -> Projection -> writer -> proxy ->
// Process cycle, so a disposed mount frees its process.
#[tokio::test(flavor = "current_thread")]
async fn a_disposed_mount_frees_its_process() {
    let (_file, ctx, view, shared) = mounted().await;
    ctx.get::<Counter>().unwrap().current().unwrap();
    let weak: Weak<Process> = Arc::downgrade(shared.lock().unwrap().as_ref().unwrap());
    shared.lock().unwrap().take();
    view.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
    settle().await;
    assert!(weak.upgrade().is_none(), "the process is still referenced");
}

// Review #7: a method that replaces the service and then throws still
// refreshes the slot.
#[tokio::test(flavor = "current_thread")]
async fn a_throwing_method_still_refreshes_the_slot() {
    let (_file, ctx, view, _shared) = mounted().await;
    let error = ctx
        .get::<Counter>()
        .unwrap()
        .call("swapThenFail", json!([9]))
        .unwrap_err();
    assert!(error.to_string().contains("after swap"));
    settle().await;
    assert_eq!(ctx.get::<Counter>().unwrap().current().unwrap(), 9.0);
    view.dispose().await.unwrap();
    ctx.shutdown().await.unwrap();
}

fn ticker() -> PathBuf {
    package().join("test/fixtures/ticker.mjs")
}

// Review #2 and #6: effects that Service methods create through their context
// are disposed with the mount, and reading a Service instance repeatedly does
// not look like a replacement.
#[tokio::test(flavor = "current_thread")]
async fn service_method_effects_end_with_the_mount_and_handles_stay_stable() {
    let process = Process::launch(
        &package(),
        &ticker(),
        json!({}),
        json!({ "ticker": ["start", "count"] }),
    )
    .await
    .unwrap();
    let handle = process.service("ticker").unwrap();
    assert_eq!(
        process.call(&handle, "start", json!([])).unwrap(),
        json!(true)
    );
    for _ in 0..3 {
        process.call(&handle, "count", json!([])).unwrap();
    }
    assert_eq!(process.service("ticker").as_deref(), Some("ticker"));
    // The interval registered by start() must not keep Node alive.
    tokio::time::timeout(Duration::from_secs(5), process.dispose())
        .await
        .expect("disposal waits for an effect nothing cleans up")
        .unwrap();
}

// Review #4: Service.check gates export like native injection.
#[tokio::test(flavor = "current_thread")]
async fn a_service_failing_its_check_is_not_exported() {
    let process = Process::launch(
        &package(),
        &ticker(),
        json!({ "ready": false }),
        json!({ "ticker": ["start", "count"] }),
    )
    .await
    .unwrap();
    assert_eq!(process.service("ticker"), None);
    process.dispose().await.unwrap();
}
