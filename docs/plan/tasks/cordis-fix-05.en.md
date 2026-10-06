# cordis-fix-05: Fix Candidate Value Drop Under a Framework Lock (PR #55 P1)

```yaml
id: cordis-fix-05
package: rutis
module: intercept
status: done
depends-on: []
```

## Objective

Fix the P1 found in remote review of PR #55: on a failure path, `ServiceWriter::set` drops the candidate `StoredValue` while holding a framework lock. If its `Drop` reenters rutis (for example, calls `Ctx::effect()`), it deadlocks. Add a regression test.

## Context

- Remote review of PR #55 (`arcships/rutis`): after the writer's old binding was replaced, writing a value whose `Drop` calls `root.effect()` deadlocked after a five-second timeout. The reviewer recommended returning the candidate on failure and dropping it outside locks.
- Baseline `795c165` (`0decf15` plus test additions); worktree `/tmp/rutis-dev55`.
- The main agent initially identified three affected failure paths in `crates/rutis/src/intercept.rs::ServiceWriter::set`:

| # | Failure path | Locks held | Candidate drop location |
|---|---|---|---|
| 1 | `registration_open()` fails (`?` early return) | admission | local to `set` |
| 2 | `transition.generation` / state check fails (`return Err`) | admission + transition | local to `set` |
| 3 | `replace_mutable_if_current` returns `None` (`?`) | admission + transition + bindings | inside registry function (owned `value`; empty slot `?` and ptr-equality/removing `return None`) |

The success path in `0decf15` already drops the old value outside locks under `catch_unwind`; the fix should align with it.

## Fix direction

- Change `registry.replace_mutable_if_current` to return the candidate on failure (for example, `Result<StoredValue /* old */, StoredValue /* candidate */>`).
- Put lock acquisition, checks, and commit in a separate scope in `set`. Return owned values with the result (success = old value; failure = candidate + reason), then uniformly drop them outside locks under `catch_unwind` and report panics to the sink without changing the returned error.

## Regression tests (`tests/service_intercepts.rs`)

1. After a binding is replaced, an old writer submits a candidate whose `Drop` reenters the framework (`root.effect()`): no deadlock (with timeout guard), returns `Stale`.
2. Same case while a binding is being removed (`removing` set): no deadlock, returns `Stale`.
3. Cover generation invalidation (for example, `set` after provider shutdown) with a candidate whose `Drop` reenters; no deadlock.

## Verification

- First write test 1 and confirm it fails by reproducing the deadlock; make it pass after the fix.
- Full `cargo +1.98.1 test -p rutis`; clippy with `-D warnings`; fmt `--check`.
- New tests must be deterministic, with no sleep-based timing guesses.

## Result

- **Conclusion: pass** (developer Wu Junjie `68491a7` + `c0b6b41`; independent review Zhou Wenbin; record [rev-cordis-fix-05](../reviews/rev-cordis-fix-05.en.md); main agent rechecked `c0b6b41` and reran the full suite).
- **Scope correction:** The main agent initially claimed all three paths deadlocked; that was **wrong**. Rust drops locals in reverse declaration order, so in paths 1 (`registration_open` failure) and 2 (generation/state check failure), the candidate is dropped after lock guards and cannot deadlock. Wu Junjie's restart experiment proved path 2 is reachable (`err.generation == 1`) and the old code does not deadlock. The real P1 was path 3, where the candidate moved into `replace_mutable_if_current` and was dropped inside it while the caller held admission, transition, and bindings locks—matching the remote report.
- **Fix:** `registry.replace_mutable_if_current` now returns `Result<StoredValue, StoredValue>` (returning the candidate on failure). `set` uses an IIFE to carry the candidate out of all lock scopes, then runs `catch_unwind` around the drop and reports to the sink, matching the `0decf15` success path. Paths 1/2 are also explicit to prevent a future refactor from moving the value into a lock-held call.
- **Red-to-green evidence:** Tests 1 (replaced binding) and 2 (removing binding) deadlocked after five seconds before the fix and pass after. Test 3 documents the reachable but non-deadlocking path 2 and checks `err.generation`.
- **Verification:** 229 passed / 0 failed (confirmed by the main agent); clean clippy and fmt; `service_intercepts` stable across 20 consecutive runs.
- Commit chain `0decf15` → `795c165` → `68491a7` → `c0b6b41` was reorganized by branch and pushed (PR #55 tip `8cac519`), then merged with the three PRs into `main` (`f37f90d`).
