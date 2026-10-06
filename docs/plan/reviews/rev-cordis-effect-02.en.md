# Acceptance Record: cordis-effect-02 (PR #54 Labeled Effect Cleanup Tree)

## Reviewed revision and diff scope

- Reviewed commit: `c9d4caf` (`feat(core): expose labeled effect ownership tree`)
- Diff: `5e107bf..c9d4caf` (stacked on PR #53 `5e107bf`; only changes introduced in this layer are assessed)
- Worktree: `/tmp/rutis-rev54`
- Changed files (8, +314 / -22):
  - `crates/rutis/src/effect.rs` (EffectMeta / EffectPhase / into_cleanups / snapshot / index cleanup)
  - `crates/rutis/src/fiber.rs` (effect_index, push_effect, FiberView::effects)
  - `crates/rutis/src/ctx.rs` (effect_named, register_*_named, labels)
  - `crates/rutis/src/bus.rs` (rename and label internal effects)
  - `crates/rutis/src/lib.rs` (export EffectMeta / EffectPhase)
  - `crates/rutis/src/fiber/transient_tests.rs` (1,000-round effect_index assertion)
  - `crates/rutis/tests/effect_tree.rs` (4 new acceptance tests)
  - `README.md` (cleanup-tree documentation)

## Verdict

**Pass.** The implementation fully covers the acceptance scenarios in design §2. Tests, clippy, and fmt are clean. No blocking issues were found.

## Acceptance criteria checked individually

| Criterion | Evidence | Result |
|---|---|---|
| Automatic labels (`anonymous` / plugin name) and explicit `effect_named` labels | `Ctx::effect` delegates to `effect_named("anonymous", f)` ([`ctx.rs:826-828`](crates/rutis/src/ctx.rs#L826)); plugin label `apply: {name}` ([`fiber.rs:552-557`](crates/rutis/src/fiber.rs#L552)); `named_effect_...` and `framework_labels_...` assert `anonymous` and `plugin apply:` | Pass |
| Nested `Effect::Many` produces real `children`; LIFO cleanup order is unchanged | `into_cleanups` recursively builds the tree ([`effect.rs:57-76`](crates/rutis/src/effect.rs#L57)); `out` is pushed in declaration order and `run_cleanups` pops LIFO. Test checks child shape `0: disposer` / `1: many` and post-dispose order `[3,2,1]` | Pass |
| Sibling `ctx.effect()` calls remain siblings; no parent/child relationship is invented | Each `effect_named` creates an independent `EffectRecord` (one top-level `EffectMeta`); there is no call-stack parent mechanism. `framework_labels_...` shows multiple top-level siblings | Pass (see observation 1) |
| Metadata disappears after early dispose; `Draining` remains visible during cleanup; index removes completed records | `snapshot()` reports `Live/Draining/Done`; drain removes the record from `effect_index` after Done ([`effect.rs:173-180`](crates/rutis/src/effect.rs#L173)); tested by `draining_record_remains_visible_until_cleanup_finishes` and `named_effect_...` | Pass |
| `EffectMeta` does not capture cleanup closures, execute user code, or retain child fibers/service values | `EffectMeta` contains only `label/phase/children` ([`effect.rs:32-36`](crates/rutis/src/effect.rs#L32)); cleanups stay in `EffectState::Live`. `effects()` clones metadata, upgrades weak references, and snapshots without invoking user code ([`fiber.rs:1130-1135`](crates/rutis/src/fiber.rs#L1130)) | Pass |
| Existing `Ctx::effect()` / `Plugin::apply()` return types and error aggregation are unchanged | `effect` still returns `Result<Disposer, CordisError>`; `apply` still returns `BoxFuture<Result<Effect, ..>>`. Cleanup order/aggregation is unchanged; existing `contract.rs::aggregate_no_flatten`, `exactly_once_same_error`, and parity aggregation tests pass | Pass |
| After 1,000 subtree shutdowns, parent metadata and effect records return to baseline | `thousand_shutdowns_reclaim_all_root_side_records` asserts each round that `root.effects.len()==1`, `root.effect_index.len()==1`, and child `effect_index` is empty ([`transient_tests.rs:152-154`](crates/rutis/src/fiber/transient_tests.rs#L152)) | Pass |
| `FiberView::effects()` provides the #27 “query from fiber” entry point | `pub fn effects(&self) -> Vec<EffectMeta>` ([`fiber.rs:1130`](crates/rutis/src/fiber.rs#L1130)); `lib.rs` exports `EffectMeta` / `EffectPhase` | Pass |

## Verification commands and results

Run from `/tmp/rutis-rev54` with installed toolchain `1.98.1` (the worktree has no `rust-toolchain.toml`; `+1.98.1` was explicitly requested):

```sh
cargo +1.98.1 test -p rutis
```

**213 passed; 0 failed.** Breakdown: unit tests 14, cleanup_errors 3, config_update 15, contract 69, dispatch_chain_probe 1, dispatch_observation 6, effect_tree 4, event_keys 11, instance_subtrees 23, lifecycle_diagnostics 4, parity 57, strict_reads 4, transient_release 1, doc tests 1.

```sh
cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings
```

Clean (`Finished`, no warnings).

```sh
cargo +1.98.1 fmt -p rutis -- --check
```

Clean (exit code 0).

## Non-blocking observations

1. **No dedicated test that sibling effects are not made parent/child.** The design §2 acceptance list (line 91) does not require this, though the body states it as a constraint (line 85). The implementation naturally satisfies it because each `effect_named` creates an independent top-level record; `framework_labels_and_child_ownership_follow_lifecycle` also shows multiple top-level siblings. Additional coverage is optional.
2. **Metadata reads deep-copy the entire tree.** `FiberView::effects()` clones `metadata` for every record (recursive clone of labels and children), giving O(n) allocation for a large effect tree. This matches the design's “first read within one fiber” and “reads copy only labels/phase/tree structure” requirements. No correctness issue; account for the cost if adding whole-tree DTOs later.
3. **`EffectMeta` is a value snapshot, not a reference view.** `snapshot()` returns owned `Option<EffectMeta>` and children are owned metadata, satisfying the rule not to retain child fibers or service values. Readers do not get live references, which the design explicitly says are not returned.
4. **Weak-reference cleanup in `effect_index` relies on drain completion.** `index.retain(...)` removes the record and stale weak entries with `strong_count()==0` when a record finishes draining ([`effect.rs:176`]). Every record eventually drains, by early disposal or fiber unload through `drain_effects`; no leak is expected, and the 1,000-round test confirms bounded capacity. This matches the existing `parent.children` cleanup pattern ([`fiber.rs:914`]).
5. **Rollback path in `register_effect_named`** ([`ctx.rs:932-945`]): if `f()` has run but lifecycle changes before registration, `record.drain()` executes cleanup. Since the record was never passed to `push_effect`, it was never in the index, so the drain's index retain is a harmless no-op. Behavior matches the pre-rename implementation.
