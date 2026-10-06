# rutis

**A plugin runtime for programs that keep running.**

Plugins say what they need and what they provide; rutis decides when they start, when they stop, and when they start again. It is an idiomatic Rust implementation of the [Cordis](https://github.com/shigma/cordis) model.

- **Dependencies drive the lifecycle**: a plugin starts once its dependencies are there, stops when they go away, and reloads when a provider is replaced.
- **Cleanup you can rely on**: everything a plugin registers is released exactly once, in reverse order; a failed load rolls back.
- **Change without downtime**: hot-update configuration and swap providers; only what depends on the change restarts.
- **Small**: pure Rust, no `unsafe`, no serde; tokio, tokio-util and thiserror are the only dependencies.

```toml
[dependencies]
rutis = "0.6"
```

```rust
use std::sync::Arc;
use rutis::{BoxFuture, CordisError, Ctx, Effect, TypedPlugin};

struct Greeting(String);
struct Listener;

impl TypedPlugin for Listener {
    type Deps = (Arc<Greeting>,);   // starts once a Greeting is provided

    fn name(&self) -> &str { "listener" }

    fn apply<'a>(&'a self, _: &'a Ctx, (greeting,): Self::Deps) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            println!("{}", greeting.0);
            Ok(Effect::Done)
        })
    }
}
```

The complete program, with a provider being replaced at runtime, is in [examples/quickstart.rs](https://github.com/arcships/rutis/blob/main/crates/rutis/examples/quickstart.rs).

## The model

1. **Plugin, the unit of assembly**: one `apply` provides services, registers listeners and records cleanup.
2. **Fiber, the lifecycle container**: a six-state machine with dependency gating, subtree shutdown and exactly-once cleanup.
3. **Services, a type-keyed registry**: with isolate scopes and instance subtree visibility.
4. **Events, four dispatch modes**: emit, parallel, serial and waterfall.
5. **Dependency-driven reload**: when a provider unloads, its consumers are evicted and load again on their own.

## Learn more

- [Application design guide](https://github.com/arcships/rutis/blob/main/docs/development-guide.en.md) and [development handbook](https://github.com/arcships/rutis/blob/main/docs/development-handbook.en.md)
- [Core features](https://github.com/arcships/rutis/blob/main/docs/core-features.en.md): hot update, dynamic events, interception, diagnostics
- Plugins in TypeScript and Python, and links between machines: the [rutis repository](https://github.com/arcships/rutis)

## Recent changes

- **0.6.1** — interfaces for the plugin control plane ([rutis-loader](https://crates.io/crates/rutis-loader)), typed plugins; no breaking changes. [Upgrade notes](https://github.com/arcships/rutis/blob/main/docs/migration-0.6.0-to-0.6.1.en.md)
- **0.6** — growing error, diagnostic and observation types are `#[non_exhaustive]`; per-key emit backlogs in diagnostics. [Migration guide](https://github.com/arcships/rutis/blob/main/docs/migration-0.5-to-0.6.en.md)
- **0.5** — `EventKey<E>` for default, named and instance channels; `EventPattern<E>` prefix subscriptions; `bail_sync` / `waterfall_sync`. [Migration guide](https://github.com/arcships/rutis/blob/main/docs/migration-0.3-to-0.5.en.md)

## License

MIT. The design comes from [Cordis](https://github.com/shigma/cordis) by Shigma.
