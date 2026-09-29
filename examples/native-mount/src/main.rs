rutis_interop::include_mounts!();

#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = rutis::Ctx::root()?;
    let mounted = ctx.plugin(bindings::Plugin::new(bindings::Config { initial: 10.0 }));
    (&mounted).await?;
    let counter = ctx
        .get::<bindings::Counter>()
        .expect("native counter service");
    println!("sync: {}", counter.add(2.0)?);
    println!("async: {}", counter.delayed_add(3.0).await?);
    println!("current: {}", counter.current()?);
    println!("native error: {}", counter.fail().unwrap_err());
    mounted.dispose().await?;
    ctx.shutdown().await?;
    Ok(())
}

#[cfg(not(unix))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    Err("the native mount development example currently requires Unix".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::bindings;
    use rutis::{BoxFuture, CordisError, Ctx, Effect, FiberState, Plugin, TypeKey};
    use std::sync::{Arc, Mutex};

    async fn mount(ctx: &Ctx, initial: f64) -> rutis::FiberView {
        let view = ctx.plugin(bindings::Plugin::new(bindings::Config { initial }));
        (&view).await.unwrap();
        view
    }

    #[tokio::test(flavor = "current_thread")]
    async fn generated_native_methods_keep_sync_and_async_shapes() {
        let ctx = Ctx::root().unwrap();
        let view = mount(&ctx, 10.0).await;
        let counter = ctx.get::<bindings::Counter>().unwrap();
        // This explicitly requires an immediate f64 result, not a Future.
        let immediate: f64 = counter.add(2.0).unwrap();
        assert_eq!(immediate, 12.0);
        let (first, second) = tokio::join!(counter.delayed_add(1.0), counter.delayed_add(2.0));
        assert!(first.is_ok() && second.is_ok());
        assert_eq!(counter.current().unwrap(), 15.0);
        assert!(matches!(
            counter.add(f64::NAN),
            Err(rutis_interop::Error::Value(_))
        ));
        assert_eq!(counter.current().unwrap(), 15.0);
        let error = counter.fail().unwrap_err();
        assert!(
            matches!(error, rutis_interop::Error::Remote { ref message, .. } if message == "counter refused operation")
        );
        assert_eq!(counter.current().unwrap(), 15.0);
        view.dispose().await.unwrap();
        assert!(ctx.get::<bindings::Counter>().is_none());
        assert!(counter.current().is_err());
        ctx.shutdown().await.unwrap();
    }

    struct Consumer {
        injects: Vec<TypeKey>,
        observed: Arc<Mutex<Vec<f64>>>,
    }

    impl Plugin for Consumer {
        fn name(&self) -> &str {
            "ordinary-counter-consumer"
        }
        fn injects(&self) -> &[TypeKey] {
            &self.injects
        }
        fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
            Box::pin(async move {
                let counter = ctx.require::<bindings::Counter>()?;
                self.observed.lock().unwrap().push(counter.add(1.0)?);
                let observed = self.observed.clone();
                Ok(Effect::Disposer(Box::new(move || {
                    // Provider teardown must keep the real process alive until
                    // the original consumer's native cleanup has completed.
                    observed.lock().unwrap().push(counter.current()?);
                    Ok(())
                })))
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn native_dependency_gate_and_cleanup_cross_the_process_boundary() {
        let ctx = Ctx::root().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let consumer = ctx.plugin(Consumer {
            injects: vec![TypeKey::of::<bindings::Counter>()],
            observed: observed.clone(),
        });
        (&consumer).await.unwrap();
        assert_eq!(consumer.state().state, FiberState::Pending);
        let view = mount(&ctx, 3.0).await;
        (&consumer).await.unwrap();
        assert_eq!(consumer.state().state, FiberState::Active);
        assert_eq!(*observed.lock().unwrap(), vec![4.0]);
        view.dispose().await.unwrap();
        assert_eq!(*observed.lock().unwrap(), vec![4.0, 4.0]);
        assert_eq!(consumer.state().state, FiberState::Pending);
        ctx.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_startup_failure_does_not_publish_a_service() {
        let ctx = Ctx::root().unwrap();
        let view = ctx.plugin(bindings::Plugin::new(bindings::Config { initial: -1.0 }));
        let error = (&view).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("initial value must be non-negative"));
        assert!(ctx.get::<bindings::Counter>().is_none());
        let _ = ctx.shutdown().await;
    }

    #[tokio::test]
    async fn two_mounts_keep_their_native_scopes_and_state_separate() {
        let root = Ctx::root().unwrap();
        let key = TypeKey::of::<bindings::Counter>();
        let left = root.isolate(key.clone(), "left");
        let right = root.isolate(key, "right");
        let left_view = mount(&left, 1.0).await;
        let right_view = mount(&right, 20.0).await;
        assert_eq!(
            left.get::<bindings::Counter>().unwrap().add(2.0).unwrap(),
            3.0
        );
        assert_eq!(
            right.get::<bindings::Counter>().unwrap().current().unwrap(),
            20.0
        );
        left_view.dispose().await.unwrap();
        assert!(left.get::<bindings::Counter>().is_none());
        assert_eq!(
            right.get::<bindings::Counter>().unwrap().add(1.0).unwrap(),
            21.0
        );
        right_view.dispose().await.unwrap();
        root.shutdown().await.unwrap();
    }
}
