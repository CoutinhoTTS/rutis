# Rust Port Design v4 Review — ZCode / GLM-5.3 (2026-08-17)

> Review target: v4 of [design-rust-port.md](design-rust-port.en.md), the paradigm-oriented approach.
> Baseline: TypeScript source, three research documents, and the archived two rounds of v2 reviews.
> Verdict: **Do not approve M1 yet.** The direction and outline are sound, but the §2 API draft has four holes. As written, it cannot support code that passes the acceptance tests defined by the document itself (§5). Fix these four points and M1 can be approved.

## Overall assessment

The paradigm decisions are right. Two of the three v2 review blockers (dual-mode waterfall and `Value` payloads) were actually resolved by removing the `internal/*` surface and using typed events, not by evading the issues. References in the three research documents were checked individually and faithfully reflect their conclusions. The concurrency plan (single lock domain, watch for terminal state, CancellationToken, yield fallback) correctly translates Tokio guidance. The 11 “Python examples” cited in M3 were confirmed empirically.

The central problem is that **the contracts promised in §§4–5 are not supported by the API draft in §2**. Several required functions do not exist or have incorrect signatures.

## Four required fixes

### 1. Listener shape is undefined and contradictory

The draft says `on()` callbacks return `ListenerResult` (§2, line 52), but never defines `ListenerResult`. This matters because all five dispatch modes depend on it:

- §0, line 11 removes the sync/async dual mode and says “all-async BoxFuture.”
- Pillar 4 in §1 and D4 still say “synchronous bail short-circuit” and “synchronous emit.”

One listener cannot be purely asynchronous and also be called directly by synchronous `bail` to obtain a return value. This is the remaining v4 form of the top v2 blocker (“listener has no error channel”).

**Recommendation: commit to all-async and make `bail` async.** The `internal/*` surface has been removed, so the only TypeScript consumer that required synchronous bail (`internal/listener`) is gone. Synchronous behavior is a JavaScript convenience, not a paradigm invariant; short-circuit semantics remain intact. Suggested shape:

```rust
pub trait Event: Send + Sync + 'static {
    const NAME: &'static str;
    type Value: Send + 'static;   // bail value for each event; also resolves open question 2
}
// The sole listener shape: bail returns Some(Value), errors are explicit in Result
Fn(&Ctx, &E) -> BoxFuture<'_, Result<Option<E::Value>, CordisError>>
```

If synchronous bail is non-negotiable, restore the dual-mode design in D4 and explain why. The current draft supports neither position.

### 2. Cleanup functions cannot report errors

Both `Effect` cleanup variants (§2, lines 66–71) return `()`:

```rust
Disposer(Box<dyn FnOnce(&Ctx) + Send>),       // returns ()
AsyncDisposer(... -> BoxFuture<'static, ()>), // also returns ()
```

But the dispose acceptance tests in §5 require “one error returned as-is, multiple errors aggregated without flattening, and repeated dispose joins the same `Arc<E>`.” With `()`, there is nowhere to return errors, so none of these contracts can be implemented. Recording disposer rejections in the aggregate error is a core EffectRecord contract in TypeScript (`fiber.ts:506-534`); §4 also says this is fully retained in v4.

**Fix:** have both variants return `Result<(), CordisError>` (with a variant carrying `Box<dyn Error>` for non-Cordis errors).

### 3. Configuration disappears from the API

`Plugin::apply(&self, ctx)` accepts no config, `Ctx::plugin(&self, p)` accepts none, and `FiberView::update` has no argument. Yet:

- D12 is entirely about “validate-before-store.”
- `CordisError::Validation` exists (§2, line 43).
- §4 claims the two update branches are retained in v4; TypeScript's branches are about config validation (`fiber.ts:857-886`).
- Config is not in §1's “not doing” list.

Half the document discusses configuration while the API cannot represent it. Choose one:

- **Restore it (recommended):** `plugin(p, config)`, `apply(ctx, config)`, and `update(config)`.
- Or explicitly drop configuration: list it as out of scope and remove D12, the Validation variant, and related §4 claims.

### 4. Waterfall signature is wrong and cannot be registered

§2, line 57:

```rust
pub async fn waterfall<E: Event>(&self, ctx: &Ctx, e: &E, next: Next<'_>) -> BoxFuture<'_, Result<Value<E>, CordisError>>;
```

There are three problems:

1. An `async fn` returning `BoxFuture` is doubly wrapped. Use a normal `fn` returning `BoxFuture`, or an `async fn` returning `Result` directly.
2. `next` is in the wrong position. In TypeScript, the caller supplies the innermost terminal continuation (`events.ts:245-254`); listeners receive the wrapped `next`. It should be a fallback behavior argument to waterfall, not a peer of the event argument.
3. Most fundamentally, `on()`'s listener shape has no `next` parameter, so waterfall listeners cannot be registered. Add a separate `on_waterfall::<E>()` with a shape such as `Fn(&Ctx, &E, Next<'_>) -> BoxFuture<'_, ...>`. This also addresses the v2 signature-lifetime blocker.

It is reasonable to defer the shape of open question 2 (`Value<E>`) until M1 implementation. Missing a registration API is not an open question; it is a gap.

## Secondary issues (should fix, but do not block starting implementation)

| # | Issue | Notes |
|---|---|---|
| 5 | `ctx.effect()` is missing | Pillar 1's cleanup registration, the `effect_yields_disposer` test, and listener cleanup on fiber unload all depend on it, but it is absent from the Ctx draft. Also, `Effect::Many(Vec)` cannot represent TypeScript's incremental async-iterable effects, which are not listed as an intentional deviation. Add an API or document the deviation. |
| 6 | Events and isolate are unspecified | Isolate covers registry separation, but the document never says whether events cross scopes (TypeScript uses `Context.filter`); §5 has no corresponding test. |
| 7 | Fiber state transitions cannot be observed | Watch exposes only the latest value, so slow subscribers may miss intermediate transitions (Loading → Active, etc.). The §5 `state_transitions` test has no observation mechanism. Add a typed state event on the bus or explicitly promise visibility only of terminal state. |
| 8 | `PluginFailed(Box<dyn Error>)` is contradictory | `apply` already returns `CordisError`; wrapping again loses `Arc<CordisError>` identity, which an exactly-once test relies on. Remove it or use `Arc<CordisError>`. |
| 9 | Panic policy is unspecified | “Spawn + contain” encounters `JoinError(panic)`, not `Err`; disposer panic handling via `catch_unwind` is also unspecified. Decide how to handle the equivalent of TypeScript routing listener throws to the logger, even if the decision is to propagate panic. |
| 10 | Dependency cycles are untested | `InjectUnsatisfied` mentions cycles, but the §5 gating tests do not check cycle detection, and D14 does not say what eviction order does in a cycle. |

## Small errors (fix opportunistically)

- The dependency list at line 103 omits **thiserror**, even though line 37 uses `#[derive(thiserror::Error)]`.
- “Do not add async-trait (thiserror optional)” appears to contain a typo.
- The claim “no futures-util” assumes parallel dispatch uses spawn + `JoinSet`; if it instead uses `FuturesUnordered`/`join_all`, futures-util is required. State the choice.
- `provide<T>(value: T)` and `provide_as<T>(value: Arc<T>)` are asymmetric; `on` returns a bare `Disposer` while `provide` returns `Result`, so error handling is inconsistent.
- It is unspecified whether two `isolate(label)` calls with the same label share a scope, as in TypeScript.
- §4's “v4 retains all” overstates the case: §7.3 suggests deferring the two-branch update to M4, and config is missing. Change this to “retained (see X) / deferred to M4.”

## Approval conditions

The four required fixes do not require backtracking:

- #1 and #4 complete the all-async design (`bail` async, separate `on_waterfall` registration, associated `Event::Value`).
- #2 adds `Result` to both Disposer variants.
- #3 restores config parameters to the three signatures.

After revision, promote these decisions to D16–D19, remove the now-decided open question 2 from §7, and change the document status from “final” back to “draft” until M1 acceptance passes. Secondary issues and small errors can be cleaned up during M1 implementation, but #5 (`ctx.effect()`) and #7 (state observation) should be resolved alongside the four required fixes because they also affect the API skeleton.
