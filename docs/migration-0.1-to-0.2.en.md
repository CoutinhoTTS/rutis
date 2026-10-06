# Migrating from 0.1.0 to 0.2.0

## Overview

**Version 0.2.0 is fully backward-compatible with 0.1.0 at the API level.** Existing code does not need to change to upgrade.

Two kernel capabilities are added (configuration hot updates and dynamic event keys), along with fixes from a Cordis contract audit. All are additive.

## Breaking change

There is only one: `TypeKey` no longer implements `Copy`.

Version 0.2.0 adds `Qualifier::Dynamic(Arc<str>)`, so `TypeKey` can no longer be `Copy`. If your code relies on `TypeKey: Copy` (for example, passing a value in multiple places without cloning it), add an explicit `.clone()`.

The impact is small: `TypeKey` is used only for service registration, event registration, and loading, not on hot paths.

```rust
// 0.1.0
let k = TypeKey::of::<MyService>();
do_something(k);
do_other(k);  // Copy, no problem

// 0.2.0
let k = TypeKey::of::<MyService>();
do_something(k.clone());
do_other(k);
```

## New capabilities

### Configuration hot updates (D32)

The static `Plugin` trait remains unchanged. The new `PluginFactory` trait supports changing configuration at runtime:

```rust
use rutis::{Plugin, PluginFactory, Ctx, CordisError, Effect, BoxFuture};

// Static mode (available in 0.1.0 and unchanged in 0.2.0)
struct MyPlugin { config: MyConfig }
impl Plugin for MyPlugin { /* ... */ }
let view = ctx.plugin(MyPlugin { config });

// Factory mode (new in 0.2.0)
struct MyFactory;
impl PluginFactory<MyConfig> for MyFactory {
    fn build(&self, config: &MyConfig) -> Result<Box<dyn Plugin>, CordisError> {
        Ok(Box::new(MyPlugin { config: config.clone() }))
    }
}
let view = ctx.plugin_with(MyFactory, config_v1);
view.update(config_v2).await?; // hot update: dry run → store → restart
```

Key constraints:

- **`build` must be a pure constructor with no side effects.** It is called once for the dry run and once for the actual load. The results need not be the same instance, but must be equivalent.
- **`injects` is a static declaration (`&[TypeKey]`) with the same shape as `Plugin::injects`.** Do not derive dependencies from configuration; revision D32f explicitly rejected that approach. To choose dependencies by configuration, split the implementation into multiple plugins and let configuration select which one to load.
- **`update()` works in all six states.** Pending plugins wait for their gate, Failed plugins can be repaired hot, and Loading cooperates with cancellation. It reuses the existing `Intent::Restart` path and adds no new transaction.

### Dynamic event keys (D33)

One event type can have multiple independent channels, with runtime strings as qualifiers:

```rust
use rutis::{Ctx, Event, TypeKey};

// Static qualifier (zero allocation)
let key = TypeKey::keyed::<MyEvent>("primary");

// Dynamic qualifier (runtime string, useful for bridged events)
let key = TypeKey::keyed_dynamic::<MyEvent>(format!("session/{}", id));

// The event bus adds keyed variants
ctx.events().on_keyed::<MyEvent>(&ctx, "channel_a", listener)?;
ctx.events().emit_keyed(&ctx, "channel_a", Arc::new(event));
ctx.events().serial_keyed::<MyEvent>(&ctx, "channel_a", &event).await?;
ctx.events().parallel_keyed::<MyEvent>(&ctx, "channel_a", Arc::new(event)).await?;
ctx.events().waterfall_keyed::<MyEvent>(&ctx, "channel_a", &event, terminal).await?;
```

Different names for the same type do not interfere: each has its own hook list and dispatch tail. All four dispatch semantics are inherited automatically.

`on_keyed`, `once_keyed`, and `on_waterfall_keyed` cover every registration shape; the `*_opt` variants with a prepend option are also available.

## Contract fixes (issues fixed since 0.1.0)

These fixes change internal behavior without changing API signatures:

| Fix | 0.1.0 behavior | 0.2.0 behavior |
|---|---|---|
| FAILED stickiness when a dependency disappears | Failed was downgraded to Pending and the error was hidden in the settle channel | Failed remains visible; retry occurs after the dependency recovers |
| `dispose` × `restart` race | Concurrent calls could leave `join` waiting forever | If `dispose` registers first, `restart` is immediately rejected with `InactiveEffect` |
| Late delivery after driver exit | Reported a false `Ok` completion | Completes with the fiber's terminal error |
| Factory `build` panic | Killed the driver task | Becomes `fail_load`; the driver survives |
| `EffectRecord` cleanup panic | Could leave the record stuck in Draining | A panic boundary guarantees transition to Done |

## Migration checklist

- [ ] Set `rutis = "0.2"` in `Cargo.toml`.
- [ ] Search for `Copy`-dependent uses of `TypeKey` and add explicit `.clone()` where needed (usually no changes).
- [ ] For plugins that need hot updates, implement `PluginFactory<C>` instead of holding configuration directly.
- [ ] For multiple event channels, use `on_keyed` / `emit_keyed` instead of `on` / `emit`.
- [ ] Run the existing test suite to check for regressions.

Existing `Plugin` implementations and `ctx.plugin()` calls need no changes. The migration is complete when `cargo test` passes.
