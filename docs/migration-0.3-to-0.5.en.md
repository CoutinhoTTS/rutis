# rutis 0.3 to 0.5: Event Keys, Pattern Subscriptions, and Synchronous Calls

Version 0.5.0 is the next planned release for this branch (0.4.0 was never released separately; its contents are included in 0.5.0). It unifies the event interface and adds pattern subscriptions and synchronous decision points. Service `TypeKey`, dependency declarations, and existing service interception keep their interfaces.

## Pass event identity as an argument

Default, named, and instance channels use the same methods:

```rust
let default = EventKey::<Ping>::of();
let named = EventKey::<Ping>::dynamic(format!("room/{id}"));
let private = named.clone().instance(ctx.instance());

bus.on(&ctx, &default, listener)?;
bus.on(&ctx, &named, listener)?;
bus.on(&ctx, &private, listener)?;
bus.emit(&ctx, &named, Arc::new(event))?;
```

| 0.3 | 0.5 |
| --- | --- |
| `on(ctx, listener)` | `on(ctx, &EventKey::of(), listener)` |
| `on_keyed(ctx, name, listener)` | `on(ctx, &EventKey::dynamic(name), listener)` |
| `on_instance(ctx, id, listener)` | `on(ctx, &EventKey::of().instance(id), listener)` |
| `on_opt(ctx, listener, opts)` | `on_opt(ctx, &key, listener, opts)` |
| `once(ctx, listener)` | `once(ctx, &key, listener)` |
| `emit(ctx, event)` | `emit(ctx, &key, event)?` |
| `parallel(ctx, event).await` | `parallel(ctx, &key, event).await` |
| `serial(ctx, event).await` | `serial(ctx, &key, event).await` |
| `waterfall(ctx, event, terminal).await` | `waterfall(ctx, &key, event, terminal).await` |

Rust cannot overload a method by argument count, so the old default methods now take an event key. Existing `*_keyed` / `*_instance` methods remain as deprecated wrappers. Instance waterfall, once, and prepend are now available through the same keyed interface.

`emit` returns whether this call was accepted; instance out-of-scope / closed calls return an error. Success does not mean listeners have finished; listener errors still go to ErrorSink. Non-instance dispatch preserves its historical context behavior; `ctx` must belong to this bus. Internal state notifications, agent notifications, and host-event forwarding also send enqueue failures to ErrorSink.

`EventKey::named` can define a const static key without allocation. Static and dynamic keys with the same name are equal; a default key differs from every named key. Identity still includes type and instance ID. Names are freely constructed without spelling validation, so business APIs should export key constants or constructors.

`EventOptions` adds a `once` field. Existing struct literals must add `once: false` or `..Default::default()`. For example, preserve prepend with `EventOptions { prepend: true, ..Default::default() }`; run once with `EventOptions { once: true, ..Default::default() }`. `on_waterfall_opt` supports once as well. A once listener is claimed when dispatch takes its snapshot; an earlier listener may stop processing so the once listener is claimed but never invoked. Its guarantee is at most once.

## Subscribe to a group of dynamic names

```rust
bus.on_pattern(&ctx, EventPattern::<RoomEvent>::prefix("room/"), listener)?;
bus.on_pattern(&ctx, EventPattern::any_prefix(["room/", "room/special"]), listener)?;
```

`PatternListener<E>` receives the matching `EventKey<E>`, a borrowed payload, and the sender's `Ctx`. The key is passed by value and reuses the dynamic name's `Arc`; the listener can retain it after the callback returns.

Patterns match only explicitly named, non-instance channels of the same event type. An empty prefix matches all such channels. Patterns cannot bypass instance isolation. Overlapping prefixes in one registration select it only once; registering the same callback twice creates two subscriptions.

Exact and pattern listeners share registration order; prepend places a listener before all matching listeners. `on_pattern_opt` / `on_waterfall_pattern_opt` support prepend and once. A pattern once listener shares one claim across every matching name, so multiple threads cannot claim it independently.

The emit tail is still built per complete event key: events with the same name remain ordered, while different names can call the same pattern listener concurrently. There is no total order across names. Unloading a pattern removes the subscription first, then waits for accepted callbacks; no new pattern callbacks are admitted afterward. Existing snapshots and unload behavior remain for non-instance exact async listeners.

A pattern callback may start its own unload and return, but must not await completion of an unload that includes itself. Use the `once` option when it should run only once.

`bus.subscriptions()` returns current exact/pattern registrations, owner, callback kind, and prefix groups. Pattern `selected` / `invoked` counters distinguish snapshot selection from actual calls; exact registrations use `None` to avoid atomic-statistic overhead on normal dispatch. It does not retain every historical dynamic name or business payload.

## Use synchronous calls when the result is needed immediately

An event type also implements `SyncEvent` and registers ordinary function listeners:

```rust
impl SyncEvent for BeforeSave {}
bus.on_sync(&ctx, &key, listener)?;
let decision = bus.bail_sync(&ctx, &key, &event)?;

bus.on_waterfall_sync(&ctx, &key, middleware)?;
let candidate = bus.waterfall_sync(&ctx, &key, &event, |_, event| Ok(event.candidate))?;
// Validate candidate, then perform the actual write.
```

`bail` returns the first `Some` in registration / prepend order. `waterfall`'s `SyncNext::call(self)` wraps the downstream result without replacing the input; not calling `next` intercepts the chain. A continuation cannot be called twice or escape as `'static`. Async and sync registrations use separate tables and do not call each other. `SyncEvent` is an additional capability; it does not prohibit async APIs for that event type.

The synchronous terminal is a local `FnOnce` and may borrow stack variables or a `MutexGuard`; it need not be Send or `'static`. Listeners still require Send + Sync + `'static`. The bus holds no framework lock while user callbacks run, but a callback must not reacquire the same business mutex already held by its caller.

Runnable example: [`sync_decision.rs`](../crates/rutis/examples/sync_decision.rs). The caller locks and reads the old value, the terminal creates a candidate, a listener rewrites the result, then the caller validates and commits inside the same critical section: `cargo run -p rutis --example sync_decision`.

Ordinary errors are returned unchanged. Listener / terminal panics are caught, reported once to ErrorSink, and returned as `CordisError::SyncEventPanicked`; a panic in ErrorSink is also isolated. Dispatch-attempt observers keep their existing contract: report panic and continue. `DispatchMode` adds `BailSync` and `WaterfallSync` variants.

If business code exhaustively matches `DispatchMode` or `CordisError`, add the new arms. Synchronous dispatch is a local Rust call; it does not add a remote dispatch mode to the bridge protocol.

Same-thread synchronous reentry on the same bus and complete key returns `ReentrantEvent`, including reentry from observers, listeners, terminals, and sinks. Different keys, instances, or buses can nest. Concurrent calls from different threads are not made globally serial.

Synchronous exact and pattern registrations support `EventOptions`. Pattern methods are `on_sync_pattern` and `on_waterfall_sync_pattern`; callbacks also receive the actual matching key.

## Lifecycle and existing interception

Synchronous snapshot and in-flight registration happen in one admission critical section. Shutdown, ordinary unload, restart, and failed-load rollback all wait for admitted calls to exit, including synchronous terminals with no listeners. Stale-generation / inactive / closed synchronous contexts are rejected. A callback may initiate its own shutdown and return, but must not block waiting for the unload completion that includes its own in-flight count.

Waiting uses owner-level counts and may include other callbacks for the same owner. Protection covers the synchronous dispatch stack, not business commits afterward. Service updates continue to recheck provider, generation, and binding identity.

Existing service interception passes each rewritten value to the next interceptor in registration order. In event waterfall, downstream still receives the original input, and results return in nested callback order. Therefore service interception keeps its existing implementation.

## dylib SDK and benchmark

The rutis breaking API version is 0.5.0; the shared SDK version is 0.3.0. SDK identity changes with the version and locked dependencies, so host and plugins must be rebuilt together. A new artifact cannot replace an old SDK file. Existing pre-execution and pre-dlopen checks remain unchanged.

```sh
cargo bench -p rutis --bench events
```

The benchmark reports synchronous empty / 1 / 8 / 64 listeners, empty instance channel, plus async exact dispatch and 0 / 1 / 8 / 64 / 1024 prefixes. A synchronous empty path has no future or spawn but still checks context, blocks reentry, and records the in-flight call. Results are local samples, not fixed latency guarantees across machines.

See [performance samples](performance-event-dispatch-2026-09-27.en.md) for machine results, method, and exact-dispatch comparison.
