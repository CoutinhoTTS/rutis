# Acceptance Record: cordis-dispatch-01 (PR #53 Pre-dispatch Event Observation)

## Reviewed revision and diff scope

- Reviewed revision: `5e107bf` (`feat(core): observe event dispatch before listener selection`)
- Diff: `7d7402d..5e107bf`
- Worktree: `/tmp/rutis-rev53`
- Changed files:
  - `crates/rutis/src/bus.rs` (+177, core implementation)
  - `crates/rutis/src/ctx.rs` (+4, `plugin_id()` accessor)
  - `crates/rutis/src/lib.rs` (+1, export `DispatchAttempt` / `DispatchMode`)
  - `crates/rutis/src/bus/transient_tests.rs` (+12, 1,000 rounds of registration/unload)
  - `crates/rutis/tests/dispatch_observation.rs` (+280, 6 acceptance tests)
  - `README.md` (+2, documentation paragraph)

## Verdict: pass

The implementation is complete and correct, and each acceptance criterion is covered. One non-blocking coverage gap remains (no explicit reentrant-dispatch test), along with several observations; none changes the verdict.

## Issues

1. **No explicit test for reentrant dispatch** (coverage gap, non-blocking)
   - Location: `crates/rutis/tests/dispatch_observation.rs` overall; compare `bus.rs:366-371` (observer callbacks run outside locks).
   - Trigger: an observer calls `emit` / `serial` / `parallel` / `waterfall` again for the same or a different event key.
   - Evidence: design §1 explicitly lists reentry in the acceptance checklist. In `observe_attempt`, admission and bus-table locks are released after observer collection (locks held in the block at `bus.rs:313-357`; callbacks run outside locks starting at `:366`). Review confirms reentry is handled as ordinary nesting and cannot deadlock. However, no test dispatches another event from an observer (`observer_can_register_listener_before_snapshot` only verifies registering a listener).
   - Blocking? No. The implementation is correct; only a regression test is missing.
2. No other blocking issues.

## Acceptance criteria checked individually

| Criterion (design §1 / task verification) | Evidence | Result |
|---|---|---|
| All four dispatch modes trigger observers | `dispatch_observation.rs:64-123` checks Emit/Serial/Parallel/Waterfall; implementation at `bus.rs:671,747,782,842,869,920` | Pass |
| Dynamic qualifiers and instance keys appear correctly in `DispatchAttempt` | Test `:99-121` checks `TypeKey::keyed_dynamic` via `emit_keyed`, `TypeKey::instance` via `emit_instance`, and rejection with `InstanceOutOfScope`; `key` field at `bus.rs:359` | Pass |
| Observers run even when there are no business listeners | Test `:83-84` (emit) and `:86-89` (serial/parallel/waterfall); `observe_attempt` precedes the `hooks.is_empty()` check in `take_hooks` (`bus.rs:671` before `:679-682`) | Pass |
| Observers run before the business-listener snapshot, so registration/removal inside an observer affects the dispatch | Test `:126-143` registers with `on` inside an observer and the following serial snapshot selects it; `observe_attempt` precedes `take_hooks`/`take_wf_hooks` (`bus.rs:671→679`, `869→870`, `920→921`) | Pass |
| Registration/unload races | Tests `:244-280` (`early_disposal...`) and `:208-241` (`shutdown_waits...`); observer removal is mutually exclusive with selection under admission + inner locks (`bus.rs:281-283`), then `wait_events` waits for in-flight callbacks (`:290-294`) | Pass |
| Reentrant dispatch | Implementation `bus.rs:366-371` invokes callbacks without locks; **no explicit test** (see issue 1) | Implementation passes; coverage incomplete |
| Observer panic isolation (ErrorSink continues; a panicking sink is also isolated) | Test `:187-205` confirms neither observer panic nor sink panic interrupts business dispatch; business listener still runs. Double `catch_unwind` at `bus.rs:366-370` | Pass |
| Isolation between instance subtrees (emitter ancestry filtering; siblings cannot see each other) | Test `:146-184`: with root/a/b observers, a's event is visible only to root+a; b cannot see it. After a shuts down, b's event is visible only to b. Implementation matches ancestry and `Arc::ptr_eq` at `bus.rs:323-349` | Pass |
| 1,000 register/unload rounds do not leak or crash | `bus/transient_tests.rs:86-97`, `dispatch_observers_prune_after_repeated_registration`, checks count 1→0 each round; `shrink_to_fit` at `bus.rs:284-288` | Pass |
| Existing event behavior is unchanged when there are no observers | Fast path `bus.rs:310-312` returns immediately for an empty observer set without admission or ancestry traversal; all existing event tests (including 57 parity and 69 contract tests) pass with no observers | Pass |
| Observer callbacks run without locks held | Admission and inner locks are scoped to `bus.rs:313-357`, before callback invocation. Test `:208-241` indirectly verifies shutdown can proceed while an observer is blocked, rather than deadlocking | Pass |
| Registration verifies that the owner belongs to this bus root | `bus.rs:264-268` compares `Arc::ptr_eq(&self.inner, &owner.events().inner)`; `registration_preflight` at `:263` | Pass |
| Observers are unloaded through effects | `bus.rs:275-296` uses `register_internal_effect`; `AsyncDisposer` removes the observer and calls `wait_events`. Test `:244-280` | Pass |

## Verification commands and results

```text
cargo +1.98.1 test -p rutis                              → 209 passed; 0 failed; 0 ignored
cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings → clean (Finished, no warnings)
cargo +1.98.1 fmt -p rutis -- --check                     → clean (exit 0)
```

Test distribution (`dispatch_observation.rs`: 6; `bus/transient_tests.rs` includes `dispatch_observers_prune_after_repeated_registration`: 1):

- unittests (`src/lib.rs`) 14; cleanup_errors 3; config_update 15; contract 69; dispatch_chain_probe 1; dispatch_observation 6; event_keys 11; instance_subtrees 23; lifecycle_diagnostics 4; parity 57; strict_reads 4; transient_release 1; doc tests 1.

## Non-blocking observations

1. **Observers are scanned linearly.** `BusInner.observers` is a `Vec`; each observed dispatch scans it in registration order, calls `owner.upgrade()`, and checks ancestry (`bus.rs:338-355`). Cost is O(n) for many observers. The design has no scaling requirement; this is acceptable, with indexing by emitter fiber as a possible future optimization.
2. **`emitter` / `emitter_instance` come from the emitting `Ctx`.** `bus.rs:361-362` calls `ctx.plugin_id()` / `ctx.instance()`. An isolate-derived Ctx retains the owning fiber's instance ID (`ctx.rs:117-119`), consistent with “emitter”; revisit if isolate context semantics change.
3. **`observe_dispatch` checks registration twice.** `bus.rs:263` calls `registration_preflight`, then `register_internal_effect` checks `registration_open` again (`ctx.rs:842-843`). This matches existing `on_instance` patterns; redundant but harmless.
4. **`DispatchMode` is not `#[non_exhaustive]`.** Adding a public enum variant later would be breaking. The current four variants are the full design, so no change is needed now; this is a future evolution note.
5. **README wording:** `README.md:168` matches the implementation (called with zero listeners, cleaned up by fiber, no rejection return value).

## Remaining uncertainty

None. The implementation matches design §1, and all verification commands pass.
