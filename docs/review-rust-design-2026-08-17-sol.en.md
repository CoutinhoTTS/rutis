# Rust Port Design v1 Review Report — gpt-5.6-sol (2026-08-17)

> Reviewed: `docs/design-rust-port.md` v1 (revised to v2 in response to this report; see v2 §0 for the change summary).
> Baseline: TypeScript source under `src/`. Decision: **the current design cannot fully reproduce the TypeScript Cordis implementation; do not approve M1 as currently written.**
> The original was preserved unchanged.

---

## Overall Decision

**Conclusion: the current design cannot fully reproduce the work of the TypeScript Cordis implementation; do not approve M1 as currently written.**

It can implement a strongly typed Rust plugin framework inspired by Cordis, but it is not a semantic port of the minimal TypeScript kernel. The main blockers are:

1. `waterfall` was translated as “mutable value passing,” while TypeScript actually uses a veto-capable, nestable continuation/middleware chain.
2. The typed-event design loses open extension of string events, a unified listener set, dynamic return values, and `thisArg` filtering semantics.
3. The `Plugin` trait incorrectly restricts plugins to “creating one Service,” and cannot express TypeScript plugins with no service, multiple services, or only effects.
4. Configuration validation is placed inside `create`, so `update()` cannot guarantee zero side effects on validation failure.
5. `BoxFuture` has no lifetime parameter; the documented `init(&Context) -> BoxFuture<_>` usually will not compile.
6. `reflect`/provider/inject, intercept, shared Runtime, internal waterfall extension points, and fiber inertia all lack implementation-level contracts.
7. The tests and the tenth contract item mainly come from the Python agent example, not from TypeScript ground truth.

Revise the design before implementing M1. If the goal is renamed to “Cordis-inspired typed Rust subset,” implementation can proceed, but the claim of “fully reproducing TypeScript” must be dropped.

## 1. Item-by-item Check of the Ten Contracts in Section 2

### 1.1 Fiber state machine: direction is right, but it is oversimplified

- TypeScript derives stable state from `uid`, `_error`, and epoch (`fiber.ts:647-652`).
- Provider/inject changes are serialized through `inertia` into `_reload()`/`_unload()` (`fiber.ts:704-717`).
- Plugin execution errors are logged inside `_reload()` and written to `_error` only if the current epoch is still valid (`fiber.ts:749-778`).
- `await()` waits for all inertia before throwing startup/configuration errors (`fiber.ts:817-823`).
- A class plugin first runs instance init hooks and then calls `instance[symbols.init]()` (`fiber.ts:265-273`).

### 1.2 Inject gating: the main flow is right, but provider availability is missing

- After construction, `_checkImpl()` runs for each item and then `_refresh()` (`fiber.ts:324-333`).
- `_refresh()` builds the epoch from provider fiber UIDs; a missing dependency makes the fiber INACTIVE (`fiber.ts:720-740`).
- Notifications update fibers filtered by inject name and isolate scope (`reflect.ts:307-335`).
- Missing behavior: a provider can have a `check` predicate (`reflect.ts:115-125`). `check` runs with the traceable service as `this`; either false or an exception makes the dependency unavailable (`fiber.ts:689-700`). Strict get requires the provider fiber to be ACTIVE (`reflect.ts:233-243`). Notification scans every fiber in the shared Runtime (`reflect.ts:314-329`).

### 1.3 EffectRecord disposal contract: core direction is right, scope description is imprecise

- Execution and cleanup use separate channels (`fiber.ts:438-450`); cleanup runs all effects, strictly LIFO within one effect, and serially across async cleanup (`fiber.ts:506-533`). A single error is returned unchanged; multiple errors become an `AggregateError` without flattening (`fiber.ts:124-130`). A dispose task runs exactly once and repeated calls join it (`fiber.ts:548-581`). `await effect` yields the disposer itself (`fiber.ts:630-632`). A stale generation's failure does not contaminate a newer generation (`fiber.ts:749-778`).
- Correction A: strict LIFO for all cleanups holds only inside one EffectRecord. At the fiber-unload level, `_disposables.clear()` takes items in reverse order but cleans them concurrently with `Promise.all` (`fiber.ts:781-799`).
- Correction B: cleanup-error observation differs for the direct caller and structural owner; `cleanupReporters` ensures it is reported exactly once (`fiber.ts:468-483`, `789-798`).

### 1.4 Five event modes: the translation is seriously inaccurate

- `emit`: TypeScript invokes listeners synchronously on the current call stack and attaches `.catch()` only to promise-like results (`events.ts:194-206`). Replacing this with `tokio::spawn` changes the synchronous prefix.
- `parallel`: throws an `AggregateError` after `Promise.allSettled` (`events.ts:182-192`); the design does not specify aggregation of multiple errors.
- `bail` condition: `null`, `undefined`, and `false` do not bail (`events.ts:8-15`); `Option::Some` cannot be claimed as an exact reproduction.
- **`waterfall` (the most serious issue):** a continuation/middleware chain. The final argument is the inner `next`; calling it enters the next layer, and not calling it vetoes the chain. A listener can wrap both the call and return, and the outermost return value is the result (`events.ts:235-254`). An `&mut V` fold cannot express veto, nesting, or around behavior. This affects `internal/config` (`fiber.ts:743-747`), `internal/update` (`fiber.ts:857-885`), `internal/get/set` (`reflect.ts:144-196`), and per-fiber update hooks (`events.ts:145-160`).
- Other omissions: prepend/global (`events.ts:113-125`), once (`events.ts:315-329`), replacement of `internal/listener` bail handlers (`events.ts:299-312`), firing `internal/dispatch` before non-internal events (`events.ts:170-179`), and binding listeners through reflect trace (`events.ts:304-307`).

### 1.5 Scoped dispatch: the description is too broad

Dispatch accepts an optional explicit `thisArg`; filtering uses `thisArg[Context.filter]`. Only non-global listeners are filtered, and the filter receives the listener's owning context (`events.ts:170-179`). Only the default Service filter compares isolate labels by service name (`service.ts:61-63`). Preserve the `global` bypass.

### 1.6 Plugin contract: covers only the class-like happy path

- TypeScript supports three entry shapes: function, constructor, and `{ apply }` object (`registry.ts:91-126`, `216-222`).
- Function/object plugins may directly return a disposer, promise, iterable, or async iterable; they need not create a Service (`fiber.ts:370-413`).
- `update` is an `internal/update` waterfall and can veto or replace (`fiber.ts:843-885`).
- An update on a non-ACTIVE fiber stores raw config, forces a new generation, and swallows startup errors (`fiber.ts:857-876`).

### 1.7 Config validation: mixing validation and construction breaks atomicity

- TypeScript has a separate `resolveConfig()`: a Standard Schema v1 entry point, input-to-output normalization, aggregated issues, and rejection of async validators (`fiber.ts:16-61`, `utils.ts:26-55`).
- An ACTIVE fiber resolves the new config before modifying `_config`/`config` (`fiber.ts:877-884`).
- “The validator is an explicit check inside create” makes validation inseparable from side effects, cannot reproduce normalization, and forces `update` to construct a new service just to validate.

### 1.8 The root fiber never dies: conclusion is correct

After construction, root clears bootstrap disposables (`context.ts:70-85`); root disposal is bound to `restart()` (`fiber.ts:334-346`).

### 1.9 `ctx.set` within the same fiber does not notify: correct, but set permissions are missing

Set is allowed only for a service that has already been provided and whose provider is the current fiber; it replaces the value without notifying (`reflect.ts:245-265`).

### 1.10 Agent loop: not a core TypeScript Cordis contract

There is no agent loop under `src/`; item 10 should move to “Rust agent example acceptance criteria.”

## 2. Important Existing TypeScript Mechanisms Missing from the Design (Decision Table)

| Mechanism | Evidence | Decision |
|---|---|---|
| Actual continuation waterfall | events.ts:235-254 | Required for M1 |
| Aggregate multiple parallel errors | events.ts:188-192 | Required for M1 |
| Synchronous `emit`, containing only promise rejections | events.ts:200-205 | Required for M1 |
| prepend/global/once | events.ts:113-125, 299-329 | Required for M1 |
| Generic `thisArg[filter]` protocol | events.ts:170-179 | Required for M1 |
| internal/dispatch | events.ts:170-175 | Required for full M1 parity |
| Replace `internal/listener` registrations | events.ts:299-312 | Required for M1/M2 |
| `extend()` inheritance and shadow preservation | context.ts:92-109 | Context scope partially required for M1/M2; dynamic shadow for M4 |
| Isolate name-to-label mapping | context.ts:111-127 | Required for M1/M2 |
| Intercept prototype chain/ancestor merging | context.ts:129-147, service.ts:75-102 | Required for M2 |
| Provider scope-key store and duplicate-provide errors | reflect.ts:277-304 | Required for M2 |
| Provider check predicate | reflect.ts:115-125, fiber.ts:689-700 | Required for M2 |
| Notify/wait for dependents before deleting a provider's own store on unload | reflect.ts:297-303 | Required for M2 |
| Strict/non-strict get | reflect.ts:225-243 | Required for M2 |
| internal/get/set waterfall | reflect.ts:144-196 | Required for full M2 parity |
| Accessor/mixin | reflect.ts:338-390 | M4; omitting it loses public extension capability |
| Service intercept config merge | service.ts:75-102 | Required for M2 |
| Callable Service/invoke+extend | service.ts:45-73 | M4; permanent removal must list lost public capability |
| Share Plugin Runtime by callback | registry.ts:128-138, 311-325 | Required for M2 |
| Registry map API and delete-by-callback | registry.ts:224-285 | delete/runtime required for M2; iteration for M4 |
| Function/class/object plugin shapes | registry.ts:91-126 | Capability required for M2 |
| Standard Schema validation entry and normalization | fiber.ts:50-61, utils.ts:26-55 | Required for M2 |
| internal/config and internal/update waterfalls | fiber.ts:743-747, 843-885 | Required for M2 |
| Inertia and load/unload sequencing | fiber.ts:704-717, 781-809 | Required for M2 |
| Forced generations/stale isolation | fiber.ts:720-740, 764-777 | Required for M2 |
| Effect iterable/async iterable | fiber.ts:76-101, 370-413 | Iterable required for M2; diagnostic metadata for M4 |
| Per-callback isolation for state/plugin observers | fiber.ts:132-149, 654-670 | Required for M2 |
| Logger/error sink | events.ts:203-205, fiber.ts:763-769 | Minimal error sink required for M1; full logger for M4 |
| traceable/caller/shadow | utils.ts:166-275 | M4; cannot be called “not a loss” |
| Full logger naming/service calls | logger.ts:44-82 | Full logger optional for M4; error sink cannot be deferred |

The following can be permanently omitted only if the goal changes to a “Rust-idiomatic subset”: JavaScript Proxy property syntax, callable-object surface shape, JS prototype identity, cross-realm brand, concatenated JS error stacks, and arbitrary JS object/class/apply entry shapes. Their capability equivalents must still be preserved.

## 3. Assessment of Rust Mechanism Mapping

### 3.1 Typed events §4.2 (blocking)

- `(TypeId, NAME)` splits same-name events and loses cross-crate interoperability.
- Statically splitting synchronous and asynchronous listeners into two sets cannot preserve one prepend order or the synchronous prefix of emit.
- The waterfall signature is wrong; use an owned payload, indexed chain, and boxed continuation.
- Listener owner context and dispatch receiver are missing.

### 3.2 BoxFuture §4.3 (blocking)

`Box<dyn Future + Send>` omits a lifetime, defaulting to `'static`; an async block borrowing `&self`/`&Context` is not `'static`, so the signature does not compile. Use `BoxFuture<'a, T>` or owned `Arc` parameters. This affects every async surface.

### 3.3 Lock hygiene §4.4

Necessary but not sufficient: a check-then-act race needs a generation-token commit check; a JoinHandle cannot be joined multiple times, so shared completion state is needed (and spawning requires `'static`); `FiberView`'s `IntoFuture` should own an Arc and loop on notification. Top-level TypeScript unload runs concurrently with `Promise.all`; do not implement it as global serialization.

### 3.4 Panic policy §4.5

If listeners are not spawned, a panic during poll does not become a JoinError; spawning everything breaks the synchronous prefix. Define four paths: synchronous Err, synchronous panic, async Err, and async panic. Normal parallel Err values should be aggregated and returned, not routed to the sink.

### 3.5 Plugin trait §4.6 (M2 architecture blocker)

- Requiring exactly one Service is wrong (plugins may provide zero or multiple services, or only effects).
- Associated types/constants are unsuitable for a dyn registry; use a type-erased descriptor plus trampoline.
- Downcasting `dyn Service` needs explicit `as_any` or `Arc<dyn Any>`. Define behavior for type changes across generations, get with a mismatched type, set replacing a type, and isolate-store keys.
- `Send + Sync` is a real capability restriction and must be listed as a deviation.
- Validation in create is unacceptable: separate pure `resolve_config` from side-effecting `apply`.

## 4. Review of the Deviation List

- `ctx.foo` → `get`: syntax may differ, but the underlying resolution capability cannot also be removed.
- traceable: this is a capability loss and should be described honestly.
- Typed events: lose open event capabilities (cross-crate interoperability, runtime event names, generic observation of internal/dispatch, dynamic return values).
- Plugin trait capability loss is missing from the list (open shapes, zero/multiple services, compile-time registration, Send+Sync).
- Changing waterfall to value passing is not a deviation; it is incorrect.
- schemastery is not Standard Schema; the validation entry point is a preserved capability.
- An M4 logger conflicts with M1 events: a minimal ErrorSink is required for M1.
- Do not use Python `on_error` to define TypeScript parity.

## 5. Coverage and Approval

- Coverage by surface method names: 60–70%; by expressible programs: 35–45%.
- Expressible: compile-time fixed event set, one strongly typed payload, no waterfall veto, one Send+Sync Service per plugin, simple validation, static dependencies, simple gating, no accessor/mixin/traceable, no runtime registry operations, and the example agent.
- Not expressible (14 categories): internal/update veto/wrapping; internal/config/get/set continuation; listener-only plugins; multi-service plugins; dynamic `check()` availability; intercept ancestor merging; shared callback registry management; runtime string events/same names across crates; async synchronous prefix; prepend/global/once order; caller-shadow; Standard Schema normalization; non-Send+Sync state; iterable effects.

**Do not approve M1.** Minimum approval conditions: waterfall continuation; emit does not spawn; aggregate all settled parallel results; listener owner/receiver/filter/global/prepend/once; an event identity decision (string primary key or an explicit statement of TypeId loss); M1 ErrorSink; `BoxFuture<'a>`; and moving Python agent tests out of the contract matrix. Before M2, also require a type-erased descriptor, plugins not bound to one Service, pure validation, `(scope, name)` store and owner-only set, provider check, inertia/generation/waiter, exactly-once shared disposal, intercept merging, shared Runtime, and delete-by-callback.

## Unresolved Uncertainties

- There is no Rust implementation, so concrete failure modes for Send/Sync, shared disposal waiters, and the dyn registry still need implementation-level validation.
- The 11 agent items have not been checked individually against the Python source (which is not TypeScript ground truth).
- Coverage is estimated, not measured with two implementations.
