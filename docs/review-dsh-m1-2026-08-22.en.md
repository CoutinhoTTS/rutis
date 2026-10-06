# Independent Review of the M1 Protocol Layer (2026-08-22)

> Subject: commit `615c035` (`feat(dsh): M1 protocol layer`), using v3.1 of [design-dsh-bridge-2026-08-21.md](design-dsh-bridge-2026-08-21.en.md) as the sole baseline.
> Method: an independent review agent compared `proto.rs` / `m1.rs` / `lib.rs` / `Cargo.toml` line by line; `cargo test -p rutis-dsh` was run (14 passed); repository grep checked M1 prerequisites.

## Overall conclusion: M1 acceptance — **conditional pass**

All 14 acceptance tests passed, and most exercised real semantics (timeout means no waiting, orphan counting for late responses, out-of-order correlation, and convergence to terminal state). Four of six connection rules are fully implemented; the method table provides a mostly sufficient surface for later milestones. Dependency discipline (rutis only, no rutis-agent/aimux) and project size (`proto.rs` 766 lines against “hundreds”; crate total 1,341 lines against an approximate 1.5k budget) meet expectations.

**Conditions for sign-off:** land fixes for Z1, F1, and F6 before M1 sign-off; move F2–F5 to the next round; resolve S/D items as M2 is finalized. The fixes are within roughly a hundred lines and do not change architecture.

## Blocking issue

- **Z1: Reserved `sessionId` / `turnId` fields are missing; module comments rewrite the design contract.** `Frame` in `proto.rs` has only `scope_id`. The module comment says “all frames reserve scopeId,” reducing the design's **three fields** to one. Design §3, rule 6 says: “Reserve `sessionId` / `turnId` / `scopeId` fields in v1, broadcast globally without filtering, enable them in v2—declare this explicitly.” M1 freezes the wire format; reservation is part of this milestone's contract. **Fix:** add two optional fields to Req/Ntf/Res (rename + `skip_serializing_if`) and mirror them in wire-format round-trip tests. If serde's tolerance of unknown fields is considered sufficient to defer them, record that as an explicit deviation and update the design.

## Required fixes

- **F1: “The first frame must be hello” applies only to Req.** The Connecting check is inside the Req arm. If the host starts with Ntf/Res, the frame is silently swallowed, the session remains Connecting, and `ready()` hangs. **Fix:** move the Connecting check above the match; any of the three non-hello frame types before handshake should transition to Failed. Add first-frame Ntf/Res tests.
- **F2: `ProtoError::Cancelled` is a dead variant.** Nothing in the repository constructs it; cancellation is actually delivered as `Remote{code:"cancelled"}`. Rule 4 requires callers to distinguish “I cancelled this” from “remote error.” **Fix:** map it when settling the request, or remove the variant; choose one.
- **F3: Reentrancy test is a shadow test.** The host sends Ntf and immediately responds with Res; a serial pump would pass all assertions too. **Fix:** hold Res after Ntf until a signal from the hook side arrives. This proves the pump processed the event while Res had not arrived.
- **F4: `notify` / `cancel` have no state gate.** They still write frames to the wire after terminal state. **Fix:** reuse the request state check.
- **F5: `wf/register` `kind` has no three-shape reservation.** Rule 5 gives `mode` a typed enum and rejection test, but `kind` is asymmetric. Design §5 says: “`kind ∈ decide|around|stream`, **reserve the v1.1 frame field immediately**.” **Fix:** add `WfKind` + declaration struct + serde rejection test, or state in a comment that stringly typed reservation is intentional and update the design.
- **F6: No recorded outcome for the §10.2 synchronous API audit (M1 prerequisite).** The commit contains no audit artifact and `docs/` has no record (unlike M0's experiment report). **Fix:** run the audit and add its conclusion. **Note:** this machine did not have the dsh repository's plugin-reference/raw data; recover or rescan the corpus first (see the pending-data issue in the experiment-m0 report).

## Recommendations

- **S1:** A hook panic leaves the request without a response; the host can only rely on timeout. Add `catch_unwind` or document the contract explicitly.
- **S2:** `ok:true` with no `result` is classified as malformed. Many method-table entries have “—” for result; an empty host response may be treated as a wire-format violation. Accept it as Null or explicitly require a result.
- **S3:** Malformed-response logic is duplicated: public `Frame::outcome()` and the pump each implement it, but the pump does not reuse the former; they may drift.
- **S4:** Pump lifetime: if all handles are dropped while wire remains alive, the pump retains an Arc forever. No M1 impact; record this for later transport implementation.

## Recorded disagreements

- **D1 — `timeoutMs` location:** the design's method table puts it inside `svc/call` params; implementation promotes it to a `request()` parameter and enforces a global bridge limit. Defensible because enforcement belongs in the bridge; revisit the method table when M2 fixes `svc/call` params.
- **D2 — Hello response omits `protocol` and echoes the host's `dshSemver`.** Defensible; leaning toward echoing protocol to allow symmetric host validation.
- **D3 — Bridge is one session lifecycle, with no restart → re-handshake path.** Bridge-side semantics for host/restart (rule 2) are undecided in both design and implementation. Resolve before M2; otherwise “kill process + relaunch” implements only half the rule.

## Acceptance rows checked (C-dimension summary)

Round trip / concurrent out-of-order responses / cancellation + orphan count / timeout (proved actual non-waiting: `m1.rs` settles the call as Timeout before the host returns any response) / handshake mismatch (protocol + semver) / capability-set difference (pure-function level; acceptable because M1 has no loading flow; do not reinvent this when wiring M1.5/M2) / duplicate-name arbitration / reentrancy (insufficient strength, see F3) / host kill (pending list + frame count + new calls immediately return HostGone after death; assertions check concrete values).

## Dependency and layering discipline (E-dimension)

Dependencies are rutis + serde/serde_json/tokio/thiserror, with no rutis-agent or aimux. Layering rules in §§1/8 are respected; the only actual rutis API used is the `BoxFuture` alias. The public surface has 17 items and is restrained.

---

## Re-review after the two-level bridge restructure (v3.2, 2026-08-22)

The architectural decision prompted by this review (two-level bridge, design v3.2) and the fixes landed together. `cargo test -p rutis-cordis -p rutis-dsh` passed **17 + 4 tests**:

| Finding | Resolution | Location |
|---|---|---|
| Z1: reserve three fields | ✅ Fixed: all three frame types carry `scopeId` / `sessionId` / `turnId` (`scopeId` interpreted by the base bridge; session/turn by the dsh layer). Wire shape is defined once and mirrored by round-trip tests. | rutis-cordis `rpc.rs` + `frame_reserved_fields_roundtrip` |
| F1: first-frame rule | ✅ Fixed: Connecting check moved above the match; non-hello Req/Ntf/Res first frames all reach Failed terminal state (invalid Req gets an error Res; Ntf/Res have no request owner and disconnect). Added Ntf/Res first-frame tests. | rpc.rs pump + `first_frame_ntf_rejected` / `first_frame_res_rejected` |
| F2: dead variant | ✅ Fixed: `CallSettled::Cancelled` settlement variant; cancellation is delivered as `ProtoError::Cancelled{id,method}`, distinguishable from remote error. | `cancel_settles_as_cancelled_and_late_res_counts_orphan` |
| F3: shadow test | ✅ Strengthened: hold Res until the event reaches the hook (five-second timeout gate); “pump does not block” is now asserted. | `reentrant_events_processed_before_call_settles` |
| F4: state gate | ✅ Fixed: `notify` / `cancel` reuse `check_open`, so terminal state rejects them. | Tail assertion in `host_death_...` |
| F5: reserve WfKind | ✅ Fixed: `WfKind{Decide,Around,Stream}` + `WfDeclaration` live in the Cordis vocabulary layer (v3.2 decision: waterfall is Cordis semantics), with a closed-domain test. | `wf_kind_three_shapes_frozen` |
| S2: missing result | ✅ Fixed: `ok:true` without result is accepted as `Null`. | `Frame::outcome` |
| S3: duplicate logic | ✅ Fixed: pump reuses `Frame::outcome()`. | pump Res arm |
| D2: protocol echo | ✅ Fixed: hello response echoes `protocol`. | `handshake_replies_symmetric_capability_set` |
| F6: §10.2 audit | ✅ Completed (see “§10.2 synchronous API audit” below): using the correct bridge surface (`LlmAdapter`), the intersection of synchronous members is empty. Corpus location and missing details are recorded here too. | This document, next section |

**M1 was signed off under the two-level bridge structure (2026-08-22):** all acceptance rows are covered by 17 rutis-cordis mechanism tests, and the dsh-side two-level handshake by 4 rutis-dsh tests. Z1/F1 release conditions and F2/F4/F5/S2/S3/D2 fixes all landed; sign-off followed completion of F6.

## §10.2 synchronous API audit (2026-08-22, F6)

**Data source:** the corpus is on SSH server `eric8810@100.121.215.57:/media/eric8810/fast-deliver/code/dsh-ecosystem/` (10 research Markdown files + machine-readable TSV under `raw/`; 9,398 shallow-cloned repositories under `repos/`, 70 GB; analysis scripts under `scripts/`). The review originally cited `ctx-repo-operations.tsv` (repository-level detail), which is missing. However, the member-level aggregation in `research/ctx-operations.md` (counts by repo × service × member) is sufficient for this audit. Regenerate repository-level detail with `scripts/ctx-repo-detail.py` if needed.

**Scope correction:** v1 does not bridge the entire `ctx.llm` service; it bridges the **`LlmAdapter` surface** (llm seam: TypeScript `LlmRuntime` remains local, and the Rust aimux implementation is registered as one adapter). Synchronous registry members on `LlmRuntime` itself are not part of the bridged surface.

**Conclusion: the intersection is empty; audit passes.** Member-by-member classification (dsh source `packages/llm/llm` + 897 community repos / 19,477 llm calls):

| Members (community call count) | Surface | Sync? | Crosses the bridge? |
|---|---|---|---|
| `registerAdapter` ×5421 / `listProviders` ×2921 / `listConfigurableProviders` ×1350 / `registerConfigurableProviders` ×628 / `registerModelDiscovery` ×585 / `providerRetryPolicy` ×405 | `LlmRuntime` (stays in TS) | Synchronous | No — local registry operations, no bridge impact |
| `stream` ×1830 / `prepareCall` ×52 / `resolveModelInfo` ×2302 / `discoverModels` ×2273 / `listModels` ×1458 / `resolveCallConfig` ×156 | `LlmRuntime` → adapter delegation | **Async** | **Yes (llm seam)** |
| `providerInfo` / `providerRetryPolicy` (adapter declarations) | `LlmAdapter` | **Synchronous** | **Registration-time snapshot; does not cross the wire:** `prepareRoutes` calls these synchronously at registration and snapshots the result (`{id,name}` + retryPolicy) into the registry. Runtime `LlmRuntime.providerRetryPolicy` reads that snapshot. The bridge adapter supplies both synchronous members from static metadata that remains in TS. |
| `llm/stream` ×462 / `llm/adapters-updated` ×270 | Event subscriptions | — | Event seam (emit notification); unrelated to synchronicity |

**Additional finding (recorded, non-blocking):** the community directly accesses private `LlmRuntime` members (`registration` ×22, `adapters` ×13, `streamWithRegistration` ×9) and mounts non-official surfaces (`addProvider` ×27, `complete` ×4, `registerProviderAuth` ×3, `isPiEngineEnabled` ×2, `__dshVisionResolveWrapped` ×3), about 83 calls total. Since `LlmRuntime` stays in TypeScript this is harmless; **these are known break points if LlmRuntime itself is Rustified**, and are also useful dimensions when selecting M1.5 corpus samples.
