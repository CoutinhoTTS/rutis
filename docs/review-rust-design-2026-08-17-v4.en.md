# Review Report: `design-rust-port.md` v4

> 2026-08-17. Review target: v4 of [design-rust-port.md](design-rust-port.en.md), the paradigm-oriented approach. Three parallel reviews covered Rust technical correctness, fidelity to TypeScript semantics, and internal document consistency, with checks against `src/events.ts`, `src/fiber.ts`, `src/context.ts`, and `src/reflect.ts`.
>
> **Overall conclusion:** The paradigm direction is sound. Key decisions such as D5 (short `std::Mutex` critical sections), D6 (watch for terminal state), D7 (`CancellationToken`), D9 (`yield_now` provides no ordering guarantee), and D14 (Erlang/OSGi precedent) are supported by the research. The issues are concentrated in two areas: the §2 API draft has four blocking defects (it cannot compile or cannot implement the stated design), and the document contains several internal contradictions. Revise before starting M1.

## 1. Blocking issues (must fix before M1)

### 1.1 `emit` signature conflicts with async listeners (lines 52–53)

```rust
pub fn emit<E: Event>(&self, ctx: &Ctx, e: &E);  // synchronous call; async variant spawns + contains
```

`tokio::spawn` requires `'static`, so borrowed `&E` / `&Ctx` cannot enter the task. Use `Arc<E>` payloads or require `E: Clone`. In addition:

- `ListenerResult` (the callback return type for `on` at line 52) is never defined and is not listed as an open question in §7.
- The document never says whether “synchronous call + async variant” means two modes of one method or two methods. The `emit_sync_async_contained` test in §5 (line 118) cannot be written without that decision.

**Fix:** standardize on `Arc<E>` (or `E: Clone`), define the minimum `ListenerResult` shape, and distinguish `emit` from its async variant.

### 1.2 `waterfall` returns a future twice and cannot compile (line 57)

```rust
pub async fn waterfall<E: Event>(...) -> BoxFuture<'_, Result<Value<E>, CordisError>>;
```

An `async fn` already wraps its return value in a Future; returning `BoxFuture` adds a second layer. Either use a regular `fn -> BoxFuture` or remove `BoxFuture`. `Next<'_>` is also undefined (it likely needs a shape such as `&mut dyn FnMut(&Ctx, E) -> BoxFuture<'_, ...>`), and the signature conflicts with D1/D4's “call → Future” description.

### 1.3 Synchronous `bail` conflicts with a unified callback type (lines 52, 56)

`bail` is synchronous and can only drive synchronous callbacks, but the callback registered by `on` appears able to return a future. The TypeScript source confirms that bail itself is purely synchronous ([`events.ts:228-233`](../src/events.ts#L228)). The design must choose and state either “bail accepts only synchronous listeners” or “bail is async.”

### 1.4 Plugin ownership and the Ctx ownership model are missing (lines 61–64, 78)

`ctx.plugin(p: impl Plugin)` takes the plugin by value. `restart`/`update` must call `apply` again, so the implementation must retain `Arc<dyn Plugin>`, but the document does not say so. `isolate(&self) -> Ctx` and `Effect::Disposer(Box<dyn FnOnce(&Ctx)>)` also imply that Ctx can be cloned (an Arc-backed kernel), but this foundation for every signature is undefined.

**Fix:** add an ownership-model section to §2: `Ctx = Arc<CtxInner>`, store `Plugin` as `Arc<dyn Plugin>`, and define where the Disposer's Ctx comes from.

## 2. Internal contradictions (the document disagrees with itself)

| # | Location | Contradiction |
|---|---|---|
| 2.1 | §0 (line 19) / D11 (line 97) vs §2 (line 37) | Says thiserror is not part of the public API, but public `CordisError` directly derives `thiserror::Error` and appears in every public signature (lines 54/57/64/75). “thiserror optional” at line 103 has no feature-gating plan. |
| 2.2 | §4 (line 107) vs §7.3 (line 137) | The two-branch `update` is called a paradigm invariant retained in v4 (it exists in source, [`fiber.ts:857`](../src/fiber.ts#L857)), but §7.3 leans toward cutting it until M4. |
| 2.3 | §5 (line 124) vs its table | Minimum acceptance is 5 pillars × (4 positive + 2 failure cases) = 30, outside the stated “~35–45” range. The reload group has only 3 representative tests, fewer than the promised 4+2. |
| 2.4 | §6 M1 (line 128) vs §5 table | M1 promises events/cancel groups, but `cancel_wakes_awaiters` (line 121) and `listener_unloads_with_fiber` (line 118) both require the M2 fiber and cannot run in M1. |
| 2.5 | §0 (line 16) vs D5 (line 91) | §0 says a token cannot be a sequence number, but D5's token includes a “force generation” (which is a sequence number). “Equality coalescing” has no source in the three research documents. |
| 2.6 | D12 (line 98) vs §2 (lines 61–65) | D12 says plugins provide `fn validate(config)`, but the `Plugin` trait has no such method. |

## 3. Factual differences from TypeScript source

### 3.1 Claims that contradict the source

1. **Isolate granularity differs (line 79).** `Ctx::isolate(label)` has no service-name parameter and isolates the whole context. TypeScript `isolate(name, label?)` isolates one service by name, and repeated calls with the same label share scope ([`context.ts:123-127`](../src/context.ts#L123)). This is a semantic difference and is not identified as one.
2. **“Reentry does not crash” hides source behavior (line 120).** In TypeScript, calling `effect()` during UNLOADING throws `INACTIVE_EFFECT` ([`fiber.ts:434-436`](../src/fiber.ts#L434), [`reflect.ts:278`](../src/reflect.ts#L278)). This is a deliberate error, not simply “no crash.”
3. **Inject gating omits the `check()` predicate (line 107, D14).** Gating requires both an implementation and a successful `impl.check()` predicate; failure evicts the consumer ([`fiber.ts:689-701`](../src/fiber.ts#L689), [`service.ts:15`](../src/service.ts#L15)).

### 3.2 Source capabilities whose status is unspecified

1. **The `this` dimension of dispatch:** the first argument to all five TypeScript dispatch modes can be a `thisArg`, used to bind listeners and filter through `Context.filter` ([`events.ts:170-179`](../src/events.ts#L170), [`context.ts:46`](../src/context.ts#L46)). The “five dispatch semantics” list at line 27 omits this orthogonal dimension.
2. **`on` options `prepend`, `global`, and `once** ([`events.ts:114-119`, `:323-329`](../src/events.ts#L114)): registration order affects serial/bail/waterfall results, so it is part of dispatch semantics.
3. **`extend()` / `intercept()`:** prototype inheritance for child contexts and merged service-configuration interception ([`context.ts:101-147`](../src/context.ts#L101), [`service.ts:86-102`](../src/service.ts#L86)); inject objects can carry intercept config ([`registry.ts:19`](../src/registry.ts#L19)). Intercept is part of the dependency-declaration paradigm, but its inclusion/exclusion is not explicit.
4. **Service lookup along the fiber parent chain** ([`reflect.ts:154-166`](../src/reflect.ts#L154)): reads search ancestors and check isolate keys, whereas the draft describes only a flat TypeId registry.

### 3.3 New semantics that are not identified as such

1. **“Correct unload order” / `eviction_order` (lines 28, 120):** TypeScript notification gathers affected fibers and drains them concurrently with `Promise.allSettled` ([`reflect.ts:299-336`](../src/reflect.ts#L299)); it has no eviction-order contract. The document claims order twice without labeling it as new semantics.
2. **`Arc<E>` identity in `exactly_once_same_error`:** TypeScript joins by waiting while `this.inertia` exists ([`fiber.ts:307-309`](../src/fiber.ts#L307)), but has no “same terminal object” contract. D6 labels this as a strengthening, which is acceptable.

### 3.4 Claims verified against source

EffectRecord LIFO ([`fiber.ts:508`](../src/fiber.ts#L508)); aggregation without flattening ([`fiber.ts:125-130`](../src/fiber.ts#L125)); exactly once ([`fiber.ts:548-552`](../src/fiber.ts#L548)); six states ([`fiber.ts:160-167`](../src/fiber.ts#L160)); five dispatch modes ([`events.ts:32`](../src/events.ts#L32)); synchronous bail; serial returns the first bail value; waterfall veto; and access during cleanup ([`reflect.ts:297-303`](../src/reflect.ts#L297)). The §4 claim that line references are reliable is broadly correct.

## 4. Clarifications needed (not blockers, but must be documented)

1. **D6 watch join protocol is incomplete (line 92).** To determine whether state is terminal, first `borrow()` then await `changed()`. If the fiber is dropped, the sender closes and `changed` returns Err, which cannot distinguish terminal state. “Repeated dispose joins the same terminal result” also assumes the fiber handle remains alive after dispose; this is unstated.
2. **Unload semantics (line 99):** A consumer's cached `Arc<T>` keeps a service instance alive after “unload”; unload only removes it from the registry. State this clearly, otherwise reload tests have no basis.
3. **D14 eviction race (line 100):** A consumer can be evicted while a callback started outside the lock is running. A single-lock protocol is insufficient, and the complexity is underestimated.
4. **`provide(T)` vs `provide_as(Arc<T>)` asymmetry (lines 75–76):** the former cannot register a trait object; whether this is intentional is unstated.
5. **`on(&self)` requires interior mutability** (`Arc<Inner> + Mutex`), which is not declared. The path by which a Disposer removes its registration is also unspecified.
6. **`AsyncDisposer` lifetime trap (line 69):** it returns `BoxFuture<'static, ()>` while taking `&Ctx`; capturing that borrow in the future fails to compile. Document that the disposer must first clone an owned handle.
7. **No serial test:** the events group at line 118 covers emit/parallel/waterfall/bail but not serial; pillar 4's five dispatch modes are incomplete. Pillar 1's 0..n assembly also has no independent test group.
8. **Dependencies:** a hand-written BoxFuture alias needs no futures crate, but parallel aggregation should state that it uses Tokio `JoinSet` instead of `FuturesUnordered`. The line 36 comment “= futures::future::BoxFuture” may imply a dependency.
9. **ErrorSink** appears only once in D4 (line 90) and M1 (line 128), with no definition, API, or test group.
10. **`Option<Value<E>>` from serial/bail (lines 55–56):** the meaning of `Option` (no listeners? veto?) is never explained.
11. **M4 “state-migrating hot update” (line 131):** the research says to evaluate state migration after M4, but the design places it in M4. The boundary between pillar 5 / M2 dependency reload (without state migration) and state migration (with it) is unclear.

## 5. Recommended disposition (in priority order)

1. **Rewrite event signatures in §2:** use `Arc<E>` payloads (or require `E: Clone`); define minimal `ListenerResult`, `Next`, `Disposer`, and `Value<E>` shapes; move `Value<E>` from §7 to an M1 blocker; fix the double-Future waterfall signature; clarify synchronous bail.
2. **Add an ownership-model section to §2:** `Ctx = Arc<CtxInner>`, store plugins as `Arc<dyn Plugin>`, and define the Disposer's Ctx source.
3. **Resolve internal contradictions:** choose whether update is retained or deferred (recommend deferring to M4 per §7.3 and update §4 accordingly); align error API statements with §2; make acceptance counts consistent (for example, “≥30, target 40”); remove M1 test promises that depend on the M2 fiber.
4. **Add a “deviations from TypeScript” list to §4:** explicitly decide isolate granularity, reentry errors, new eviction-order contract, parent-chain lookup, and whether intercept/check predicates are included.
5. **Add eviction-race analysis to D14:** define what happens to a consumer evicted while its callback is running outside the lock.

The paradigm direction and ecosystem evidence do not need revision. The issues are concentrated in Rust lifetime details in the API draft and internal consistency of the document.
