# cordis-intercept-03: Independent Review of PR #55 (Strict Service Read / Provider Write Interception)

```yaml
id: cordis-intercept-03
package: rutis
module: intercept
status: done
depends-on: []
```

## Objective

Independently review whether PR #55 (commits `4c1e160` + `0decf15`, branch `feat/cordis-service-intercepts`) fully implements §3, “Strict service read and provider write interception,” of the design. Return pass or blocked.

## Context

- The design document is on main and absent from the reviewed branch: `/media/eric8810/fast-deliver/code/rutis/docs/design-cordis-observation.md`. Review against §3.
- Reviewed range: `c9d4caf..0decf15` (stacked on PR #54); worktree `/tmp/rutis-rev55` was checked out at `0decf15`.
- Review only changes added at this layer; lower layers were reviewed by cordis-dispatch-01 and cordis-effect-02.
- The design explicitly treats interceptors as trusted extension points and does not require value-source proof for untrusted interceptors; the corresponding #29 acceptance wording was corrected.

## Paths

- New `crates/rutis/src/intercept.rs`
- `crates/rutis/src/ctx.rs`, `registry.rs`, `error.rs`, `lib.rs`
- `crates/rutis/tests/service_intercepts.rs`

## Verification

Check every §3 criterion.

Read interception:

- `get` / `get_as` bypass the hook chain.
- `require` / `require_as` implement `Continue | Replace(Arc<T>) | Deny` correctly.
- Undeclared, out-of-scope, and not-ready services are rejected before hooks; hooks cannot make an unreadable service readable.
- Hooks match on the complete `TypeKey` plus effective isolate scope; registration fiber is an ancestor of the reader.
- Replacement affects only this return value, not binding identity; `ServiceAccess` records the original binding identity.
- Synchronous reentry for the same key and operation returns a clear error; reentry on a different key is allowed.
- Hook panics become explicit errors; shutdown waits for admitted hooks; hooks remove themselves on unload.

Write interception:

- `provide_mut_as` returns `ServiceWriter<T>`; stale-generation handles, removing bindings, non-owners, and type/instance out-of-scope writes all fail.
- `ServiceWriter::set` verifies the caller is the binding's provider and rechecks Arc identity, provider, and generation at commit before atomic replacement.
- The old `Arc<T>` is not mutated in place; holders retain the old value.
- Writes preserve provider, generation, dependency tuple, and eviction relations; they do not automatically reload consumers.
- Write hooks are filtered by provider ancestry; panic does not commit the candidate.
- Ordinary immutable services gain no read lock just because mutable slots exist.
- `0decf15` fix: the replaced old value is dropped outside framework locks.

Commands:

```sh
cargo +1.98.1 test -p rutis
cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings
cargo +1.98.1 fmt -p rutis -- --check
```

## Result

- **Conclusion: pass** (reviewer Zhou Wenbin, 2026-09-24; record: [rev-cordis-intercept-03](../reviews/rev-cordis-intercept-03.en.md)).
- Reviewed `4c1e160` + `0decf15` (`c9d4caf..0decf15`); 223 tests passed / 0 failed (`service_intercepts` 8/8); clippy and fmt clean.
- All eight read-interception and seven write-interception criteria passed; no blockers.
- Six non-blocking observations: no dedicated tests for different-key reentry or writes during removal; no `hits == 0` assertion for early out-of-scope rejection; occasional spurious begin/finish in `select`; `has_any` registration/read race (design does not require linearizability); same-key reentry includes the scope dimension.
