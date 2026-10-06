# cordis-dispatch-01: Independent Review of PR #53 (Pre-Dispatch Event Observation)

```yaml
id: cordis-dispatch-01
package: rutis
module: bus
status: done
depends-on: []
```

## Objective

Independently review whether PR #53 (commit `5e107bf`, branch `feat/cordis-dispatch-observation`) fully implements §1, “Pre-dispatch event observation,” of the design document. Return pass or blocked.

## Context

- The design doc is on the main repository branch and absent from the reviewed branch: `/media/eric8810/fast-deliver/code/rutis/docs/design-cordis-observation.md`. Review against §1 and the three-hook-scope table under “Purpose and boundaries.”
- Reviewed range: `7d7402d..5e107bf`; worktree `/tmp/rutis-rev53` was checked out at this tip.
- Baseline Cordis source facts are in the design table (`events.ts` `_resolve` and `internal/dispatch`).

## Paths

- `crates/rutis/src/bus.rs`, `crates/rutis/src/ctx.rs`, `crates/rutis/src/lib.rs`
- `crates/rutis/src/bus/transient_tests.rs`
- `crates/rutis/tests/dispatch_observation.rs`

## Verification

Check each §1 acceptance criterion:

- All four dispatch modes (Emit / Serial / Parallel / Waterfall) trigger observers.
- Dynamic qualifiers and instance keys appear correctly in `DispatchAttempt`.
- Observers run even when there are no business listeners.
- Observers run before the business-listener snapshot, so observer registration/removal affects the following dispatch.
- Registration/unload races, reentrant dispatch, and observer panic isolation (ErrorSink continues).
- Isolation between instance subtrees: filter by the emitter's ancestor chain; sibling instances cannot see each other.
- 1,000 registration/unload cycles do not leak or crash.
- Existing event behavior is unchanged when no observer is installed.
- Observers run without holding admission, bus-table, registry, or fiber-state locks.
- Registration checks that the owner belongs to the bus root; observers unload through effects.

Run inside the worktree:

```sh
cargo +1.98.1 test -p rutis
cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings
cargo +1.98.1 fmt -p rutis -- --check
```

## Result

- **Conclusion: pass** (reviewer Chen Zhiyuan, 2026-09-24; record: [rev-cordis-dispatch-01](../reviews/rev-cordis-dispatch-01.en.md)).
- Reviewed `5e107bf` (`7d7402d..5e107bf`); `cargo +1.98.1 test -p rutis`: 209 passed / 0 failed; clippy and fmt clean.
- All 13 acceptance criteria passed. The only coverage gap was no explicit reentrant-dispatch test; review confirmed callbacks run outside the lock (`bus.rs:366-371`) and do not deadlock.
- Five non-blocking observations: observer scan is O(n); emitter field comes from `Ctx`; duplicate registration check; `DispatchMode` is not `non_exhaustive`; README wording consistency.
