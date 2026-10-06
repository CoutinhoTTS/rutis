# Design: Cordis Inspection and Interception in rutis

Status: implemented. [PR #53](https://github.com/arcships/rutis/pull/53), [PR #54](https://github.com/arcships/rutis/pull/54), and [PR #55](https://github.com/arcships/rutis/pull/55) merged to main (#55 includes the fix that drops failed-write candidates outside locks and supplemental acceptance tests). [#40](https://github.com/arcships/rutis/issues/40), [#27](https://github.com/arcships/rutis/issues/27), and [#29](https://github.com/arcships/rutis/issues/29) are closed. Baselines: rutis `7d7402d`, Cordis [`56b3d4f`](https://github.com/cordiverse/cordis/tree/56b3d4f725681cf4556c1a8695a709cc3b6eed74).

## Purpose and boundaries

Consumers already use `Ctx::diagnostics()` to inspect plugin dependency graphs, initialization timeouts, and scope assembly; preserve this on-demand architecture snapshot. `FiberView::watch()` and `FiberStatusChanged` continue to provide current state observation. The root diagnostic event stream proposed by closed [#28](https://github.com/arcships/rutis/issues/28) / [#37](https://github.com/arcships/rutis/pull/37) has no consumer, so this design does not restore it. [#38](https://github.com/arcships/rutis/pull/38), which proposed removing snapshots, is also closed.

Cordis inspection is split between runtime objects and synchronous hooks; it has no single `diagnostics()` snapshot. This design handles three separate capabilities: pre-dispatch observation, cleanup-item trees, and service read/write interception. Each API runs at the action boundary rather than inferring what happened from a background subscription stream.

| Cordis source behavior | rutis today | Design response |
| --- | --- | --- |
| [`events.ts` `_resolve`](https://github.com/cordiverse/cordis/blob/56b3d4f725681cf4556c1a8695a709cc3b6eed74/packages/core/src/events.ts#L72-L81) synchronously emits `internal/dispatch` before listener selection, even with no listeners; return value cannot allow/deny | Only business `on`, `emit`, `serial`, `parallel`, `waterfall` | Add a synchronous, read-only dispatch-attempt observer |
| [`events.ts` `on`](https://github.com/cordiverse/cordis/blob/56b3d4f725681cf4556c1a8695a709cc3b6eed74/packages/core/src/events.ts#L154-L165) lets `internal/listener` rewrite listener registration | Registration goes directly into bus table | Record as a separate difference; no current consumer needs registration replacement, so do not combine it with dispatch observation |
| [`fiber.ts` `effect` / `getEffects`](https://github.com/cordiverse/cordis/blob/56b3d4f725681cf4556c1a8695a709cc3b6eed74/packages/core/src/fiber.ts#L275-L346) retains labels and actual nested cleanup relationships | `EffectRecord` stores pending cleanup only; `Effect::Many` is flattened on registration | Add a per-fiber cleanup tree built from actual ownership |
| [`reflect.ts` property access](https://github.com/cordiverse/cordis/blob/56b3d4f725681cf4556c1a8695a709cc3b6eed74/packages/core/src/reflect.ts#L71-L123) uses `internal/get` / `internal/set` waterfalls; explicit `ctx.get` bypasses property-read hooks | Strict `require` exists; `get` is an optional locator; binding values cannot be replaced | Add type-safe interception after strict reads; add provider-owned write handles for replaceable services |
| `internal/plugin`, `internal/status`, `internal/service` report runtime changes | Whole-tree snapshots, per-fiber watch, and status events already serve separate purposes | Do not add a second root event log; define one separately if a live consumer appears |

Cordis `parallel` also passes `emit` mode to `_resolve`. rutis's new mode field records the real call as `Emit` or `Parallel`; it does not copy this string-level detail. A throwing Cordis `internal/dispatch` listener can interrupt dispatch, but it is not an explicit review/approval decision API.

All three hooks are scoped by ancestry of the registering fiber. Complete-key, isolate-scope, and instance checks still apply separately:

| Hook | Ancestor chain used to select registered fiber | Result |
| --- | --- | --- |
| Pre-dispatch observer | Event emitter | Subtree observer cannot see sibling subtree dispatches |
| Strict-read interceptor | Service reader | Subtree interceptor cannot take over a sibling subtree read |
| Provider-write interceptor | Service provider | Subtree interceptor cannot take over a sibling subtree write |

## 1. Pre-dispatch event observation (#40)

### Public API sketch

```rust
pub enum DispatchMode { Emit, Serial, Parallel, Waterfall }

pub struct DispatchAttempt<'a> {
    pub key: &'a TypeKey,       // type, qualifier, and instance ID
    pub mode: DispatchMode,
    pub emitter: PluginId,
    pub emitter_instance: InstanceId,
    pub event: &'a (dyn std::any::Any + Send + Sync),
}

impl EventBus {
    pub fn observe_dispatch(
        &self,
        owner: &Ctx,
        observer: impl for<'a> Fn(&DispatchAttempt<'a>) + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError>;
}
```

The observer borrows the event and may downcast it only during the callback. The interface does not copy or retain the payload. Because an observer can inspect event contents, it is a trusted framework extension point, not a mechanism for untrusted plugins. Registration is owned by the `owner` fiber's effect. At registration, verify that `owner` belongs to the bus's root; at dispatch, call only observers whose registering fibers are ancestors of the emitter. A root can observe the whole tree; a Session-scoped plugin sees only its subtree, and sibling instances remain invisible. Instance events still undergo existing instance-ID and shutdown-admission checks first.

Call each observer once per valid dispatch attempt, even when there are no business listeners. `emit` invokes it on the current thread before enqueueing; `serial`, `parallel`, and `waterfall` invoke it on first future poll, before selecting business listeners. Invoke synchronously in registration order, without admission, bus-table, registry, or fiber-state locks. Listener registration/removal inside an observer can affect the subsequent business-listener snapshot, matching Cordis's observe-before-select order. Reentrant dispatch is ordinary nested dispatch; do not promise global cross-thread observer order. Existing same-key `emit` ordering still follows actual enqueue order.

Observers return `()`, so they cannot reject dispatch. Catch panics and send them to ErrorSink; isolate panics from ErrorSink too, then continue business dispatch. Unload observers through effects. A new dispatch no longer selects a removed observer; synchronous callbacks already selected count as in-flight work on their fiber, and subtree shutdown waits for them. Do not hold locks while waiting, and do not let observers retain the borrowed event. An instance dispatch rejected by concurrent shutdown does not trigger the observer. An observed attempt may still fail later revalidation and not be admitted; hence `DispatchAttempt`, not `DispatchAccepted`.

**Review boundary:** this hook can inspect and record before dispatch, but cannot guarantee durable storage or allow/deny dispatch. If business logic needs “reject means no dispatch,” design a separate explicit policy API and a result-bearing `try_emit`; do not pretend the existing `emit` returning `()` provides this contract. Registration replacement like `internal/listener` is also a separate requirement, not part of this hook.

Acceptance: all four dispatch modes, dynamic qualifiers, instance keys, zero listeners, observer-before-snapshot ordering, registration/unload races, reentrancy, panic, isolation between instances, and 1,000 registration/unload cycles. Existing event parity remains unchanged without observers.

## 2. Labeled effect cleanup tree (#27)

```rust
pub enum EffectPhase { Live, Draining }
pub struct EffectMeta {
    pub label: String,
    pub phase: EffectPhase,
    pub children: Vec<EffectMeta>,
}

impl Ctx {
    pub fn effect_named(
        &self,
        label: impl Into<String>,
        f: impl FnOnce() -> Effect,
    ) -> Result<Disposer, CordisError>;
}
impl FiberView {
    pub fn effects(&self) -> Vec<EffectMeta>;
}
```

Keep the return types of existing `Ctx::effect()` and `Plugin::apply()`. Default labels are `anonymous` and the plugin name. Plugin mounts, service provides, and listener registrations use framework-generated type/key/instance labels without service values, configuration, or event payloads. Preserve actual nested `Effect::Many` structure as `children`; identify each child with a stable ordinal and kind. Parallel `ctx.effect()` registrations remain siblings; do not invent parent-child relationships from the call stack. Add an explicit composition API for custom child labels only if users need it; do not casually add variants to the public `Effect` enum.

At registration, `EffectRecord` keeps both a pure metadata tree and the existing LIFO cleanup list. Metadata must not capture cleanup closures. Since `FiberInner.effects` is currently taken as a whole during unload, add a reclaimable weak index so `effects()` sees `Draining` during cleanup and removes the entry once the record is `Done`. Reads copy only labels, phase, and tree structure; they do not run user code or retain child fibers/service values. #27 permits retrieval from a fiber **or** `Ctx::diagnostics()`; start with `FiberView::effects()` to satisfy that entry point. Add it to the whole-tree diagnostics DTO only if architecture diagrams truly need it.

`EffectPhase` describes the whole cleanup record and applies to nested children. The first version does not track which leaf in `Many` is currently running.

Acceptance: automatic/explicit labels, real `Many` nesting with LIFO cleanup, early disposal, visibility during cleanup, removal on completion, unchanged error aggregation, and parent metadata/effect records returning to baseline after 1,000 subtree shutdowns.

## 3. Strict service read and provider write interception (#29)

### Reads

Keep `get` / `get_as` as explicit optional locators that bypass interception. `require` / `require_as` first perform existing instance visibility, type, active-state, dependency-declaration, and binding-availability checks in their existing order. Only then, outside locks, run synchronous typed hooks matching the current `(complete TypeKey, effective isolate scope)`. Select only hooks whose registering fiber is an ancestor of the reader; a Session hook must not take over a sibling Session's process-level service read. Hooks run in registration order with the resolved `Arc<T>` and can continue, replace only this read's result with another `Arc<T>`, or deny it. Denial gives `ServiceReadError` an explicit interception reason; panic becomes an explicit error. Hooks cannot make undeclared, out-of-scope, or unavailable services readable. Replacement affects only this result, not binding or `(provider, generation, key, scope)` identity. `ServiceAccess` still records the original binding and may additionally say whether this result was intercepted.

Interceptors are trusted extension points. `Arc<T>` cannot prove which instance a value came from: an interceptor holding a sibling instance's value can return it as a same-type replacement. Instance/scope checks guarantee only that the **binding being read** and the **fiber allowed to register the hook** are in scope; they cannot prove the origin of a value created by a hook. To force untrusted interceptors to avoid cross-instance values, either disable `Replace` or return only binding handles with private origin proof. No origin proof is implemented; #29 acceptance assumes trusted interceptor code.

Candidate API: `Ctx::intercept_require_as::<T>(key, hook) -> Result<Disposer, CordisError>`, with `Continue | Replace(Arc<T>) | Deny`. Synchronous reentry for the same key/operation returns a clear error; different-key reentry is allowed. Hooks belong to the owner fiber's effects, and registration/removal obey instance-subtree boundaries. Snapshot hooks before reads and call them after unlocking; shutdown waits for admitted hooks to finish.

### Writes

Services registered through existing `provide_as` remain immutable. Add `provide_mut_as`, returning a cleanup handle and generation-bound `ServiceWriter<T>`; only this handle can write to the binding it created. Writes through an old-generation handle, a binding being removed, a non-owner, or a type/instance-out-of-scope context all fail. This prevents an old async task from replacing a same-key service from a newer generation, even if it retained the same `Ctx`.

`ServiceWriter::set(&provider_ctx, Arc<T>)` explicitly checks that the caller is the fiber that created the binding; a transferable writer handle alone cannot prove caller identity. Then run synchronous write hooks for that key/scope outside locks, selecting only hooks whose registering fiber is an ancestor of the provider so sibling subtrees cannot interfere. Hooks may continue, replace with a same-type candidate, or deny. At commit, recheck binding Arc identity, provider, and generation, then atomically replace the value. Mutable bindings use their own mutable value slot; ordinary immutable services need no extra read lock. Successful writes preserve provider, generation, dependency tuple, and eviction relationships. The old `Arc<T>` is not mutated in place; subsequent reads get the new value. A write does not reload consumers automatically; external changes to health predicates still require `refresh()` to recheck gating. Hook panic returns a write error and does not commit the candidate.

This is stricter than Cordis `internal/get/set`: Cordis's get hook runs before final dependency lookup; rutis interceptors cannot bypass type, instance, or dependency-declaration boundaries. The synchronous entry point also cannot reuse async `EventBus::waterfall`; it may reuse only its chained ordering model.

Acceptance: `get_as` bypass, continue/replace/deny for `require_as`, reject undeclared/out-of-scope before hooks, match by full key and isolate scope, same-generation writes and stale-generation rejection, old Arcs retain old values, no write-triggered eviction, reentry and panic behavior, hook self-removal at unload with no old-generation callbacks, and existing service/lifecycle parity.

## Implementation order and delivery boundaries

1. Implement pre-dispatch observation for #40 first: this is a specific existing Cordis action boundary that needs inspection. Keep it in a separate PR from snapshots and service interception.
2. Implement the effect tree for #27; begin with per-fiber reads, not a larger whole-tree DTO.
3. For #29, implement strict-read interception first, then replaceable services and write interception; independently verify generation, old-Arc, and update-race behavior.

Every step preserves the architectural use of existing `Ctx::diagnostics()` and runs `cargo +1.98.1 test -p rutis`, `clippy -p rutis --all-targets -- -D warnings`, and `fmt -p rutis -- --check`. All three PRs passed independent review and remote review before merge (records in [parity analysis](plan/analysis/cordis-observation-parity.en.md)). PR #55 includes the failed-write candidate drop fix (`df65961`) and supplemental acceptance tests (observer reentry, cross-key reentry, write during removal). Consumer-repository build, architecture diagrams, and Session/Branch diagnostic workflows are accepted separately.
