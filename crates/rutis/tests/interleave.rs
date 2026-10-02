//! Load, unload and restart requests racing on worker threads (#42).
//!
//! Each case fires a pseudo-random mix of operations at one fiber from
//! several tasks and then checks invariants that must hold for every
//! interleaving: nothing hangs, generations never overlap, and every
//! cleanup a generation registered runs exactly once.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, FiberView, Plugin, TypeKey};

const ROUNDS: u64 = 100;
const TASKS: u64 = 6;
const OPS_PER_TASK: usize = 8;

async fn soon<F: std::future::IntoFuture>(f: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
}

/// A small deterministic generator, so a failing round can be replayed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % bound
    }
}

/// What the generations of one fiber did.
#[derive(Default)]
struct Ledger {
    /// Generations between their apply and their cleanup.
    live: AtomicUsize,
    max_live: AtomicUsize,
    /// Cleanups registered and cleanups run.
    registered: AtomicUsize,
    cleaned: AtomicUsize,
}

impl Ledger {
    fn check(&self, round: u64) {
        assert_eq!(
            self.registered.load(Ordering::SeqCst),
            self.cleaned.load(Ordering::SeqCst),
            "round {round}: every registered cleanup runs exactly once"
        );
        assert_eq!(self.live.load(Ordering::SeqCst), 0, "round {round}");
        assert!(
            self.max_live.load(Ordering::SeqCst) <= 1,
            "round {round}: two generations were live at once"
        );
    }
}

/// Where a parent puts the newest child it started.
type ChildSlot = Arc<Mutex<Option<FiberView>>>;

/// Records its generations in a ledger; its apply yields a few times so
/// requests can arrive while it is Loading.
struct Tracked {
    ledger: Arc<Ledger>,
    injects: Vec<TypeKey>,
    child: Option<(Arc<Ledger>, ChildSlot)>,
}

impl Plugin for Tracked {
    fn name(&self) -> &str {
        "tracked"
    }

    fn injects(&self) -> &[TypeKey] {
        &self.injects
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            let ledger = self.ledger.clone();
            let live = ledger.live.fetch_add(1, Ordering::SeqCst) + 1;
            ledger.max_live.fetch_max(live, Ordering::SeqCst);
            let cleanup = ledger.clone();
            let registered = ctx.effect(move || {
                Effect::Disposer(Box::new(move || {
                    cleanup.live.fetch_sub(1, Ordering::SeqCst);
                    cleanup.cleaned.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }))
            });
            match registered {
                Ok(_) => {
                    ledger.registered.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) => {
                    ledger.live.fetch_sub(1, Ordering::SeqCst);
                    return Err(error);
                }
            }
            if let Some((child_ledger, slot)) = &self.child {
                let child = ctx.plugin(Tracked {
                    ledger: child_ledger.clone(),
                    injects: vec![],
                    child: None,
                });
                *slot.lock().unwrap() = Some(child);
            }
            Ok(Effect::Done)
        })
    }
}

/// Runs `op` from `TASKS` tasks, each drawing `OPS_PER_TASK` op codes.
async fn storm<F>(round: u64, codes: u64, op: F)
where
    F: Fn(u64) -> BoxFuture<'static, ()> + Send + Sync + 'static,
{
    let op = Arc::new(op);
    let mut tasks = Vec::new();
    for task in 0..TASKS {
        let op = op.clone();
        tasks.push(tokio::spawn(async move {
            let mut rng = Lcg(round * 1_000 + task);
            for _ in 0..OPS_PER_TASK {
                op(rng.next(codes)).await;
            }
        }));
    }
    soon(async {
        for task in tasks {
            task.await.expect("op task");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_settle_and_dispose_interleave() {
    for round in 0..ROUNDS {
        let ctx = Ctx::root().unwrap();
        let ledger = Arc::new(Ledger::default());
        let view = ctx.plugin(Tracked {
            ledger: ledger.clone(),
            injects: vec![],
            child: None,
        });
        let target = view.clone();
        storm(round, 8, move |code| {
            let view = target.clone();
            Box::pin(async move {
                // Results are not checked: a restart after dispose is
                // rejected, which is one of the interleavings.
                match code {
                    0..=3 => drop(view.restart().await),
                    4..=6 => drop((&view).await),
                    _ => drop(view.dispose().await),
                }
            })
        })
        .await;
        soon(view.dispose()).await.ok();
        assert_eq!(view.state().state, FiberState::Disposed, "round {round}");
        ledger.check(round);
        soon(ctx.shutdown()).await.unwrap();
    }
}

#[derive(Debug)]
struct Dep;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dependency_churn_interleaves_with_restart() {
    for round in 0..ROUNDS {
        let ctx = Ctx::root().unwrap();
        let ledger = Arc::new(Ledger::default());
        let view = ctx.plugin(Tracked {
            ledger: ledger.clone(),
            injects: vec![TypeKey::of::<Dep>()],
            child: None,
        });
        let provided = Arc::new(tokio::sync::Mutex::new(None));
        let (target, host) = (view.clone(), ctx.clone());
        storm(round, 6, move |code| {
            let (view, ctx, provided) = (target.clone(), host.clone(), provided.clone());
            Box::pin(async move {
                match code {
                    0 | 1 => {
                        let mut slot = provided.lock().await;
                        if slot.is_none() {
                            *slot = Some(ctx.provide(Dep).unwrap());
                        }
                    }
                    2 | 3 => {
                        // Held until the service is gone, so the next
                        // provide does not find it still bound.
                        let mut slot = provided.lock().await;
                        if let Some(disposer) = slot.take() {
                            disposer.dispose().await.unwrap();
                        }
                    }
                    4 => drop(view.restart().await),
                    _ => drop((&view).await),
                }
            })
        })
        .await;
        // With the dependency present the consumer ends up Active.
        if ctx.get::<Dep>().is_none() {
            ctx.provide(Dep).unwrap();
        }
        soon(&view).await.expect("settles");
        assert_eq!(view.state().state, FiberState::Active, "round {round}");
        soon(view.dispose()).await.unwrap();
        ledger.check(round);
        soon(ctx.shutdown()).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parent_restart_interleaves_with_child_operations() {
    for round in 0..ROUNDS {
        let ctx = Ctx::root().unwrap();
        let parent_ledger = Arc::new(Ledger::default());
        let child_ledger = Arc::new(Ledger::default());
        let child_slot: ChildSlot = Arc::new(Mutex::new(None));
        let parent = ctx.plugin(Tracked {
            ledger: parent_ledger.clone(),
            injects: vec![],
            child: Some((child_ledger.clone(), child_slot.clone())),
        });
        soon(&parent).await.expect("parent loads");
        let (target, slot) = (parent.clone(), child_slot.clone());
        storm(round, 6, move |code| {
            let parent = target.clone();
            // The newest child; an older one may already be gone.
            let child = slot.lock().unwrap().clone();
            Box::pin(async move {
                match (code, child) {
                    (0 | 1, _) => drop(parent.restart().await),
                    (2, Some(child)) => drop(child.restart().await),
                    (3, Some(child)) => drop(child.dispose().await),
                    (4, Some(child)) => drop((&child).await),
                    _ => drop((&parent).await),
                }
            })
        })
        .await;
        soon(parent.dispose()).await.unwrap();
        // Disposing the parent took every child generation with it.
        if let Some(child) = child_slot.lock().unwrap().take() {
            assert_eq!(child.state().state, FiberState::Disposed, "round {round}");
        }
        parent_ledger.check(round);
        child_ledger.check(round);
        soon(ctx.shutdown()).await.unwrap();
    }
}
