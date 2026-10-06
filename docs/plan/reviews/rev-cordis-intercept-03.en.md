# Acceptance Record: cordis-intercept-03 (PR #55 Strict Service Reads and Provider Write Interception)

## Reviewed revision and diff scope

- Reviewed commits: `4c1e160` (feat: intercept strict reads and mutable service writes) and `0decf15` (fix: drop replaced service values outside framework locks)
- Diff: `c9d4caf..0decf15` (stacked on PR #54)
- Worktree: `/tmp/rutis-rev55`, checked out at `0decf15`
- Changes in this layer: about 1,123 lines (`intercept.rs` +481; `ctx.rs` +140, `registry.rs` +64, `error.rs` +45, `lib.rs` +7, README +2; 434 new lines in `tests/service_intercepts.rs`)

## Verdict

**Pass.** The implementation fully covers both read- and write-interception criteria in design §3. All verification commands passed, and tests cover the design's acceptance scenarios. A few positive behaviors lack dedicated assertions (see non-blocking observations).

## Acceptance criteria, checked individually

### Read interception

| Criterion | Evidence (test / code) | Result |
|---|---|---|
| `get/get_as` bypass interceptor chain | [`ctx.rs:445-475`] `get_as` calls `read_binding_as` directly, not `apply_read_interceptors`; `strict_reads_chain_and_locator_bypass_with_recorded_denial` confirms `seen == 0` after `get_as` | Pass |
| `require/require_as` supports `Continue \| Replace(Arc<T>) \| Deny` | [`intercept.rs:22-27`] defines the three `ServiceIntercept<T>` variants; [`intercept.rs:162-175`] maps each in the callback. Tests cover replacement (11), denial (`InterceptDenied`), and continue (a reentrant hook returns Continue) | Pass |
| Undeclared, out-of-scope, or unavailable access is rejected before hooks; hooks cannot make unreadable services readable | [`ctx.rs:520-579`] checks OutOfScope → TypeMismatch → Inactive → Undeclared → Unavailable before `apply_read_interceptors` ([`ctx.rs:639`]); interception runs only when `value == Some`. `strict_read_checks_reject_before_interceptor_runs` confirms Undeclared / Unavailable with `hits == 0` | Pass |
| Hooks match the full TypeKey and effective isolate scope; registering fiber must be an ancestor of the reader | [`intercept.rs:31-33`] uses `HookKey=(TypeKey, Option<ScopeId>)`; registration and read use corresponding scopes ([`intercept.rs:233`, `:283`]); [`intercept.rs:88-110`] filters owners by the `weak_fiber` parent chain. `hooks_match_full_key_scope_and_reader_ancestry` covers sibling isolation, left/right isolate scopes, and out-of-scope instance keys | Pass |
| Replacement affects only the current return value, not binding identity; `ServiceAccess` records the original binding | [`ctx.rs:639-647`] does not write the interception result into `Binding`; [`ctx.rs:648-661`] records `found` (original provider/generation). Test confirms `access.provider.is_some()` and `failure == InterceptDenied` | Pass |
| Same-key/same-operation synchronous reentry returns a clear error; different-key reentry is allowed | [`intercept.rs:137-158`] `ReentryGuard` uses thread-local `ACTIVE_HOOKS` keyed by `(kind, (TypeKey, scope))`; same-key read/write cases in `strict_reads_chain...` return `InterceptReentrant` | Pass (different-key positive case noted below) |
| Hook panics become explicit errors; shutdown waits for admitted hooks; hooks remove themselves on unload | Panic: `catch_unwind` → `InterceptPanicked` ([`intercept.rs:118-123`]); shutdown: `HookFlight` calls `begin_event`/`finish_event` for owners ([`:130-150`]), tested by `subtree_shutdown_waits_for_selected_read_hook`; self-removal: effect `AsyncDisposer` removes under admission lock ([`:246-259`]), tested by `repeated_hook_disposal_reclaims_tables` and `thousand_child_shutdowns_reclaim_instance_hook_keys` | Pass |

### Write interception

| Criterion | Evidence (test / code) | Result |
|---|---|---|
| `provide_mut_as` returns `ServiceWriter<T>`; stale-generation handles, removing bindings, non-owners, and type/instance scope violations fail | [`ctx.rs:775-786`] `provide_mut_as`; [`intercept.rs:367-388`] `set` checks shared identity, provider upgrade, `Arc::ptr_eq(actor, provider)`, scope, `in_instance_key`, and preflight. `mutable_writer_respects_owner_generation_and_write_hooks` covers non-owner and stale generation; `hooks_match_...` covers instance `WrongOwner` | Pass (removing-binding case in observation 2) |
| `set` verifies the caller owns the fiber that created the binding; commit rechecks Arc identity, provider, and generation before atomic replacement | [`intercept.rs:376-386`] checks `Arc::ptr_eq(&actor, &provider)`; [`intercept.rs:402-419`] rechecks `registration_open`, `transition.generation`, and state under admission; [`registry.rs:128-143`] `replace_mutable_if_current` checks `Arc::ptr_eq` and `removing`, then uses `mem::replace` | Pass |
| Existing `Arc<T>` is not mutated in place; holders of the old Arc retain the old value | [`registry.rs:55-63`] `replace_mutable` swaps the slot with `std::mem::replace` and returns the old value; `mutable_writer_...` confirms `*old == 1` remains unchanged after writing | Pass |
| Write preserves provider, generation, dependency tuple, and eviction relationships; it does not automatically reload consumers | [`intercept.rs:367-427`] `set` changes only the value slot; it does not touch `provider_id/provider_gen/last_deps` or call `notify_key_changed`. Test confirms `view.state().state == Active` (write does not evict) | Pass |
| Write hooks are selected along the provider's ancestor chain; a panic does not commit the candidate value | [`intercept.rs:393-401`] runs `Write` hooks with the validated provider as actor; `select` filters the provider ancestry. `hooks_match_...` confirms root writes do not invoke a hook owned by a; `mutable_writer_...` confirms panic → `InterceptPanicked` and value remains 13 | Pass |
| Ordinary immutable services do not gain a read lock for mutable slots | [`registry.rs:42-64`] separates lock-free `ValueSlot::Fixed` and locked `Mutable`; `provide_as` uses `mutable=false` ([`ctx.rs:762-763`]) | Pass |
| Fix `0decf15`: the replaced old value is dropped outside framework locks | [`intercept.rs:412-427`] drops `old` after `transition` and `_admission`; `catch_unwind` reports a user Drop panic to the sink. Covered by `replaced_value_drop_can_read_registry_after_commit` and `replaced_value_drop_panic_reports_to_sink_after_commit` | Pass |

## Verification commands and results

Toolchain: `1.98.1-x86_64-unknown-linux-gnu` (installed; no `rust-toolchain.toml` in the worktree, so `+1.98.1` was specified explicitly).

| Command | Result |
|---|---|
| `cargo +1.98.1 test -p rutis` | Pass: **223 passed / 0 failed** (222 unit + integration tests and 1 doc test). `tests/service_intercepts.rs`: 8/8 |
| `cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings` | Clean; no warnings |
| `cargo +1.98.1 fmt -p rutis -- --check` | Clean (exit 0) |

## Issue list

No blocking issues.

## Non-blocking observations

1. **No dedicated test that different-key reentry is allowed.** Design/task acceptance lists this explicitly, but `service_intercepts.rs` only tests same-key reentry errors (one read and one write). `ReentryGuard` at [`intercept.rs:137-158`] keys by `(kind, key)`, so different keys cannot collide; implementation is correct, only the positive assertion is missing.
2. **No dedicated test that a write fails while its binding is being removed.** `replace_mutable_if_current` checks `removing` ([`registry.rs:137-139`]) and returns Stale. `old_writer_cannot_commit_after_binding_is_replaced_during_hook` covers replacement during the hook, not the separate state where `removing` is set before replacement.
3. **No explicit `hits == 0` assertion that an out-of-scope request is rejected before invoking hooks.** `hooks_match_...` checks `OutOfScope`; code order returns before interception. Non-blocking.
4. **`select` can perform a spurious begin/finish pair.** When `has_any` is true but there is no matching hook for the key, it still calls `begin_event` and immediately `finish_event` on the actor fiber ([`intercept.rs:95-110`, `:113-116`]). Minor overhead, no correctness impact.
5. **Race between `has_any` and table lock during registration/read.** If `has_any` is false, `run` returns without locking the table. A concurrent registration that lands between `push` and `fetch_add` can be missed by that read. This is the same non-linearizable registration/concurrent-read boundary as EventBus; the design does not require linearization, so non-blocking.
6. **“Same-key” reentry is keyed by `(TypeKey, scope)`.** Because `HookKey` includes scope, reentry on the same TypeKey in different isolate scopes is not treated as same-key. This matches scope being part of the effective match key.

## Key design checks (with evidence)

- Interceptors are trusted extension points: `Replace` can inject a same-type value from a sibling instance, an accepted design boundary (design §3, line 99). The implementation does not prove provenance, as specified.
- Writes are stricter than Cordis `internal/get/set`: rutis interceptors cannot cross type, instance, or dependency-declaration boundaries; [`ctx.rs:520-579`] checks run before hooks.
- Hook registration/removal follows instance-subtree boundaries: registration calls `check_instance` ([`intercept.rs:226`]); effects belong to the owner fiber ([`:242-259`]).
