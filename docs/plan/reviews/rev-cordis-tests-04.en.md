# Acceptance Record: cordis-tests-04 (Three Tests Closing Acceptance Coverage Gaps)

## Reviewed revision and diff scope

- Reviewed commit: `795c165` (`test(core): add observer reentry, cross-key reentry, and eviction-write tests`)
- Diff: `0decf15..795c165`
- Worktree: `/tmp/rutis-dev55`
- Changed files (tests only; no implementation changes):
  - `crates/rutis/tests/dispatch_observation.rs`: +48 lines
  - `crates/rutis/tests/service_intercepts.rs`: +93 lines
  - Total: 2 files, +141 insertions

## Verdict

**Pass.** Each of the three new tests exercises the previously missing behavior; assertions match the implementation paths and do not pass vacuously. The `service_intercepts` suite passed 20 consecutive runs. Full regression tests, clippy, and fmt were clean. Test style matches the existing files.

## Per-test review

### Gap 1: Reentrant observer dispatch (from rev-cordis-dispatch-01, issue 1)

| Item | Details |
|---|---|
| Test | `observer_reentry_observes_nested_dispatch_and_business_listener_runs` |
| File | `crates/rutis/tests/dispatch_observation.rs` |
| Behavior covered | An observer calls `emit` again. The nested dispatch is observed, does not deadlock, and invokes its business listener. |
| Assertions | 1. `attempts == vec![1, 2]`: both outer `serial(Ping(1))` and nested `emit(Ping(2))` are observed. 2. `result.unwrap().is_none()`: the outer serial finds that the inner emit already took the hooks and returns None, so the test does not pass vacuously. 3. `inner_hits == 1`: the business listener runs in the nested emit's tail task. 4. A five-second `tokio::time::timeout` prevents a deadlock from hanging the suite. |
| Code-path check | `observe_attempt` (`bus.rs:366-371`) calls observers without locks. Nested `emit` → `emit_keyed_inner` (`bus.rs:667-722`) reacquires admission and inner locks safely. The outer serial calls `take_hooks` (`bus.rs:872`) only after the observer returns; the nested emit has already taken them, so None is correct. |
| Result | Pass |

### Gap 2: Different-key reentry is allowed (from rev-cordis-intercept-03, observation 1)

| Item | Details |
|---|---|
| Test | `different_key_reentry_allowed_for_read_and_write` |
| File | `crates/rutis/tests/service_intercepts.rs` |
| Behavior covered | While a hook for key A runs, reading or writing another hooked key B succeeds instead of returning `InterceptReentrant`; both read and write paths are covered. |
| Assertions | **Read:** key A's require hook successfully calls `require_as::<u64>(key_b)`; B's hook runs (`b_read_hits == 1`) and replaces the value with 12 (`b_value_seen == Some(12)`, original 2 + replacement 10). **Write:** key C's set hook successfully calls `writer_d.set(30)`; D's hook runs (`d_write_hits == 1`) and replacement yields `*root.get_as(key_d) == 130` (30 + 100). |
| Code-path check | `ReentryGuard::enter` (`intercept.rs:202-210`) deduplicates by `(HookKind, HookKey)`. Different keys have different HookKeys and cannot collide. |
| Result | Pass |

### Gap 3: Writing through a binding being removed fails (from rev-cordis-intercept-03, observation 2)

| Item | Details |
|---|---|
| Test | `writer_set_fails_stale_during_binding_removal` |
| File | `crates/rutis/tests/service_intercepts.rs` |
| Behavior covered | When `binding.removing` is set but the registry slot has not yet been replaced, `writer.set` returns `Stale`. |
| Assertion | `err.reason == ServiceWriteFailure::Stale`. Whether the race lands after `removing` is set or after the key disappears, the outcome is Stale. |
| Stability analysis | 1. `dispose()` starts `evict_and_finalize`; its first synchronous statement is `mark_removing_if` (`ctx.rs:1144`), before its first await, so `removing` is set promptly. 2. The test polls diagnostics with `yield_now()` up to 1,000 times, without sleep. 3. Both outcomes (`removing=true` → `replace_mutable_if_current` rejects at line 138; key already gone → Arc identity check at line 137 or `registration_preflight` line 384 rejects) map to Stale. The assertion does not depend on exact micro-timing. |
| Code-path check | `replace_mutable_if_current` (`registry.rs:128-143`) checks `Arc::ptr_eq`, then `removing`; either failure returns None. `set` maps this to Stale (`intercept.rs:415-416`). |
| Result | Pass |

## Stability verification

The `service_intercepts` test file was run 20 times consecutively; all passed (10/10 each run), with no intermittent failures:

```text
for i in $(seq 1 20); do cargo +1.98.1 test -p rutis --test service_intercepts; done
→ 20/20 passes, 0 failures
```

## Verification commands and results

Toolchain: `1.98.1-x86_64-unknown-linux-gnu`.

| Command | Result |
|---|---|
| `cargo +1.98.1 test -p rutis` | **226 passed / 0 failed** (16+3+15+69+1+7+4+11+23+4+57+10+4+1+1 doc test). `service_intercepts`: 10/10; `dispatch_observation`: 7/7. |
| `cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings` | Clean (`Finished`, no warnings) |
| `cargo +1.98.1 fmt -p rutis -- --check` | Clean (exit 0) |

## Code quality

- Naming: snake_case async test functions, consistent with existing tests (`observer_reentry_...`, `different_key_reentry_...`, `writer_set_fails_stale_...`).
- Helpers: reused existing file-level types (`Ping` / `Count`, `Capture`); no new types.
- Assertions: `assert_eq!` plus `unwrap_err().reason`, matching existing patterns.
- Race handling: bounded `yield_now()` polling, consistent with existing tests such as the 100-iteration loop in `shutdown_waits_for_selected_synchronous_observer`.
- Dependencies: no new crate dependencies; all types were already imported at file scope.
- Deadlock protection: reentry test uses `tokio::time::timeout`, appropriately preventing a broken implementation from hanging the suite.

## Non-blocking observations

None.

## Remaining uncertainty

None. All three gaps have dedicated tests whose assertions match the implementation paths; verification is green.
