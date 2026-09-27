//! Run with `bash tools/compare-event-dispatch.sh [baseline commit]`.
//! Seven alternating samples, 500k calls each; same binary/dependency versions.
//! Legacy keyed wrappers are shared by 0.3 and 0.4, with no pattern listeners.
use std::{hint::black_box, time::Instant};

macro_rules! probe {
    ($module:ident, $api:ident) => {
        #[allow(deprecated)]
        mod $module {
            use super::*;
            use $api::{BoxFuture, CordisError, Ctx, Event, Listener};
            struct Ping;
            impl Event for Ping {
                const NAME: &'static str = "probe";
                type Value = ();
            }
            struct Pass;
            impl Listener<Ping> for Pass {
                fn call<'a>(
                    &'a self,
                    _: &'a Ctx,
                    _: &'a Ping,
                ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
                    Box::pin(async { Ok(None) })
                }
            }
            pub fn sample(runtime: &tokio::runtime::Runtime, listeners: usize) -> f64 {
                let root = Ctx::root().unwrap();
                for _ in 0..listeners {
                    root.events().on_keyed(&root, "room/hit", Pass).unwrap();
                }
                let elapsed = runtime.block_on(async {
                    for _ in 0..10_000 {
                        root.events()
                            .serial_keyed(&root, "room/hit", &Ping)
                            .await
                            .unwrap();
                    }
                    let begin = Instant::now();
                    for _ in 0..500_000 {
                        black_box(
                            root.events()
                                .serial_keyed(&root, "room/hit", &Ping)
                                .await
                                .unwrap(),
                        );
                    }
                    begin.elapsed().as_nanos() as f64 / 500_000.0
                });
                runtime.block_on(root.shutdown()).unwrap();
                elapsed
            }
        }
    };
}
probe!(baseline, old);
probe!(updated, new);
fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _enter = runtime.enter();
    for listeners in [0, 1, 8, 64] {
        let mut old = Vec::new();
        let mut new = Vec::new();
        for round in 0..7 {
            if round % 2 == 0 {
                old.push(baseline::sample(&runtime, listeners));
                new.push(updated::sample(&runtime, listeners));
            } else {
                new.push(updated::sample(&runtime, listeners));
                old.push(baseline::sample(&runtime, listeners));
            }
        }
        old.sort_by(f64::total_cmp);
        new.sort_by(f64::total_cmp);
        println!(
            "listeners={listeners}: old={:.1} ns new={:.1} ns delta={:+.1}%",
            old[3],
            new[3],
            (new[3] / old[3] - 1.0) * 100.0
        );
    }
}
