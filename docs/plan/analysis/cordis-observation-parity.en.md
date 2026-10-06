# Cordis Observation and Interception Parity: Delivery Analysis

**Request:** Independently accept three stacked implementation PRs against [design-cordis-observation](../design-cordis-observation.en.md), producing separate reports so the user can decide whether to merge them to `main`.

## State and input versions

| Layer | PR branch | Commit | Reviewed range |
|---|---|---|---|
| #53 pre-dispatch observation | `feat/cordis-dispatch-observation` | `5e107bf` | `7d7402d..5e107bf` |
| #54 effect cleanup tree | `feat/cordis-effect-tree` | `c9d4caf` | `5e107bf..c9d4caf` |
| #55 service read/write interception | `feat/cordis-service-intercepts` | `4c1e160`, `0decf15` | `c9d4caf..0decf15` |

- All three branches are based on `7d7402d` (the design document baseline). `main` is at `d0b7498`; the only difference is the design document and README changes from MR #39.
- The design document is on the main repository branch, not in the implementation branches; reviewers read it from the main repository's absolute path.
- This round is review only; it does not merge changes. Results are written back to each task file.

## Task assignments

| Task | Work | Worktree | Status |
|---|---|---|---|
| [cordis-dispatch-01](../tasks/cordis-dispatch-01.en.md) | Independently review PR #53 | `/tmp/rutis-rev53` | done (pass) |
| [cordis-effect-02](../tasks/cordis-effect-02.en.md) | Independently review PR #54 | `/tmp/rutis-rev54` | done (pass) |
| [cordis-intercept-03](../tasks/cordis-intercept-03.en.md) | Independently review PR #55 | `/tmp/rutis-rev55` | done (pass) |
| [cordis-tests-04](../tasks/cordis-tests-04.en.md) | Add tests for three coverage gaps (#53 reentry; #55 different-key reentry and writes during removal) | `/tmp/rutis-dev55` | done (pass, `795c165`) |
| [cordis-fix-05](../tasks/cordis-fix-05.en.md) | Fix PR #55 P1: a failed write deadlocks when the candidate value is dropped under a lock (with regression test) | `/tmp/rutis-dev55` | done (pass, `68491a7` + `c0b6b41`) |

The three reviews were independent and read-only; none changed the implementation under review. After the user's decision, the main agent checked the findings and found that one claim (`hits == 0` for an out-of-scope case) was false: the `hooks_match` test already asserts the hook ran. The other findings were valid; only three missing tests warranted action, so `cordis-tests-04` was assigned.

## Timeline

- 2026-09-24: The user chose to review and report first, without merging to `main`. Three independent reviews were assigned (deepseek-v4-pro).
- 2026-09-24: All reviews passed with no blockers; 209/213/223 tests passed respectively, with clean clippy and fmt. There were 12 non-blocking observations, mainly coverage gaps. Merge decision pending user review.
- 2026-09-24: The main agent verified each observation: 11 were valid; one (no assertion that the out-of-scope hook ran) was already covered by `hits == 1` in `hooks_match`. The user chose to add a #53 reentry test and two #55 gap tests; `cordis-tests-04` was assigned to add tests only at `0decf15`.
- 2026-09-24: `cordis-tests-04` completed as `795c165` (+141 lines, tests only); independent review passed (Zhou Wenbin), with 226/226 tests and clean clippy/fmt. `service_intercepts` passed 20 consecutive runs. The full chain was ready; merge decision pending.
- 2026-09-24: Remote PR reviews arrived: #53/#54 approved without issues; #55 reported P1 because a failed write dropped a candidate value under a framework lock, allowing a reentrant `Drop` to deadlock (the reviewer supplied a reproducer). The main agent confirmed the mechanism and initially identified three paths; `cordis-fix-05` was assigned using red/green testing.
- 2026-09-25: `cordis-fix-05` completed and passed independent review. Reinspection corrected the three-path assessment: due to Rust's reverse declaration-order drop, paths 1/2 drop the candidate after releasing the lock and do not deadlock. The real P1 was only path 3, as the remote review said. A restart experiment confirmed path 2 is reachable (`err.generation == 1`) and does not deadlock on the old code; test 3 now documents this behavior (`c0b6b41`). Fix `68491a7` made lock boundaries explicit on all paths (IIFEs); red-to-green evidence showed tests 1/2 deadlocked before the fix. Full suite: 229/229, clean clippy/fmt, 20 stable runs. Local chain `0decf15 → 795c165 → 68491a7 → c0b6b41` was ready; merge decision pending.
- 2026-09-25: Per the user's decision, work was reorganized by branch: #53 received the reentry test (`5bc64fc`), #54 was rebased, and #55 included two coverage tests, the P1 fix, and semantic clarification (tip `8cac519`). After push, remote review passed and all three merged (`main` at `f37f90d`). #40 closed automatically; #27 and #29 were closed after comparing their acceptance criteria. The design document was marked implemented and this report archived.
