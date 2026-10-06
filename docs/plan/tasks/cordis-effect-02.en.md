# cordis-effect-02: Independent Review of PR #54 (Labeled Effect Cleanup Tree)

```yaml
id: cordis-effect-02
package: rutis
module: effect
status: done
depends-on: []
```

## Objective

Independently review whether PR #54 (commit `c9d4caf`, branch `feat/cordis-effect-tree`) fully implements §2, “Labeled effect cleanup tree,” of the design document. Return pass or blocked.

## Context

- Design doc (on main, absent from the reviewed branch): `/media/eric8810/fast-deliver/code/rutis/docs/design-cordis-observation.md`; review against §2.
- Reviewed range: `5e107bf..c9d4caf` (stacked on PR #53); worktree `/tmp/rutis-rev54` was checked out at this tip.
- Review only changes added at this layer; PR #53 was reviewed by cordis-dispatch-01.

## Paths

- `crates/rutis/src/effect.rs`, `fiber.rs`, `ctx.rs`
- `crates/rutis/src/fiber/transient_tests.rs`
- `crates/rutis/tests/effect_tree.rs`

## Verification

Check each §2 acceptance criterion:

- Automatic labels (`anonymous` / plugin name) and explicit `effect_named` labels.
- Real nested `children` for `Effect::Many`; preserve LIFO cleanup.
- Parallel `ctx.effect()` registrations are siblings, with no invented parent-child relation.
- Metadata disappears after early disposal, remains visible as `Draining` during cleanup, and the index is removed after completion.
- `EffectMeta` does not capture cleanup closures, execute user code, or retain child fibers/service values.
- Existing `Ctx::effect()` / `Plugin::apply()` return types and error aggregation are unchanged.
- After 1,000 subtree shutdowns, parent-fiber metadata and effect records return to baseline.
- `FiberView::effects()` provides the “query from fiber” entry point required by #27.

Run inside the worktree:

```sh
cargo +1.98.1 test -p rutis
cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings
cargo +1.98.1 fmt -p rutis -- --check
```

## Result

- **Conclusion: pass** (reviewer Lin Xiaowen, 2026-09-24; record [rev-cordis-effect-02](../reviews/rev-cordis-effect-02.en.md)).
- Reviewed `c9d4caf` (`5e107bf..c9d4caf`); 213 tests passed / 0 failed; clippy and fmt clean.
- All eight acceptance criteria passed; no blockers.
- Five non-blocking observations: no dedicated test for siblings; `effects()` deep-copies the full tree in O(n); snapshots contain values rather than references; `effect_index` weak-reference GC relies on drain (consistent with existing pattern); registration rollback is a no-op path.
