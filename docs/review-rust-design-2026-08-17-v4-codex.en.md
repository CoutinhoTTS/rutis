# Review of the min-cordis Rust Port Design v4

> Reviewed document: [design-rust-port.md](design-rust-port.en.md)
> Review date: 2026-08-17
> Scope: core model, public API, async lifecycle, dependency reload, and acceptance plan
> Conclusion: **direction approved; design freeze not yet approved**

## One-sentence conclusion

Moving v4 from a “one-to-one port” to a “paradigm port” is the right choice, and the five core pillars are mostly well chosen. But EventBus, Effect, Fiber transitions, and isolate dependency indexing do not yet form closed contracts that can be implemented directly.

Change current status from “final” to “RFC / decisions pending,” resolve four blockers, then start M1 and M2. Otherwise core APIs will likely need rewriting during implementation.

## Overall assessment

| Dimension | Assessment | Notes |
|---|---|---|
| Paradigm boundary | Good | Five pillars are clear and non-goals are bounded; no continued burden of JS-specific semantics |
| Rust direction | Mostly right | Typed events, explicit PluginId, CancellationToken, and not holding locks across await are reasonable |
| API completeness | Insufficient | Multiple public types are placeholders and some signatures conflict |
| Concurrency correctness | Insufficient | Relationship between `watch`, generation, duplicate dispose, and rapid reload is unresolved |
| Scope correctness | Insufficient | Reverse dependency index omits isolate identity |
| Testability | Fair | Contract-test direction is good, but key race tests that determine architecture are missing |

Design choices worth keeping:

- Use paradigm contracts rather than TS's 96 tests as acceptance standard.
- Explicit `PluginId`; do not depend on closure addresses.
- `CancellationToken` as unified cooperative cancellation.
- Only short critical sections under transition mutex; never hold lock across await.
- Keep provider self-access while cleaning it up.
- Preserve cleanup errors: return one error unchanged and aggregate multiple errors.

The issues below do not ask to restore TS APIs; the contracts v4 itself declares cannot yet be implemented using its draft API.

## Blocking issue 1: EventBus type model is not closed

### Current problem

The draft offers only one listener-registration method:

```rust
pub fn on<E: Event>(
    &self,
    ctx: &Ctx,
    f: impl Fn(&Ctx, &E) -> ListenerResult + Send + Sync + 'static,
) -> Disposer;
```

But the five dispatch modes require three distinct capabilities:

- `emit`: call synchronously; run async results in background.
- `parallel` / `serial`: await async results.
- `bail`: synchronously obtain result to short-circuit.
- `waterfall`: listener receives `next` and returns final chain value.

`ListenerResult` and `Value<E>` are undefined, and there is no explanation of how event types, listener return values, and dispatch modes are related.

Two further issues:

1. `async fn waterfall(...) -> BoxFuture<...>` creates two Future layers. Use ordinary `fn -> BoxFuture` or `async fn -> Result`, not both.
2. `emit(ctx: &Ctx, e: &E)` cannot pass an async listener to a Tokio background task because that Future must own `'static` data and cannot borrow `ctx` and `e` from the caller's stack. Tokio's [`spawn`](https://docs.rs/tokio/latest/tokio/task/fn.spawn.html) explicitly requires `Send + 'static` Future.

### Impact

This is a foundational M1 interface. If implemented as drafted, reaching `bail`, async `emit`, or waterfall type erasure will require redesigning Hook table and public API.

### Recommended decision

Choose one of two paths first:

#### Path A: Keep synchronous bail

Separate listener types and registration APIs, e.g.:

- `on_sync` for synchronous `emit` / `bail`.
- `on_async` for `parallel` / `serial`.
- `on_waterfall` where listener explicitly receives `Next`.

This has clearest semantics but increases API surface.

#### Path B: Make entire event system async

All listeners return `BoxFuture`; `bail` becomes async short-circuit too. This is simplest, but gives up the “synchronous bail” promise among five modes.

Either way:

- Add associated return type to `Event`, or split into `Event` / `QueryEvent` / `WaterfallEvent`.
- Fire-and-forget dispatch owns payload via `Arc<E>` or `E`; do not give borrowed data to background task.
- Pass `Next` to waterfall listener, not as an ordinary parameter of EventBus dispatch method.
- Before implementation, write a complete API skeleton that passes `cargo check`.

### Acceptance criteria

- Listener signatures are explicit for each of five modes.
- No undefined `ListenerResult` / `Value<E>`.
- Waterfall has one Future layer.
- Async emit captures no non-`'static` borrow.
- Event and return value can be safely recovered by `TypeId` after type erasure.

## Blocking issue 2: Effect API cannot express cleanup failure

### Current problem

Draft Effect:

```rust
pub enum Effect {
    Done,
    Disposer(Box<dyn FnOnce(&Ctx) + Send>),
    AsyncDisposer(Box<dyn FnOnce(&Ctx) -> BoxFuture<'static, ()> + Send>),
    Many(Vec<Effect>),
}
```

Both sync and async disposers return only `()`, so the API cannot implement declared contract tests:

- Return one cleanup error unchanged.
- Aggregate multiple cleanup errors.
- Do not flatten user-created Aggregate.
- Repeated dispose observes same `Arc<CordisError>`.

Also, `FnOnce(&Ctx) -> BoxFuture<'static, ()>` may read Ctx only before constructing Future; returned Future cannot continue borrowing it across await. This is insufficient for common async cleanup.

Current TS version separates execution result from cleanup result and provides strict LIFO, execute-all, aggregation, and repeat-call joining; see [`src/fiber.ts`](../src/fiber.ts#L438-L633). v4 need not copy implementation, but must preserve the invariants it already promises.

### Recommended change

Let disposer capture owned context it needs and uniformly return `Result`:

```rust
pub enum Effect {
    Done,
    Disposer(Box<dyn FnOnce() -> Result<(), CordisError> + Send>),
    AsyncDisposer(Box<dyn FnOnce() -> BoxFuture<'static, Result<(), CordisError>> + Send>),
    Many(Vec<Effect>),
}
```

If cleanup needs Ctx, closure can capture owned/clonable Ctx instead of borrowing caller's `&Ctx`.

Also require internal `EffectRecord`, rather than relying on `FnOnce` alone for exactly-once:

- First caller starts cleanup.
- Later callers join same cleanup task.
- `Many` executes strict reverse order, serially.
- A cleanup failure does not prevent remaining cleanup.
- Return one error unchanged; wrap multiple errors in `Aggregate`.
- Cache cleanup result as shared terminal state.

### Acceptance criteria

- Sync and async cleanup both return errors.
- Concurrent calls to one disposer execute it once.
- All callers receive same terminal result.
- Tests define `Many` execution order and aggregation.
- Async cleanup does not depend on dangling `&Ctx` borrow.

## Blocking issue 3: `watch<FiberState>` alone cannot join transitions

### Current problem

Design uses `tokio::sync::watch` for all of:

- Public Fiber-state observation.
- Waiting for one load/unload to complete.
- Supporting late subscribers.
- Ensuring repeated dispose joins same result.
- Sharing same error identity.

These responsibilities are not identical.

Tokio `watch` retains only latest value; a receiver can miss intermediate state. A new subscription considers current value already seen, so direct `changed()` waits for next change instead of immediately returning current terminal state. Correct code first checks current snapshot then loops waiting. See change-notification guidance in [`tokio::sync::watch`](https://docs.rs/tokio/latest/tokio/sync/watch/).

More importantly, `watch` only signals “state changed”; it does not prove:

- Two dispose calls await same generation's unload.
- Restart cannot overwrite an older caller's error.
- During fast `Loading → Active → Unloading`, a caller does not mistake next generation's state for prior generation's completion.
- Exactly one cleanup task was executed.

### Recommended change

Separate “observe state” from “operation completion”:

```rust
struct FiberSnapshot {
    generation: u64,
    state: FiberState,
    last_error: Option<Arc<CordisError>>,
}

struct Transition {
    generation: u64,
    operation: Option<Arc<TransitionTask>>,
    token: TransitionToken,
}
```

- `watch<FiberSnapshot>` only observes state and diagnostics.
- `TransitionTask` represents the unique load/unload operation for a generation and stores a shareable completion result.
- Transition mutex decides whether to create a task or join existing one.
- Waiter records target generation and returns only when that generation's operation completes.

Keep `watch`, but do not describe it as “naturally exactly-once.” Exactly-once comes from storing and reusing the same `TransitionTask` under lock.

### Acceptance criteria

- Concurrent dispose creates only one unload task.
- Each caller in dispose/restart race waits for correct generation.
- Rapid state changes cause neither false completion nor indefinite wait.
- Late subscriber checks current snapshot before awaiting change.
- Prior-generation error is not overwritten by next-generation state.

## Blocking issue 4: reverse dependency index omits isolate

### Current problem

Registry allows same service type in separate isolates, but D14 reverse index is:

```rust
HashMap<TypeKey, Vec<PluginId>>
```

If two isolates provide same `TypeKey`, unloading provider in scope A cannot distinguish A and B consumers from this index alone, and may incorrectly evict Fiber in scope B. This breaks core pillar “same type can coexist across scopes.”

### Recommended change

At minimum distinguish:

```rust
struct ServiceKey {
    type_id: TypeId,
    qualifier: Option<KeyId>,
}

struct BindingKey {
    scope_id: ScopeId,
    service: ServiceKey,
}

struct ProviderId {
    plugin_id: PluginId,
    generation: u64,
}
```

Build reverse index by `BindingKey` or actual resolved `ProviderId`, not only `TypeKey`.

Prefer recording actual provider for each active consumer:

```text
consumer generation
    └── dependency BindingKey
            └── resolved ProviderId
```

Then provider unload, same-type replacement, and rapid re-registration can precisely determine whether consumer is stale.

Document isolate lookup rules too:

- Does child scope fall back to parent?
- Does isolate isolate whole service container or only specified ServiceKey?
- Do same labels intentionally merge scopes?
- Who owns ScopeId lifetime?

### Acceptance criteria

- Unloading provider in scope A does not evict consumer in B.
- Keyed multi-instance services in same scope do not cross-bind.
- After rapid provider replacement, old generation unload does not evict consumer bound to new provider.
- Isolate lookup and parent fallback have independent tests.

## Important issue 5: current model cannot reliably detect dependency cycles

### Current problem

`Plugin` declares only `injects()`, not `provides()`. Services register dynamically while `apply()` runs.

Consider:

```text
Plugin A: depends on B, provides A after startup
Plugin B: depends on A, provides B after startup
```

Both stay Pending, so `apply()` never runs; framework cannot see the services they intend to provide and cannot prove cycle exists.

This conflicts with `InjectUnsatisfied` promising “dependency cycle or unsatisfied.” But temporarily unsatisfied dependency is a valid Pending state and should not immediately be an error.

### Recommended decision

Keep dynamic plugins in first release and narrow promise:

- Missing dependency remains Pending; no error.
- Remove general cycle detection promise for first release.
- Split `InjectUnsatisfied` into clearer errors or remove for first release.
- M4 may add optional `provides()` manifest for static diagnostics and startup cycle detection.

If cycle detection is a hard first-release requirement, require complete `provides()` declaration before execution; services can no longer be entirely dynamic.

## Important issue 6: public promises for config/update conflict

### Current problem

Document simultaneously says:

- `FiberView` supports `dispose/restart/update`.
- D12 requires plugin-provided `validate(config)`.
- “Relationship to TS” promises preserving update's two-branch invariant.
- Open issue leans toward M2 doing `dispose + restart` only, leaving update to M4.

But current `Plugin` trait has no config associated type, config argument, or `validate`; `Ctx::plugin()` takes no config.

### Recommended decision

Given document's lean, choose the smallest first-release plan:

- M2 provides only `dispose + restart`.
- Configuration is part of constructed Plugin instance.
- Remove update from first-release `FiberView`, contract tests, and invariant descriptions.
- Design `PluginFactory<Config>`, config erasure, and update transaction in M4.

If keeping update in M2, first answer:

- How is config type erased behind dyn Plugin?
- Who validates new config?
- On validation failure, is old config retained?
- During loading/unloading, which generation does update select?
- Does update mutate same Plugin or construct new instance?

## Important issue 7: resource ownership and cancellation propagation are not in API

### Resource ownership

`on()` and `provide()` return `Disposer`; plugin `apply()` also returns `Effect`. It is unclear:

- Are listener/service automatically owned by current Fiber?
- Must plugin put disposer in returned Effect too?
- If manually disposed early, does Fiber unload run it a second time?
- Does isolated Ctx preserve original Fiber ownership?

Current TS behavior automatically makes listeners/services registered through Fiber Context into Fiber effects; returned disposer is for early release. For `ctx.provide()` ownership and “wait for consumers to drain before removing own snapshot,” see [`src/reflect.ts`](../src/reflect.ts#L277-L304).

Specify the same ownership principle in v4 without copying TS interface:

> Listener, service, and child plugin created through a Fiber's Ctx are automatically owned by that Fiber. Returned Disposer means early release and need not be placed again into Plugin's returned Effect.

Plugin's returned Effect handles external resources not registered through Ctx.

### Cancellation propagation

D7 selects `CancellationToken`, but Ctx and `Plugin::apply` do not expose it, so plugins cannot cooperatively react to Fiber unload.

Give each generation independent child token and expose at least one:

```rust
ctx.cancellation_token()
ctx.cancelled().await
```

Specify unload order:

1. Mark transition unloading.
2. Cancel current generation.
3. Wait for apply currently executing to exit.
4. Run EffectRecord cleanup in strict order.
5. Publish final state.

Also state this is cooperative cancellation. If plugin never observes token and does not return, dispose may wait forever; first release must not imply arbitrary Rust Future can be forcibly terminated.

## Documentation fixes to make at the same time

These do not block architecture, but fix them in next revision:

1. `CordisError` unconditionally uses `#[derive(thiserror::Error)]`, so `thiserror` is regular, not optional dependency.
2. If `serde/serde_json` are only for agent example and LLM boundary, put them in agent crate, not core.
3. D10 says registration returns `PluginId`, but public API shows only `FiberView`; clarify whether PluginId is a field or separately returned.
4. `ErrorSink` appears in milestones without interface, ownership, or failure policy.
5. Specify panic policy: terminate task, convert to `CordisError`, or require callers not to panic.
6. `Event::NAME` can remain a log field, but clarify it does not participate in uniqueness or dispatch.

## Recommended race and failure tests

Current matrix has good coverage, but the tests below determine architectural correctness and should be contract requirements before implementation:

### EventBus

- Async emit can safely read payload after caller returns.
- Async listener error goes only to ErrorSink, not unobserved task error.
- After waterfall veto, do not invoke later listeners or terminal next.
- Deterministic behavior when listener unload races in-flight dispatch.

### Effect / Fiber

- Ten concurrent dispose calls execute cleanup once and join same result.
- Dispose/restart race: every waiter observes its own generation.
- Rapid `Loading → Unloading → Loading` does not lose wakeup.
- Late error from old generation does not contaminate new generation.
- Calling dispose during cleanup does not deadlock.
- Registering new effect during cleanup fails explicitly.

### Registry / Reload

- Unloading provider in isolate A does not affect isolate B.
- Delete provider then immediately recreate same ServiceKey; old unload does not evict new consumer.
- During provider cleanup, it can access own snapshot, but ordinary consumer cannot create new stale binding.
- Multi-level dependency chain drains from consumer toward provider.
- Missing dependency remains Pending indefinitely and is not mislabeled Failed.

### Cancellation

- Cancel during loading wakes plugin observing token.
- Repeated cancel is idempotent.
- Parent Fiber cancellation propagates to all child Fibers.
- Non-responsive plugin is explicitly recorded as cooperative-cancellation limitation.

## Recommended revision order

### Step 1: Freeze four foundational models

Define and review only these types first; do not write business implementation:

1. Event, Hook, Next, and five dispatch results.
2. Effect, EffectRecord, cleanup errors.
3. FiberSnapshot, Transition, TransitionTask, generation.
4. ServiceKey, BindingKey, ScopeId, ProviderId.

These four groups determine whether later implementation needs rework.

### Step 2: Build a compilable API skeleton

Create minimal crate containing traits, enums, type aliases, and empty implementations, and run:

```text
cargo check
cargo test --doc
```

Validate dyn compatibility, Future lifetimes, Send/Sync, and type erasure; do not rush the state machine.

### Step 3: Implement transitions and EffectRecord first

EventBus is relatively independent, but Fiber lifecycle underpins Registry, Service, and reload. Use pure state-machine tests to prove:

- Same-generation joining.
- Cross-generation isolation.
- Exactly once.
- Cancellation and cleanup order.
- Error identity.

### Step 4: Implement scoped service binding and reload

Record actual ProviderId on consumer first, then build reverse index. Do not first write global scan/index keyed only by TypeKey and add isolate later.

### Step 5: Integrate agent example last

Agent example is good end-to-end acceptance, but should not determine core event/lifecycle API backwards. Keep `serde_json::Value`, LLM messages, and ToolSpec in boundary crate.

## Pass criteria for next design revision

Design is “freezable and implementable” only when:

- [ ] Five event modes have complete, compilable listener signatures.
- [ ] Fire-and-forget holds no non-`'static` borrow.
- [ ] Effect expresses sync/async success and failure.
- [ ] Concurrent dispose explicitly joins same TransitionTask.
- [ ] `watch` is observation only; operation completion modeled separately.
- [ ] Every transition carries generation.
- [ ] Service identity includes TypeId, qualifier, and ScopeId.
- [ ] Consumer records actual bound ProviderId.
- [ ] Pending, Failed, and Disposed are distinct.
- [ ] Unique decision made on whether config/update is in M2.
- [ ] Fiber ownership of Ctx-registered resources is explicit.
- [ ] CancellationToken is exposed to plugins.
- [ ] Critical race tests are in acceptance matrix.
- [ ] Public API skeleton passes `cargo check`.

## Final opinion

**Conditionally approve roadmap choice; defer design freeze.**

V4's most important improvement is escaping the burden of “simulating JS in Rust for compatibility.” The roadmap is not wrong; the gap is precise definitions between principles and implementable contracts.

Once EventBus, Effect, Transition, and Scope Binding are resolved, remaining milestones can continue on the current direction. If they are not resolved first, they will all surface during M1/M2 and force simultaneous rewrites of core API and tests.
