# Decision: Standalone aimux-llm Plugin + Business-Neutral Bridge + dsh Default LLM (2026-08-23, v2)

> Decision owner: user. This document is the alignment record and the sole authority for subsequent implementation. It takes precedence over conflicting older assumptions, including the v3.2 design's tendency to keep the bridge in `llm-pi-ai` form and the former layering that treated `rutis-dsh` as the only home for dsh vocabulary.
>
> v2 change: the user decided that “the bridge must be business-neutral and connect automatically.” Mapping (`GenerateOptions` → `CallOptions`) moves from Rust to the TS face, leaving Rust with zero dsh knowledge. `rutis-dsh` is no longer a “dsh vocabulary layer”; it remains only as a launcher (see §7.1).
>
> **Implementation status (2026-08-23, `7399433`): objects A–E are implemented and accepted.** All three §6 acceptance checks passed (two-turn `llm_e2e` streaming/history/usage/noise injection through the new composition; 171 workspace tests; real-machine one-shot `finish=stop`, `key=request`, registry-derived capabilities, and clean shutdown). §7.1 chose A (retain the entry point). Implementation corrected one contract defect: DTO credentials use camelCase (`apiKey`); the first test exposed that snake_case was leaking through.

## 1. Background and motivation

M0–M2 acceptance took a shortcut: llm aggregation/business logic was welded into both sides of the bridge (TS plugin + Rust runner). This served as acceptance scaffolding, but it is the wrong architecture to preserve—the pipeline contains business logic. The experiment phase is over (M0/M1/M1.5/M2 and the complete npm official-dsh real-host flow all work); no new experiment is needed. What remains is design and implementation.

## 2. Decisions (user-approved)

1. **A standalone Rust `aimux-llm` plugin:** llm capability is an independent rutis plugin depending on rutis + aimux; **no bridge dependency and no knowledge of any dsh shapes.**
2. **The bridge is business-neutral and connects automatically:** `rutis-cordis` is the only compatibility layer—protocol + Cordis vocabulary + registry-driven service dispatch (declare/forward whatever is registered; derive capability sets from the registry, never hard-code them). **No Rust code knows about dsh.** The only dsh-aware code is the TS-side rutis-bridge plugin (the llm face): dsh consumes llm through its TS interface (`registerAdapter`), so type translation belongs in TypeScript. This is plug-shape conversion, not business logic. It sends **aimux-neutral shapes** (`CallOptions` / `StreamPart` JSON) and does not inspect dsh shapes.
3. **Configure dsh's default llm as aimux-llm:** settings route name `aimux-llm` (configured provider: deepseek, which calls the DeepSeek API; it can be changed), and `agent-default-model` points to this route. No new TS-side code is required.

## 3. Explicit non-goals

- Do not restructure the TS plugin internally; only change input construction from “pass through dsh shape” to “construct aimux shape.”
- Do not add another validation round, demo plugin, or experiment. The only acceptance gates are a green `cargo test --workspace` and no regression in `rutis-dsh up` behavior.
- Event seams, fd3, and npm publishing are outside this decision; keep current behavior.

## 4. Implementation objects and requirements

### Object A: `crates/aimux-llm` (new Rust plugin crate)

- Dependencies: `rutis` + `aimux-core` / `aimux-providers`; zero bridge knowledge and zero dsh knowledge.
- Implement as a rutis plugin: `apply` registers the **`llm` service**, whose surface uses native aimux shapes (the neutral protocol schema):
  - `stream(CallOptions JSON) → StreamPart JSON stream`;
  - `listModels(provider, key) → model list` (cached);
  - provider factory and cache per `(provider, key, model)`; if no key is configured, fall back with `UnconfiguredModel` semantics (construction failure does not block startup; invocation reports the error).
- Move from rutis-dsh: the LlmSeam factory/cache/stream-loop implementation (including parsing `CallOptions`). Do not move parsing of dsh `GenerateOptions`; rewrite that on the TS face.
- Move its behavior tests too (keyed routing, caching, fallback, error paths).

### Object B: Update TS-face inputs (rutis-bridge plugin; the only dsh-aware layer)

- Current behavior: pass dsh `GenerateOptions` through unchanged and translate in Rust.
- Required: construct aimux shapes in TypeScript (`system/messages/tools → prompt`; pass provider/model/credentials through). Response mapping stays unchanged because it already uses neutral parts.
- The term `GenerateOptions` must disappear from Rust.

### Object C: Complete generic dispatch in `rutis-cordis` (compatibility layer; no business logic)

- Service dispatch is entirely business-neutral: look up by name in the registry; forward `svc/call` JSON; correlate streaming parts to the `(method=stream call, dispatchId)` and return them through part notifications. The hello capability list (`services`) is derived from the registry.
- Move from rutis-dsh: the `svc/call` dispatch skeleton (generalize the dispatch portion of `LlmSeam.hooks`; methods come from each service's declaration).

### Object D: Launcher (former rutis-dsh runner; no dsh vocabulary)

- Responsibilities only: bind the bridge port; spawn `dsh` (`PATH` / `RUTIS_DSH_BIN`); compose the rutis runtime + aimux-llm; derive hello capabilities from the registry; wait for both sides to converge.
- Migrate/delete: remove `dshSemver` and hello dsh-section checks (M1 artifacts with no consumers); move llm business logic to A; keep event-observation logs as generic notification logs in the launcher.

### Object E: dsh configuration

```yaml
llm-aimux:
  providers:
    aimux-llm:
      provider: deepseek
      apiKeyEnv: DEEPSEEK_API_KEY
agent-default-model:
  provider: aimux-llm
  model: deepseek-chat
```

Configuration only; no code.

## 5. Layering (v2)

```text
rutis-bridge (TS face)  only dsh-aware layer; dsh types ↔ aimux-neutral shapes
rutis-cordis            business-neutral compatibility: registry dispatch + Cordis vocabulary + part streaming
aimux-llm ──→ rutis     standalone llm service plugin (aimux-native shape = protocol schema)
launcher                process composition; no domain vocabulary
```

llm moves from “the core of the bridge” to “the first rutis plugin supplied to dsh through a business-neutral bridge”—the first instance of “dsh can use Rust plugins provided by rutis.”

## 6. Acceptance criteria (revised late on 2026-08-23: owner specified **web profile** as the acceptance form)

1. `cargo test --workspace --no-fail-fast` is green. The sole designed exception is `host_cordis` e2e failing loudly when `DSH_ROOT` / `MIN_CORDIS_ROOT` are absent.
2. **Complete web turn (primary acceptance form):** run `rutis-dsh up --profile web` with no API keys set in the runner; in a fresh browser session (default model routes to aimux-llm), send a message and render a real answer. Runner evidence: `[aimux-llm] stream ... key=request ... finish=stop` (credentials pass per request through dsh credential storage); dsh session stream records `provider: aimux-llm`; events flow back; the model picker successfully calls `listModels`.

   Keep the headless one-shot as a scripted regression path, but **not as the primary acceptance form**.
3. No new experiments or validation rounds.

### Independent audit (2026-08-23, requested by owner against original requirements)

Conclusion: items 1/2/4/5/6/7 are satisfied; item 3 is substantially satisfied (two remnants were removed: dshSemver echo in `rpc.rs` and hard-coded hello capabilities in TS); item 8's acceptance gap was filled in the section above. The audit also found that three aimux-llm tests were not rerun after renaming serde field to `apiKey`; this was fixed. New rule: **after a rename or contract change, rerun the full test suite before committing; any “green” claim in the commit message must come from a just-completed run.**

## 7. One remaining decision

1. Launcher location: **A** keep `crates/rutis-dsh` only as the launcher (smallest change; “dsh” in the name means “launches the dsh process,” not “contains dsh knowledge”); **B** move it into `rutis-cordis` as a generic host binary and delete the `rutis-dsh` crate. Default: A.
