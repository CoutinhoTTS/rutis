# Rust Port Design v2: Second Review — deepseek-v4-pro (2026-08-17)

> Review target: v2 of [design-rust-port.md](design-rust-port.en.md), subsequently revised to v3 (see the v3 §0 “v2 → v3” table).
> Baseline: all 9 files under `src/`, 96 measured `tests/*.spec.ts` cases, and 10 cases in `test/core.test.ts`. Verdict: **Neither M1 nor M2 was ready for direct approval.** The status improved from “do not approve” in v1 to “approve after small fixes.”

## Summary

Five of v1's seven critical failure points were fixed and two were partly fixed. None of the 15 mechanisms marked mandatory in v1 §2 was omitted. However, the concurrency/async revisions retained **three M1 blockers (A/C/B)** and **four M2 protocol gaps (D/E/F/G)**. Estimated M2 coverage after fixes was about 85–90%; the events-alignment claim was optimistic by one or two items.

## M1 blockers

- **A — D3 waterfall lifetime does not compile as specified.** The four `'_` lifetimes in `dyn Fn` are independently late-bound and cannot tie the `Next` borrow, `&mut [Value]` borrow, and returned `BoxFuture` to one `'a`. Use a helper trait such as `fn call<'a>(&'a self, ...) -> BoxFuture<'a, Value>`. Renaming the v1 §3.1 shape reproduces the issue.
- **B — `emit` loses the synchronous prefix of async listeners.** TypeScript `emit` calls the callback synchronously, so an async body runs until its first `await` (`events.ts:200-207`). Rust futures are lazy and run only once polled after spawning. Either poll once with a no-op waker or document this as a deviation.
- **C — The synchronous error channel is missing.** The return enum has no `Err`, contradicting D7's “synchronous Err is rethrown.” TypeScript `parallel` aggregates synchronous throws too, while `serial`'s `await cb()` converts a synchronous throw into a rejection. “No Err in the enum,” “aggregate as values,” and “do not catch panic” cannot all hold; the claimed parallel alignment in §6 is inconsistent.

## M2 gaps

- **D — Epoch representation and atomic ordering:** “content hash or sequence number” is underspecified; sequence numbers break equality coalescing (`reentrant.spec`). Separate atomic reads of epoch and generation can tear, unlike TypeScript's single token (`fiber.ts:739`). D6's `AtomicU64` also conflicts with D10's “read under lock.”
- **E — The chain-switch protocol is unspecified and described inaccurately.** TypeScript `_setEpoch` (`fiber.ts:704-718`) updates the epoch and rereads/rechains on completion; it is not a command queue. No protocol addresses the lost-wakeup race between unlocking, setting `None`, and a concurrent epoch change.
- **F — `internal/dispatch` must run before the snapshot.** TypeScript `dispatch()` (`events.ts:170-180`) emits `internal/dispatch` before building the listener list, so listeners registered there participate in the same dispatch. v2 does not pin down this order.
- **G — `Value` payloads conflict with function/capability arguments.** `internal/get/set` carry context and errors, not `Value`; `internal/dispatch` waterfall arguments include `next` (asserted by `internal-hooks.spec:36-40`); the Rust representation for per-fiber `_hooks` is missing.

## Audit summary

- Of v1 §3.1–3.6's seven failure points, `BoxFuture<'a>`, snapshot reentrancy, `Handle`, `futures-util`, and label-keyed stores were fixed. Inertia (D/E) and waterfall (A/G) were only partly fixed.
- The three M1 prerequisites in v1 §5 (`BoxFuture`/`Arc`/`Handle`) were covered; waterfall and parallel/serial retained A/C/B.
- All 11 M2 prerequisites had a design location, but the per-fiber `_hooks` representation was not expanded (G).
- All 15 mandatory items in v1 §2 were present. All nine omissions in the deviation list were added and classified correctly.
- A spot-check of 12 contract line references was accurate, with minor offsets for the inertia field (`fiber.ts:213`) and `EffectRecord` (`fiber.ts:429-634`).
- **Test matrix corrections:** 62 should be 96; `dispose.spec` has 13, not 14; `plugin.spec` has 10, not 11. All four `reflect.spec` cases were missing (inject leak → C3, duplicate provide → C3, `Context.is` → out of scope, mixin → B6).

## Verdict

- **M1:** Do not approve yet. Fix the D2 error channel and D3 signature, and list the B deviation.
- **M2:** Conditional approval after specifying the D6/D10 protocol: atomic epoch ordering, chain switching, `internal/dispatch` ordering before the snapshot, and `Value` versus function-shaped arguments.
- Estimated post-fix M2 coverage: about 85–90%. The events surface retains one or two deviations around synchronous error aggregation in parallel and `internal/dispatch` diagnostic fidelity.
