# dsh Bridge Design (v3.2): Two-Seam Foundation Bridge (Two Levels: Core Bridge × dsh Surface)

> 2026-08-21 v1 (agent layer) → v2 (kernel layer, min-cordis foundation) → v3 (direction revised: TS runs the full dsh stack, Rust provides the foundation; rutis-agent remains an independent baseline) → **v3.1: incorporated findings from the independent review ([review-dsh-bridge-2026-08-21.md](review-dsh-bridge-2026-08-21.md)) that remain valid under the v3 direction** (transport layer, version facts, reserved protocol fields, live-object substitutes, waterfall type taxonomy, reordered M0, fidelity tiers, M7 moved earlier).
> Basis: three rounds of direction discussion, D31, [design-dual-core-2026-08-20.md](design-dual-core-2026-08-20.md), ecosystem measurements ([plugin-reference](../deepseek-harness/plugin-reference/)), and review recalculations.
> **2026-08-22: M0 completed. Both questions passed, including a real-process rerun of the official pwsh loader-composition spec. Foundation decision = min-cordis; the review's §B concern about a loader fork is invalid. Experiment: [experiment-m0-min-cordis-2026-08-22.md](experiment-m0-min-cordis-2026-08-22.md).**
> **2026-08-22 v3.2: split the bridge into two levels—`rutis-cordis` (foundation bridge, generic Cordis surface: loader arbitration/service registration/four event dispatch modes/isolate scopes) and `rutis-dsh` (dsh surface: LLM seam/event type mapping/substitute table/dshSemver/session-field semantics). Decision basis: M1.5/M7 corpus is the Cordis plugin ecosystem (8,889 candidate repositories, 423 use `ctx.loader`); the bridge must load arbitrary Cordis plugins, not speak only dsh. Vocabulary ownership is in §3.2. M1 independent review ([review-dsh-m1-2026-08-22.md](review-dsh-m1-2026-08-22.md)) findings Z1/F1/F2/F5 are assigned to the appropriate layer.**
> **2026-08-22: M1 signed off (two-level structure, all 17+4 tests green). M1.5 minimum-corpus matrix executed: L0 = 4/10; official closure of 236 packages has no gaps (confirms §10.11); remaining gaps are community workspace-internal and third-party packages. Corpus needs shape filtering (fork/VSCode forms). Experiment: [experiment-m1p5-corpus-2026-08-22.md](experiment-m1p5-corpus-2026-08-22.md).**
> **2026-08-22: M2 signed off (6 commits through 82736d1): real TcpWire transport + event seam + end-to-end LLM seam + complete turn (two rounds: tool call crosses boundary → host executes → result returned → final-turn text) + L3 mapping (system/messages/tools/finish/usage field by field) + `console.log` injection acceptance (frame stream remains intact under 300 lines of stdout noise). Transport scope: loopback TCP (behavior of the dedicated-channel decision); native fd3/unix-socket implementation remains in Linux lane. Turn semantics are verified; full-stack assembly of the real dsh agent loop is the next phase (second layer).**

## 1. Goal, Layering Discipline, and Scope

**Goal (two faces, v3 scope):**

1. **Foundation direction (all v1 content):** on the TS side, run a **complete dsh deployment**—real agent loop, real services, real plugins. Rust provides **model calls (aimux), event observability, and services migrated piece by piece**. The bridge is a foundation, not the host.
2. **Orchestration direction (deferred, with reevaluation trigger):** TS calls rutis as an engine. Defer until dsh loop semantics converge and rutis-agent grows from a baseline into an engine. **Review disagreement:** independent review §6 argued to advance orchestration (bounded uncertainty, zero breakage, prerequisites overlap with wave 1). This design's conservative deferral is the direction-discussion decision, not a rejection of those arguments. Reevaluate at the first roadmap review after foundation M2 passes.

**Layering discipline (fixed; three legs from v3.2):**

```text
rutis-agent  ──→ rutis                    independent baseline, no dsh knowledge, separately tested/released
rutis-cordis ──→ rutis                    foundation bridge: Cordis concepts (loader arbitration/services/
                                          four event dispatch modes/scopeId), no dsh knowledge
rutis-dsh    ──→ rutis-cordis + aimux     sole home of dsh relationships (LLM seam/event mapping/
                                          substitute table/dshSemver/sessionId/turnId semantics)
rutis-agent and rutis-cordis are siblings over the kernel and do not depend on one another; rutis-dsh builds on the foundation bridge.
```

Everything dsh-specific (dsh protocol event names, dshSemver, session-field semantics) stays in `rutis-dsh`. Everything Cordis-specific (loading, service names, event-dispatch semantics, isolate) stays in `rutis-cordis`. Any Cordis TS plugin (corpus or community plugin) can load and be observed through the foundation bridge without being a dsh deployment. The baseline's 169 tests prove its self-containment: bridge refactors, bridge bugs, or a direction reversal do not change the baseline.

**Coverage: fidelity tiers (v3.1 replaces binary metric):**

The v2 “94% loadable” claim is **withdrawn**—review recalculation showed it cannot be reproduced (same-source “service set ⊆ bridged set” was 1,242 repositories / 18.6%), and static call sets were never the right threshold (value-level imports were not covered; see §10.11). Starting with v3.1, measure **fidelity tiers**, each paired with corpus counts and known breakage inventory:

| Tier | Meaning | Assertion |
|---|---|---|
| L0 | Loads | Host loading succeeds without exception |
| L1 | Registration takes effect | Tool schema / prompt sections enter the assembly result of the real TS loop |
| L2 | Events arrive | Subscribed events reach Rust observers in emission order |
| L3 | Model-surface fidelity | Complete turns through the LLM seam match the native adapter (chunk granularity / finish / usage) |
| L4 | Event-payload fidelity | Substitutes for live-object payloads (agent/signal/actor) are synthesized correctly (§4.3) |
| L5 | Rust-migrated service fidelity | Behavior remains equivalent after sandbox/fs replacement (define piece by piece when due) |

**Non-goals (updated v3.1):** ~~load TS tools into a Rust driver~~ (adapter layer removed); embed a JS engine; bridge synchronous APIs; Electron plugins and 84 distributions; bridge waterfall (disabled in v1, enabled for Rust-migration wave, §5).

## 2. Overall Structure

```text
┌─ rutis process ────────────┐        ┌─ Node host process (full dsh deployment) ─┐
│ rutis kernel               │        │ min-cordis foundation (preferred) ↳      │
│ aimux (model integration) │◄─ fd3 ─►│ fallback vendor/cordis                    │
│ rutis-cordis (base bridge)│ or sock │ dsh agent loop + service packages + TS plugins │
│ rutis-dsh (dsh surface)   │        │ TS bridge also has two levels: base bridge│
│ (observers: UI/telemetry/  │        │ (loading/generic evt forwarding) + dsh    │
│  corpus)                   │        │ bridge (LLM adapter)                     │
└───────────────────────────┘        └────────────────────────────────────────────┘
   Pure Cordis plugins (M1.5/M7 corpus) connect only to foundation bridge.
   rutis-agent (independent baseline; no bridge; outside this data flow)
```

- **Transport (review §4.1 adopted):** use a **dedicated channel for frames—fd 3 or Unix socket**; leave stdout/stderr for plugin logs. The newline-JSON-over-stdout approach is withdrawn: one `console.log` in any TS plugin corrupts framing, triggers restart under rule 2, and can loop. (Among 8,889 candidate repositories, many are generated; we cannot assume they never print.)
- **Foundation decision (M0, reordered after review §5):** (1) load `tool-bash` on min-cordis and print schema; (2) pass its `loader-composition.spec.ts`. The real fork point in min-cordis is **loader / include / group** (about 1,400 lines explicitly removed; 16 official dsh loader-composition specs are package-level contracts, and 423 community repositories use `ctx.loader`), not Proxy (1359 references to Context in dsh package; min-cordis reflect surface is comparable). Config validation is closed (schemastery implements `~standard`; validation paths match line-for-line). **Fallback cost is lower than v2 estimated:** dsh vendors Cordis in its own repository as a workspace dependency, so versions are naturally pinned; there is no version-chasing cost.
- **Lifecycle pairing:** host process belongs to bridge-plugin fiber; TS plugin lifecycle is managed by TS, while Rust only observes evt stream. **Hard constraint (spirit of review §3.7, Rust-side):** every service provided by the bridge is an optional dependency for rutis-agent (soft query), never included in driver `injects`; host restart must not trigger driver generation change. Under v3 the driver does not connect to bridge at all, but keep this as a red line for future composition.

## 3. Protocol (Bridge Protocol v1.1: Kernel Primitives + Reserved Fields)

Three message classes, bidirectional concurrency, each request has a correlation ID; **every frame reserves `scopeId`** (not filtered in v1—kernel decision D29 says events are not isolate-filtered; filtering is dsh semantics and will be implemented bridge-side without kernel changes).

```json
{"type":"req","id":1,"method":"...","params":{...}}
{"type":"res","id":1,"ok":true,"result":{...}}
{"type":"res","id":1,"ok":false,"error":{"code":"...","message":"..."}}
{"type":"ntf","method":"...","params":{...}}
```

**Six fixed connection rules:**

1. **`hello` handshake + capability negotiation (two levels, v3.2):** host first sends `{protocol: 1, base, baseSemver, stack, caps: {services, wfKinds, scopes}}`—the **foundation-level** handshake, validated by `rutis-cordis` (`base` ∈ min-cordis|cordis, loader/event capabilities). A dsh deployment **adds `dsh: {dshSemver, services}`**, validated by `rutis-dsh;` pure Cordis hosts omit it. Rust returns symmetric capabilities by level. **Version facts (corrected in review §4.6):** measured dsh tag is `dsh-v0.1.0-rc.7`, all 219 packages use the same version; `dshSemver` declares that value. **npm release facts (rechecked 2026-08-22):** official packages are on npm; `next` dist-tag points to `0.1.1-rc.2`, with all historical versions present; `latest` still points to old `0.0.1-rc.1`, so use an exact version or `@next`. The terminal host (`host/`) uses exact npm versions aligned to dshSemver, not local checkout; M2 local-source references are experimental only. Capability differences are applied **at load time**: `plugin/load` returns `injects`, compare with host capabilities, and explicitly reject or warn on non-empty difference (§10.8). Version mismatch is reported during handshake (failure at either level fails handshake).
2. **Host-level `host/restart`:** kill and relaunch process; covers cases cancellation cannot reach.
3. **Declaration and same-name arbitration:** replace the whole plugin as one unit (idempotent reload); reject later duplicate name and identify existing owner.
4. **Complete cancellation semantics (review §4.5):** prefix `target` with type (`call:` / `inv:` / `disp:`) to avoid namespace collision; **drop and count late `res` as orphan responses after cancellation**; caller declares timeout for each call class in the request (bridge supplies default; configurable global maximum).
5. **Event dispatch modes (review §3.2 reserved):** `evt/on` declaration includes `mode ∈ emit | parallel | serial`. `emit` uses ntf; `parallel` / `serial` use request shape (`evt/invoke {dispatchId, event, mode, params}` → `res {bail?}`), with timeout on Rust side; timeout counts as failure and does not wait. The v1 foundation direction consumes only emit; reserve parallel/serial for Rust-migration wave (storage flush semantics): `session/flush` (parallel, 1,190 corpus references) and `agent/turn-stopping` (serial, 273) **must not silently degrade to ntf**.
6. **Session ownership (split by layer in v3.2):** reserve `sessionId` / `turnId` / `scopeId` in all v1 frames, broadcast globally without filtering, and explicitly enable in v2. Define the three wire fields once in the `rutis-cordis` frame envelope, shared by both levels. **Semantic ownership is layered:** `scopeId` is Cordis isolate semantics (foundation bridge); `sessionId`/`turnId` are dsh session semantics (`rutis-dsh`; foundation bridge only forwards them without interpretation).

### 3.2 Vocabulary Ownership (Added in v3.2)

| Concept | Layer | Location |
|---|---|---|
| Frame envelope (req/res/ntf + correlation ID + three reserved fields) | Wire format | `rutis-cordis` (defined once) |
| Event dispatch mode (emit/parallel/serial) | Cordis | `rutis-cordis` (four-mode event-bus semantics) |
| Waterfall kind (decide/around/stream) | Cordis | `rutis-cordis` (same; type taxonomy in §5) |
| Plugin loading/name arbitration/idempotent reload | Cordis | `rutis-cordis` (loader semantics) |
| `base` (min-cordis/cordis), loader capabilities | Cordis | `rutis-cordis` hello |
| `scopeId` (isolate) | Cordis | `rutis-cordis` |
| `dshSemver`, dsh service set (injects difference) | dsh | `rutis-dsh` (`dsh` hello section) |
| `sessionId`/`turnId` semantics | dsh | `rutis-dsh` |
| LLM seam (`svc/define` LLM surface → aimux) | dsh | `rutis-dsh` |
| `agent/*` event mapping and live-object substitutes | dsh | `rutis-dsh` |
| Host-restart orchestration for `host/restart` | dsh | `rutis-dsh` (`rutis-cordis` supplies process handle) |

### Method Tables (v1.1)

**Rust → TS host**

| Method | Params | Result | Purpose |
|---|---|---|---|
| `plugin/load` · `plugin/unload` | `{pluginId, entry, config}` | `{ok, injects}` | Controlled loading (foundation experiment and corpus use) |
| `svc/call` | `{service, method, params, timeoutMs}` | Call result | Call TS-side service |
| `wf/enter` | `{invocationId, event, value}` | See §5 | Two-phase waterfall (enabled in Rust-migration wave) |
| `cancel` | `{target: "call:12"\|"inv:3"}` | Notification | Propagate cancellation (rule 4) |
| `host/restart` | `{reason}` | — (kill + relaunch) | Rule 2 |

**TS host → Rust**

| Method | Params | Result | Purpose |
|---|---|---|---|
| `hello` | Rule 1 | Capability set | Handshake |
| `svc/define` | `{service, methods}` | `{ok}` | **LLM seam** (TS declares LLM surface; Rust implements via aimux); Rust-migrated services use same channel in reverse |
| `evt/on` | `{pluginId, events:[{name, mode}]}` | `{ok}` | Declare event subscriptions (**event seam**) |
| `evt/emit` | `{event, params}` | Notification | Send TS-side events over bridge |
| `evt/invoke` | `{dispatchId, event, mode, params}` | `{bail?}` | Parallel/serial dispatch (reserved) |
| `wf/next` | `{invocationId, value}` | — | Two-phase continuation point (§5, enabled in migration wave) |
| `wf/register` | `{pluginId, events:[{name, kind}]}` | `{ok}` | kind ∈ decide\|around\|stream (§5) |

**Two choices for event mapping (review §4.3 decision: typed):** each bridged event has a Rust type in `rutis-dsh` (closed compile-time set, consistent with version declarations and centralized adapters; preserves per-type ordering and does not collapse everything into one chain). A new dsh event requires a Rust release, which is inherent in versioned declarations. **Reject** a single erased envelope: it would collapse all bridged events into the same D31 tail chain, letting one slow observer block all of them, and native rutis listeners would lose types.

### 3.5 Semantic Differences (What the Bridge Cuts; Discipline for Each)

| # | Semantics cut | Difference | Implementation discipline |
|---|---|---|---|
| 1 | **Synchronous emit** | TS emit is synchronous/inline; across bridge it is asynchronous one-way | Rust observers must not assume “emit returned means processed”; TS plugins are unaware and remain synchronous in their own process |
| 2 | **Event order** | Same type: emission order = arrival order (D31 tail + one-connection FIFO); across types no order guarantee | Retain D31 boundary; with typed mapping “same type” means Rust event type |
| 3 | **Cancellation strength** | `cancel` → cooperative AbortSignal | Treat timeout as failure (rule 4); host/restart handles persistent non-response |

## 4. Two Seams (All in v1) and Rust-Migration Seams (Added Incrementally)

1. **LLM seam:** register bridge on TS side as a **dsh LLM adapter** (use ecosystem mechanism; do not hijack ctx.llm): `registerAdapter("aimux", …)`. Stream crosses to aimux; chunks return as evt stream with bounded-queue backpressure. Preserve dsh's own adapter ecosystem; aimux joins as an optional adapter. **Fidelity focus (review §3.3b):** `llm/stream` is a dsh **streaming waterfall** (value is AsyncIterable). Implement this seam in adapter layer, not wf layer, to avoid stream-protocol problems. Differences in chunk granularity / finish / usage are the source of L3 claims (§9 M2).
2. **Event seam:** `evt/on` declaration → `evt/emit` outbound → Rust observers (UI/telemetry/corpus). **Drop strategy varies by event category (review §4.2):** drop and count discrete events (tool boundaries, state changes); **never drop ordered incremental streams** (text-delta class)—coalesce adjacent deltas or reconnect/restart. Dropping an ordered stream silently corrupts reconstructed text.
3. **Payload substitute table (review §3.5 adopted; defines L4):** for each bridged event carrying live objects (`agent: Agent`, `signal: AbortSignal`, `exec`/token, `session: Session`, fs intent `actor`), specify **wire representation (ID/snapshot), side that synthesizes substitute, and unavailable members**. Example: `actor` crosses as `{agentId, sessionId}` snapshot. Official fs-observation-policy navigates down (`actor?.agent?.session`) and uses session object as accounting key; substitute must synthesize equivalent key semantics or read-before-write gates silently fail. Maintain table per event type; it is an executable form of the long tail of behavioral fidelity and the source of M7 assertions.
4. **Rust-migration seam (from wave 2):** implement sandbox/fs/jobs as Rust services; expose replacements for same-named TS services in reverse through `svc/define`; use waterfall when a service needs gating (§5).

**Ownership decisions (v3.1):** 169 agentLoop users run inside real loop and are not removed; preserve `llm.registerAdapter` ecosystem and coexist with aimux; leave jobs/subprocess/webServer in TS. **The dual-pipeline problem (review §3.6) does not exist in v3:** Rust does not call `tools.execute`; TS loop runs the full pipeline itself. If orchestration direction is enabled, first decide pipeline authority (review recommends Rust-authoritative + TS dispatch-only entry point; record as prerequisite in §6).

## 5. Cross-Boundary Waterfall (Protocol v1.1 Specified; Enabled in Rust-Migration Wave)

**Core constraint (wording corrected in v3.1, review §3.3):** it is not “next does not cross the bridge”; it is **“functions do not cross; continuation points do.”**

**Three kinds (`wf/register` declaration; v1.1 frame field reserved now):**

- **decide:** one `wf/enter` → run complete TS chain → `{pass|veto|error}`. For gates such as `tools/pre-execute`.
- **around** (review §3.3a): middleware needs the true downstream result after `await next()` (`tools/execute`, `system-prompt/assemble`, `agent/request-error`, `approval/request` are four core extension points of this shape). **Two-phase continuation:** Rust sends `wf/enter`; when TS chain reaches innermost next, it sends `wf/next` back; Rust runs remaining chain and Terminal and returns true result; TS unwinds and responds to `wf/enter`. One extra round trip per bridged chain, not per middleware. Requires bidirectional concurrent re-entry—M1 already has re-entry tests, near-zero incremental cost. **Without this, the lower half of around middleware runs on a stub value without reporting an error** (the hardest behavioral-fidelity trap).
- **stream:** value is AsyncIterable; `{action, value}` cannot represent it. **Explicitly reject at load time** (declaration itself errors); do not load and silently do nothing.

**Enablement timing:** not enabled in v1 foundation direction (TS middleware runs in the real loop); first gated Rust service (sandbox wave) enables decide; around follows orchestration/approval (v2). JSON discipline (plain data + one shared source generated by aimux serde/ts_rs) remains.

**Correction to evidence for interleaving boundaries (review §3.3c):** the v2 citation “none of 159 repositories need interleaving” is **invalid** (the file counted call sites only, without usage-shape analysis; the top entry included AI templates and packaged official core). The boundary itself still holds—under two-phase continuation, each bridge-side chain is inserted as one block in registration order—but the basis is dsh event signatures and protocol structure, not repository counts. Record this lesson: cited evidence must support the claim.

## 6. Orchestration Direction (Deferred, Conditions, Anchor, Prerequisites)

Defer until dsh loop semantics converge and rutis-agent becomes an engine. When due, implement `sessions` (create/get/list/fork), `subagents` (**`startContinuable` is first-class**, 12,256 mainstream calls), `llm/stream`, and `tools/invoke`, all as svc/evt composition.

**Prerequisites when due (review §3.4/§3.6):** scope filtering (`scopeId` already reserved; implement filtering in bridge, not kernel D29—without it, plugins attached to a subagent receive whole-tree events, a semantic bug rather than a missing filter); decide tool-pipeline authority. **Reevaluation trigger:** first roadmap review after foundation M2 passes (review §6 argues for earlier because uncertainty is bounded, breakage is zero, and prerequisites overlap wave 1).

## 7. Performance Budget (Measured Scope)

| Item | Order of magnitude | Comparison |
|---|---|---|
| One small-JSON round trip | 0.1–1 ms | 20–100 ms between tokens |
| LLM seam chunk stream | 10–100 µs per side | Two orders of magnitude below low token interval |
| Resident Node process | 30–80 MB + 50–150 ms startup | One-time |
| Large payload (1 MB) | 1–10 ms | — |

**Backpressure:** bounded queue per subscriber. **Drop policy by class** (§4.2: discrete drop+count / ordered stream coalesce or disconnect / do not block).

## 8. Implementation Structure and Rough Size

| Component | Location | Dependency | Scale |
|---|---|---|---|
| Protocol layer (bidirectional concurrent RPC + stream + fd3/socket) | `crates/rutis-cordis/src/rpc.rs` | rutis only | Hundreds of lines |
| Foundation bridge vocabulary (loader arbitration/event mode/wf kind/base hello validation/scopeId) | `crates/rutis-cordis` | **rutis only; no dsh knowledge** | ~0.5k lines including tests |
| dsh surface (LLM seam/event mapping/substitute table/dshSemver/engine reservation) | `crates/rutis-dsh` | **rutis-cordis + aimux** | ~1k lines including tests |
| TS host (foundation + full-stack composition + aimux adapter + evt forwarding) | `host/` (npm, in this repo) | min-cordis (preferred) / vendor Cordis (fallback) | ~600–1000 lines |
| ~~Thin `rutis-dsh-agent` adapter~~ | — | — | **Removed in v3** |

Dependency order: **M0 foundation experiment → bridge v1 (two seams) → minimum corpus matrix (§9) → sandbox/fs Rust migration (first svc/wf seam) → … → loop migration (rutis-agent grows) → orchestration surface**.

## 9. Acceptance (Layered and Independently Verifiable)

| Layer | Scope | Acceptance |
|---|---|---|
| **M0 foundation experiment (two questions)** | (1) load tool-bash on min-cordis and print schema; (2) pass loader-composition.spec | Both pass → min-cordis; either fails → vendor Cordis (near-zero cost) |
| M1 protocol | Full kernel primitives over in-memory wire: round trip, concurrency, cancellation (including dropping late res), timeout, handshake mismatch and capability difference, name arbitration, **re-entry**, **kill host process → Rust loses only LLM seam; observer continuity is recorded** | `cargo test -p rutis-dsh`, no Node; prerequisite: recheck §10.2 |
| **M1.5 minimum corpus matrix (review §7.2 moved earlier)** | Minimum corpus of 10 repos (include sample heavy in static value-level imports), run L0/L1 | Expose host npm closure issues as early and cheaply as possible, especially findings that change host shape |
| **M2 full-stack turn (core)** | Full dsh stack + real plugin; LLM seam through aimux; complete turn; event stream back to Rust; **test plugin injects `console.log` without corrupting frames** (fd3 validation) | Integration tests (requires Node); community plugin test suite if available; L3 fidelity assertions (chunk/finish/usage) |
| M3 event path | Arrival order = emission order; **ordered-stream coalescing correctness** (reconstruct text without loss) | Multi-thread regression in order_probe mode |
| M4 waterfall | Defer to sandbox wave: three decide cases + **one around case (TS `await next()` must receive true Rust Terminal result)** | Define when due |
| M5 orchestration surface | Deferred (§6) | Define when due |
| M6 composition | Run corpus sample set through full stack | Pass rate ≥ threshold (set and record before first release) |
| M7 corpus (**ongoing facility, top metric**) | Stratified 20–50 real plugins by service distribution; run full stack with both seams under **fidelity-tier matrix** (count L0–L4 at every level + known breakage inventory), update weekly | The only convergence mechanism for “runs correctly” |

## 10. Limitations and Risks (Honest Inventory)

1. Functions do not cross; continuation points do (§5) → interleaving granularity is whole chain per side; payloads remain plain data and follow substitute-table discipline.
2. Synchronous APIs do not cross the bridge. Before M1, filter `raw/ctx-repo-operations.tsv` for callers of bridged synchronous service members and confirm empty intersection.
3. Version drift: align to measured `dsh-v0.1.0-rc.7`; centralized, discrete adapters.
4. If bridge is unavailable, TS stack falls back to its own adapter without crashing; Rust baseline is unaffected.
5. Trust model: TS plugins are trusted at bash level; tighten after sandbox migration.
6. Re-entry (evt flowing back while LLM seam executes) is handled by bidirectional concurrency + correlation IDs; explicitly test in M1.
7. **LLM-adapter fidelity (L3):** streaming differences between aimux and native dsh adapter; target directly with M2 community suite.
8. **Live-object substitutes (L4):** wrong substitutes for agent/signal/exec/session/actor silently corrupt behavior (actor navigation in fs-observation-policy is a named example); maintain table per event and assert in M7.
9. **min-cordis composition risk (loader is the actual fork):** loader/include/group were removed (~1,400 lines); dsh has package-level loader-composition contract. M0 question 2 directly tests this. Fallback cost is near zero (Cordis vendored and pinned in repo).
10. **rc.7 ↔ 4.0.1 drift:** min-cordis derives from Cordis 4.0.0-rc.7, dsh vendors 4.0.1 (fiber.ts 888 lines vs 754); they are already diverged branches. Record M0 result here.
11. **Host npm closure ≈ full dsh** (review §3.1c): static call set is not the threshold; value-level imports (peerDeps + imported values) are. One tool-bash plugin pulls in 12 `@deepseek-ai/dsh-*` packages. The “thin foundation + a few services” picture is false; host is a full dsh composition. v3 (TS runs full stack) is designed accordingly; M1.5 corpus validates it.
12. **dsh core internal usage has not been scanned:** before migrating a TS service to Rust, the call-site inventory for that service inside dsh core is prerequisite data.
