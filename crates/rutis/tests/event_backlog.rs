//! Same-key emits queue behind each other; the queue is visible in the
//! runtime diagnostics.

use std::sync::Arc;
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};
use tokio::sync::Semaphore;

struct Job;
impl Event for Job {
    const NAME: &'static str = "backlog/job";
    type Value = ();
}

/// Handles one event per permit.
struct Slow(Arc<Semaphore>);
impl Listener<Job> for Slow {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Job,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            self.0.acquire().await.unwrap().forget();
            Ok(None)
        })
    }
}

fn pending(ctx: &Ctx, key: &EventKey<Job>) -> Option<(usize, Duration)> {
    let key = key.describe();
    ctx.diagnostics()
        .event_backlogs
        .into_iter()
        .find(|backlog| backlog.key.describe() == key)
        .map(|backlog| (backlog.pending, backlog.oldest))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_key_emits_waiting_behind_a_slow_listener_are_visible() {
    let ctx = Ctx::root().unwrap();
    let permits = Arc::new(Semaphore::new(0));
    let busy = EventKey::<Job>::named("busy");
    let idle = EventKey::<Job>::named("idle");
    ctx.events().on(&ctx, &busy, Slow(permits.clone())).unwrap();
    ctx.events().on(&ctx, &idle, Slow(permits.clone())).unwrap();

    for _ in 0..3 {
        ctx.events().emit(&ctx, &busy, Arc::new(Job)).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(20)).await; // only lets the oldest age
    let (count, oldest) = pending(&ctx, &busy).expect("busy key has a backlog");
    assert_eq!(count, 3);
    assert!(oldest >= Duration::from_millis(20), "{oldest:?}");
    // A key with no accepted emits has no entry.
    assert_eq!(pending(&ctx, &idle), None);

    // Each handled event leaves the backlog; the entry goes once it is empty.
    permits.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while pending(&ctx, &busy).map(|(count, _)| count) != Some(2) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one event handled");
    permits.add_permits(2);
    tokio::time::timeout(Duration::from_secs(5), async {
        while pending(&ctx, &busy).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("backlog drained");
    ctx.shutdown().await.unwrap();
}
