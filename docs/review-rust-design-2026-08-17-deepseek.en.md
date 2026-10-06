# Rust Port Design v1 Review — deepseek-v4-pro (2026-08-17)

> Review target: v1 of [design-rust-port.md](design-rust-port.en.md), subsequently revised to v2 (see the v2 §0 change summary).
> Baseline: 9 files under `src/`, 62 `tests/*.spec.ts` cases, and 10 `test/core.test.ts` cases. Verdict: **Do not approve full M1–M3 implementation; revise the design first.**
> The source document was left unchanged.

## 0. Overview

The direction is right (contracts before mechanism mapping; traceable surfaces are out of scope), and the happy-path lifecycle plus five dispatch modes are described mostly faithfully. There are three systemic problems:

1. **Wrong baseline:** §6's test matrix is anchored to Python test names/contracts. Contract 10 is a Python port artifact; the TypeScript repository has no agent loop.
2. **Capabilities silently omitted:** Standard Schema validation, merged intercept configuration, callable Service (`internal invoke/extend`), `internal/*` extension points, generator effects, once/prepend/global, and registry delete-by-callback are in neither the “not doing” list nor the deviation list.
3. **Concurrency mapping will fail in implementation:** `BoxFuture<'static>` cannot capture borrowed `&Context`; an RwLock is not reentrant and deadlocks on synchronous reentrant listeners; `tokio::spawn` assumes a runtime is always present; inertia is reduced to a “disposal task handle,” losing its core synchronization behavior.

Estimated net capability coverage if implemented as written: about 50–60%, concentrated in the surface layer.

## 1. Compare the ten contracts one by one

### Contract 1 (state machine) — mostly faithful, one ordering description is wrong

- `FiberState` (`fiber.ts:160-167`); `_getState` derives state as `uid===null → DISPOSED`; `_error → FAILED`; `epoch!==INACTIVE → ACTIVE`; otherwise `PENDING` (`fiber.ts:647-652`).
- PENDING → LOADING is triggered when `_refresh()` computes a nonempty epoch and `_setEpoch` calls `_reload` (`fiber.ts:729-741`, `704-718`), not merely because injection is complete.
- `init` is the `Service.init` symbol; its return value is the effect (`fiber.ts:264-272`).

### Contract 2 (inject gating) — direction is right, ancestor lookup and `check` are missing

- The notification filter compares `ctx[symbols.isolate][name]` (`reflect.ts:314-336`).
- Missing: `ctx.foo` store lookup along the ancestor chain (`reflect.ts:155-166`, walking `fiber.parent.fiber`, comparing isolate labels, and short-circuiting with an error if `prop in fiber.inject`); and the `Impl.check` predicate (`reflect.ts:123-124`, `fiber.ts:689-701`), whose false/throw result leaves the consumer Pending.

### Contract 3 (disposal) — conclusion is faithful, but effect shapes and report deduplication are missing

Compared with `fiber.ts:429-634`: full LIFO and serial async cleanup (`runCleanups` uses `splice(0).reverse()` and awaits each); one error returned as-is and multiple errors aggregated without flattening (`combineCleanupErrors`); exactly once with repeated calls joining (`startDisposal`); awaiting an effect yields a disposer (`wrapper.then`); stale generation check (`_reload`, `epoch===oldEpoch`). All are correct.

Missing: (a) effect shapes `Disposable | Promise<Disposable> | Iterable | AsyncIterable` (`fiber.ts:83-93`) and epoch-abort semantics; (b) “report failure exactly once” deduplication through `cleanupReporters` / `effectInertia` (`fiber.ts:112-117`, `468-484`).

### Contract 4 (events) — three distortions/omissions

- TypeScript emit contains errors through `ctx.logger.error` (`events.ts:200-207`); the design's “route to on_error sink” is Python terminology.
- Parallel's `AggregateError` behavior is omitted (`events.ts:188-192`).
- Waterfall is fundamentally misrepresented: it uses CPS; listeners receive `(...args, next)` and can veto or replace the result (`events.ts:245-254`). An `&mut V` fold cannot express this.
- Serial's return of the first bail value is omitted (`events.ts:215-220`).

### Contract 5 (scoped dispatch) — mostly right, but “label field” is singular and global is missing

Filtering uses `thisArg[Context.filter]` plus `hook.global` (`events.ts:176-178`). Service filtering compares isolate labels by service name (`service.ts:61-63`). Store is keyed by label (`ReflectService.store: Dict<Impl, symbol>`, `reflect.ts:209`, `237-243`); keying by name breaks isolation. `global` and `prepend` are omitted.

### Contracts 6/7 (plugin + validation) — PENDING update branch, internal/update, and Standard Schema are all missing

- Non-ACTIVE update branch: delayed validation, committing `_config`, and forced refresh into a new generation (`fiber.ts:861-876`).
- `internal/update` waterfall can veto or replace restart (`fiber.ts:880-885`).
- `resolveConfig` invokes `runtime.Config['~standard'].validate` and throws aggregate `ValidationError` (`fiber.ts:50-62`, `utils.ts:31-56`); removing schemastery does not mean the Standard Schema entry point was removed.

### Contract 8 (root does not die) — faithful

Root dispose means restart (`fiber.ts:345`).

### Contract 9 (`set` does not notify) — faithful

See `reflect.ts:254-265`.

### Contract 10 (agent loop) — not a TypeScript contract

Label it as a “Python example port.”

## 2. Uncovered mechanisms (decision table)

| Mechanism | Evidence | Decision |
|---|---|---|
| `Impl.check` predicate | `reflect.ts:123-124`, `fiber.ts:689-701` | Required in M2 |
| Ancestor-chain store lookup | `reflect.ts:155-166` | Required in M2 |
| Store keyed by isolate label | `reflect.ts:209`, `237-243` | Required in M2 |
| All `internal/*` extension points | `events.ts:340-363`, etc. | M4 or explicit non-goal; leaning toward retaining update/config in M2 |
| Standard Schema validation entry point | `fiber.ts:50-62` | Required in M2 or explicitly declare it dropped |
| Merged intercept configuration | `context.ts:141-147`, `service.ts:86-102` | M4 or explicitly dropped |
| Callable Service (`invoke`) + `extend` | `service.ts:17-21`, `65-73` | M4/permanent non-goal, but declare it |
| Accessor / mixin | `reflect.ts:345-391` | M4/permanent non-goal, but declare it |
| once/prepend/global | `events.ts:113-119`, `299-329` | Add prepend/once in M2; global is coupled to contract 5 |
| Registry delete-by-callback + runtime identity + multiple forms | `registry.ts:216-222`, `252-261`, `92-137` | Delete-by-callback in M2; multiple forms in M4, but declare them |
| Generator / async-generator effects | `fiber.ts:83-93`, `370-414` | At least async single-disposer in M2; generator form in M4, but declare it |
| PENDING update branch | `fiber.ts:861-876` | Required in M2 |
| `Context.is` branding | `context.ts:61-68` | Permanent non-goal (`TypeId` replaces it) |
| Logger | `logger.ts` | M4; however, containment sink dependency is discussed in §3 |

## 3. Async/concurrency mapping assessment

### 3.1 BoxFuture signature does not compile

`Box<dyn Future + Send>` defaults to `'static`; borrowed `&self` / `&Context` cannot be captured. Make it `BoxFuture<'a, T>` or use `Arc<Context>` / `Arc<Self>`. This will be hit on the first day of M2 implementation.

### 3.2 Synchronous reentry deadlocks

TypeScript is single-threaded and lock-free; synchronous `dispose()` during `once()` dispatch is safe (`events.ts:323-329`). RwLock is not reentrant: dispatch holds a read lock while calling a listener, and the listener calls `off()` to take a write lock → deadlock. Snapshot and release before callbacks, or use an atomic structure.

### 3.3 Emit assumes a runtime exists for spawn

Synchronous contexts (plugin construction, inside `create`, drop paths) can make `tokio::spawn` panic with “no reactor running.” Capture a Handle when constructing Context. Route Err and panic through separate channels.

### 3.4 Exactly-once and inertia

- Exactly-once requires a shared future: a JoinHandle can replay JoinError, but not the Result error; use `futures_util::FutureExt::shared()` or oneshot/OnceCell, effectively forcing a futures-util dependency and contradicting the “minimal dependencies” goal.
- **Inertia is not a disposal task.** It is the promise that serializes load/unload transitions (`fiber.ts:213`, `704-718`, `817-823`), merging duplicate notifications, delaying dispose during load, starting reload after unload, and implementing inertia lock 1/2/3 (`fiber.spec`). An RwLock plus “read, decide, write, release, await” cannot express inertia lock 2.

### 3.5 Stale generation needs an atomic check order under Tokio

`_reload` checks epoch before and after awaits (`fiber.ts:749-779`). Epoch reads/writes must be atomic or protected by a lock; without a defined order, failure in an old generation can poison a new one.

### 3.6 Summary of implementation failure points

1. `BoxFuture<'static>` vs `&Context` → compile failure.
2. Synchronous reentry vs RwLock → deadlock.
3. Spawn from synchronous context → panic.
4. Shared future → forced futures-util dependency.
5. Missing inertia → wrong merge/delay semantics.
6. Waterfall `&mut V` → cannot express veto.
7. Store keyed by name → breaks isolation.

## 4. Deviation list review: incomplete

Already listed but worded incorrectly: §5.3 describes behavior from Python's `on_error` perspective; compare directly with TypeScript (“synchronous errors are rethrown”).

Missing items: Standard Schema, merged intercepts, callable invoke + extend, accessor/mixin, once/prepend/global, `internal/*` extension points, delete-by-callback/runtime introspection/multiple forms, generator effects, and object-form inject with per-service config. Put each in either “not doing” or the deviation list; otherwise the document misleadingly implies that everything else is aligned.

## 5. Verdict

**Do not approve full M1–M3; revise the design first.**

- M1 may proceed once waterfall continuation is restored; BoxFuture lifetimes/Arc, synchronous reentry structure, and runtime Handle are addressed; and parallel AggregateError plus serial's bail return value are covered.
- M2 is not ready. Design Tokio equivalents for inertia serialization/merge/delayed transitions, a shared future for exactly-once (including the futures-util decision), atomic epoch ordering, Standard Schema, `Impl.check`, ancestor lookup, label-keyed store, PENDING update branch, `internal/update` + config, and delete-by-callback.
- Restore a TypeScript-based test baseline (including the five `internal-hooks` cases, 19 reentrant cases, and disposal generator shapes), all currently lacking regression tests.

Coverage estimate: 50–60% as written; 85–90% after M2 gaps are addressed. Remaining gaps center on traceable/intercept/accessor/generator/multiple-form/logger surfaces designated M4 or permanent non-goals.

## Unresolved uncertainty

- `shadow` / `associate` / `decorator.spec` were not read case by case (out-of-scope surfaces, low impact).
- No test suite was run and Python implementation was not read (only test names checked); Python `on_error`/logger boundaries rely on the HANDOFF summary.
- The `'static` conclusion is a language-rule inference; confirm with the compiler.
