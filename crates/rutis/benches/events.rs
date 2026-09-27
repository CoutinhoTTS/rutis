//! Run with `cargo bench -p rutis --bench events`. Local samples are not
//! portable latency guarantees or hard CI performance thresholds.
use std::hint::black_box;
use std::time::Instant;

use rutis::{
    BoxFuture, CordisError, Ctx, Event, EventKey, EventPattern, Listener, PatternListener,
    SyncEvent, SyncNext,
};

struct Ping;
impl Event for Ping {
    const NAME: &'static str = "bench/ping";
    type Value = u64;
}
impl SyncEvent for Ping {}
struct Pass;
impl Listener<Ping> for Pass {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Box::pin(async { Ok(None) })
    }
}
impl PatternListener<Ping> for Pass {
    fn call<'a>(
        &'a self,
        _: &'a Ctx,
        _: EventKey<Ping>,
        _: &'a Ping,
    ) -> BoxFuture<'a, Result<Option<u64>, CordisError>> {
        Box::pin(async { Ok(None) })
    }
}
fn measure(mut call: impl FnMut(), iterations: usize) -> f64 {
    for _ in 0..1000 {
        call();
    }
    let start = Instant::now();
    for _ in 0..iterations {
        call();
    }
    start.elapsed().as_nanos() as f64 / iterations as f64
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
    for listeners in [0, 1, 8, 64] {
        let root = Ctx::root().unwrap();
        let key = EventKey::of();
        for _ in 0..listeners {
            root.events()
                .on_sync(&root, &key, |_: &Ctx, _: &Ping| Ok(None))
                .unwrap();
            root.events()
                .on_waterfall_sync(
                    &root,
                    &key,
                    |_: &Ctx, _: &Ping, next: SyncNext<'_, Ping>| next.call(),
                )
                .unwrap();
        }
        report(
            &format!("bail_sync exact listeners={listeners}"),
            measure(
                || {
                    black_box(root.events().bail_sync(&root, &key, &Ping).unwrap());
                },
                iterations,
            ),
        );
        report(
            &format!("waterfall_sync exact listeners={listeners}"),
            measure(
                || {
                    black_box(
                        root.events()
                            .waterfall_sync(&root, &key, &Ping, |_, _| Ok(1))
                            .unwrap(),
                    );
                },
                iterations,
            ),
        );
        runtime.block_on(root.shutdown()).unwrap();
    }
    for patterns in [0, 1, 8, 64, 1024] {
        let root = Ctx::root().unwrap();
        let key = EventKey::named("room/hit");
        root.events().on(&root, &key, Pass).unwrap();
        for i in 0..patterns {
            root.events()
                .on_pattern(
                    &root,
                    EventPattern::prefix(if i == 0 {
                        "room/".into()
                    } else {
                        format!("miss/{i}/")
                    }),
                    Pass,
                )
                .unwrap();
        }
        let loops = if patterns > 64 {
            iterations / 10 + 1
        } else {
            iterations
        };
        let samples = runtime.block_on(async {
            for _ in 0..1000 {
                root.events().serial(&root, &key, &Ping).await.unwrap();
            }
            let start = Instant::now();
            for _ in 0..loops {
                black_box(root.events().serial(&root, &key, &Ping).await.unwrap());
            }
            start.elapsed().as_nanos() as f64 / loops as f64
        });
        report(&format!("serial exact + patterns={patterns}"), samples);
        runtime.block_on(root.shutdown()).unwrap();
    }
    let root = Ctx::root().unwrap();
    let scoped = EventKey::<Ping>::of().instance(root.instance());
    report(
        "waterfall_sync instance no listeners",
        measure(
            || {
                black_box(
                    root.events()
                        .waterfall_sync(&root, &scoped, &Ping, |_, _| Ok(1))
                        .unwrap(),
                );
            },
            iterations,
        ),
    );
    runtime.block_on(root.shutdown()).unwrap();
}
