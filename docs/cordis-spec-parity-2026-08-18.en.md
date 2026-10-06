# Parity Checklist for Original Cordis Specs (Rust)

> 2026-08-18. Purpose: identify which semantics from the original 96 Cordis specs should be tested automatically in Rust, which should not, and why.
> Method: two independent reviewers read every assertion in `tests/*.spec.ts` and classified it against the v5 paradigm boundary ([design-rust-port.md](design-rust-port.en.md), §1 five pillars + §4 deviations).
> Criterion: is it a **language-independent paradigm invariant**? Do not parity-test JavaScript-specific mechanisms (Proxy, string event names, `internal/*`, synchronous bail, traceable/caller-shadow, `Context.filter`, intercept, async-generator effects, update config).
> Count: 96 cases → **31 full parity / 27 partial parity / 38 no parity** (counted case by case; the table is authoritative. The initial rough count 27/21/48 did not match and was reconciled; see §5.2). Total cases with portable semantics: 58 (31 full + 27 kernel-only), matching the five pillars.

## 1. Classification overview

| Spec file | Cases | Full parity | Partial parity | No parity | Main reason excluded |
|---|---:|---:|---:|---:|---|
| fiber.spec.ts | 8 | 5 | 2 | 1 | update config (M4) |
| dispose.spec.ts | 13 | 4 | 9 | 0 | async-generator effect carrier (all 13 have portable semantics) |
| reentrant.spec.ts | 27 | 13 | 8 | 6 | internal/* surface, update config |
| events.spec.ts | 7 | 0 | 3 | 4 | Context.filter, removed bail, string event names |
| plugin.spec.ts | 10 | 5 | 2 | 3 | Proxy/inspect/registry iterator |
| service.spec.ts | 5 | 2 | 1 | 2 | traceable/caller-shadow |
| isolate.spec.ts | 3 | 2 | 0 | 1 | Context.filter event filtering |
| reflect.spec.ts | 4 | 0 | 1 | 3 | Proxy property syntax |
| associate.spec.ts | 5 | 0 | 0 | 5 | dotted string paths + Proxy attachment |
| decorator.spec.ts | 1 | 0 | 1 | 0 | TypeScript decorator syntax |
| invoke.spec.ts | 2 | 0 | 0 | 2 | intercept + caller-shadow |
| internal-hooks.spec.ts | 7 | 0 | 0 | 7 | internal/* extension surface |
| shadow.spec.ts | 4 | 0 | 0 | 4 | caller-shadow |
| **Total** | **96** | **31** | **27** | **38** | — |

## 2. Full parity (31 cases: pure paradigm kernel; automate first)

### Fiber state machine + dependency gating (9)

| Original case | Paradigm invariant asserted |
|---|---|
| fiber: inertia lock 1 | A dependency disappearing during LOADING does not unload immediately; completion enters UNLOADING; after re-provide, it transitions LOADING → ACTIVE again. |
| fiber: inertia lock 2 | Re-providing the same key to the same fiber during LOADING lets the in-flight load finish directly in ACTIVE. |
| fiber: inertia lock 3 | Disposing a provider returns its consumer to PENDING (eviction + cascade). |
| fiber: plugin error | `apply` throws → FAILED; listeners on the failed fiber do not fire. |
| fiber: dispose error | A throwing dispose still runs exactly once; `dispose()` resolves normally. |
| reentrant: coalesces duplicate dependency notifications | Without a transition, duplicate dependency notifications coalesce; `apply` runs once. |
| reentrant: distinguishes provider incarnations without a global counter | Provide/inject in two roots are independent; re-provide reloads only its own consumer. |
| service: pending inject | Inject callback waits until dependencies and init are ready, then proceeds. |
| service: multiple injects | Topological gating: foo → qux; bar → foo + qux; each init runs exactly once. |

### Exactly-once cleanup + error handling (15)

| Original case | Paradigm invariant asserted |
|---|---|
| dispose: async return 1 | Register cleanup after async setup completes; dispose runs it in order. |
| dispose: async return 2 | Calling dispose before setup completes still waits for setup to settle, then cleans up. |
| dispose: return with error | A synchronous effect error is thrown immediately; no cleanup is registered. |
| dispose: async return with error | Async setup rejection leaves no cleanup behind. |
| reentrant: keeps plugin execution failure separate from rollback cleanup failure | Execution error uses the await channel; rollback cleanup error goes to logger; terminal state is FAILED. |
| reentrant: returns one disposal promise and joins cleanup already in progress | Reentrant dispose returns the same promise; restart waits for in-progress cleanup. |
| reentrant: attempts every cleanup in LIFO order and aggregates failures deterministically | Run all cleanups LIFO; aggregate failures deterministically. |
| reentrant: preserves an AggregateError thrown by user cleanup as one failure | Preserve a user AggregateError as one aggregate member; do not flatten it. |
| reentrant: keeps a direct cleanup failure observable through the shared promise | After cleanup fails, all reentrant dispose waiters observe the same error. |
| reentrant: contains cleanup failure at structural restart | Cleanup failure during restart is logged; fiber returns to ACTIVE. |
| reentrant: separates synchronous execution and rollback cleanup failures | Synchronous execution error is thrown immediately; rollback cleanup error goes to logger. |
| reentrant: removes a synchronously failed effect after rolling back collected cleanup | On effect failure, roll back collected cleanup exactly once. |
| reentrant: makes reentrant restart await async rollback without replaying the execution failure | Restart blocks on async rollback and resolves when released, without replaying execution failure. |
| reentrant: makes reentrant restart await async execution and cleanup | Restart waits for async setup and all its async cleanup to settle. |
| reentrant: rejects effect registration during unload | Effect registration during unload reports `INACTIVE_EFFECT`; fiber returns to ACTIVE. |

### Plugin assembly + cascading + isolate (7)

| Original case | Paradigm invariant asserted |
|---|---|
| plugin: apply functional plugin | Call the function plugin once and pass it its options. |
| plugin: inactive context | After fiber disposal, plugin/effect/on throw and callbacks do not run. |
| plugin: nested plugins | Register nested plugins; dispose cascades through all child plugins and listeners; second dispose is idempotent. |
| plugin: root dispose | Root dispose cascades to child fibers and runs exactly once; idempotent. |
| plugin: Service.init | Invoke init at startup and run its returned cleanup exactly once on dispose. |
| isolate: isolated context | `isolate('foo')` cuts off parent visibility; scopes have independent provide/inject; callbacks are disposed after cleanup. |
| isolate: shared label | Same label shares one service; different labels isolate (condition: confirm Rust retains shared-label semantics). |

## 3. Partial parity (27 cases / 20 rows: portable kernel, JavaScript-specific assertion carrier)

The **semantic kernel is language-independent**, but assertions are carried by Proxy, string event names, async-generator effects, `internal/*`, or update config. Extract and rewrite the kernel in Rust form; do not copy the original assertion verbatim.

| Original case | Portable kernel | JavaScript-specific carrier |
|---|---|---|
| fiber: restart wrapped fiber | Replay apply after restart and return to ACTIVE | Proxy/prototype wrapper around fiber (`hasOwn`) |
| fiber: update config while injected service reloads | Provider update evicts and reloads consumer in order | update config + `getPrototypeOf` Proxy surface |
| dispose: dispose by plugin / dispose manually | `fiber.dispose` runs cleanup exactly once | `getEffects()` label tree (JS introspection) |
| dispose: yield dispose | LIFO `[3,2,1]` + exactly once + reentry returns same promise | generator effect + string event label |
| dispose: async yield 1–4 | LIFO; after abort, only yielded cleanup has landed | async-generator effect (intentionally excluded) |
| dispose: yield with error / async yield with error | Preserve cleanup yielded before the throw | sync/async generator |
| reentrant: does not let a stale execution failure poison the current generation | Old-generation async failure does not poison current generation (stale epoch) | update-config trigger mechanism |
| reentrant: logs disposal observer failures without rejecting disposal | Observer throw is logged; dispose still resolves; terminal DISPOSED | internal/plugin observer |
| reentrant: does not await async disposal observers but still observes rejections | Dispose does not wait for async observers; rejection is still observed and logged | internal observer |
| reentrant: lets parent disposal during publication drain pending child effects | Parent dispose drains effects of an inactive child; child was never applied | internal/plugin hook |
| reentrant: makes a loading parent join child cleanup already in progress | Parent and child disposal join the same cleanup already in progress | internal/plugin hook |
| reentrant: separates asynchronous execution and disposal failures | Send execution and dispose errors through separate channels | async-generator effect |
| reentrant: logs auto-rollback cleanup failure once when a structural owner joins | Log automatic rollback cleanup error once | async generator |
| reentrant: accepts effects while a child is pending or loading | Effect registration in PENDING/LOADING is legal and cleanup runs on dispose | Inspect PENDING via internal/plugin |
| events: ctx.on() / ctx.once() | Register → emit invokes; after dispose it no longer invokes; once fires once | string event names |
| events: ctx.waterfall() | `next` passes values through; omitting next stops the chain | string event names |
| plugin: ctx.registry / compare snapshot | After unload hooks are restored; reinstall is consistent (restore exactly once) | JS hook array/iterator |
| service: compare snapshot | Snapshot is restored after unload/reinstall (exactly once) | JS hook snapshot |
| reflect: service inject leak | Accessing a service after fiber dispose throws inactive | Proxy get trap |
| decorator: @Inject on class method | Method runs only after dependency registration; dispose after unload | TypeScript decorator syntax |

## 4. No parity (38 cases; JavaScript-specific and intentionally excluded)

Grouped by exclusion rationale; each maps to the v5 §4 deviation list:

| Reason excluded | Cases |
|---|---|
| **update config (deferred to M4)** | fiber: update config on wrapped fiber; reentrant: returns the asynchronous internal/update waterfall result, keeps wrapped fiber state canonical, coalesces an update before initial apply, continues the next generation after cleanup errors |
| **internal/* extension surface** | All 7 `internal-hooks.spec` cases; reentrant: resolves dependencies added during publication, rolls back runtime ownership when publication throws |
| **Proxy property syntax / class-inheritance reflection** | reflect: Context.is(), access check, service injection; all 5 associate.spec cases (dotted string paths + Proxy attachment); plugin: context inspect |
| **traceable / caller-shadow** | service: traceable effect (with/without inject); both invoke.spec cases; all 4 shadow.spec cases; associate: inspect |
| **Context.filter event filtering** | events: `ctx.parallel()`, `ctx.emit()`, `ctx.serial()`; isolate: isolated event |
| **Synchronous bail (removed)** | events: `ctx.bail()` |
| **JavaScript dynamic/duck typing** | plugin: apply object plugin, apply invalid plugin |

## 5. Implementation recommendations

1. **The 58 parity candidates (31 full + 27 kernel-only) correspond exactly to the five pillars** and are the scope for automated parity. Create `parity.rs` (or split by pillar) and reuse the original `it('...')` title as the test name for traceability.
2. For the **27 partial cases**, rewrite assertions around the kernel in Rust form: string event names → typed events; async generators → manual effect sequence; Proxy-wrapper assertions → direct state assertions.
3. Rewrite fiber inertia-lock timing cases (which rely on Vitest fake timers) using Tokio controllable time or explicit synchronization; do not copy the timers directly.
4. Resolve two conditional items first: whether Rust preserves isolate `shared label` semantics; whether synchronous generators from dispose.spec are included (the design explicitly excludes async iterable effects but does not mention synchronous generators).
5. Record all 48 no-parity cases (this document does so). External messaging can say: “Of the 96 original specs, 48 test language-independent paradigms and are fully automated in Rust; 48 cover JavaScript-specific mechanisms (Proxy, string events, `internal/*`, traceable, etc.) and are intentionally not ported.”

## 5.2. Implementation record (completed 2026-08-18)

**All 57 tests in `crates/rutis/tests/parity.rs` pass.** The 31 full-parity cases and the §3 kernels were split into individual tests; test names use snake_case forms of the original `it()` titles with source file/line comments. Fake timers were replaced with deterministic synchronization (Notify gates + watch-state observation). “Not settled” assertions rely on the gate not being released, not elapsed time. Declared channel deviations are recorded in comments: execution errors use the await channel and do not also enter ErrorSink; fiber-level dispose returns cleanup errors (TypeScript resolves + logs; D6/D20 channel).

**Parity exposed and fixed four implementation gaps** that the prior 72 tests had not covered:

1. **Missing inertia lock 2** (`fiber.spec:27`): if the same key is re-provided while loading, a queued recheck compares the old triple and reloads the generation (apply twice). Fix: when load completes, if the dependency set has been fully refreshed with no missing dependencies, adopt it in place (`fiber.rs` `load`); the in-flight load proceeds directly to ACTIVE, applying once.
2. **Same-key re-provide rejected during eviction window:** TypeScript releases the registry slot synchronously on dispose; Rust's two-phase removal returned `ServiceExists` for every provide before finalize. Fix: allow a new provide to replace a removing binding; the eviction finalizer checks Arc identity (`registry` `insert_binding` / `finalize_binding_if`) and cannot affect the replacement.
3. **Lost wakeup in TransitionTask** (same pitfall as §8 item 7): retaining only `watch::channel(..).0` drops the initial receiver, closing the channel immediately. `complete` then silently fails to send; if the consumer completes quickly (equality coalescing skips it) and the joiner subscribes late, the completion value is lost and join waits forever. The isolate parity case reproduced this deterministically. Fix: TransitionTask retains a permanent receiver.
4. **Dependency identity lacked scope:** the same provider fiber can provide the same key in different isolate scopes; `(PluginId, gen, TypeKey)` collides, so unloading the default scope can evict an isolated consumer. Fix: add scope to make dependency identity a four-tuple (exact D21 matching semantics; synchronize `last_deps` / `resolve_deps` / `consumers_of` / `evict_and_finalize`). Also fix `get` caller-liveness checks: services are invisible through Unloading/Disposed contexts, except self-access within the provider subtree (kernel of reflect.spec “service inject leak,” matching TypeScript inactive-context semantics).

**Case-by-case reconciliation** against the original `it()` list: 96 cases = 31 full + 27 partial + 38 excluded. All 31 §2 rows and 20 §3 rows landed; four rows contain multiple tests (dispose by plugin/manually, async yield 1–4, yield-with-error pair, on/once pair), expanding to 27 cases. The 58 parity cases produce 57 test functions; the sole difference is plugin `ctx.registry`, a coverage no-op in the original (only iterates registry; no `expect`, no portable kernel), which is explicitly exempted. The initial rough 27/21/48 count was corrected to 31/27/38 based on case-by-case counts.

**Result:** 57 parity + 58 contract + 13 agent + 1 doc = 129 tests; single- and multithreaded modes and 10+ consecutive runs all pass; clippy `-D warnings` and fmt are clean. The conditional “preserve isolate shared-label semantics” was verified against the current implementation and passes parity.

**Addition (2026-08-19): fill the emit-order parity gap.** Cordis JS `emit` invokes listeners inline and synchronously in emission order, so order comes for free and the spec never explicitly asserts it. Spawning one task per Rust event silently lost the guarantee (back-to-back same-type emits in multithreaded runs showed about 30% reordering, maximum displacement 52; single-threaded and ≥1 ms spacing showed none, so existing parity stayed green). Fix: emit tail chain (D31; serialize dispatch by emission order for the same event type, with remove/spawn/insert atomic under one lock). Regression tests: `rutis/tests/dispatch_chain_probe.rs` (concurrent same-type emit chain does not fork) and `rutis-agent/tests/order_probe.rs` (single emitter's order equals arrival order under multi_thread). This fills a Rust-side gap; it is not a semantic deviation. No ordering is promised across event types (declared boundary in D31).

## 6. External messaging (based on this checklist)

> Rust validation against Cordis: all 96 original specs reviewed individually. The Rust version automatically parity-tests 58 language-independent paradigm invariants (31 full cases + 27 kernel-only cases: fiber state/timing, exactly-once LIFO cleanup and non-flattened aggregation, dependency gates, cascading unload, dependency-triggered reload, and event dispatch). Thirty-eight JavaScript-specific mechanisms (Proxy property syntax, string events, `internal/*`, traceable/caller-shadow, `Context.filter`, synchronous bail, update config) are intentionally not ported under the design. Contract tests contain comments with Cordis source line references as semantic anchors.
