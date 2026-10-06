# cordis-tests-04: Add Tests for Three Acceptance Coverage Gaps

```yaml
id: cordis-tests-04
package: rutis
module: tests
status: done
depends-on: []
```

## Objective

At tip `0decf15` (top of the stacked PR #55 series), add three tests covering gaps found by independent reviews. Add tests only; do not change implementation code.

## Context

- Baseline: `0decf15` (all three layers of PRs #53/#54/#55); worktree `/tmp/rutis-dev55`.
- Design document: `/media/eric8810/fast-deliver/code/rutis/docs/design-cordis-observation.md`.
- Three gaps (from issue 1 in [rev-cordis-dispatch-01](../reviews/rev-cordis-dispatch-01.en.md) and observations 1/2 in [rev-cordis-intercept-03](../reviews/rev-cordis-intercept-03.en.md)):

1. **Observer reenters dispatch** (`tests/dispatch_observation.rs`): dispatch again (emit or serial) inside the observer callback; assert that nested dispatch is also observed, does not deadlock, and its inner business listener runs. Design §1 explicitly requires a “reentrancy” test.
2. **Reentry on a different key is allowed** (`tests/service_intercepts.rs`): while hook A runs, read/write another hooked key B; it should succeed rather than return `InterceptReentrant`. Cover read and write separately or at least cover the scenarios in the design text. Design §3 says same-key/same-operation synchronous reentry errors clearly, while different-key reentry is allowed.
3. **Write fails during binding removal** (`tests/service_intercepts.rs`): after `binding.removing` is set but before the slot is replaced, `writer.set` returns `Stale`. Design §3 says a binding being removed always rejects writes.

## Paths

- `crates/rutis/tests/dispatch_observation.rs` (gap 1)
- `crates/rutis/tests/service_intercepts.rs` (gaps 2 and 3)

## Verification

- Each new test passes and asserts the intended behavior rather than doing no-op work.
- Full regression: `cargo +1.98.1 test -p rutis`.
- `cargo +1.98.1 clippy -p rutis --all-targets -- -D warnings`.
- `cargo +1.98.1 fmt -p rutis -- --check`.

## Result

- **Conclusion: pass** (developer Wu Junjie, `795c165`; independent reviewer Zhou Wenbin; record [rev-cordis-tests-04](../reviews/rev-cordis-tests-04.en.md)).
- Only two test files changed (+141 lines); no implementation changes. Tests: `observer_reentry_observes_nested_dispatch_and_business_listener_runs`, `different_key_reentry_allowed_for_read_and_write`, and `writer_set_fails_stale_during_binding_removal`.
- 226 tests passed / 0 failed; clean clippy/fmt; `service_intercepts` passed 20 consecutive runs, confirming deterministic timing.
- Commit `795c165` was later reorganized by branch; the reentry test went into PR #53 (`5bc64fc`), and all three PRs merged to main (`f37f90d`).
