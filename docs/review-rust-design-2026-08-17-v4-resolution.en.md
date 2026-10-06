# Resolution for Revising `design-rust-port.md` v4 → v5 (v2)

> First version: 2026-08-17; v2 merged Codex and ZCode's second-round reviews on 2026-08-17.
> Review inputs: [review-rust-design-2026-08-17-v4.md](review-rust-design-2026-08-17-v4.en.md) (Dim), [review-rust-design-2026-08-17-v4-codex.md](review-rust-design-2026-08-17-v4-codex.en.md), [review-rust-design-2026-08-17-v4-zcode.md](review-rust-design-2026-08-17-v4-zcode.en.md), and second-round Codex/ZCode feedback on the v1 resolutions.
> Revision principle: **minimize redundancy during architecture design**—do not split traits when an associated type suffices, do not add multiple structs where one Arc suffices, and do not add mechanisms to support promises that can be removed.
> Verdict: downgrade v4 from “final” to **“draft”**. Freeze only after the following revisions are incorporated into v5.

## 1. Blockers (review consensus; accept all)

1. **Listener type is undefined and self-contradictory** (all three reviews): `ListenerResult` / `Value<E>` are never defined; “all-async BoxFuture” (§0) conflicts with “synchronous bail short-circuit / synchronous emit” (§1, D4).
2. **Waterfall signature cannot compile and its registration surface is missing** (all three): `async fn -> BoxFuture` returns a future twice; the `on()` listener shape has no `next` parameter, so waterfall listeners cannot be registered.
3. **Effect cleanup cannot report errors** (Codex and ZCode): both Disposer variants return `()`, directly contradicting §5's contracts for a single error returned as-is, multiple errors aggregated, and repeated dispose joining the same `Arc<E>`.
4. **Config disappears without explanation** (all three): `Plugin::apply` / `Ctx::plugin` / `FiberView::update` accept no config, but D12 is entirely about validate-before-store, a `Validation` variant exists, and §4 claims both update branches remain.

## 2. Event system (D16–D17)

### D16: Make the event system fully async, remove bail, use four dispatch modes

- Use one listener shape and a helper trait to resolve lifetimes. Do not use the alias route: in an alias, the return-position `BoxFuture<'_>` has no input from which to infer `'_`, producing E0106.

```rust
pub trait Event: Send + Sync + 'static {
    const NAME: &'static str;        // diagnostics only; not identity or dispatch
    type Value: Send + 'static;      // serial short-circuit value; also resolves open question 2
}

trait Listener<E: Event>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, e: &'a E)
        -> BoxFuture<'a, Result<Option<E::Value>, CordisError>>;
}
// blanket impl for Fn(&Ctx, &E) -> BoxFuture<'_, ...>
```

- **Remove bail but retain serial short-circuit semantics.** Use four dispatch modes: emit (fire-and-forget), parallel (run all concurrently and aggregate errors), serial (in order until short-circuit), waterfall (middleware continuation with veto). TypeScript bail and serial already share short-circuit semantics; their only difference is synchronicity. The only synchronous bail consumer was `internal/listener`, removed with the internal surface. Update §§1/5 and D4 to say “four dispatch modes.”
- **Fire-and-forget (`emit`) payloads use `Arc<E>`**, so spawned tasks own their data rather than borrowing the caller's stack.
- **Parallel/serial JoinSet tasks own a cloned Ctx + `Arc<E>`**; convert borrows to owned values before spawning so tasks satisfy `'static`.
- Do not split `Event` / `QueryEvent` / `WaterfallEvent` into three traits. Dispatch differences belong in methods, not the type system.

### D17: Add a separate waterfall registration surface

- Add `on_waterfall::<E>()` with this listener shape:

```rust
trait WaterfallListener<E: Event>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, e: &'a E, next: Next<'a>)
        -> BoxFuture<'a, Result<E::Value, CordisError>>;
}
```

- `Next<'a>` is roughly `&'a mut dyn FnMut(&'a Ctx, E) -> BoxFuture<'a, Result<E::Value, CordisError>>`. **Before v5 freeze:** the public waterfall signature must pass `cargo check` in the minimal skeleton and support listener registration; a borrowed `Next` is sufficient if it compiles. **M1 may refine** the exact `Next` shape (borrowed vs owned). Timing is explicit: compilability is a freeze gate; details of shape are up to M1 implementation.
- Fix `waterfall` to a regular `fn -> BoxFuture` (remove the double future). `next` is the terminal fallback supplied by the caller, not a peer argument to the event.

## 3. Effects and errors (D18, D25, D30)

### D18: Effect cleanup returns errors

```rust
pub enum Effect {
    Done,
    Disposer(Box<dyn FnOnce() -> Result<(), CordisError> + Send>),
    AsyncDisposer(Box<dyn FnOnce() -> BoxFuture<'static, Result<(), CordisError>> + Send>),
    Many(Vec<Effect>),
}
```

Cleanup closures **capture an owned Ctx** (clone the `Arc<CtxInner>`); do not pass `&Ctx` as an argument. This removes the compile-time trap of capturing a borrow in a `'static` future.

### D25 + second-round correction: separate error identity from error wrapping

- **Identity belongs to `TransitionTask`:** cache transition result as `Arc<CordisError>`, so multiple observers join the same Arc. The `exactly_once_same_error` test asserts this.
- **Wrapping belongs to `CordisError` variants:** retain `PluginFailed` with `#[source] Box<dyn Error + Send + Sync>` for foreign non-Cordis errors from apply. Propagate a `CordisError` returned by apply directly; do not wrap it recursively. `PluginFailed` must not recursively contain `CordisError`.
- Uniform fallback for non-Cordis errors: wrap disposer/listener panics or foreign errors as `PluginFailed` at the task boundary, then aggregate or route to ErrorSink.

### D30 (new): Specify panic boundaries for tasks

- Fire-and-forget spawn **must observe its JoinHandle** (collect in a JoinSet or forward errors from inside the task). Route panic via `JoinError` to ErrorSink; dropping the handle directly loses panics and is prohibited.
- Catch panic while polling async disposers at the task boundary (`catch_unwind` or convert JoinSet's JoinError) into `PluginFailed`, then aggregate through the same path as Err.
- Listener panics and returned errors both enter ErrorSink, matching TypeScript's routing of listener throws to its logger.

## 4. Plugin and config (D19)

### D19: Bake config into the instance; erase types during construction

- Choose the **minimal route** (Codex proposal): config is fixed when the plugin is constructed and is not an associated type on `Plugin`.

```rust
// Plugin has no type Config; each concrete plugin stores its own config
pub trait Plugin: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn injects(&self) -> &[TypeKey] { &[] }
    fn validate(&self) -> Result<(), CordisError> { Ok(()) }  // validate its owned config
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>>;
}

// Still accepts by value; internally stores an Arc
fn plugin(&self, p: impl Plugin) -> FiberView;
```

- The concrete type (e.g. `MyPlugin { config: MyConfig }`) receives config in `new(config)` and `validate` checks its owned copy. Registry stores only `Arc<dyn Plugin>` and **does not use `Any` erasure**.
- Defer `FiberView::update` to M4, when `PluginFactory<Config>`, config erasure, and the update transaction are designed. Change §4's “both update branches retained” to “semantics retained; implementation point is M4.”

## 5. Concurrency and indexes (D20–D22 + second-round corrections)

### D20: Exactly once uses one Arc under a lock; watch is observation only

- Store `Option<Arc<TransitionTask>>` under the transition mutex. Dispose/restart in the same generation join that Arc; a new generation (generation field inside task) gets a new task.
- watch is only for external state observation and diagnostics, not exactly-once. **Do not create** separate `FiberSnapshot` / `Transition` structs; embed generation in `TransitionTask` and use the watch payload.
- State the late-subscriber protocol: first `borrow()` to check whether the current snapshot is terminal, then loop on `changed().await`.

### D21: Key reverse dependency index by (PluginId, generation, ServiceKey)

- When a consumer activates, record the actual binding it used: `(PluginId, generation, ServiceKey)`; reverse-index with the same triple. **Second-round correction:** `(PluginId, generation)` alone is too broad—a plugin can provide 0..n services, and early disposal of one service must not evict consumers of its other services. `ServiceKey` must be part of the key.
- On provider unload, evict by exact triple, naturally isolating isolate scopes.
- **Do not introduce** separate `ServiceKey` / `BindingKey` / `ProviderId` types: `ServiceKey = TypeKey` (TypeId + qualifier); `ProviderId = (PluginId, generation)` tuple.
- Add ServiceKey to `isolate`: `isolate(&self, key: ServiceKey, label: &str) -> Ctx`, matching TypeScript granularity ([`context.ts:123-127`](../src/context.ts#L123)). Repeated calls with the same label share scope (TypeScript semantics); child scope falls back to parent scope.

### D22: Downgrade dependency-cycle detection

- Remove the promise of general cycle detection in v1. Missing dependencies remain Pending (valid state; no error); remove the word “cycle” from `CordisError::InjectUnsatisfied`.
- M4 may add an optional `provides()` manifest for static diagnostics; not in v1.

### D24 correction: specify status-event publication and ordering

- Publish `FiberStatusChanged` as follows: enqueue FIFO under transition lock, dispatch outside the lock. Never call user code while holding the lock; this matches D5 (“callbacks execute outside locks”) and lets listeners reenter the registry (`ctx.plugin()`) without deadlock.
- **Specify ordering:** guarantee **event commit order** (FIFO enqueue; watch payload carries generation as sequence); **do not guarantee** async listener **completion order** (spawn scheduling order is not contractual; D9). Tests assert commit order only, never callback completion order.
- Do not introduce another ordering mechanism or fields beyond the publication queue.

## 6. Missing decisions to add (D23, D27–D29)

### D23: Add `Ctx::effect()` to the draft

Pillar 1's “return cleanup,” the `effect_yields_disposer` test, and listener cleanup on fiber unload all depend on it. Explicitly list TypeScript's incremental async-iterable effect as an intentional deviation.

### D27 (new): Expose CancellationToken to plugins

- Every fiber generation has its own child token, exposed through `ctx.cancelled().await` (or `ctx.cancellation_token()`).
- State the five unload steps: 1) mark transition Unloading → 2) cancel current generation → 3) wait for in-progress apply to exit → 4) run EffectRecord cleanup strictly → 5) publish final state.
- Document cooperative-cancellation limits: if a plugin never observes the token and never returns, dispose waits indefinitely. The first release does not claim it can forcibly terminate an arbitrary Rust Future.

### D28 (new): State resource ownership rules

- Listeners, services, and child plugins created through a Fiber's Ctx **automatically belong to that Fiber**. Their returned `Disposer` means early release only; it does not need to and should not be put back into `Effect`.
- `Effect` is only for external resources not registered through Ctx.
- A Ctx created by isolate retains ownership of the original Fiber.

### D29 (new): Decide event visibility across isolates

- TypeScript filters events using `Context.filter` + thisArg ([`events.ts:170-179`](../src/events.ts#L170)). v4 decision: **event dispatch is not filtered across isolates**; isolate scopes only service registry. Event bus is globally unique and listeners receive global events. Users needing scoped events can create a child bus with isolate themselves (do not block this). Add this to intentional deviations.

## 7. §4 deviation list (new; decide each item explicitly)

| TypeScript capability | Decision | Milestone |
|---|---|---|
| Synchronous bail short-circuit | Remove (D16); semantics folded into serial | — |
| `internal/*` event surface | Intentionally not preserved | — |
| `check()` dependency predicate ([`fiber.ts:689-701`](../src/fiber.ts#L689)) | Retain | M2 |
| `on` prepend option / listener-order control | Retain (affects serial/waterfall results) | M1 |
| `once` | Retain | M1 |
| Service config interception (`intercept`, [`service.ts:86-102`](../src/service.ts#L86)) | Intentionally not preserved (removed with Proxy surface) | — |
| Service lookup along fiber parent chain ([`reflect.ts:154-166`](../src/reflect.ts#L154)) | Retain, implement via Ctx parent traversal | M2 |
| `Context.filter` event filtering | Intentionally not preserved (D29) | — |
| Two-branch `update` | Retain semantics | M4 |
| Incremental async-iterable effect | Intentionally not preserved | — |
| “Unload = remove from registry; consumer's cached `Arc<T>` keeps instance alive” | State explicitly; basis for reload tests | M2 |
| Eviction drain order (consumers concurrent, provider last) | State exact contract | M2 |
| Reentrant provide/effect during unload reports `INACTIVE_EFFECT` | Retain ([`fiber.ts:434-436`](../src/fiber.ts#L434)) | M2 |

## 8. Fix document consistency (cleanup)

1. thiserror is a normal dependency (§2 line 37 directly derives it); add it to dependency list. Fix typo “do not add async-trait (thiserror optional)” to “do not add async-trait.”
2. Move serde/serde_json out of core crate and into the agent example crate.
3. Qualify “do not add futures-util”: parallel uses `tokio::task::JoinSet`.
4. Explain `provide<T>(value: T)` / `provide_as<T>(value: Arc<T>)` Arc asymmetry: former is a convenient value-semantic entry point; latter supports trait objects/shared instances. Make failure handling consistently `Result` (change `on`'s bare `Disposer` to `Result<Disposer>`).
5. Explicitly make `PluginId` a `FiberView` field (`FiberView.id: PluginId`).
6. Define minimum ErrorSink API: `Arc<dyn Fn(CordisError) + Send + Sync>`, default implementation sends to logger; include in M1 deliverables.
7. “v4 retains everything” in §4 is too strong; mark each item against the §7 deviation list.
8. Reconcile §5 acceptance counts: minimum is 5 pillars × (4 + 2) = 30; say “≥30, target 40.” Add serial to events tests; add a separate pillar 1 (0..n assembly) group; add eviction order, concurrent dispose, and cross-generation race tests to lifecycle/reload group.
9. Remove two M1 test promises that require fiber (`cancel_wakes_awaiters`, `listener_unloads_with_fiber`) and move them to M2 acceptance.
10. Open question 2 (`Value<E>` shape) is decided by D16; remove it from §7 and renumber remaining items.

## 9. Freeze gates for v5

- [ ] All four event modes have complete, compilable listener signatures (helper-trait route; no `ListenerResult` placeholder).
- [ ] Fire-and-forget holds no non-`'static` borrow (payload `Arc<E>`); spawned task owns Ctx + `Arc<E>`.
- [ ] Parallel/serial JoinSet tasks satisfy `'static`.
- [ ] Both Effect Disposer variants return `Result<(), CordisError>` and closures capture owned Ctx.
- [ ] Concurrent dispose joins the same `Arc<TransitionTask>` under lock; watch is observation only.
- [ ] Reverse dependency edge includes `(PluginId, generation, ServiceKey)`.
- [ ] `isolate()` includes ServiceKey and documents that same labels share scope.
- [ ] `InjectUnsatisfied` makes no cycle-detection promise.
- [ ] `Plugin` has optional `validate` (checks owned config), and `FiberView::update` is marked M4.
- [ ] `Ctx::effect()`, `ctx.cancelled()`, ErrorSink definition, and panic task boundaries are all in the draft.
- [ ] Status events say “FIFO enqueue under lock, dispatch outside lock”; ordering says “commit order guaranteed, listener completion order not guaranteed”; tests match.
- [ ] Public waterfall signature passes `cargo check` in the minimal skeleton and supports listener registration (v5 freeze gate); exact `Next` shape can be finalized in M1.
- [ ] `CordisError` layering is explicit: identity belongs to `TransitionTask`, wrapping to `PluginFailed` (`#[source] Box<dyn Error>`), with no recursive wrapping.
- [ ] Resource ownership (D28), event/isolate decision (D29), and complete §4 deviation list are documented.
- [ ] Serial/bail distinction is resolved (remove bail; four dispatch modes).
- [ ] Public API skeleton passes `cargo check` in a minimal crate containing only traits/enums/type aliases + empty impls; specifically verify helper-trait lifetimes, `Arc<dyn Plugin>` erasure, and JoinSet `'static`.

After all gates pass, v5 can return from “draft” to “final” and M1 can be approved.
