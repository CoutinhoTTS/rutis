# rutis

An idiomatic Rust implementation of the Cordis core paradigm (split out from [min-cordis](https://github.com/eric8810/min-cordis) as a standalone library).

## Five Pillars

1. **Plugin = unit of assembly**: one `apply` provides services / listeners / cleanup
2. **Fiber = lifecycle container**: six-state machine + dependency gating + permanent subtree shutdown + exactly-once cleanup
3. **Service = type-keyed registry + instance subtree visibility + isolate scoping**
4. **Event bus = four dispatch semantics** (emit / parallel / serial / waterfall), with independent and ordered dispatch for instance events
5. **Dependency-driven reloading**: when a provider is unloaded, its consumers are evicted and automatically reloaded

## Usage

```toml
[dependencies]
rutis = "0.6.0"
```

The kernel has zero serde, zero unsafe, and depends only on tokio / tokio-util / thiserror. See the [repository docs](https://github.com/arcships/rutis/tree/main/docs) for design and cross-checking documentation.

First-time users should read the [application design guide](https://github.com/arcships/rutis/blob/main/docs/development-guide.md), then implement following the [development handbook](https://github.com/arcships/rutis/blob/main/docs/development-handbook.md). Companion examples can be run in the repository: `cargo run -p rutis --example development_workflow`.

## 0.6.1

Interfaces added for the plugin control plane (new crate [rutis-loader](https://crates.io/crates/rutis-loader)); no breaking changes: `impl Plugin for Box<dyn Plugin>`, `Ctx::view`, `Ctx::dispose_self`, `FiberView::instance`, `FiberView::set_config`, and the `ServiceChanged` event emitted when a service is registered or removed. See the [0.6.0 → 0.6.1 upgrade notes](../../docs/migration-0.6.0-to-0.6.1.md).

## 0.6

Error, diagnostic, and observation types that will keep growing are now marked `#[non_exhaustive]`, so adding fields or variants is no longer a breaking change; `EventOptions` is now constructed via `EventOptions::default().prepend(true)`. Diagnostics gained per-key emit backlogs (`event_backlogs`). See the [0.5 → 0.6 migration notes](../../docs/migration-0.5-to-0.6.md).

## 0.5 Event Interface

`EventKey<E>` unifies default, named, and instance channels; `EventPattern<E>` supports prefix subscriptions with source keys. `SyncEvent` gained `bail_sync` / `waterfall_sync`, letting synchronous endpoints borrow the current call stack.

See the [0.3 → 0.5 migration notes](../../docs/migration-0.3-to-0.5.md).

## License

MIT (inherited from [Cordis](https://github.com/shigma/cordis) © Shigma).
