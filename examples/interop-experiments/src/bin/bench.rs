//! Experiment: the cost of calling a mounted Cordis plugin across the
//! process boundary. `cargo run --release -p interop-experiments --bin bench`

#[cfg(unix)]
mod unix {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use interop_experiments::bench::{self, Bench, Row};
    use rutis::Ctx;

    fn summary(label: &str, mut samples: Vec<Duration>) {
        samples.sort();
        let total: Duration = samples.iter().sum();
        let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
        println!(
            "{label:<34} n={:<6} mean {:>10.1?}  p50 {:>10.1?}  p99 {:>10.1?}  max {:>10.1?}",
            samples.len(),
            total / samples.len() as u32,
            at(0.5),
            at(0.99),
            samples[samples.len() - 1]
        );
    }

    fn measure(label: &str, count: usize, mut run: impl FnMut()) {
        for _ in 0..count.min(200) {
            run();
        }
        let samples = (0..count)
            .map(|_| {
                let started = Instant::now();
                run();
                started.elapsed()
            })
            .collect();
        summary(label, samples);
    }

    pub async fn main() {
        let mut mounts = Vec::new();
        for _ in 0..5 {
            let ctx = Ctx::root().unwrap();
            let started = Instant::now();
            let view = ctx.plugin(bench::Plugin::new(bench::Config {}));
            (&view).await.unwrap();
            mounts.push(started.elapsed());
            ctx.shutdown().await.unwrap();
        }
        summary("mount (spawn Node + apply)", mounts);

        let ctx = Ctx::root().unwrap();
        let view = ctx.plugin(bench::Plugin::new(bench::Config {}));
        (&view).await.unwrap();
        let service: Arc<Bench> = ctx.get::<Bench>().unwrap();

        measure("sync noop", 5000, || {
            service.noop().unwrap();
        });
        let mut samples = Vec::new();
        for index in 0..5200 {
            let started = Instant::now();
            service.noop_async().await.unwrap();
            if index >= 200 {
                samples.push(started.elapsed());
            }
        }
        summary("async noop", samples);

        for size in [1 << 10, 64 << 10, 1 << 20] {
            let text = "x".repeat(size);
            measure(&format!("echo string {} KiB", size >> 10), 300, || {
                assert_eq!(service.echo(&text).unwrap().len(), size);
            });
        }
        for count in [10, 1000, 10000] {
            measure(&format!("receive {count} rows"), 200, || {
                assert_eq!(service.rows(count as f64).unwrap().len(), count);
            });
        }
        let rows: Vec<Row> = service.rows(1000.0).unwrap();
        measure("send 1000 rows", 200, || {
            service.take(&rows).unwrap();
        });

        let item = service.item().unwrap();
        measure("live object: read property", 5000, || {
            item.current().unwrap();
        });
        measure("live object: call method", 5000, || {
            item.add(1.0).unwrap();
        });
        measure("get live object + drop", 2000, || {
            drop(service.item().unwrap());
        });

        let callbacks = 1000;
        let started = Instant::now();
        service.each(callbacks as f64, |_| Ok(())).unwrap();
        println!(
            "{:<34} {callbacks} callbacks in {:?} ({:?} each)",
            "callback Node -> Rust",
            started.elapsed(),
            started.elapsed() / callbacks
        );

        for concurrency in [1, 8, 64] {
            let total = 20000;
            let started = Instant::now();
            let tasks: Vec<_> = (0..concurrency)
                .map(|_| {
                    let service = service.clone();
                    tokio::spawn(async move {
                        for _ in 0..total / concurrency {
                            service.noop_async().await.unwrap();
                        }
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
            let elapsed = started.elapsed();
            println!(
                "{:<34} {:>8.0} calls/s",
                format!("async noop, {concurrency} concurrent"),
                total as f64 / elapsed.as_secs_f64()
            );
        }
        let total = 20000;
        let threads = 8;
        let started = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let service = service.clone();
                std::thread::spawn(move || {
                    for _ in 0..total / threads {
                        service.noop().unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        println!(
            "{:<34} {:>8.0} calls/s",
            "sync noop, 8 threads",
            total as f64 / started.elapsed().as_secs_f64()
        );

        drop((item, service));
        ctx.shutdown().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    unix::main().await
}

#[cfg(not(unix))]
fn main() {
    eprintln!("mounted Cordis plugins are supported on Unix only");
}
