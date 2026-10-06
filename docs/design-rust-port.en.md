# rutis Rust Port Design (v5, Paradigm-First Route)

> 2026-08-17 v5 draft. **Approach:** port paradigms—implement Cordis's core paradigms in idiomatic Rust rather than anchoring to all 96 TS specs; acceptance = self-verification through paradigm contracts.
> **Status:** draft. v4 went through three reviews ([review-rust-design-2026-08-17-v4.md](review-rust-design-2026-08-17-v4.md), [review-rust-design-2026-08-17-v4-codex.md](review-rust-design-2026-08-17-v4-codex.md), [review-rust-design-2026-08-17-v4-zcode.md](review-rust-design-2026-08-17-v4-zcode.md)) and two decision revisions ([review-rust-design-2026-08-17-v4-resolution.md](review-rust-design-2026-08-17-v4-resolution.md)). Only after applying those revisions and checking release conditions in §9 may this be changed back to final.
> **History:** v1→v3 one-to-one route and two rounds of two-model reviews are archived in [review-rust-design-2026-08-17-sol.md](review-rust-design-2026-08-17-sol.md), [review-rust-design-2026-08-17-deepseek.md](review-rust-design-2026-08-17-deepseek.md), [review-rust-design-2026-08-17-round2-sol.md](review-rust-design-2026-08-17-round2-sol.md), and [review-rust-design-2026-08-17-round2-deepseek.md](review-rust-design-2026-08-17-round2-deepseek.md). v3 semantic research (contract line-number anchors) remains a reference asset.
> **Ecosystem grounding:** three research reports ([research-rust-async-2026-08-17.md](research-rust-async-2026-08-17.md), [research-rust-ecosystem-2026-08-17.md](research-rust-ecosystem-2026-08-17.md), [research-hot-reload-2026-08-17.md](research-hot-reload-2026-08-17.md)): official async/tokio docs + Bevy/tower/shaku/Tauri precedents + hot-reload examples (lifecycle-level precedents = Erlang/OSGi; code-level dylib/subsecond are orthogonal complements).

## 0. v4 → v5 Change Summary

| v4 issue | v5 resolution | Decision |
|---|---|---|
| `ListenerResult` / `Value<E>` undefined; fully async model conflicts with sync bail/emit | Unified listener helper trait `call<'a>` returns `BoxFuture<'a>`; associated type `Event::Value`; **remove bail; keep four dispatch modes** | D16 |
| Waterfall `async fn -> BoxFuture` produces double future; `on()` cannot register waterfall listeners | Separate `on_waterfall` registration surface + `WaterfallListener`; make `waterfall` ordinary `fn -> BoxFuture`; `next` is caller-provided fallback continuation | D17 |
| Two Effect Disposer variants return `()`, leaving nowhere for errors | Both return `Result<(), CordisError>`; closure captures owned `Ctx`, not `&Ctx` | D18 |
| Config disappeared (apply/plugin/update accepted none) | Bake config into plugin instance (owned at construction); `Plugin::validate` validates owned config; defer `update` to M4 | D19 |
| `PluginFailed(Box<dyn Error>)` conflicts with `Arc<CordisError>` identity | Separate concerns: identity belongs to `TransitionTask` (cache `Arc<CordisError>`); wrapping belongs to `PluginFailed` (`#[source] Box<dyn Error>`), no recursive nesting | D25 |
| Panic policy unspecified | Define task boundaries: observe every spawned JoinHandle (collect with JoinSet); route panic via JoinError to ErrorSink; catch async disposer panic at task boundary and wrap as `PluginFailed` | D30 |
| Reverse index only `HashMap<TypeKey, Vec<PluginId>>`; isolate cross-talk and granularity too coarse | Reverse edge is `(PluginId, generation, ServiceKey)` tuple | D21 |
| `isolate(label)` lacks service name and differs from TS granularity | `isolate(key: ServiceKey, label)`; isolate by ServiceKey and merge same label | D21 |
| Cycle-detection promise cannot be met (services registered dynamically; mutually dependent A/B both stay Pending) | Remove cycle detection; `InjectUnsatisfied` means only “unsatisfied”; M4 may add `provides()` manifest | D22 |
| Fiber state cannot expose intermediate transitions (watch last-value can skip states) | Typed `FiberStatusChanged` event on its own bus; enqueue FIFO under lock and dispatch outside; guarantee commit order, not listener completion order | D24 |
| CancellationToken chosen but not exposed to plugin | Independent child token per fiber generation, exposed as `ctx.cancelled().await`; specify five-step unload ordering | D27 |
| Resource ownership undefined (are listeners/services owned by fiber automatically?) | Registration through `Ctx` is automatically fiber-owned; Disposer only releases early and is not returned to Effect | D28 |
| Event visibility across isolates unspecified | Decision: events are not filtered; isolate only scopes registry; users create sub-bus for scoped events | D29 |
| Forgot `ctx.effect()` | Add to Ctx draft; incremental (async-iterable) effect is intentionally not preserved | D23 |
| thiserror missing from dependency list; serde in core crate | thiserror is normal dependency; move serde/serde_json into agent example crate | §8 |

## 1. Paradigm Definition (Implementation Goals: Five Pillars)

1. **Plugin = assembly unit:** one `apply` can provide 0..n services, register 0..n listeners, and return cleanup (effect).
2. **Fiber = lifecycle container:** state machine (Pending/Loading/Active/Failed/Disposed/Unloading) + dependency gating (all declared dependencies ready before start, including `check()` predicate) + cascading unload + exactly-once cleanup.
3. **Service = type-keyed registry + scope:** `TypeId` primary key + isolate by ServiceKey (same type may coexist across scopes; multiple instances of same interface use explicit keys, shaku Keyed pattern); service lookup walks `Ctx` parent chain.
4. **Event bus = four dispatch semantics:** emit (fire-and-forget) / parallel (concurrent all-settled, aggregate errors) / serial (ordered until short-circuit) / waterfall (middleware continuation, can veto).
5. **Dependency-driven reload:** provider unload → fibers declaring dependency on it are evicted and automatically reloaded; reload uses reverse dependency edges `(PluginId, generation, ServiceKey)`, evict consumers first and drain them concurrently, provider last.

**Not part of the paradigm (not implemented):** Proxy property syntax, traceable/caller-shadow, JS object/callback reference identity, runtime string events, `internal/*` extension-point surface (TS equivalent), synchronous bail, service-config `intercept`, `Context.filter` event filtering, incremental (async-iterable) effects.

## 2. Core API Draft

### Read the Ownership Model First

- `Ctx = Arc<CtxInner>` and is cheap to `Clone`; `isolate`/`plugin` both return a new `Ctx` sharing the kernel.
- A registered `Plugin` is stored as `Arc<dyn Plugin>` (config is already baked into the instance; see D19).
- Disposer/Effect closures capture owned `Ctx` (clone the Arc), not call-stack borrows.
- Listeners/services/child plugins registered through a Fiber's `Ctx` are **automatically owned by that Fiber**; `Disposer` only releases early and is not returned to `Effect` (D28).

### Types and Errors

```rust
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum CordisError {
    #[error("service {0:?} not found in scope")] ServiceNotFound(String),
    #[error("plugin failed")] PluginFailed(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("multiple errors: {errors:?}")] Aggregate { errors: Vec<CordisError> },  // Do not flatten
    #[error("fiber disposed")] InactiveEffect,
    #[error("config validation failed: {issues:?}")] Validation { issues: Vec<String> },
    #[error("dependency unsatisfied: {0:?}")] InjectUnsatisfied(Vec<String>),  // No cycle-detection promise
}
```

### Events: Typed Payloads + Callback Registry

```rust
pub trait Event: Send + Sync + 'static {
    const NAME: &'static str;        // Diagnostics/logging only, not identity or dispatch
    type Value: Send + 'static;      // Type of serial short-circuit value
}

// Helper trait handles lifetime: BoxFuture<'_> in an alias has no input from which
// to infer '_ (E0106).
trait Listener<E: Event>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, e: &'a E)
        -> BoxFuture<'a, Result<Option<E::Value>, CordisError>>;
}
// Blanket impl for Fn(&Ctx, &E) -> BoxFuture<'_, Result<Option<E::Value>, CordisError>>

trait WaterfallListener<E: Event>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, e: &'a E, next: Next<'a>)
        -> BoxFuture<'a, Result<E::Value, CordisError>>;
}
// Next<'a> = &'a mut dyn FnMut(&'a Ctx, E) -> BoxFuture<'a, Result<E::Value, CordisError>>
// (Minimal skeleton must pass cargo check before v5 freeze; settle exact shape in M1.)

pub struct EventBus { /* internally: HashMap<TypeId, Vec<Hook>>; Arc<Inner> + Mutex */ }
impl EventBus {
    pub fn on<E: Event>(&self, ctx: &Ctx, l: impl Listener<E>) -> Result<Disposer, CordisError>;
    pub fn on_waterfall<E: Event>(&self, ctx: &Ctx, l: impl WaterfallListener<E>) -> Result<Disposer, CordisError>;
    pub fn once<E: Event>(&self, ctx: &Ctx, l: impl Listener<E>) -> Result<Disposer, CordisError>;
    // Prepend through EventOptions { prepend: bool }

    pub fn emit<E: Event>(&self, ctx: &Ctx, e: Arc<E>); // fire-and-forget; spawn and observe JoinHandle
    pub async fn parallel<E: Event>(&self, ctx: &Ctx, e: Arc<E>) -> Result<(), CordisError>; // JoinSet, aggregate all errors
    pub async fn serial<E: Event>(&self, ctx: &Ctx, e: &E) -> Result<Option<E::Value>, CordisError>; // ordered until short-circuit
    pub fn waterfall<E: Event>(&self, ctx: &Ctx, e: &E, next: Next<'_>) -> BoxFuture<'_, Result<E::Value, CordisError>>;
}

pub type ErrorSink = Arc<dyn Fn(CordisError) + Send + Sync>; // Defaults to logger
```

### Plugins: dyn-Compatible, Config Baked into Instance

```rust
pub trait Plugin: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn injects(&self) -> &[TypeKey] { &[] }                   // Dependency-gating declaration
    fn validate(&self) -> Result<(), CordisError> { Ok(()) } // Validate owned config
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>>;
}

pub enum Effect {
    Done,
    Disposer(Box<dyn FnOnce() -> Result<(), CordisError> + Send>),
    AsyncDisposer(Box<dyn FnOnce() -> BoxFuture<'static, Result<(), CordisError>> + Send>),
    Many(Vec<Effect>),
}
```

### Service Registry: TypeId Key + Explicit Keys for Multiple Instances

```rust
// ServiceKey = TypeKey(TypeId + qualifier)
impl Ctx {
    pub fn provide<T: Send + Sync + 'static>(&self, value: T) -> Result<Disposer, CordisError>;
    pub fn provide_as<T: ?Sized + Send + Sync + 'static>(&self, key: TypeKey, value: Arc<T>) -> Result<Disposer, CordisError>;
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>>; // Walk parent chain; check isolate
    pub fn effect(&self, f: impl FnOnce() -> Effect) -> Result<Disposer, CordisError>; // D23
    pub fn plugin(&self, p: impl Plugin) -> FiberView; // FiberView.id: PluginId
    pub fn isolate(&self, key: ServiceKey, label: &str) -> Ctx; // Isolate by ServiceKey
    pub fn cancelled(&self) -> impl Future<Output = ()>; // D27: expose CancellationToken
}
```

## 3. Key Design Decisions (with Evidence)

| # | Decision | Basis (research/prior art/review) |
|---|---|---|
| D1 | Handwrite `BoxFuture<'a,T>` alias; use it for every dyn-compatible trait | Rust Reference dyn compatibility; async-trait expands to this shape; explicit ABI preferred over macro |
| D2 | Typed event payloads (`Event` + `Listener` helper trait), erase internally as `Arc<dyn Any + Send + Sync>`; `serde_json::Value` only at serialization boundary (agent crate) | Bevy cheatbook/bevy#1431; qubit-event-bus |
| D3 | Callback-registry/event-hook bus using owned `Arc<dyn Listener>`; no channel | users.rust-lang EventBus consensus; Tauri listen/emit; hooks are immediate push, channels are buffered pull |
| D4 | Four modes: emit fire-and-forget (spawn, observe JoinHandle, route panic/Err to ErrorSink); parallel aggregate `Aggregate{errors}` (JoinSet); serial ordered until short-circuit; waterfall = CPS (`Next<'_>`), veto by not calling next; all async | tower Service precedent; resolution D16 (remove bail because it already shares serial short-circuit semantics; synchronous behavior served only removed `internal/listener`) |
| D5 | Fiber state machine: `transition: Mutex<Transition>` holds state/token changes in one lock domain; **under lock enqueue FIFO only, never hold across await; callbacks execute outside locks** | Official tokio shared-state guidance (short std Mutex critical sections); std RwLock deadlock example → snapshot then release; resolution D24 correction |
| D6 | Exactly once = one `Arc<TransitionTask>` under lock (contains generation); joins in same generation share Arc, new generation gets new Arc; watch only observes (late subscriber calls `borrow()` then loops on `changed().await`) | Codex review: watch last-value cannot prove one execution per generation; minimal resolution D20 fix (avoid three parallel mechanisms) |
| D7 | Unified cooperative `CancellationToken`: independent child per fiber generation, exposed via `ctx.cancelled().await`; five unload steps: mark unloading → cancel generation → wait for apply exit → clean EffectRecords → publish terminal state. State cooperative limit: apply that ignores token can make dispose wait forever | Official tokio graceful-shutdown docs; resolution D27 |
| D8 | Runtime attachment: inject `Handle` at construction when possible; otherwise `Handle::try_current()` → explicit error; never create runtime implicitly | tokio Handle docs |
| D9 | `yield_now` is only a fairness hint; stale/epoch checks use token comparison under lock, no scheduler-order assumption; listener completion order is not a contract | tokio yield_now docs; resolution D24 |
| D10 | Plugin identity = `FiberView.id: PluginId` + display name `Plugin::name()`; optional `is_unique`-style deduplication | Bevy Plugin trait (name/is_unique) |
| D11 | Errors: derive `CordisError` with thiserror (**normal dependency**), `Error + Send + Sync + 'static`, **not Clone**; share through Arc; aggregate in Vec | API guidelines C-GOOD-ERR; thiserror docs |
| D12 | Config validation: `Plugin::validate` validates owned config before storing; no schema library | Self-sufficient paradigm; resolution D19 |
| D13 | Service locator `get::<T>() -> Option<Arc<T>>`: explicit, typed key, Option failure; walk parent chain | Bevy `Res<T>`/Tauri `state::<T>()`; resolution §7 inventory |
| D14 | Dependency-gated reload: reverse edge `(PluginId, generation, ServiceKey)`; provider unload evicts exact matching consumers (drain consumers concurrently, provider last, preserve self-access during cleanup) | Erlang/OSGi paradigm precedents; resolution D21 fix (isolate cross-talk + per-service granularity) |
| D15 | Optional tower adapter layer (not core); `Service` + `Layer` target request paths; do not force lifecycle hooks into them | tokio blog “Inventing the Service trait” |
| D16 | Fully async event system, remove bail, four modes; listener helper `call<'a>`; associated `Event::Value`; emit/parallel payload `Arc<E>`, spawned task owns `Ctx` + `Arc<E>` | resolution D16; second-round zcode/codex (helper trait resolves E0106, JoinSet requires `'static`) |
| D17 | Separate `on_waterfall` registration + `WaterfallListener`; `waterfall` ordinary `fn -> BoxFuture`; `next` is caller-provided fallback continuation | resolution D17; zcode review (missing registration surface is a defect, not an open question) |
| D18 | Both Effect Disposer variants return `Result<(), CordisError>`; closures capture owned `Ctx`, not `&Ctx` | resolution D18; codex/zcode review (no destination for cleanup errors) |
| D19 | Bake config into instance: `Plugin` has no associated type; concrete plugin owns config from `new(config)` and validates it; registry stores only `Arc<dyn Plugin>`, no `Any`; defer update to M4 | resolution D19; Codex minimal route |
| D20 | Exactly once = one `Arc<TransitionTask>` under lock; watch is observation only | See D6 |
| D21 | Consumer match exactly `(PluginId, generation, ServiceKey)`; `isolate(key: ServiceKey, label)` by ServiceKey, merge same label, child scope falls back to parent. After simplification (§8.24), no reverse index: when provider is removed, consumers are fibers declaring that key and whose `last_deps` includes tuple | See D14; resolution D21; simplification S5 |
| D22 | Drop cycle detection; missing dependency remains Pending; `InjectUnsatisfied` makes no cycle claim; M4 may add `provides()` manifest | Codex review (dynamic service registration makes cycles unprovable) |
| D23 | Add `Ctx::effect()` to draft; intentionally do not preserve incremental (async-iterable) effects | resolution D23; zcode review (pillar 1 dependency) |
| D24 | `FiberStatusChanged`: enqueue FIFO under lock, dispatch outside; guarantee commit order (generation as sequence), not listener completion order | resolution D24; zcode + user review correction |
| D25 | Error layers: identity in `TransitionTask` (cache `Arc<CordisError>`); wrapping in `PluginFailed` (`#[source] Box<dyn Error>`); no recursion | second-round resolution D25; codex/zcode reviews |
| D26 | (Merged into D27) | — |
| D27 | Expose CancellationToken + five unload steps + cooperative cancellation limit | See D7 |
| D28 | Resource ownership: `Ctx` registrations automatically owned by fiber; Disposer only releases early, not returned to Effect; Ctx from isolate retains original fiber ownership | resolution D28; Codex review; TS [reflect.ts:277-304](../src/reflect.ts#L277) |
| D29 | Do not filter events across isolates; isolate scopes only registry; users create sub-bus for scoped events | resolution D29; intentional omission of TS `Context.filter` |
| D30 | Panic task boundaries: observe every spawned JoinHandle (collect with JoinSet); route listener panic via JoinError to ErrorSink; catch async disposer panic at task boundary and wrap as `PluginFailed` | resolution D30; second-round Codex review |
| D31 | Emit dispatch order (added 2026-08-19; revises per-event spawning in D4): retain fire-and-forget, but **serialize same event type in emission order**—tail chain: under one lock, “take previous task handle → spawn new task → store as tail” (must be atomic; two lock sections let concurrent same-type emits fork chain; experiment observed 4 concurrent listener tasks). In each task, await previous task, then await listeners one by one in registration order. Cost: listeners for same event become serial rather than concurrent (required by delivery order). CatchUnwind contains panic without breaking chain; same-type emit from inside listener queues at tail without deadlock. **No order guarantee across event types** (known boundary; consumer needing total order should sequence in payload); listener completion order is still not a contract (D9) | Match synchronous order of Cordis/dsh emit (free on JS single thread; dsh `core/agent/src/dispatch.ts` snapshots and calls contained listeners sequentially). Rust per-event spawning loses it: back-to-back emits on multiple threads were misordered ~30%, max displacement 52 (no reordering single-thread or with ≥1ms gap). Regression pinned by `rutis/tests/dispatch_chain_probe.rs` (no concurrent chain forks) and `rutis-agent/tests/order_probe.rs` (single-emitter order) |

**Dependencies:** tokio (rt-multi-thread, sync, macros) + tokio-util (CancellationToken) + thiserror (normal dependency). **No futures-util** (parallel uses `tokio::task::JoinSet`), no async-trait. **serde/serde_json only in agent example crate**, not core.

## 4. Relationship to the TypeScript Version (Explicit Deviation Inventory)

Semantic research follows v3 (line-number anchors are trusted). The table below makes individual decisions; do not claim broadly that “v4 preserves everything.”

| TS capability | Decision | Landing |
|---|---|---|
| Synchronous short-circuit `bail` ([events.ts:228-233](../src/events.ts#L228)) | Remove; semantics merge into serial (D16) | — |
| `internal/*` event surface ([events.ts:340-362](../src/events.ts#L340)) | Intentionally not preserved | — |
| `check()` predicate gating ([fiber.ts:689-701](../src/fiber.ts#L689)) | Preserve | M2 |
| `on` prepend option and listener ordering ([events.ts:114-119](../src/events.ts#L114)) | Preserve (affects serial/waterfall result) | M1 |
| `once` ([events.ts:323-329](../src/events.ts#L323)) | Preserve | M1 |
| Service-config interception via `intercept` ([service.ts:86-102](../src/service.ts#L86)) | Intentionally not preserved (removed with Proxy surface) | — |
| Service lookup along fiber parent chain ([reflect.ts:154-166](../src/reflect.ts#L154)) | Preserve via Ctx parent traversal | M2 |
| `Context.filter` event filtering ([events.ts:170-179](../src/events.ts#L170)) | Intentionally not preserved (D29) | — |
| Two-branch `update` ([fiber.ts:857-886](../src/fiber.ts#L857)) | Preserve semantics | M4 |
| Incremental (async-iterable) effect ([fiber.ts:397-409](../src/fiber.ts#L397)) | Intentionally not preserved | — |
| Sync generator effect (`yield` returns each disposer) | Intentionally not preserved: Rust has no generator syntax; use `Effect::Many`/multiple `ctx.effect()` calls. Core retains LIFO/exactly-once semantics | — |
| “Unload = remove registry entry; consumer-cached `Arc<T>` keeps instance alive” ([reflect.ts:297-303](../src/reflect.ts#L297)) | Document explicitly; basis for reload-group tests | M2 |
| Eviction drain order (consumers concurrent, provider last) ([reflect.ts:299-336](../src/reflect.ts#L299)) | Specify exact contract | M2 |
| Reentrant provide/effect during unload returns `INACTIVE_EFFECT` ([fiber.ts:434-436](../src/fiber.ts#L434)) | Preserve | M2 |
| EffectRecord LIFO ([fiber.ts:508](../src/fiber.ts#L508)), aggregate without flattening ([125-130](../src/fiber.ts#L125)), exactly once ([548-552](../src/fiber.ts#L548)) | Preserve (D18/D20) | M2 |
| Six-state machine ([fiber.ts:160-167](../src/fiber.ts#L160)) | Preserve | M2 |

The 96 TS specs are inspiration only; paradigm-pure parts (fiber/dispose/re-entrant behavior) can be selectively used as port assertions.

## 5. Paradigm Contract Tests (Acceptance and Self-Verification)

| Group | Contract | Representative tests |
|---|---|---|
| assembly (pillar 1) | Assemble 0..n services/listeners/cleanups; `effect` returns Disposer; registration automatically belongs to fiber | `plugin_provides_n_services`, `plugin_registers_n_listeners`, `effect_yields_disposer`, `auto_ownership` |
| lifecycle | All state transitions; init failure → Failed; dispose idempotent; root dispose cleans subtree and can restart; concurrent dispose joins same `Arc<TransitionTask>`; isolate generations; no lost wakeup for `Loading→Unloading→Loading` | `state_transitions`, `init_failure_marks_failed`, `dispose_idempotent`, `root_restart`, `concurrent_dispose_join`, `cross_generation_isolation` |
| gating | Missing dependency → Pending; late provider activates regardless of arrival order; failed `check()` predicate evicts; long-term missing dependency remains Pending without error | `waits_for_dependency`, `late_provider_activates`, `check_evicts`, `pending_not_failed` |
| dispose | Serial LIFO within record; one error unchanged/multiple aggregated without flattening; exactly once (join same `Arc<E>`); no deadlock on dispose during cleanup; registering effect during cleanup returns `INACTIVE_EFFECT` | `lifo_serial`, `aggregate_no_flatten`, `exactly_once_same_error`, `dispose_during_dispose`, `effect_during_cleanup_fails` |
| events | Emit fire-and-forget (async-safe payload read; panic/Err to ErrorSink); parallel aggregates all errors; serial ordered to short-circuit; waterfall veto/wrap/outermost result; prepend order; once; deterministic listener-unload/dispatch race | `emit_async_safe`, `emit_error_sink`, `parallel_aggregates`, `serial_bails`, `waterfall_veto_around`, `prepend_order`, `once_once`, `listener_unload_race` |
| registry | TypeId register/lookup/duplicate error; multiple instances by explicit key; isolate by ServiceKey, merge same label; parent traversal | `typed_roundtrip`, `keyed_multi_instance`, `isolate_scoping`, `parent_chain_lookup` |
| reload | Gating reload order (consumers concurrent, provider last); self-access during cleanup; re-entry errors; unloading isolate A does not evict B; one service eviction does not cascade | `eviction_order`, `self_access_during_cleanup`, `reentrant_provide_fails`, `isolate_no_cross_evict`, `single_service_evict` |
| cancel | CancellationToken stops agent; fiber unload cascades and wakes waiters; cancel during loading wakes plugin observing token; repeated cancel idempotent; parent cancel propagates to child | `agent_stop`, `cancel_wakes_awaiters`, `cancel_during_loading`, `cancel_idempotent`, `cancel_cascades` |
| agent (M3) | Eleven paradigm semantics in Python examples | `agent_*` series |

**Acceptance:** at least 30 tests, target 40; each paradigm pillar has at least 4 positive cases and 2 failure modes.

## 6. Milestones

- **M1:** type system + EventBus (four modes + `on_waterfall` + prepend/once) + ErrorSink + Handle + panic task boundaries → events group.
- **M2:** Ctx/Registry (TypeId+key+isolate) + Fiber (state machine/gating/EffectRecord/TransitionTask/watch observation) + `Ctx::effect` + `ctx.cancelled` + dependency reload → assembly/lifecycle/gating/dispose/registry/reload/cancel groups.
- **M3:** agent example crate (CancellationToken stop, LLM trait, ToolSpec, event observation; serde/serde_json in this crate) → agent group; runnable demo.
- **M4 (optional):** tower adapter, `PluginFactory<Config>` + `FiberView::update`, `provides()` manifest cycle diagnostics, advanced named scopes for multiple instances, state-migration hot reload (Erlang `code_change` equivalent). Composition with code-level reload tools (subsecond/hot-lib-reloader): those supply new code; this framework safely swaps components (dispose → evict → assemble → reload); orthogonal, not coupled.

## 7. Open Questions

1. Keep `Event::NAME` as a string for diagnostics/logging only—decision already made; whether to add `#[allow(dead_code)]` during M1 is an implementation detail.
2. Key type for multiple instances of one type: `&'static str` vs newtype `Key<T>`—prefer the latter (spirit of shaku Keyed); decide in M2.
3. Should `FiberView::restart` accept a replacement config (precursor to M4 `update`)? Decide in M4.

## 8. Implementation Record

**Implementation completed 2026-08-17** (M1+M2+M3, M4 untouched). Code is in `rust/` workspace. Simplification retrospective (candidate list and execution plan): [simplification-rust-impl-2026-08-18.md](simplification-rust-impl-2026-08-18.md).

- **Crates:** `rutis` (core, `crates/rutis`, ~2,100 lines), `rutis-agent` (example application with demo/TUI).
- **Dependencies:** tokio 1.53.1 (rt-multi-thread/sync/macros/time), tokio-util 0.7.19, thiserror 2.0.20; agent crate adds serde/serde_json at boundary (§3). No futures-util/async-trait ✓.
- **Tests:** core contracts 58 (§5 all groups + review additions + simplification batch; both single-thread and parallel modes green) + agent 13 + doc 1 = **72**. `cargo run -p rutis-agent --example demo` works; `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` clean.
- **Implementation decisions/deviations** (all finalized within review decisions):
  1. `Aggregate { errors: Vec<Arc<CordisError>> }`, `ErrorSink = Fn(Arc<CordisError>)`: consequence of D11 (not Clone) and D25 (Arc caches identity); members retain identity.
  2. Added `CordisError::ServiceExists` (duplicate registration for same key/scope); `InjectUnsatisfied` has no cycle language ✓ (D22).
  3. `parallel` payload is `Arc<E>` (JoinSet `'static` requirement); `serial` is inline sequential await with `&E` payload (matches §2 draft signature; after review it was finalized that the implementation had always been registration-ordered, one spawn+await at a time; JoinSet wording was misleadingly “concurrent,” so it was rewritten inline with CatchUnwind panic containment).
  4. Final `Next<'a, E>`: typed zero-argument `call()` (TS waterfall semantics: fixed payload, values flow back through return values); waterfall uses inline CPS; listener panic propagates to dispatcher (borrows call stack and cannot spawn).
  5. HRTB return-type inference is limited for listener closures: blanket impl covers `Fn`; idiomatic use is function item or small struct (tests demonstrate both).
  6. `parallel` returns one error unchanged and aggregates multiple errors (crate-consistent semantics; TS always aggregates, declared deviation).
  7. Watch liveness: `FiberInner` retains permanent receiver—without a receiver tokio watch considers channel closed and `send` silently fails (a confirmed implementation trap).
  8. settle (`FiberView` `IntoFuture`) means resolved + not Loading/Unloading + **no in-flight intents** (`intents_inflight` count), preventing “resolved Pending + unprocessed reload notification” race (confirmed in demo scenario).
  9. Five-step unload is implemented with serialized intents; dispose/restart/eviction **pre-cancel current generation token before enqueueing** (driver is serialized; without pre-cancel, running apply may not reach step 2); apply is not aborted, driver continues after cooperative exit (faithful to D7 step 3 “wait for apply exit”). Each generation gets a new token (not a derived child); load rotates it.
  10. Root driver is permanent (root may restart; application-lifecycle resource); non-root driver exits after terminal state, while handle retains cached terminal result for join.
  11. Additional implementation APIs: `Ctx::get_as` / `provide_as_with_check` / `refresh` (re-run `check()` predicate) / `root_view` (root dispose/restart entry point); `FiberView::name`; `FiberStatusChanged{seq}` carries sequence.
  12. Early eviction of one service: `mark_binding_removing` (strict lookup fails immediately) → evict → after drain `finalize_binding` (preserve self-access during cleanup, §4 “unload = registry removal”).
  13. Agent tool error fed back as `error: {e}` (simplified Python repr); `Snapshot.resolved` field added for settle semantics.
- **Independent post-implementation review (2026-08-17):** consistency was high; all 13 declared deviations in §8 were confirmed. Two undeclared issues were found, fixed, and covered by contract tests:
  14. (Fixed) root `dispose → restart → dispose again` reused stale terminal task (second dispose was a no-op); root restart now clears `terminal_task` (fiber.rs Restart branch), pinned by `root_restart_dispose_cycle`.
  15. (Fixed) once listener was moved to dispatch tail, breaking registration order (§4); now atomic claim under lock (`fired: AtomicBool` swap), **preserve its position**, and leave claimed item in registry for Disposer/unload cleanup (functional equivalent of TS self-removal), pinned by `once_keeps_position`.
  16. (Deviation addendum) Waterfall third argument named `terminal: T` (public `Terminal` trait; D17 settles caller-provided fallback continuation); `Next<'a, E>` made generic. Inline apply adds CatchUnwind poll boundary (necessary beyond D30 or plugin panic kills driver). Token is its own `Mutex<CancellationToken>`, not literally inside D5 “single lock domain” (state transition alone uses transition lock; ordering verified). Core never constructs `InjectUnsatisfied` (gating failure remains silently Pending per D22); only defensive path in agent example bypasses gating.
- **Fixes from implementation review ([review-rust-impl-2026-08-17.md](review-rust-impl-2026-08-17.md)):** of 10 reported core bugs, 9 were confirmed and fixed. One (serial concurrency) was a false positive by code inspection (one spawn+await at a time is structurally registration-ordered), but an adversarial test was added. Agent fixes #13/#15; #12/#14 kept as-is to match Python reference. Finalized in this round:
  17. **Rollback failed load** (#1; aligns TS fiber.ts:749-779 failure path): `fail_load` goes through UNLOADING, drains partially registered apply resources, then enters Failed; cleanup errors route to ErrorSink; Failed retains load error (atomic assembly, pillar 1). Test `apply_failure_rolls_back`.
  18. **Fix hanging family** (#2/#3/#4): `dispose()` registers terminal task **when called**, not on first future poll; restart rejects `InactiveEffect` if `terminal_task` exists/driver is alive; add `FiberInner.alive` + `drain_stale` before driver exit (drain leftover intents and complete tasks, including late posts racing alive flag); move EffectRecord cleanup into independent spawn task so every caller joins shared terminal result—dropping `Disposer::dispose()` future no longer leaves Draining (cancellation-safe).
  19. **Complete panic boundaries** (#6/#7): catch panic from user `validate`/`check`, mapping to Failed / not ready (TS fiber.ts:695-698; logging difference in item 21); catch both async cleanup `f()` call and Future poll; cleanup task always writes Done (#11 closed). Closed join/settle channel now returns Err rather than fake Ok (#8).
  20. **Atomic provide** (#9): insertion is synchronous primary operation; duplicate registration (`ServiceExists`) returns to caller, no longer “error sink + fake Ok”; Effect only registers removal cleanup, with rollback for narrow registration-failure race. `plugin()` no longer sends load intent in inactive-parent recovery branch (#10).
  21. **Retained behavior/declarations:** `check()` panic is not logged (TS logs; Registry has no sink dependency; semantically “not ready”); `stopped`/`steps` persist across runs and empty LLM response is empty string (matches Python reference; review suggestions for per-run cancellation and `InvalidResponse` are enhancements for later); `TransitionTask` still does not embed generation (D6 behavior covered by restart clearing terminal_task; shape deviation declared); tool execution moved into task boundary and runner panic becomes model-visible (#13); agent event assertion changed to content and emission order independent of arrival order (#15). Added reload-loop tests: `provider_reload_reactivates`, `check_recovery_reactivates`, `parallel_waits_all`, `concurrent_provide_single_winner`, `disposer_drop_is_cancel_safe`, `evict_after_consumer_disposed_completes`, etc.
  22. (Final round) Rewrite serial as inline sequential await (`&E`, no spawn; CatchUnwind converts panic to `PluginFailed`): same semantics as prior one-spawn-and-await-at-a-time version, with registration-order short-circuit pinned in both modes by `serial_register_order_adversarial`; removes misleading JoinSet appearance and restores §2 draft signature. Add `serial_panic_contained`. Review's “short-circuit by completion order” finding was disproven by code/tests; every implementation was registration-ordered.
- **Simplification batch** ([simplify-conclusion-2026-08-18.md](simplify-conclusion-2026-08-18.md) and amendment; executed 2026-08-18): five correctness fixes first, then three consolidations. All 72 tests pass in both runtime modes, clippy `-D warnings` clean, fmt baseline established.
  23. **Correctness:** shared `Arc<Binding>` identity (remove handwritten Clone; setting `removing` visible to everyone immediately); atomic `Ctx::effect` registration (state check and enqueue effects under same critical section, factory outside lock; if lifecycle already passed, drain new record immediately and return `InactiveEffect`); route cleanup errors during non-terminal unload to ErrorSink instead of swallowing; distinguish JoinError panic/cancel (`into_panic` itself panics for cancellation, fixed across core/agent); dead fiber's `cancellation_token` returns pre-cancelled token. Emit now uses one spawn layer (CatchUnwind inside + route at end).
  24. **Consolidation (one fact stated once):** remove once listener under lock when claiming—it is exactly once by bus-lock mutual exclusion; remove `fired` atomic and duplicate claim logic while preserving snapshot position; unify `Hook<C>` generic; **Settle barrier**—settle (`IntoFuture`) enqueues `Intent::Settle` in mailbox and joins; FIFO directly answers “is it stable?”, replacing five side channels (`resolved`/`intents_inflight`/`notify_inflight_drained`/double borrow/yield drain protocol; errors recognize Failed only, invariant). `post_join` rechecks alive after send to close exit race. **Delete reverse index:** eviction returns to sole source `last_deps` (consumers_of: declared inject key and last_deps contains tuple), removing cross-structure synchronization; see D21 revision.
  25. Line count unchanged (fmt and comments offset); net concepts removed: one cross-structure storage system, two synchronization mechanisms (fired/inflight), special resolved flag, handwritten Clone, watch-poll settle, yield-drain protocol. Snapshot public fields lose `resolved`. Public API retained (`ServiceNotFound`/`Key<T>` remain); Adapter remains (necessary Rust type shape). Added tests `restart_cleanup_errors_to_sink`, `mixed_concurrent_ops_complete`.
- **Cordis spec parity batch** ([cordis-spec-parity-2026-08-18.md](cordis-spec-parity-2026-08-18.md) §5.2; executed 2026-08-18): create `tests/parity.rs`; all eligible cases from original 96 specs (§2 full parity + §3 kernel) become 57 tests, retaining original `it()` titles and source-line comments; replace fake timers with deterministic synchronization. Parity tests exposed and fixed four implementation gaps:
  26. **Inertia lock 2** (`fiber.spec:27`): if dependencies are fully refreshed and none missing when load completes, adopt new set in place and transition in-flight load directly to ACTIVE rather than restarting a new generation.
  27. **Binding slot reusable during eviction:** allow a same-key new provide to replace a binding being removed (TS dispose synchronously frees slot); evictor finalizes by Arc identity (`finalize_binding_if`) and cannot delete new binding accidentally.
  28. **Keep TransitionTask alive** (same trap as §8.7): retain permanent receiver to prevent losing completion value on “quick completion + late subscription” (parity test reproduced lost wake deterministically).
  29. **Dependency identity includes scope:** extend `last_deps`/eviction tuple from three to `(PluginId, gen, TypeKey, scope)`—same provider/key in different isolate scopes no longer become each other's consumers. Add caller-inactive check to `get` (service is invisible through inactive fiber context; provider subtree self-access exempt, matching TS inactive-context behavior). Total 129 tests (57 parity + 58 contracts + 13 agent + 1 doc); single-/multi-thread modes and repeated runs green; clippy/fmt clean.

## 9. Release Conditions (Check Before Freezing v5)

- [ ] All four event modes have complete, compiling listener signatures (helper-trait route; no `ListenerResult` placeholder).
- [ ] Fire-and-forget holds no non-`'static` borrow (payload `Arc<E>`); spawned task owns `Ctx` + `Arc<E>`.
- [ ] JoinSet tasks for parallel/serial satisfy `'static`.
- [ ] Both Effect Disposer variants return `Result<(), CordisError>`; closure captures owned `Ctx`.
- [ ] Concurrent dispose joins the same `Arc<TransitionTask>` under lock; watch is observation only.
- [ ] Reverse dependency edge includes `(PluginId, generation, ServiceKey)` tuple.
- [ ] `isolate()` signature includes ServiceKey and same-label merge is specified.
- [ ] `InjectUnsatisfied` makes no cycle-detection promise.
- [ ] `Plugin` has optional `validate` (validates owned config); `FiberView::update` marked M4.
- [ ] `Ctx::effect()`, `ctx.cancelled()`, ErrorSink definition, and panic task boundaries are all in draft.
- [ ] CordisError layering is stated: identity belongs to `TransitionTask`, wrapper belongs to `PluginFailed` (`#[source] Box<dyn Error>`), no recursion.
- [ ] Resource ownership rule (D28), cross-isolate event decision (D29), and §4 deviation inventory are documented.
- [ ] Remove bail; use four modes; update §1/§5/D4 consistently.
- [ ] State events specify “enqueue FIFO under lock, dispatch outside”; ordering specifies “commit order guaranteed, listener completion order not guaranteed”; tests match.
- [ ] Public waterfall signature passes `cargo check` in minimal skeleton and can register listener (v5 freeze gate); exact `Next` shape may be finalized in M1.
- [ ] Public API skeleton passes `cargo check` (minimal crate containing only traits/enums/type aliases + empty implementations), especially helper-trait lifetimes, `Arc<dyn Plugin>` erasure, JoinSet `'static`.
