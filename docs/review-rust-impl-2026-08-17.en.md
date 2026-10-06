# min-cordis Rust Implementation Review (Against Design v5)

> Date: 2026-08-17. Reviewed: [`rust/`](../rust/) workspace (min-cordis core, 2,075 lines, plus min-cordis-agent, 415 lines; all 55 tests passing).
> Baseline: [design-rust-port.md](design-rust-port.md) v5 (API draft §2, decisions D1-D30 §3, deviations §4, contract tests §5, release criteria §9).
> Method: three internal parallel reviews (concurrency correctness / test quality / design consistency) plus an independent gpt-5.6-sol review without hints. Sol identified five additional genuine bugs; all were independently confirmed.

## Overall Assessment

**The architectural boundaries held and all core paradigms are sound; the implementation has 10 genuine bugs, none of which require architectural changes.**

Each key boundary set by v5 was checked and remained intact:

- **Crate layering:** no serde in the core; serde exists only in the agent crate, as required by the “boundary crate” decision.
- **Typed events:** internal `Arc<dyn Any>` erasure appears only in the bus/event files; the public API is generic throughout. There are no string events or degraded `Value` payloads.
- **dyn compatibility:** `Plugin`, `Listener`, and `WaterfallListener` all work behind `Arc<dyn ...>`; generics have not contaminated the registry.
- **No locks across await:** the fiber transition lock is used only to enqueue and update state; callbacks run outside it (D5/D24 physically enforced: `set_state` enqueues, `flush_status` dispatches outside the lock).
- **Exactly-once behavior belongs to TransitionTask, observation to watch:** the two mechanisms remain separate (D20/D25).
- **Config stored in plugin instance:** the registry stores only `Arc<dyn Plugin>` and does not erase config through `Any` (the minimal Codex route was followed).
- **Events are not filtered by isolate:** there are no scope checks on the dispatch path (D29 implemented).

All 16 release conditions were implemented as intended, and the D-series decisions have code-comment anchors. **There is no `unsafe` code.** Ownership uses conventional Arc/Weak, short Mutex critical sections, and dedicated synchronization primitives (watch/mpsc/Notify/CancellationToken, each for its intended role).

## Bugs That Must Be Fixed (10, by severity)

### Tier 1: Resource Leaks and Permanent Hangs

#### 1. Resources are not rolled back after `apply` fails (new finding from Sol; most important single finding across the four reviews)

[`fiber.rs:327-340`](../rust/crates/min-cordis/src/fiber.rs#L327). `fail_load` clears only dependency edges and `last_deps`; **it does not drain effects**. Services, listeners, and child plugins registered by a failing plugin remain alive:

- Event listeners from the failed plugin still receive events (the bus does not check fiber state).
- Child plugins may remain Active.
- Service-binding placeholders remain, so a later provide with the same key gets `ServiceExists`.

**Atomicity of failed assembly is lost.** This is an implementation-level consequence of pillar 1 (plugin as an assembly unit), not covered by the design-review phase. `ctx.provide`, event registration, and `ctx.plugin` immediately add an EffectRecord to the fiber ([ctx.rs:290-307](../rust/crates/min-cordis/src/ctx.rs#L290), [ctx.rs:221-287](../rust/crates/min-cordis/src/ctx.rs#L221)); after `apply` returns Err, nothing cleans them up.

**Fix:** make `fail_load` perform the same LIFO rollback as unload before entering Failed; add a contract test that registers a service and listener, then returns Err.

#### 2. Concurrent `dispose()` and `restart()` can leave restart waiting forever (new finding from Sol)

[`fiber.rs:580-595`](../rust/crates/min-cordis/src/fiber.rs#L580). `restart` checks only `state == Disposed`. If Dispose is queued but state has not changed, Restart is queued too. After processing Dispose, a non-root driver returns ([fiber.rs:437](../rust/crates/min-cordis/src/fiber.rs#L437)); the queued Restart never receives `TaskDone::Done`, leaving its caller stuck at [fiber.rs:599-609](../rust/crates/min-cordis/src/fiber.rs#L599).

**Fix:** have restart return `InactiveEffect` when `terminal_task.is_some()`, or complete all queued tasks before driver exit. Add a timeout test for concurrent dispose/restart.

#### 3. Concurrent consumer Dispose and provider eviction can hang provider cleanup forever (new finding from Sol)

[`ctx.rs:257-284`](../rust/crates/min-cordis/src/ctx.rs#L257). Consumer Dispose is queued and the driver exits; a subsequent RefreshDepsJoin has nobody to process it. The provider's disposer waits forever on `join_task`, cascading into an upper-level unload hang.

**Fix:** complete the eviction task if `post` fails, or drain/reject remaining intents with completion signals before driver exit. Add a test that disposes provider and consumer concurrently.

#### 4. `EffectRecord::drain` is not cancellation-safe; dropping its future leaves it permanently Draining (new finding from Sol)

[`effect.rs:64-86`](../rust/crates/min-cordis/src/effect.rs#L64). The first caller changes the state to Draining and performs cleanup inside its own future. If `select!`, a timeout, or abort drops that future, the state is not restored. Every later drain joins and waits forever ([effect.rs:123-137](../rust/crates/min-cordis/src/effect.rs#L123)). The public `Disposer::dispose()` returns a freely cancellable future ([effect.rs:153-159](../rust/crates/min-cordis/src/effect.rs#L153)).

**Fix:** move cleanup into an independent tokio task; all callers join the shared result. Add a test that aborts the first disposer and then calls dispose again.

#### 5. `serial` dispatches concurrently rather than in sequence (found in the internal three-way review)

[`bus.rs:272-296`](../rust/crates/min-cordis/src/bus.rs#L272). The loop calls `spawn_on` for each hook and immediately awaits `join_next()`: all hooks run at once, and short-circuiting follows **completion order**. This breaks TypeScript serial semantics (await listener N before calling N+1, short-circuit in **registration order**): side effects from later listeners may already have occurred even though an earlier listener short-circuits.

`serial_bails` passes because listeners take almost no time, so spawn order is approximately completion order.

**Fix:** spawn and await sequentially without a JoinSet; change the signature from `Arc<E>` back to `&E`. Add an adversarial case with a slow `Some` first listener and a fast `Some` second listener.

### Tier 2: Panics Kill the Driver

#### 6. A panic in `validate()` / `check()` kills the fiber driver (new finding from Sol)

[`fiber.rs:284-288`](../rust/crates/min-cordis/src/fiber.rs#L284), [`registry.rs:169-188`](../rust/crates/min-cordis/src/registry.rs#L169). `apply` has a CatchUnwind boundary ([fiber.rs:294-300](../rust/crates/min-cordis/src/fiber.rs#L294)); these two user-callback boundaries do not. A panic terminates the fiber driver, `intents_inflight` is not decremented, and FiberView await/restart/dispose all hang.

**Fix:** catch panics at every user-callback boundary; map validate panic to Failed and check panic to a dependency error/ErrorSink. Test that FiberView completes reliably after validate/check panics.

#### 7. An async cleanup closure can panic while calling `f()` outside the task boundary (new finding from Sol)

[`effect.rs:102-107`](../rust/crates/min-cordis/src/effect.rs#L102). The code calls `f()` before spawning; a panic from `f()` itself is not converted into CordisError and kills the driver directly.

**Fix:** catch panic from the `f()` call too, then spawn only after a Future is created successfully. Add separate tests for panic during closure invocation and during Future poll.

#### 8. `join_task` / `settle_inner` treat a dropped sender as success (found in the internal three-way review)

[`fiber.rs:606-608`](../rust/crates/min-cordis/src/fiber.rs#L606). If `rx.changed().await` returns `Err`, the code returns `Ok(())`. Dropping the sender when FiberInner is released makes dispose silently return Ok, hiding an unload that never ran. `settle_inner` has the same issue ([fiber.rs:637](../rust/crates/min-cordis/src/fiber.rs#L637)).

**Fix:** return `Err(Arc<CordisError>)` when the sender is dropped.

### Tier 3: Known Races

#### 9. Provide TOCTOU silently loses a service (internal three-way finding, confirmed by Sol)

[`ctx.rs:217-240`](../rust/crates/min-cordis/src/ctx.rs#L217). The preflight lookup and `insert_binding` inside the closure are not mutually exclusive. For concurrent calls with the same key, the later `ServiceExists` goes only to error_sink; the caller receives Ok and a useless Disposer, and the service is not registered. This breaks the `provide_as` Result contract.

**Fix (Sol):** make insertion the synchronous atomic primary operation, return `ServiceExists` directly to the caller, then register an Effect that removes that exact binding. Test two tasks providing the same key concurrently.

#### 10. Window between `plugin()` spawning and registering the parent effect (internal three-way finding)

[`ctx.rs:313-332`](../rust/crates/min-cordis/src/ctx.rs#L313). If the parent is already Disposed, the code attempts recovery through `registered.is_err()`, but `spawn_fiber` has already registered in `inject_index`. After Dispose is processed, a non-root driver returns; the later RefreshDeps is never processed and leaves a half-registered fiber.

**Fix:** register the parent effect before `spawn_fiber`, or explicitly clean `inject_index` on the recovery branch.

#### 11. `mem::take(effects)` during unload has no guard (internal three-way finding)

[`fiber.rs:356`](../rust/crates/min-cordis/src/fiber.rs#L356). If a record drain panics, every later effect is lost and the state remains Unloading.

## Issues in the Agent Crate (New Findings from Sol)

#### 12. `stopped` / `steps` are shared across runs

[`agent/lib.rs:209-217`](../rust/crates/min-cordis-agent/src/lib.rs#L209). `stop()` permanently sets true, so a later run immediately returns Stopped ([agent/lib.rs:244-251](../rust/crates/min-cordis-agent/src/lib.rs#L244)). Two concurrent runs cancel each other, and event step numbers interleave ([agent/lib.rs:295-301](../rust/crates/min-cordis-agent/src/lib.rs#L295)).

**Fix:** give each run its own run state and cancellation token, with the fiber-level token as the parent cancellation source. Define `steps()` as a cumulative metric or remove it. Test two consecutive runs and concurrent runs.

#### 13. A tool-runner panic panics the Agent Future

[`agent/lib.rs:319-340`](../rust/crates/min-cordis-agent/src/lib.rs#L319). The tool error contract handles only `Result::Err`; a panic during runner invocation or polling cannot be converted into a model-visible `"error: ..."`, leaving a gap with the crate docs (“feed tool failures back to the model”; [agent/lib.rs:11-12](../rust/crates/min-cordis-agent/src/lib.rs#L11)).

**Fix:** catch panics at the tool execution boundary. Add tests for panic while synchronously creating the Future and while polling it asynchronously.

#### 14. Empty LLM response silently returns an empty string

[`agent/lib.rs:303-305`](../rust/crates/min-cordis-agent/src/lib.rs#L303). When `content == None && tool_calls.is_empty()`, the code returns `Ok("")`, hiding a backend protocol error. Public `LlmResponse` fields make this state easy to construct.

**Fix:** add `AgentError::InvalidResponse`, or model responses as an enum that excludes invalid states.

#### 15. Agent event test asserts order although emit does not guarantee completion order

[`agent.rs:394-408`](../rust/crates/min-cordis-agent/tests/agent.rs#L394). Emit creates an independent task per listener ([bus.rs:221-239](../rust/crates/min-cordis/src/bus.rs#L221)), and scheduling provides no completion-order guarantee; this can fail intermittently.

**Fix:** if ordering is part of the contract, use serial dispatch or sort assertions by sequence number.

## Test Gaps (Merged from Two Reviews)

| Gap | Related bug |
|---|---|
| Adversarial serial case: slow first `Some`, fast second `Some` (`contract.rs:773`) | Bug 5 |
| Full automatic-reload loop: provide again → consumer returns Active (`contract.rs:966`, `:1089`); reactivate after `check_evicts` predicate returns true | Second half of pillar 5 |
| Transactional rollback after apply partially registers resources and fails | Bug 1 |
| Concurrent dispose/restart; concurrent consumer/provider dispose | Bugs 2 and 3 |
| Dispose again after aborting the first disposer | Bug 4 |
| FiberView completes reliably after validate/check panic | Bug 6 |
| Separate coverage for cleanup-closure invocation panic and Future poll panic | Bug 7 |
| Return value for concurrent provide using the same key | Bug 9 |
| Agent consecutive runs, concurrent runs, per-run cancellation; empty LLM response; LLM panic; tool panic | Bugs 12–14 |
| Parallel all-settled semantics with fast error + slow Ok (`contract.rs:743`) | Test strength |
| `auto_ownership` reads counter (`contract.rs:232`); `emit_async_safe` checks `e.value == 42` (`contract.rs:688`) | Test strength |

High-risk contracts `concurrent_dispose_join` (`:356`), `exactly_once_same_error` (`:617`), and `dispose_during_dispose` (`:639`) assert genuine concurrency and genuine `Arc::ptr_eq` identity; their coverage is strong enough.

## Design Consistency

### Declared deviations (§8; reasonable implementation choices)

- Serial payload changed from `&E` to `Arc<E>` (JoinSet `'static` requirement).
- Waterfall's third argument changed from `Next<'_>` to generic `Terminal<E>`, adding a public `Terminal` trait.
- `Next` changed from `&mut dyn FnMut(&Ctx, E)` to zero-argument `pub struct Next<'a, E>`.

### Undeclared deviations (minor)

- `TransitionTask` does not contain a generation ([fiber.rs:70](../rust/crates/min-cordis/src/fiber.rs#L70)); restart clearing `terminal_task` provides the behavior, so the difference is structural rather than behavioral.
- `Ctx` exposes additional `handle()` ([ctx.rs:69](../rust/crates/min-cordis/src/ctx.rs#L69)), `events()` ([ctx.rs:116](../rust/crates/min-cordis/src/ctx.rs#L116)), and `cancellation_token()` ([ctx.rs:338](../rust/crates/min-cordis/src/ctx.rs#L338)); these are read-only accessors and do not violate the ownership model.
- `CordisError::ServiceNotFound` is dead: `get` uses Option (D13), and the core never constructs it.
- `Plugin::injects()` returns a slice, forcing plugins to retain a `Vec<TypeKey>` (Sol); fixed dependencies could use a static array or associated constant.
- `AgentLoopPlugin::with_max_steps(0)` fails only at runtime with `MaxSteps(0)` (Sol); validating at construction would expose config errors earlier.
- Duplicate tool names are silently overwritten by `HashMap::collect` (Sol, [agent/lib.rs:233-241](../rust/crates/min-cordis-agent/src/lib.rs#L233)); the constructor should return Result and reject duplicates.
- The order of `tool_schemas()` from `HashMap::values()` is unstable (Sol, [agent/lib.rs:253-268](../rust/crates/min-cordis-agent/src/lib.rs#L253)); preserve original Vec order when deterministic request snapshots are needed.

### Independently checked and ruled out

- **Waterfall recursive aliasing** ([bus.rs:15-33](../rust/crates/min-cordis/src/bus.rs#L15)): `invoke(self)` consumes by value and `&mut` moves exclusively down the chain; there is no alias.
- **Once `fired` claiming** ([bus.rs:198-207](../rust/crates/min-cordis/src/bus.rs#L198)): `swap(true)` under the lock ensures exactly once under concurrency; serial skips are filtered on the next attempt.
- **CatchUnwind** ([event.rs:166-186](../rust/crates/min-cordis/src/event.rs#L166)): wrapping poll in `AssertUnwindSafe` is standard and does not cause UB.
- **`resolve_dep` timing** ([registry.rs:170-189](../rust/crates/min-cordis/src/registry.rs#L170)): notification chain is complete (`inject_index` is registered in `spawn_fiber` before any load); no permanent wait.
- **StoredValue double wrapper** ([registry.rs:13-24](../rust/crates/min-cordis/src/registry.rs#L13)): the `Box<Arc<T>>` erasure path correctly restores fat pointers for `T: ?Sized`.
- **EffectRecord::join lost wakeup:** relies on `Notify::notified()`'s “registered on creation” semantics, confirmed by tokio documentation.

## Clippy Does Not Pass (Confirmed by Sol)

`cargo clippy --workspace --all-targets -- -D warnings` fails:

- Unused import `TaskDone` ([ctx.rs:11-13](../rust/crates/min-cordis/src/ctx.rs#L11)).
- Unnecessary raw-pointer type conversion ([registry.rs:123-128](../rust/crates/min-cordis/src/registry.rs#L123)).
- Several unused imports, variables, and dead-code warnings in tests.

## Architecture Review (Independent gpt-5.6-sol Review)

**Overall score: 8/10.** Five architectural observations:

1. **Module boundaries are reasonable; core coupling is concentrated in `Ctx/Fiber/Registry`.** `event` defines typed interfaces, `bus` handles erasure and dispatch, and `effect` manages resource cleanup. `ctx ↔ fiber` reference each other around a shared lifecycle object within one crate and compile successfully; architecturally they form one runtime kernel. Consider merging them into a private `runtime` layer later to avoid `registry` directly depending on `FiberInner/Intent` ([registry.rs:51](../rust/crates/min-cordis/src/registry.rs#L51), [fiber.rs:98](../rust/crates/min-cordis/src/fiber.rs#L98)).

2. **Public interfaces and internal mechanisms are clearly separated.** `Plugin/Event/Listener/Terminal` stay typed; erased adapters, `EffectRecord`, `TransitionTask`, and `StoredValue` are crate-private. The main leak is public `Disposer`/`FiberView` returning `Arc<CordisError>`, exposing the internal shared-error-identity strategy in the API ([effect.rs:154](../rust/crates/min-cordis/src/effect.rs#L154)). This is an acceptable lifecycle contract, but raises the cost of future error-model changes.

3. **Type-erasure boundaries are correct.** Registration and invocation remain generic; only heterogeneous containers erase through `TypeId/Any`. Event payload borrowing is erased, return values are erased as owned values, and services store `Arc<T>`, all consistent with ownership semantics. Runtime downcast risk is contained in the internal adapter layer.

4. **Concurrency primitives match the semantics.** `watch` (latest state + repeatable join), `mpsc` (serialize state-machine intents), `Notify` (one-time cleanup), `CancellationToken` (cooperative cancellation), and std Mutex (short critical sections) each have a clear role. **Risk:** `UnboundedSender` can accumulate work under frequent refresh/update ([fiber.rs:114](../rust/crates/min-cordis/src/fiber.rs#L114)); before M4, consider intent coalescing or bounded backpressure.

5. **Extensibility is moderately good.** A tower adapter and `PluginFactory<Config>` can be added as an outer crate/generic construction layer; `provides` can be added through a default `Plugin` method. **`update` touches both “config stored in immutable `Arc<dyn Plugin>`” and the generation state machine** ([plugin.rs:6](../rust/crates/min-cordis/src/plugin.rs#L6)), and needs an explicit replacement protocol; this is the only M4 architectural change that should be planned in advance. Overall, the model resembles Bevy plugin assembly plus an actor supervisor and suits a dynamic-lifecycle framework. Keep tower in an adapter layer, outside the core.

## Fix Priority

1. **Bug 1 (rollback after apply failure):** most serious, largest resource-leak surface, and violates pillar 1 atomicity.
2. **Bugs 2, 3, and 4 (three permanent-hang cases):** fatal concurrency paths.
3. **Bugs 6 and 7 (panics kill the driver):** minimum robustness requirement.
4. **Bug 5 (serial ordering):** semantic mismatch.
5. **Bugs 8, 9, 10, and 11:** close known race conditions.
6. **Bugs 12–15 (agent):** M3 quality.
7. **Fill test gaps and clear clippy; remove dead `ServiceNotFound`; align `TransitionTask` shape with D6 by storing its generation.**
