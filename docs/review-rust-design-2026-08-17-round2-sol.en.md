# Rust Port Design v2: Second Review — gpt-5.6-sol (2026-08-17)

> Review target: v2 of [design-rust-port.md](design-rust-port.en.md), later revised to v3 (see the v3 §0 “v2 → v3” table).
> Baseline: TypeScript source. Verdict: **v2 was not ready for M1 approval.** It improved substantially over v1: among eight prerequisite categories, four were fixed, five partly fixed, and none wholly unaddressed; however, the partial fixes included three M1 blockers.

## Final assessment

**Do not approve M1.** M2 coverage improved, but the current draft cannot claim complete implementation; “85–90% after M2” is too high. Estimated coverage: 65–75% if implemented as written, 78–85% after fixing the blockers in this report, and about 85% after M4. Suggested goal: “Reproduce Cordis's core lifecycle, event, dependency, and plugin-management semantics, while providing Rust alternatives for JavaScript dynamic objects, Proxy, and traceable surfaces.”

## Three M1 blockers

1. **`ListenerReturn` has no error channel** (D2/D7 contradiction). `Sync(Value)` / `Async(BoxFuture<Value>)` contain no `Result`, so they cannot implement synchronous errors being rethrown, async errors going to the sink, and parallel aggregating all errors. Add `Sync(Result<...>)` or a separate recoverable error type.
2. **Waterfall is made uniformly asynchronous.** TypeScript `waterfall()` is synchronous and returns a value directly (`events.ts:245-254`). `Next -> BoxFuture` forces `internal/get/set` (`reflect.ts:153-167/191-193`), `_resolveConfig` (`fiber.ts:743-746`), and ACTIVE update's validate-before-store flow to become async or use `block_on` (which can deadlock under Tokio). Provide both synchronous and asynchronous continuation forms.
3. **`serde_json::Value` cannot carry capabilities.** `internal/plugin` passes a Fiber, `internal/listener` replaces a disposer, `internal/get` returns service objects, and `internal/get/set` carry Context/Error. Use a layered `DynamicValue` or typed internal APIs.

## M2 issues

- `RuntimeKey` identity remains unresolved (§8): TypeScript resolves a plugin and uses the resulting apply-reference identity; allocating an ID per registration can incorrectly merge or split runtimes.
- Shared-future output must be cloneable. Clone semantics and error identity for `Result<(), CordisError>` are undefined; consider `Arc<CordisError>`.
- Store-key definitions conflict: `HashMap<LabelId, Impl>` versus D9's `(LabelId, name)`. TypeScript uses a single symbol key.
- C4 treats iterable/async-iterable effects as core, while M2 excludes `Stream`; reconcile the three conflicting statements.
- Intercept milestone conflicts: v1 says it is required for M2, v2 moves it to M4 while claiming M2 completes C1–C9 and reaches 85–90%.
- An `apply: fn` pointer cannot capture closure state; use a trampoline or `Arc<dyn Fn>`.
- There is no clear implementation decision or dedicated test for per-callback isolation of observers (`internal/plugin/status`).

## Other findings

- **Synchronous prefix of async `emit` (high risk):** Rust async functions run on first poll. Spawning directly loses TypeScript's “run until the first await” behavior. Poll once, establish a construction-time rule, or document the deviation.
- `once` (dispose then call) is consistent with snapshot dispatch, but needs three reentrancy tests.
- An 18-reference line-number spot-check was accurate, with two offsets (`emit` ends at 206; `dispatch` at 179).
- **Test counts are wrong:** there are 96 `tests/*.spec.ts` cases, not 62; `dispose.spec` has 13, not 14; `plugin.spec` has 10, not 11. The matrix lacks a case-by-case mapping and therefore does not substantiate 85–90% coverage.
- The v2 deviation list correctly reclassified traceable capability loss and moved ErrorSink into M1, but still omits five items: the severity of the JSON-only limitation, synchronous waterfall becoming async, async prefix behavior, closure state in plugins, and error identity/Clone.

## Minimum changes for M1 approval

Seven items: return a `Result` from listeners; solve or declare the `emit` prefix behavior; support synchronous and asynchronous waterfall continuations without `block_on` in `internal/get/set`; replace “all values are `Value`” with capability-aware `DynamicValue`; support replacement types for `internal/listener`; add once/snapshot/reentrancy tests; correct the 96/13/10 test counts.

Before M2 approval, also resolve seven items: exact `RuntimeKey` identity, shared-output Clone/Arc strategy, consistent store keys, consistent intercept milestone, C4/M2 conflict, captured state for `apply`, and per-callback observer-isolation tests.
