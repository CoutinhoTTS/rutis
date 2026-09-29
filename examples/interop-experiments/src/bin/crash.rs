//! Experiment: what a rutis application sees when the Node process of a
//! mounted Cordis plugin crashes or stalls. Prints observations; asserts
//! nothing. `cargo run -p interop-experiments --bin crash`

#[cfg(unix)]
mod unix {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use interop_experiments::fragile::{self, Fragile};
    use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberView, Plugin, TypeKey};

    /// A rutis plugin that depends on the mounted service.
    struct Consumer {
        stopped: Arc<AtomicBool>,
        injects: [TypeKey; 1],
    }

    impl Plugin for Consumer {
        fn name(&self) -> &str {
            "consumer"
        }
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            let stopped = self.stopped.clone();
            Box::pin(async move {
                ctx.effect(move || {
                    Effect::Disposer(Box::new(move || {
                        stopped.store(true, Ordering::SeqCst);
                        Ok(())
                    }))
                })?;
                Ok(Effect::Done)
            })
        }
    }

    struct Setup {
        ctx: Ctx,
        mount: FiberView,
        consumer: FiberView,
        stopped: Arc<AtomicBool>,
        pid: u32,
    }

    async fn setup() -> Setup {
        let ctx = Ctx::root().unwrap();
        let mount = ctx.plugin(fragile::Plugin::new(fragile::Config::default()));
        (&mount).await.unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let consumer = ctx.plugin(Consumer {
            stopped: stopped.clone(),
            injects: [TypeKey::of::<Fragile>()],
        });
        (&consumer).await.unwrap();
        let pid = ctx.get::<Fragile>().unwrap().pid().unwrap() as u32;
        Setup {
            ctx,
            mount,
            consumer,
            stopped,
            pid,
        }
    }

    fn signal(pid: u32, signal: &str) {
        std::process::Command::new("kill")
            .args([signal, &pid.to_string()])
            .status()
            .unwrap();
    }

    fn alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_default()
                .contains(") Z ")
    }

    async fn report(setup: &Setup) {
        let s = setup;
        println!("  node alive:          {}", alive(s.pid));
        println!("  mount fiber:         {:?}", s.mount.state().state);
        println!(
            "  consumer fiber:      {:?} (stopped: {})",
            s.consumer.state().state,
            s.stopped.load(Ordering::SeqCst)
        );
        match s.ctx.get::<Fragile>() {
            Some(service) => {
                let started = Instant::now();
                let result = service.ping();
                println!(
                    "  service in ctx:      yes; ping -> {result:?} in {:?}",
                    started.elapsed()
                );
            }
            None => println!("  service in ctx:      no"),
        }
    }

    async fn teardown(setup: Setup) {
        let started = Instant::now();
        let disposed = tokio::time::timeout(Duration::from_secs(5), setup.mount.dispose()).await;
        println!(
            "  dispose mount:       {:?} in {:?}",
            disposed.map(|result| result.map_err(|error| error.to_string())),
            started.elapsed()
        );
        println!(
            "  consumer after:      {:?} (stopped: {})",
            setup.consumer.state().state,
            setup.stopped.load(Ordering::SeqCst)
        );
        let started = Instant::now();
        let shutdown = tokio::time::timeout(Duration::from_secs(5), setup.ctx.shutdown()).await;
        println!(
            "  shutdown root:       {:?} in {:?}",
            shutdown.map(|result| result.map_err(|error| error.to_string())),
            started.elapsed()
        );
        if alive(setup.pid) {
            println!("  node still alive after teardown; killing");
            signal(setup.pid, "-9");
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    pub async fn main() {
        println!("1. SIGKILL while idle");
        let s = setup().await;
        signal(s.pid, "-9");
        settle().await;
        report(&s).await;
        teardown(s).await;

        println!("\n2. process.exit(17) inside a synchronous call");
        let s = setup().await;
        let started = Instant::now();
        let result = s.ctx.get::<Fragile>().unwrap().exit(17.0);
        println!(
            "  exit call:           {result:?} in {:?}",
            started.elapsed()
        );
        settle().await;
        report(&s).await;
        teardown(s).await;

        println!("\n3. SIGKILL during a pending async call");
        let s = setup().await;
        let service = s.ctx.get::<Fragile>().unwrap();
        let pending = tokio::spawn(async move { service.hang().await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let killed = Instant::now();
        signal(s.pid, "-9");
        let result = tokio::time::timeout(Duration::from_secs(5), pending).await;
        println!(
            "  pending call:        {result:?} in {:?}",
            killed.elapsed()
        );
        report(&s).await;
        teardown(s).await;

        println!("\n4. exception thrown from a timer");
        let s = setup().await;
        println!(
            "  call:                {:?}",
            s.ctx.get::<Fragile>().unwrap().throw_later()
        );
        settle().await;
        report(&s).await;
        teardown(s).await;

        println!("\n5. unhandled rejection");
        let s = setup().await;
        println!(
            "  call:                {:?}",
            s.ctx.get::<Fragile>().unwrap().reject_later()
        );
        settle().await;
        report(&s).await;
        teardown(s).await;

        println!("\n6. event loop blocked for 2s by a synchronous call");
        let s = setup().await;
        let service = s.ctx.get::<Fragile>().unwrap();
        let blocker = std::thread::spawn({
            let service = service.clone();
            move || service.spin(2000.0)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = Instant::now();
        let timed = tokio::time::timeout(Duration::from_millis(500), service.later(1.0)).await;
        println!(
            "  async call, 500ms timeout: {:?} in {:?}",
            timed.map(|result| result.map_err(|error| error.to_string())),
            started.elapsed()
        );
        let started = Instant::now();
        let sync = service.ping();
        println!("  sync call:           {sync:?} in {:?}", started.elapsed());
        println!("  blocker:             {:?}", blocker.join().unwrap());
        drop(service);
        report(&s).await;
        teardown(s).await;

        println!("\n7. Node process frozen (SIGSTOP)");
        let s = setup().await;
        signal(s.pid, "-STOP");
        let service = s.ctx.get::<Fragile>().unwrap();
        let started = Instant::now();
        let timed = tokio::time::timeout(Duration::from_millis(500), service.later(1.0)).await;
        println!(
            "  async call, 500ms timeout: {:?} in {:?}",
            timed.map(|result| result.map_err(|error| error.to_string())),
            started.elapsed()
        );
        drop(service);
        println!("  node alive:          {}", alive(s.pid));
        let started = Instant::now();
        let disposed = tokio::time::timeout(Duration::from_secs(2), s.mount.dispose()).await;
        println!(
            "  dispose mount, 2s timeout: {:?} in {:?}",
            disposed.map(|result| result.map_err(|error| error.to_string())),
            started.elapsed()
        );
        signal(s.pid, "-CONT");
        settle().await;
        println!("  after SIGCONT, mount fiber: {:?}", s.mount.state().state);
        teardown(s).await;

        println!("\n8. process.exit(3) while the plugin is applied");
        let ctx = Ctx::root().unwrap();
        let mount = ctx.plugin(fragile::Plugin::new(fragile::Config {
            exit_on_apply: Some(3.0),
        }));
        let started = Instant::now();
        let result = (&mount).await;
        println!(
            "  mount:               {:?} in {:?}",
            result.map(|_| ()).map_err(|error| error.to_string()),
            started.elapsed()
        );
        println!("  mount fiber:         {:?}", mount.state().state);
        ctx.shutdown().await.unwrap();

        println!("\n9. remount after a crash");
        let s = setup().await;
        signal(s.pid, "-9");
        settle().await;
        let _ = s.mount.dispose().await;
        let again = s
            .ctx
            .plugin(fragile::Plugin::new(fragile::Config::default()));
        (&again).await.unwrap();
        settle().await;
        println!(
            "  new mount:           {:?}; consumer {:?} (stopped: {})",
            again.state().state,
            s.consumer.state().state,
            s.stopped.load(Ordering::SeqCst)
        );
        println!(
            "  ping via new mount:  {:?}",
            s.ctx.get::<Fragile>().map(|service| service.ping())
        );
        s.ctx.shutdown().await.unwrap();
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
