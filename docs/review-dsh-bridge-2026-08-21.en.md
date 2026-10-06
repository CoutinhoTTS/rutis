# Review: Directional Problems in the dsh Bridge

> 2026-08-21. Reviews [design-dsh-bridge-2026-08-21.md](design-dsh-bridge-2026-08-21.md) (v2) alongside [design-dual-core-2026-08-20.md](design-dual-core-2026-08-20.md) (including the 08-21 direction revision).
> Evidence: all rutis source; measured dsh source (`../deepseek-harness`, tag `dsh-v0.1.0-rc.7`).

The 08-21 direction revision (run complete dsh stack in TS, use Rust only as a base, narrow bridge to llm + events) is right. But most of the v2 bridge design became invalid after that revision, and **the new direction's own assumptions have not yet been tested**. The first three points below say the actual work in the new direction is much smaller than written; the latter three question whether the Rust-porting roadmap itself pays off.

## 1. Under the new direction, Rust is called only once per turn

First write out execution path. With the complete dsh stack in TS, one step runs in this order:

```text
agent/pre-step (in TS) → agent/request (in TS) → llm/stream waterfall (in TS)
   → innermost: call Rust aimux ← the only cross-process call
→ parse chunk and get tool calls (in TS)
→ tools/pre-execute → tool body → tools/post-execute (all in TS)
```

The tool body may make another cross-process call if it later uses Rustified fs/sandbox. That means:

**In the new direction, Rust is a TS RPC server, called once per step for llm, plus zero to several times per tool execution. Rust does nothing proactively beyond that.**

This is not minimizing the work; it makes the workload clear. The v2 protocol (`hello` / `plugin/load` / `svc/define` / `svc/resolve` / `evt/on` / `wf/register` / `wf/invoke` / `cancel` / `host/restart`, 13 methods) was designed for “two plugin frameworks load each other's plugins.” The new direction does not need to load the other side's plugins.

## 2. dsh already defines the llm seam; do not redesign it

dsh `llm` service has `registerAdapter(providers: string[], adapter: LlmAdapter)`. `LlmAdapter` is an abstract class with only one abstract method:

```ts
abstract class LlmAdapter {
  providerInfo(provider: string): LlmProviderInfo
  providerRetryPolicy(provider: string): ResolvedRetryPolicy | undefined
  listModels(provider: string): Promise<readonly LlmModelInfo[]>
  resolveModel(provider: string, model: string, signal?): Promise<LlmResolvedModelInfo>
  abstract stream(options: GenerateOptions): AsyncIterable<StreamChunk>   // only abstract method
}
```

Both types are plain data:

- `GenerateOptions` = `{provider, model, messages, system?, tools?, temperature?, maxTokens?, stop?, signal?, sessionId?, purpose?}`; everything except `signal` is JSON-serializable.
- `StreamChunk` = union of seven variants (`block-start` / `text-delta` / `reasoning-delta` / `tool-call-delta` / `block-end` / `usage` / `finish`), all JSON-serializable.

**Thus the entire llm seam is one request (GenerateOptions without signal), one chunk stream, one cancellation frame, and four metadata methods.** No live objects, scopes, waterfalls, or bidirectional plugin loading. This is the one naturally clean seam in the design, and dsh already defines its interface.

Two choices must be fixed:

1. **Register an adapter; do not replace `ctx.llm`.** dsh `llm/stream` is a waterfall, with retry/cache/metrics/logging around it (official `llm-retry` works this way). Community `registerAdapter` was used 5,421 times. Registering an adapter preserves these layers and gives Rust only the innermost “send request, receive chunks.” Replacing service discards them.
2. **Drop v2 §4's statement that aimux exclusively owns engine-provider API and does not inherit dsh's model-adapter ecosystem.** Under the new direction aimux is one of many adapters, not the exclusive owner.

## 3. The rutis core is not used in this direction

Under the new direction Rust has three jobs: accept llm requests, observe TS events, and eventually host Rustified services. Does each need rutis core (six fiber states / dependency gating / cascading unload / dependency-driven reload / service registry)?

| Rust-side component | Needs core? | Reason |
|---|---|---|
| aimux RPC server | No | tokio + serde are enough |
| sandbox `confine` | No | Pure function |
| fs operations | No | Function call |
| Event observation | Depends on consumers | If Rust has one consumer, `tokio::sync::broadcast` is enough |

The core's five pillars solve “many interdependent, hot-swappable plugins in one process.” The new direction explicitly keeps the plugin ecosystem in TS; Rust has no plugins. **So the `rutis` crate is not used on the production path of this roadmap.**

This does not mean it lacks value: it is a Rust implementation of Cordis semantics with 115 contracts and 4,677 lines of parity tests. It can be open-sourced independently and may be useful if the loop is Rustified later. But it means **the v2 bridge was so complex because it assumed it had to connect two cores; that assumption is false in the new direction, so bridge design should not center on cores.**

If Rust-side assembly is genuinely needed later (e.g. configuring five or six Rustified services), start with `struct App { llm, sandbox, fs }` of a few dozen lines, not a fiber state machine.

## 4. Sandbox is a poor first Rust port; its benefit is near zero

The dual-core document says sandbox is a strong candidate because of the trust boundary and picks sandbox/fs first. Inspect dsh implementation:

`sandbox` service has **one method**:

```ts
abstract confine(argv: readonly string[], policy: SandboxPolicy): ConfinedArgv
```

It rewrites a command line into a restricted command line. The OS does the actual isolation: `sandbox-local` uses Linux `bwrap` and landlock (through native addon `@deepseek-ai/node-addon-landlock-run`) and macOS seatbelt. **The trust-boundary enforcement already happens outside TS.**

Moving `confine` from TS to Rust moves a pure “assemble argv” function. Isolation strength and performance do not change.

The decision-making power belongs to `sandboxPolicy`:

```ts
readonly defaultMode: SandboxMode
resolve(request: SandboxPolicyRequest): SandboxExecutionPolicy
overrideOf(session: Session): SandboxMode | undefined     // requires Session
```

`overrideOf` needs a `Session`. The source of truth for session is TS (§6). **So the useful part of sandbox cannot move, and the movable part has no benefit.**

By the same analysis, fs is even worse as the first port: it is used by 318 repositories, has two waterfalls (`fs/write-intent` / `fs/edit-intent`), and its `actor` parameter is a live object. Official `fs-observation-policy` does `(actor as FsObservationActor)?.agent?.session`, navigating to session object and using it as key in a read-before-write ledger.

**Conclusion: reorder Rustification candidates based on CPU usage and whether they need to run without Node**, not “distance from agent core loop” (which measures coupling, not value). Real candidates are CPU-heavy token counting, diff, and code search, not sandbox/fs.

## 5. Incremental Rustification produces no distribution benefit until the last piece

This is the roadmap's most important issue.

The dual-core document lists five Rustification criteria: semantic convergence, mechanism rather than wording, performance/distribution sensitivity, not tied to npm ecosystem, trust boundary. Evaluate against current candidates:

- **Performance:** agents mostly wait for model (20–100 ms per token). Moving fs or sandbox to Rust saves microseconds. **Not met**, unless moving CPU-heavy work.
- **Trust boundary:** sandbox execution already happens at OS/native layer. **Not met** (§4).
- **Distribution:** main rationale, but **as long as any component remains TS, Node is still required.** Every intermediate step adds a cross-process cost but gives no distribution benefit until the last part moves. Yet the “permanent dual-core” decision explicitly says the last part will never move.
- **Semantic convergence:** decision 4 admits loop has not converged and should not be replicated.

So current roadmap is **paying ongoing intermediate-state costs for an endpoint that, by definition, will never arrive.**

This does not prove roadmap wrong; it means **it needs another rationale**. There are two plausible goals; choose one:

1. **Goal: “better dsh.”** Rust does only what TS clearly does poorly (CPU-heavy or requiring system-level capabilities), does not seek broad coverage; rutis core and rutis-agent are off-roadmap. Main outputs are a TypeScript dsh plugin plus a thin Rust binary.
2. **Goal: “an agent usable without Node.”** Incremental porting is wrong; define and complete minimum viable set at once (loop + several tools + llm, no plugin ecosystem). **This is what `rutis-agent` already does** and should not be downgraded to a “comparison baseline.”

These goals require opposite roadmaps and cannot share one plan. The dual-core document currently keeps both (decisions 1 and 3), so each decision swings between them.

## 6. Decide state ownership before splitting seams

Regardless of goal, decide first: **who owns session?**

dsh session is persistent source of truth, with logs, projections, forks, persistence (`sessionPersistence` used by 240 repos, `sessionProjections` by 196, `sessionQuery` by 190). rutis `Session` is an in-memory `Vec<ModelMessage>` and is discarded on replacement; a test pins this: `crates/rutis-agent/tests/integration.rs:214` `assert_ne!(agent2.id(), session1); // reload means a new session`.

Under the new direction, session clearly belongs to TS. This has hard implications:

- **No Rust service may hold state across turns** unless indexed by session identity.
- To index by session, Rust must know session creation and destruction—**that is the actual purpose of the “event seam.”**
- But nothing on Rust side currently needs session indexing. So **the event seam has no consumer today.** Building infrastructure for a nonexistent consumer is wrong for v1.

**Recommendation: v1 implements only llm seam. Add event seam when first Rust service needs session identity; then required events, ordering, and loss semantics will be clear.**

## 7. Remove directly

- **M0 (install tool-bash into min-cordis).** New direction runs complete dsh stack in TS, so use dsh-vendored `@deepseek-ai/cordis` 4.0.1. There is no base-framework choice, and M0 experiment results do not affect decisions. (As a result, v2 §10.10's “largest risk” also disappears.)
- **v2 coverage promise (94% loadable).** It measures “can TS plugin load into rutis”; under new direction plugins are never loaded into rutis. See appendix A.1 for metric problems; relevant only if direction reverses.
- **v2 protocol method table.** Of 13 methods, new direction needs only streaming call and cancellation.

## 8. Recommended next steps

1. In dual-core document, **choose one of its two goals in §5** and clearly mark other “not pursued for now.” Without this, later decisions will keep swinging.
2. Rewrite bridge v3 as design for **`aimux-llm-adapter`**: one dsh TS plugin registering `LlmAdapter` and one Rust binary. Protocol is the three items in §2. It should be an order of magnitude shorter than v2.
3. Reorder Rustification list by CPU usage and “need to run without Node,” not distance from core loop. Profile real workloads before selecting first component.
4. Do not use stdout for transport. Any TS `console.log` corrupts frames. Use fd 3 or Unix socket; reserve stdout/stderr for logs. This applies regardless of direction and should be fixed now.

## Appendix: detailed issues in v2 (plugin direction)

Everything below applies only to “TS plugins loaded into rutis host.” The new direction does not need it. Retained so it need not be re-researched if direction changes.

### A. Eight structural problems (v2 plugin direction)

#### A.1 Coverage metric: definition mismatches actual gate and numbers are unreproducible

Document §1 defines “**loadable**: static call set ⊆ **bridged service set** — currently measured at 94%.”

Problems:

**(a) Wrong gate.** What determines whether a TS plugin can load is the **service set assembled by host** (TS side), not the bridged service set. A plugin using `ctx.slots.register` needs `slots` in host, not a bridge for `slots`. Thus 94% either measures “host installed every dsh service package” (always 100%, uninformative) or measures something else. Neither reading supports a “v1 delivery promise.”

**(b) Number cannot be reproduced.** Recompute from the same raw data cited by document, `raw/ctx-repo-operations.tsv` (50,304 rows):

| Metric | Recomputed value |
|---|---:|
| Repositories with ctx operations | 6,673 |
| Member set ⊆ {`tools.register`, `systemPrompt.section`} | 872 |
| Service set ⊆ {tools, systemPrompt} | 1,016 |
| Service set ⊆ {tools, systemPrompt, events, sessions, subagents, llm} | 1,242 (**18.6%**) |

Document's 1,374 / 2,707 / 29% / 94% do not match these values. They may come from another scan, but **current repository has no data that produces 94%.**

**(c) More importantly, static call set is not the whole gate.** M0's sample `tool-bash` has 12 `@deepseek-ai/dsh-*` packages in `peerDependencies`; source statically imports values: `defineTool`, `TOOL_ABORTED` from `@deepseek-ai/dsh-tools`; `HarnessError` from dsh-llm; `approveEscalation` / `canonicalPath` / `validateEscalationArgs` from dsh-sandbox; `DSH_ENV_PREFIX` from dsh-shell. A ctx-call scan misses this layer. The actual premise of “load unchanged into host” is **host's npm closure ≈ a complete dsh**. This does not undermine architectural feasibility, but it overturns “thin host plus ~10 services” picture; coverage metric should reflect it.

**Prescription:** abandon binary “loadable” metric; use fidelity tiers in §6.2, each with reproducible corpus count and known gaps.

#### A.2 Event surface: only two of five dispatch modes bridged; two high-frequency events silently lose semantics

dsh has 56 events (generated `api-catalog.ts`): **41 emit / 13 waterfall / 1 serial / 1 parallel**.

| Event | Mode | Corpus references | Result under current protocol |
|---|---|---:|---|
| `session/flush` | **parallel** (wait for all listeners) | 1,190 | `evt/dispatch` is `ntf`, does not wait → **flush returns before complete; data can be lost** |
| `agent/turn-stopping` | **serial** (wait in order, short-circuit) | 273 | Same → shutdown hook is not awaited |

Document §3.5 #1 (“if semantics require confirmation, use request rather than event”) pushes responsibility to plugin author, but author writes `ctx.on('session/flush', ...)`: **host chose wrong transport shape; plugin did not misuse API.**

**Prescription:** include `mode` in `evt/on` declaration. `emit` uses `ntf`; parallel/serial use `req` (`evt/invoke {dispatchId, event, mode, params}` → `res {bail?}`), with timeout on Rust side. On timeout, use existing §3.5 #3 discipline (failure count, do not wait). Protocol delta: one field and one method.

#### A.3 Waterfall surface: supports only decision middleware, not around or stream

Document §5 model: `wf/invoke` sends value once; TS runs whole chain and returns `pass/veto/error`; Rust then decides whether to call its own next.

This works for **decision middleware** (`tools/pre-execute`, `tools/post-execute`, `agent/pre-step`, `fs/write-intent`, `fs/edit-intent`). It fails for two other kinds:

**(a) Around middleware**—processes downstream result **after** `await next()`:

```text
'tools/execute'(exec, next: () => Promise<ToolExecutionResult>): Promise<ToolExecutionResult>
'system-prompt/assemble'(assembly, context, next: () => Promise<PromptAssembly>): Promise<PromptAssembly>
'agent/request-error'(payload, next: () => Promise<RequestErrorAction>): Promise<RequestErrorAction>
'approval/request'(req, next: () => Promise<ApprovalOutcome>): Promise<ApprovalOutcome>
```

Under current protocol, TS `next()` can return only a **stub**, because real downstream (Rust chain tail + Terminal) runs only after `wf/invoke` returns. Thus code after `await next()` runs on fake data **without an error**. This is the clearest instance of §10.7 (“protocol compatibility is not behavior compatibility”), and these are four core extension points, not long tail.

**(b) Stream middleware**—`llm/stream(options, next: () => AsyncIterable<StreamChunk>): AsyncIterable<StreamChunk>`. Waterfall value is async iterator; JSON `{action, value}` cannot express it. Official `llm-retry` has this form; corpus references `llm/stream` 462 times.

**(c) The document's only evidence for this simplification is invalid.** It says “none of 159 waterfall repositories need interleaving. Source: `community-code-analysis.md` §2 (ctx coupling scan).” That section only says “159 repositories use `ctx.waterfall`” and has a call-count table; **it contains no usage-shape analysis**. It also says its leaders `mstar-*` (263–327 calls) are “likely AI-generated boilerplate” and `deepseek-harness-desktop/cli/gui` are “distributions embedding official source.” The citation cannot support the conclusion it was used to justify.

**Prescription:** `wf/register` declares `kind ∈ decide | around | stream`.

- `decide`: preserve current protocol.
- `around`: **two-phase continuation**. Rust sends `wf/enter {invocationId, event, value}`. When TS chain reaches innermost `next()`, send reverse `wf/next {invocationId, value}` to Rust; Rust runs remaining chain and Terminal, returns real result; TS unwinds with real result and finally replies to `wf/enter`. Cost is one extra round trip per side chain, not per middleware, and requires bidirectional concurrent reentrancy. **M1 already tests reentrancy** (§10.6), so incremental cost is near zero. Reword root constraint from “next cannot cross boundary” to “**function cannot cross boundary; continuation point can**.”
- `stream`: explicitly reject in v1—when `wf/register` sees `llm/stream`, **fail at load time**, not silently break after plugin loads.

#### A.4 No isolate/scope, though it is load-bearing for subagents

Document §3 rule 4: “All `evt/emit` and requests carry optional `sessionId`/`turnId`; **v1 means global broadcast, no filtering**.”

Measurement: dsh has dedicated `@deepseek-ai/dsh-scope` for scope routing, not optional decoration:

- 14 events route by agent, some by session (`scoped-events.generated.ts`).
- Scope **nests**: `scopeParents` maintains parent chain; listeners on ancestor receive events dispatched in descendant scope, and registration view inherits down the chain.
- Many event signatures use `this: Scoped<Agent>`; `this` binding itself carries scope.

The top orchestration scenario is subagents: `subagents.startContinuable` used 12,256 times; `subagent/end` has 1,676 references. **Without scope filtering, plugin attached to one subagent receives events for all agents in entire tree.** This is incorrect behavior, not just a missing filter.

Also note §1.3: rutis core already decided events are not filtered by isolate (D29). Filtering must therefore live in **adapter layer**.

**Prescription:**

- Make `scopeId` first-class on `plugin/load`, `evt/on`, `evt/dispatch`, `wf/invoke`, and `svc/call`.
- Add `scope/create {scopeId, parentScopeId}` / `scope/dispose {scopeId}` so Rust agent/session lifecycle drives TS scope tree.
- Implement filtering in `rutis-dsh-agent`, **without changing core D29**. State explicitly that scope is dsh semantics owned by adapter.

#### A.5 Name each live-object substitute; “plain-data payload” is not enough

Document §5 says four existing cross-boundary event payloads already meet “plain data.” Inspection of those and related payloads:

| Payload member | Actual content |
|---|---|
| `agent: Agent` | Live object: `session` / `inbox` / `ctx: Context` / `cancel()` / `whenIdle()` / `runMaintenance()` / `send()` / `followup()` / `steer()` / `inject()` |
| `signal: AbortSignal` | Live object |
| `exec: ToolExecution` | Contains `signal` + `agent?` + `token: ToolExecutionToken` |
| `session: Session` | Live object and persistent source of truth for logs |
| `actor: object \| undefined` | Opaque execution context for fs intent; consumers navigate it to live objects |

The last is especially critical. Official `fs-observation-policy` does:

```ts
private owner(actor: object | undefined): object | undefined {
  return (actor as FsObservationActor | undefined)?.agent?.session   // src/index.ts:36-40
}
```

It casts the “opaque actor,” navigates to `.agent.session`, and **uses the session object itself as a key in its read-before-write ledger**. Also `fs/write-intent` means “first listener that returns intent decides exclusively,” not composition with peers. Across bridge, `actor` can only be an ID: navigation chain breaks, exclusive decision ownership depends on whether ID rules match across sides, and **no error is raised**. “Plain data discipline” does not cover this; the substitute synthesis is wrong.

**Prescription:** do not write “already satisfied.” Add a **substitute table**: each live object → wire representation → side that synthesizes substitute → unavailable members. This is executable version of §10.7's “behavior fidelity long tail” and source of M7 assertions.

#### A.6 Dual tool pipeline: no decision on which side is authoritative

Document §4.1: adapter invokes `svc/call tools.execute` at runtime; §6 says `tools/invoke` runs full Rust three-stage pipeline (gating is not bypassed for external calls).

But TS `tools.execute(exec)` is **not a raw execution entry**; it is dsh's full pipeline: `tools/pre-execute` → guard/restrict → around `tools/execute` → `tools/post-execute` → invariants. A model tool call then runs:

```text
rutis: tools/pre-execute (Rust chain) → svc/call tools.execute
                                       → dsh: tools/pre-execute (TS chain) → real execution → tools/post-execute (TS chain)
       ← result ← tools/post-execute (Rust chain)
```

Pre/post each run twice. Consequences are not just “a few extra ms”: `approval/request` prompts twice for approval, and `ctx.invariants` fails due to stage order (dsh itself asserts `tools/execute must follow tools/pre-execute` in `packages/core/tools/src/invariant.ts`).

**Prescription:** explicitly choose ownership. Recommend **Rust owns pipeline** (it owns loop and gating); TS exposes a **dispatch-only** entry point. dsh already has `ToolRuntimeScheduler.prepare/dispatch/finalize` (`@internal`). Bridge calls `dispatch`, not `execute`. If impossible, fallback makes TS authoritative and Rust does not run three stages—but **choose one and record it in §4**.

#### A.7 Lifecycle red line: bridge must not be in driver `injects`

§1.4 already has evidence; make it a rule:

- If `rutis-dsh` bridge provides any service key injected by driver, host crash → bridge fiber unload → service removed → driver evicted → reload → `AgentDriver::new` → `Session::new()` → **history erased**.
- Correct shape: bridge service is an **optional dependency** for driver (soft query with `ctx.get`, or `provide_as_with_check` + internal mutable registry in adapter); never put it in `injects`.
- Pairing “one TS plugin = one TS fiber = one rutis fiber” (§2) is good, but add: **what is paired is plugin, not capability**; host liveness affects only “capability contributed by TS,” not the loop itself.

**Prescription:** add hard rule after §2 lifecycle pairing and M1 assertion: kill host → `agent.id()` unchanged.

#### A.8 Adapter is not thin: rutis-agent lacks two seams

Document estimates `rutis-dsh-agent` at “a few hundred lines.” Based on §1.5 implementation facts, it either writes hacks or first changes `rutis-agent`:

| Missing seam | Current state | Needed |
|---|---|---|
| Dynamic tool registration | `ToolRegistry` immutable; `ToolsPlugin::apply` builds once | Internal mutable registration surface or aggregated `tools/resolve` point |
| Execute-around point | Only `tools/pre-execute` (veto only, `Option<String>`) and `tools/post-execute` (modify result) | Around waterfall `tools/execute` (value `ToolOutput`, terminal is current `registry.execute`) |

Also, adding `tools/execute` gives Rust side a symmetric `around` seam from §3.3, and makes **rutis eat its own dog food**, matching existing “framework uses itself” statement in `rutis-agent` lib.rs.

**Prescription:** move both from “adapter” to “prerequisites,” alongside prompt assembly service and session logging in §8 dependency order.

### B. Reordered M0 risk list: min-cordis vs Cordis (still relevant after direction change)

| Risk | Document claim | Measured result |
|---|---|---|
| Standard Schema / config validation | Not mentioned | ✅ **Closed:** both use `Config['~standard'].validate`; schemastery implements `~standard` |
| `Context` Proxy / `Service` base class | “Unknown dependency depth” | Moderate: dsh packages reference `Context` 1,359 times, `Service` 134, `Fiber` 34; min-cordis has all of them (`reflect.ts` 419 lines vs Cordis 418) |
| **loader / include / group** | **Not mentioned** | ⚠️ **New risk:** min-cordis explicitly removes these (~1,400 lines); official dsh has 16 `loader-composition.spec.ts` tests (composition through loader is package contract), and 423 community repos use `ctx.loader` (`entries` ×3,612 / `create` ×2,591) |
| **rc.7 ↔ 4.0.1 drift** | Not mentioned | ⚠️ **New risk:** min-cordis forked from Cordis `4.0.0-rc.7`, while dsh vendors `4.0.1`; `fiber.ts` has 888 lines vs Cordis 754, already divergent branches |
| Cost of fallback | “Return to v1 situation: keep tracking @deepseek-ai/cordis versions” | ✅ **Actually lower:** Cordis is an in-repository dsh vendor package (`vendor/cordis`, workspace dependency); version is naturally pinned when composing dsh services, so no “version tracking” problem |

**Revised M0:** min-cordis's real divergence point is **loader**, not Proxy. Change experiment to two questions: (1) Can `tool-bash` load under min-cordis and print schema? (2) Does its `loader-composition.spec.ts` pass? If second fails, use real vendored Cordis—which has **almost no fallback cost** because it is in dsh repo.

### C. Concrete protocol defects (left from v2; items 1 and 2 also apply to new direction)

**1. Do not frame over stdout.** Newline-delimited JSON on stdout means any TS plugin `console.log` **corrupts a frame** and triggers `host/restart` under §3 rule 2; one log line becomes process restart and may loop. Official package is disciplined (`console.log` appears only five times), but ecosystem has 8,889 candidate repositories, many batch-generated.

→ Use fd 3 or Unix socket for frames; keep stdout/stderr for plugin logs. This removes an entire failure class, not just a bug.

**2. Classify `evt/dispatch` drop policy.** “Drop + count” is right for discrete events such as `agent/tool-call`; wrong for **ordered delta streams** such as `AgentTextDelta`: TS plugins rebuilding text from deltas get silently corrupted text.

→ Set policy by event class: discrete events drop + count; ordered streams coalesce adjacent deltas or disconnect; never drop ordered stream data.

**3. State event mapping choice.** (See §1.1.) “One Rust type per event” provides type safety and granular order, at cost that adding events requires Rust release. “Single erased envelope” allows runtime extensibility, at cost that all bridged events become one tail chain and native rutis listeners lose types. **Recommend former** (matches “versioned declarations, centralized adaptation”), but name it explicitly instead of leaving to implementation.

**4. `hello` must negotiate capability set, not only versions.** Today `{protocol, base, baseSemver, dshSemver}` → `{accepted}`. Version cannot express “this host lacks approval.”

→ Exchange `{modes, wfKinds, scopes, services}` both ways in hello. Compare `plugin/load` returned `injects` with host capability set; nonempty difference gets explicit degradation warning at **load time** (the mechanism wanted in §10.8 but lacking a carrier).

**5. Specify full `cancel` semantics.** `cancel {target}` is currently a notification; undefined: target namespace (can callId collide with invocationId?), whether that ID may still reply `res` after cancel, and who sets timeout. §3.5 #3 sets policy (“timeout counts as failure; do not wait”), but timeout duration is absent from protocol.

**6. Version target disagrees with measurement.** Document says “align with dsh 0.3.x” in three places (§1 non-goals, §3 hello, §6); measured repository tag is `dsh-v0.1.0-rc.7`, all 219 package versions `0.1.0-rc.7`. Either 0.3.x is future target (say it does not exist yet) or a typo. First step in a version-declaration policy is to state correct version.

---

*Review numbers are reproducible: ctx operation stats from `../deepseek-harness/plugin-reference/raw/ctx-repo-operations.tsv`; event/service signatures from `../deepseek-harness/packages/extensions/tool-cordis/src/api-catalog.ts` (generated); dsh version from tag `dsh-v0.1.0-rc.7`. Ecosystem data is snapshot from 2026-08-20 and may drift.*
