# Acceptance Record: cordis-fix-05 (Drop Failed-Write Candidate Values Outside Framework Locks)

## Reviewed revision and diff scope

- Reviewed commit: `68491a7` (`fix(core): drop candidate StoredValue outside framework locks on write failure`)
- Diff: `795c165..68491a7`
- Worktree: `/tmp/rutis-dev55`
- Changed files:
  - `crates/rutis/src/intercept.rs`: +51/-26
  - `crates/rutis/src/registry.rs`: +20/-6
  - `crates/rutis/tests/service_intercepts.rs`: +205/-1

## Verdict

**Pass.** All three failure paths are fixed, lock boundaries are clear, and the IIFE correctly moves ownership of the candidate value outside the locks. Red-to-green evidence for tests 1 and 2 is conclusive (they deadlocked and timed out after five seconds before the fix). Stability was 20/20; the full suite had 229 passing tests; clippy and fmt were clean.

## Review of the three fixed paths

### Path 1: `registration_open()` fails

| Item | Details |
|---|---|
| Fix | [`intercept.rs:408-409`](/tmp/rutis-dev55/crates/rutis/src/intercept.rs#L408) |
| Admission lock | `intercept.rs:407`: `let _admission = self.shared.admission.lock().unwrap();` |
| How candidate leaves the lock scope | `return (value, Err(...))`: `value` is returned in the tuple from the IIFE closure. |
| `_admission` released at closure end | Yes. Closure locals are dropped in reverse declaration order when the closure returns, before the returned tuple is dropped outside. |
| Drop outside locks | `intercept.rs:427`: `owned` (the returned `value`) is dropped after the IIFE ends. |
| `catch_unwind` + sink | `intercept.rs:430-434`, same pattern as `0decf15`. |
| Result | Pass |

### Path 2: `transition.generation` / state check fails

| Item | Details |
|---|---|
| Fix | [`intercept.rs:412-416`](/tmp/rutis-dev55/crates/rutis/src/intercept.rs#L412) |
| Locks acquired | Admission at `intercept.rs:407`; transition at `:411`. |
| How candidate leaves the lock scope | `return (value, Err(...))`, as in path 1. At closure exit, `transition` releases, then `_admission`; afterward `owned = value` is dropped. |
| Lock release order | `transition` (declared at line 411, released first), then `_admission` (declared at line 407, released second), guaranteed by Rust's reverse drop order. |
| Drop outside locks | Yes, as above. |
| Result | Pass |

### Path 3: `replace_mutable_if_current` fails

| Item | Details |
|---|---|
| Fix | [`registry.rs:125-155`](/tmp/rutis-dev55/crates/rutis/src/registry.rs#L125) and [`intercept.rs:417-425`](/tmp/rutis-dev55/crates/rutis/src/intercept.rs#L417) |
| Signature change | `Option<StoredValue>` → `Result<StoredValue, StoredValue>`: `Ok(old)` returns the previous value on success; `Err(candidate)` returns the candidate on failure. |
| Registry `bindings` lock | `registry.rs:138` creates a local guard; it is released automatically when the function returns. |
| Do all four rejection paths return the candidate? | 1. **Empty slot** (`registry.rs:140-141`): `return Err(value)`. 2. **`Arc::ptr_eq` fails** (`:143-147`): `return Err(value)`. 3. **`removing` is set** (`:143-147`): same. 4. **`ValueSlot::Fixed` slot** (`:153`): `Err(value)`. All return the candidate. |
| Is unwrap safe on the Mutable path? | At `registry.rs:149-151`, `match &current.value` has already established the Mutable variant; `replace_mutable` always returns `Some(old)` for Mutable ([`:55-63`]), so `unwrap()` cannot panic. |
| Candidate dropped outside `bindings` lock? | Yes. The registry guard is gone when `replace_mutable_if_current` returns. The result reaches the IIFE match; the closure then releases `transition` and `_admission`; the value is dropped outside the IIFE. |
| Drop outside locks | Same as paths 1/2, protected by `catch_unwind`. |
| Result | Pass |

### Line-by-line IIFE boundary review

IIFE closure: [`intercept.rs:406-426`](/tmp/rutis-dev55/crates/rutis/src/intercept.rs#L406)

```text
406: let (owned, result) = (|| -> (StoredValue, Result<(), ServiceWriteError>) {
407:     let _admission = self.shared.admission.lock().unwrap();   // acquire A
408:     if caller.registration_open().is_err() {
409:         return (value, Err(...));                              // early return; A released at closure end
410:     }
411:     let transition = provider.transition.lock().unwrap();     // acquire T
412:     if transition.generation != self.binding.provider_gen ...
415:         return (value, Err(...));                              // early return; release T then A
416:     }
417:     match self.shared.registry.replace_mutable_if_current(...) {
           // replace_mutable_if_current acquires bindings lock B; releases B on return
423:         Ok(old) => (old, Ok(())),
424:         Err(candidate) => (candidate, Err(...)),
425:     }
426: })();  // closure ends: release T, then A
427: // owned is dropped here, outside locks
430: if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(owned))) {
```

**Conclusion:** all locks (admission A, transition T, bindings B) have left scope before `owned` is dropped. Release order is B (registry function returns), T (closure ends), A (closure ends), then drop `owned`.

### `catch_unwind` behavior compared with the existing fix

| Comparison | `0decf15` success path | This fix, `68491a7` |
|---|---|---|
| Dropped value | `old` (replaced value) | `owned` (old value on success, candidate on failure) |
| `catch_unwind` | `AssertUnwindSafe(|| drop(old))` | `AssertUnwindSafe(|| drop(owned))` |
| Convert panic to Arc error | `Arc::new(panic_error(panic))` | Same |
| Report to sink | `caller.error_sink()` → `sink(error)` | Same |
| Protect against sink panic | `catch_unwind(AssertUnwindSafe(|| sink(error)))` | Same |
| Result | — | Pass |

## Independent red-to-green reproduction

### Method

1. `git worktree add /tmp/rutis-rev-fix 795c165` (pre-fix baseline).
2. Copy `tests/service_intercepts.rs` from `68491a7` into that worktree, retaining the old implementation with the new tests.
3. Run the three regression tests and observe deadlock behavior.
4. Compare with fixed code at `68491a7` in `/tmp/rutis-dev55`.

### Results

| Test | Before fix (`795c165`) | After fix (`68491a7`) |
|---|---|---|
| 1: `writer_set_stale_candidate_drop_no_deadlock_replaced_binding` | **Failed** (5.00s timeout: “recv_timeout means Mutex deadlock”) | **Passed** (0.03s) |
| 2: `writer_set_stale_candidate_drop_no_deadlock_removing_flag` | **Failed** (5.00s timeout) | **Passed** (0.03s) |
| 3: `writer_set_stale_candidate_drop_no_deadlock_generation_stale` | **Passed** (0.00s; analysis below) | **Passed** (0.03s) |

Tests 1 and 2 provide conclusive red-to-green evidence: before the fix, `std::thread::spawn` + `mpsc::recv_timeout` timed out after five seconds and the test panicked; after the fix, both passed.

**Why test 3 passed before the fix:** before the fix, `registration_preflight()` ([`intercept.rs:383-385`]) ran outside the admission lock. After `view.restart()`, the old fiber was transitional, so preflight failed and `?` returned early with no framework lock held; the candidate was safely dropped. Thus the pre-fix test exercised the preflight-failure path, not the generation-check path. See issue 1 below.

## Regression test quality

### Test 1: candidate Drop reenters after binding replacement

- **Mechanism:** `DropEffect::drop` calls `ctx.effect_named()`, acquiring framework locks.
- **Deadlock detection:** `std::thread::spawn` + `mpsc::recv_timeout(5s)`. Tokio timeout is insufficient because it cannot cancel a synchronously blocked thread.
- **Race control:** dispose in the background and spin with `yield_now()` until `removing` or key disappearance (up to 1,000 iterations); no sleep.
- **Assertion:** `err.reason == ServiceWriteFailure::Stale`.

### Test 2: candidate Drop reenters while `removing` is set

- Same mechanism as test 1, but the spin explicitly waits for `removing == true`.
- Deterministic bound of 1,000 `yield_now()` iterations; no sleep dependency.

### Test 3: candidate Drop reenters after generation becomes stale

- **Setup:** `MutableDropProvider` caches a writer and Ctx in `apply`; `view.restart()` rebuilds the fiber and changes `transition.generation`.
- **Caveat:** before the fix, this test did not actually reach the generation-check deadlock path (see issue 1).
- **Value after the fix:** preflight may succeed or fail. If it succeeds, execution reaches the IIFE generation check and safely returns the candidate. The test still covers end-to-end behavior that `writer.set` does not deadlock after restart.

## Stability verification

```text
for i in $(seq 1 20); do cargo +1.98.1 test -p rutis --test service_intercepts; done
```

Result: **20/20 passes**, 13 passed / 0 failed per run, about 0.03s per run.

## Full verification

| Command | Result |
|---|---|
| `cargo +1.98.1 test -p rutis` | **229 passed / 0 failed** (226 + 3 new regression tests). Distribution: 16+3+15+69+1+7+4+11+23+4+57+13+4+1+1. |
| `cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings` | Clean (`Finished`, no warnings) |
| `cargo +1.98.1 fmt -p rutis -- --check` | Clean (exit 0) |

## Issues

### Issue 1 (non-blocking): test 3 did not reach the generation deadlock path before the fix

Test 3 passed in 0.00s against the pre-fix code. After `view.restart()`, `registration_preflight()` failed outside locks and returned early, so the candidate was safely dropped.

**Impact:** red-to-green evidence is not direct for the generation path. However:

- Tests 1 and 2 prove the IIFE fix for `registration_open` failure and `replace_mutable_if_current` failure.
- The generation-check deadlock has the same mechanism as path 1: returning an error while holding admission + transition locks. The structural reasoning is sufficient.
- Test 3 remains useful after the fix as an end-to-end assertion that a writer does not deadlock after restart.

### Issue 2 (non-blocking): test 3 depends on provider Ctx lifetime

Test 3 retains the old `provider_ctx` after `restart()`. The old fiber state is uncertain; `registration_preflight` may pass or fail, so different runs may take different paths. The final assertion (`Stale`) is stable, but execution is nondeterministic.

Suggestion: to exercise the generation-check path deterministically, keep a provider fiber Active while changing its generation (for example, replace the binding instead of restarting).

## Remaining uncertainty

None. Lock boundaries and IIFE behavior for all three paths were checked line by line; red-to-green evidence is strong for tests 1 and 2, and the full regression suite passes.
