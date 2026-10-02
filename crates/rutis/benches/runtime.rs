//! Run with `cargo bench -p rutis --bench runtime`. Local samples are not
//! portable latency guarantees or hard CI performance thresholds.
use std::hint::black_box;
use std::time::Instant;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};

struct Store(u64);

/// Provides `Store`.
struct Provider;
impl Plugin for Provider {
    fn name(&self) -> &str {
        "bench-provider"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.provide(Store(1))?;
            Ok(Effect::Done)
        })
    }
}

/// Depends on `Store` and reads it once; keeps its context for later reads.
struct Consumer([TypeKey; 1], std::sync::Mutex<Option<Ctx>>);
impl Plugin for Consumer {
    fn name(&self) -> &str {
        "bench-consumer"
    }
    fn injects(&self) -> &[TypeKey] {
        &self.0
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            black_box(ctx.require::<Store>()?.0);
            *self.1.lock().unwrap() = Some(ctx.clone());
            Ok(Effect::Done)
        })
    }
}

fn report(name: &str, ns: f64) {
    println!("{name:40} {ns:12.1} ns/op");
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let iterations = std::env::var("RUTIS_BENCH_ITERATIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100_000)
        .max(1);
    println!("iterations={iterations}; release build; 2 runtime workers");

    let root = Ctx::root().unwrap();
    runtime.block_on(async { (&root.plugin(Provider)).await.unwrap() });
    // A strict read must name a declared dependency: read from a consumer.
    let consumer = std::sync::Arc::new(Consumer([TypeKey::of::<Store>()], Default::default()));
    runtime.block_on(async { (&root.plugin(ConsumerRef(consumer.clone()))).await.unwrap() });
    let reader = consumer.1.lock().unwrap().clone().unwrap();
    let read = |strict: bool| {
        for _ in 0..1000 {
            black_box(reader.get::<Store>());
        }
        let start = Instant::now();
        for _ in 0..iterations {
            if strict {
                black_box(reader.require::<Store>().unwrap());
            } else {
                black_box(reader.get::<Store>());
            }
        }
        start.elapsed().as_nanos() as f64 / iterations as f64
    };
    report("service get", read(false));
    report("service require", read(true));

    // One plugin from mount to disposed, and a dependent consumer that is
    // mounted, started once its dependency is ready, and disposed.
    let cycles = iterations / 100 + 1;
    let cycle = |dependent: bool| {
        runtime.block_on(async {
            let start = Instant::now();
            for _ in 0..cycles {
                let view = if dependent {
                    root.plugin(Consumer([TypeKey::of::<Store>()], Default::default()))
                } else {
                    root.plugin(Provider2)
                };
                (&view).await.unwrap();
                view.dispose().await.unwrap();
            }
            start.elapsed().as_nanos() as f64 / cycles as f64
        })
    };
    report("plugin load + dispose", cycle(false));
    report("dependent plugin load + dispose", cycle(true));
    runtime.block_on(root.shutdown()).unwrap();
}

/// A plugin with no dependencies and no services.
struct Provider2;
impl Plugin for Provider2 {
    fn name(&self) -> &str {
        "bench-plain"
    }
    fn apply<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async { Ok(Effect::Done) })
    }
}

/// Mounts a shared consumer, so the bench can keep its context.
struct ConsumerRef(std::sync::Arc<Consumer>);
impl Plugin for ConsumerRef {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn injects(&self) -> &[TypeKey] {
        self.0.injects()
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        self.0.apply(ctx)
    }
}
