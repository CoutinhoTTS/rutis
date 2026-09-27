//! The registration context owns the listener; the callback receives the
//! emitter's context. Run with `cargo run -p rutis --example listener_ctx_ownership`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rutis::{BoxFuture, CordisError, Ctx, Effect, Event, Listener, Plugin};

struct Tick;

impl Event for Tick {
    const NAME: &'static str = "example::Tick";
    type Value = ();
}

struct OwnedListener {
    owner: Ctx,
    cleaned: Arc<AtomicUsize>,
}

impl Listener<Tick> for OwnedListener {
    fn call<'a>(
        &'a self,
        emitter: &'a Ctx,
        _event: &'a Tick,
    ) -> BoxFuture<'a, Result<Option<()>, CordisError>> {
        Box::pin(async move {
            assert_ne!(emitter.instance(), self.owner.instance());
            let cleaned = self.cleaned.clone();
            self.owner.effect(move || {
                Effect::Disposer(Box::new(move || {
                    cleaned.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }))
            })?;
            Ok(None)
        })
    }
}

struct Register(Arc<AtomicUsize>);

impl Plugin for Register {
    fn name(&self) -> &str {
        "register-listener"
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            ctx.events().on::<Tick>(
                ctx,
                &rutis::EventKey::of(),
                OwnedListener {
                    owner: ctx.clone(),
                    cleaned: self.0.clone(),
                },
            )?;
            Ok(Effect::Done)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Ctx::root()?;
    let cleaned = Arc::new(AtomicUsize::new(0));
    let plugin = root.plugin(Register(cleaned.clone()));
    (&plugin).await?;
    root.events()
        .serial(&root, &rutis::EventKey::of(), &Tick)
        .await?;
    plugin.shutdown().await?;
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    root.shutdown().await?;
    Ok(())
}
