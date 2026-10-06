# Core features

[中文](core-features.md) · [Development handbook](development-handbook.en.md) · [API docs](https://docs.rs/rutis)

What the `rutis` core offers beyond plugins, fibers, services and events, and where its boundaries are. To get started, read the quick start in the [README](../README.md) and the [application design guide](development-guide.en.md) first.

## Config hot update

change config at runtime, reusing the state machine's exactly-once cleanup; affected consumers follow automatically:

```rust
struct MyFactory;
impl PluginFactory<MyConfig> for MyFactory {
    fn build(&self, cfg: &MyConfig) -> Result<Box<dyn Plugin>, CordisError> { /* ... */ }
}

let view = ctx.plugin_with(MyFactory, cfg_v1);
view.update(cfg_v2).await?;   // dry-run failure leaves everything untouched; success unloads + reloads
```

## Dynamic event names

events whose names are only known at runtime (host events, script-registered channels): typed events + dynamic qualifiers inherit all four dispatch semantics and lifecycle cleanup for free:

```rust
let key = rutis::EventKey::<RoomEvent>::dynamic(name);
ctx.events().on(&ctx, &key, listener)?;
ctx.events().emit(&ctx, &key, Arc::new(event))?;
```

## Patterns and synchronous decisions

`EventPattern::prefix("room/")` subscribes to dynamic channels and delivers the actual matching key. Events implementing `SyncEvent` can use `bail_sync` / `waterfall_sync`; the terminal can borrow the caller's local variables or MutexGuard. See the [0.3 → 0.5 migration guide](migration-0.3-to-0.5.en.md).

## Observing dispatch

`ctx.events().observe_dispatch(&ctx, observer)` calls the observer synchronously before business listeners are selected, even when there are none. An observer sees only dispatches within the subtree of the fiber that registered it and is cleaned up with that fiber; a `DispatchAttempt` carries the full event key, the dispatch mode, the emitter and the borrowed event. It records attempts; it cannot veto a dispatch.

## Cleanup tree

`ctx.effect_named("cache watcher", || effect)` names a cleanup entry; `fiber.effects()` returns the fiber's read-only cleanup tree. A composite `Effect::Many` keeps its real nesting, an entry being cleaned up shows as `Draining`, and it leaves the tree once done. Plugin loads, service registrations and event listeners are labelled automatically.

## Intercepting service reads and writes

`ctx.intercept_require_as::<T>(key, hook)` applies only to strict reads that passed the declaration, instance and readiness checks; an explicit `get_as` still locates the service directly. `ctx.intercept_set_as::<T>(key, hook)` handles updates to writable services. `provide_mut_as` returns a `ServiceWriter<T>` bound to the registration's generation, which the provider updates with `writer.set(&ctx, Arc::new(value))`; earlier `Arc<T>` snapshots keep their old value, and a write does not reload consumers. An interceptor can substitute a value of the same type or refuse the operation, so register trusted code only.

## API boundaries

`require/require_as` are strict reads corresponding to Cordis's ordinary plugin service access. They check `injects()` along the fiber ancestry and distinguish undeclared, unavailable, out-of-scope, and inactive reads, retaining the call site. If a read is both out of scope and inactive, the instance boundary takes precedence; registration and instance dispatch define their own error order. `get/get_as` correspond to Cordis's explicit `ctx.get()` locator: they return `Option` without enforcing declarations. A service is normally hidden while its provider is inactive or the reader is unloading, except that the provider's subtree can read its own service during cleanup. Instance keys also have subtree visibility checks. The `Ctx` passed to `on` owns a listener; the callback's `Ctx` belongs to the emitter. Capture the registration `Ctx` when the callback must register resources for its own plugin. See the compiling [listener ownership example](../crates/rutis/examples/listener_ctx_ownership.rs).

## Dependency diagnostics

`ctx.diagnostics()` lists live fibers with their identity, state, declared and bound dependencies, and service bindings. `injects[].status` distinguishes missing, out-of-scope instance, provider not ready, being removed, rejected by `check`, and `check` panicked; `TypeKey::describe()` gives the type name, qualifier and instance id. A read uses only registered metadata and the latest gate result: it calls no plugin or `check` and triggers no lifecycle transition. It reads fibers and bindings one by one and is **not an atomic snapshot of the tree**: under concurrent lifecycle changes, the states, dependencies and bindings in one result may come from different moments. A `check` status may lag at the last gate result; after `refresh()`, wait for the affected fibers to settle and read again. Differences from Cordis in dispatch observation, cleanup trees and service interception are covered in the [design draft](design-cordis-observation.en.md).

## Errors and panics

Synchronous and asynchronous `apply` panics become plugin errors; a `check()` panic leaves a dependency unready; a `waterfall` callback panic propagates to its caller. `settle` is a FIFO barrier for one fiber, and Pending can be a stable result. Root `dispose()` remains restartable, while `shutdown()` closes it permanently; dropping a waiting future does not stop cleanup already in progress. `update(config)` reassembles a plugin without replacing process code. An early `Disposer::dispose()` failure returns to its caller without notifying the error sink; `Ctx::take_cleanup_errors()` consumes and releases these retained errors. Unconsumed errors join a terminal unload result or reach the error sink on reload.

## Relation to Cordis

rutis is an idiomatic Rust implementation of the [Cordis](https://github.com/shigma/cordis) paradigm, not a translation: all 96 original specs reviewed line by line, the 58 language-agnostic invariants locked by automated parity tests; every other difference is explicitly declared (decision table + non-port list + audit record). Known deliberate strengthenings: cross-effect cleanup is strictly serial LIFO (cordis runs concurrently), per-key emit ordering is rebuilt explicitly.
