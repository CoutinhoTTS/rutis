# Host Development Mode: Developing Plugins Against a Real Host (2026-09-25)

> Developer-experience surface for plugin layering in #52; depends on dylib SDK hot replacement in #45/#56.
> Status: design document. The channel skeleton is implemented (`crates/rutis-dev`, built on rutis-loader): `hello` / `describe` / `status` / `watch` / `load` / `swap` / `unload-dev`; loaded items enter the loader overlay layer, and swap is all-or-nothing. Bus probe, recording, synthesized dev manifests, configurable retention limit, and `cargo xtask dev` are not implemented.
> Core judgment from the 2026-09-25 discussion about plugin development: **plugin development must happen against a real host; the host itself is the development platform, not merely a runtime container.**

## 1. Motivation and positioning

In a complete plugin system, the real basis for plugin development is **what actually exists in the running host**: which services are registered at this moment, how events flow, who calls your service and how, and what the timing and data look like. These facts:

- are not in the SDK—other plugins can dynamically introduce objects and event types that the host publisher could not have defined in advance;
- are not in documentation—the environment is live and differs with every deployment;
- cannot be reproduced with mocks—mocks cover interface shape, not behavior, state, or data flow.

Away from a real host (for example, in a standalone development host or mock stack), you can only make a demo play. The real development loop is:

```
edit code → compile .so → hot-swap into the running real host → observe behavior → edit again
```

The environment never restarts and other plugins remain untouched. The host must support every step of this loop. This document designs four capabilities:

| Capability | What it solves |
|---|---|
| dev channel (§3) | How hot replacement reaches the real host |
| Environment description (§4) | Where development facts come from (the sole authority for dynamic interfaces and events) |
| Observability (§5) | What to inspect after loading the new version |
| Build alignment (§6) | Why a locally built artifact can be loaded into the real host |

**Division of responsibility with #45/#56:** dylib design answers “how to replace code safely” (mechanism); this document answers “which host developers use, what they use to write code, and what they inspect afterward” (workflow). Protocol plugins (#46–#48) also need real-host debugging; the dev channel applies to both (§8).

**What “real host” means for dev:** a complete business deployment—the same host plus all business plugins—running on a developer machine or shared dev environment with development data. **It is not a production instance** (§7 security boundary). Production hosts do not enable the dev channel.

## 2. Design principles

1. **Atomic loop:** from saving source to the new version taking effect in the host is one command and one round trip, without restarting anything in between.
2. **The environment is the sole authority:** interfaces/events used to write a plugin come from runtime queries (§4), not offline docs or a predefined SDK subset.
3. **Layer validation without weakening security:** identity verification (`SDK_ID`, §5.4 of the dylib SDK design) runs **unchanged** in dev—dev is not a compatibility backdoor. Only packaging steps (manifest, content-addressed cache, release hash) are relaxed; dev loads directly from `target/debug` artifacts.
4. **Observability before features:** without event-flow and registry visibility, hot swapping is blind debugging. Observability is a first-class part of dev mode, not an optional debugger.

## 3. Dev channel

### 3.1 Form

The host enables it explicitly at startup (configuration or CLI option) and listens on a **local** Unix domain socket / Windows named pipe. The protocol is JSON Lines (zero-dependency encoding/decoding, aligned with the schema direction in #47; the channel message schema belongs in #47's interface definition and is versioned). Production builds do not listen by default (§7).

### 3.2 Initial command set

| Command | Semantics | Failure behavior |
|---|---|---|
| `hello` | Return host identity: SDK_ID, SDK artifact SHA-256, toolchain, target, host version | — |
| `describe` | Snapshot of environment description (§4) | — |
| `watch` | Subscribe to change stream: events (filter by name/fiber/direction), registry changes, fiber state | — |
| `status` | Current fiber/module registry state (including retained dylib versions, §9) | — |
| `load` | Load a plugin (local `.so` path; no release manifest required in dev) | Return `LoadError` unchanged |
| `swap` | Hot-replace a specified fiber (reuse all `FiberView::update` semantics) | **Old version keeps running**; return the full error |
| `unload-dev` | Remove a dev-loaded module from the registry | Only dev-loaded entries can be removed |

Validation for `load`/`swap` is identical to production (the six steps in dylib SDK design §7.1) with two differences only: step 1's manifest is **synthesized live** by the dev channel from the `.so` bootstrap blob (no release artifact required); step 3's content-addressed cache copy is skipped (load from the original path).

Keeping the current version after a failed `swap` is mandatory: if `update` dry-run rejects, `DylibFactory::check_module` rejects, or any `LoadError` occurs, the fiber must not leave its current version. The developer's next save-build-swap attempt remains available.

### 3.3 Relationship to `Loader`

The dev channel is a wrapper layer above `rutis-dylib` (new `rutis-dev` crate); it does not change the production path in `Loader::load`. Code for synthesizing dev manifests is shared with the packager so that there is only one manifest implementation and they cannot drift.

## 4. Environment description

### 4.1 Contents

`describe` returns a JSON snapshot containing:

- **Fiber list:** id, plugin id/name/version, state (Active/Pending/…), injects, and provided service keys;
- **Service registry:** service key (TypeKey + qualifier) → provider fiber and version generation (`provider_gen`);
- **Event surface:** known event keys (including D33 keyed qualifiers), subscriber count for each, and subscriber fibers;
- **Dependency graph:** who injects from whom and current gate state (who is waiting on whom).

All data comes from existing structures: registry, bus registration surfaces, `RuntimeDiagnostics`, and `DependencyDiagnostics`. The addition is an aggregated output, not new bookkeeping.

### 4.2 Dynamic interfaces and events (core use case)

In a full plugin system, services and event types introduced by other plugins are **not in the SDK interface crate** (see #52: that crate contains only the officially controlled subset). `describe` is the developer's only way to obtain the interfaces that actually exist in the environment:

- **Protocol surface** (plugins connected through #46–#48): services/events have schemas from #47 interface definitions; `describe` returns schema references and versions. This fully describes dynamic interfaces.
- **Kernel-level dylib surface:** Rust types cannot be exported online. `describe` provides the **TypeKey qualifier + provider plugin + version generation**. Developers can reference the same interface-crate type in their own crate (if the provider put the type there), or interact through the protocol surface. If a type is absent from every shared crate, the service is marked `opaque` from the dev perspective; only its name and generation are visible.

State this boundary plainly: **describe always exposes names and topology; schemas are complete for the protocol surface; Rust types are not promised to be exportable online.** Mark unavailable information explicitly; do not silently omit it.

### 4.3 `watch`: a live reference

A static snapshot is not enough; developers also need to see “how events actually flow.” `watch` pushes:

- metadata as events pass through the bus (§5.1);
- registry additions/removals (service availability and subscription changes);
- fiber state transitions (`FiberStatusChanged` already exists).

Keeping `watch` running while writing a plugin is like coding against a live environment. Companion tools: `cargo xtask describe` (one-time export) and `cargo xtask watch` (continuously print the stream).

## 5. Observability

### 5.1 Event-flow trace

Add a pluggable bus probe (must not change the dispatch hot path; attach the probe at emit/on registration surfaces):

| Field | Meaning |
|---|---|
| key | Event key, including keyed qualifier |
| origin | Publisher fiber / plugin id |
| listeners | Subscribers actually reached (fiber / plugin id) |
| ts / seq | Timestamp and monotonic sequence number |
| payload | Size and summary only by default; full payload can be explicitly enabled (§5.3) |

### 5.2 Hot replacement and dependency observation

- Every `load` / `swap`: version, result, failed step (which of the six steps in dylib SDK design §7.1), and duration.
- Dependency gates: waiting, admission, eviction, and consumer reload records (diagnostics already exist; expose them through the dev channel).
- Module registry: retained versions per plugin id, `Arc::strong_count`, and memory use (dylib SDK design §9).

### 5.3 Record and replay

- **Recording (initial release):** persist the `watch` event stream in order (including full payload when explicitly enabled). Developers reproduce an issue once in the real environment and get a file for offline analysis.
- **Replay (later; not in initial release):** resend recorded events in order to a new plugin version. Replay requires reconstructible events and complete payloads; estimate the cost separately and do not promise it yet.

## 6. Build alignment

For a locally built plugin to load in the real host, its SDK_ID must match the value embedded in the host (dylib SDK design §5.4).

1. **Normal path:** `hello` returns the host's SDK_ID, artifact hash, and toolchain. Before compiling, `cargo xtask dev` handshakes; if the toolchain differs, it immediately reports an error and the host's actual value. This prevents “compiled successfully, then found it cannot load.”
2. **Reproducible builds work (V1):** local builds are aligned, giving the shortest loop.
3. **Fallback if V1 does not work:** the host publisher builds artifacts from the same batch (submit source → CI produces `.so` using the host's batch). The loop takes longer, but `xtask dev` stays the same; only the build step becomes fetching a remote artifact.

One `cargo xtask dev` command runs the whole loop:

```
handshake/alignment → watch source → cargo build → swap → stream host events/logs/errors
```

## 7. Security boundary

- The dev channel listens only on local IPC; **it is not exposed over the network**.
- Production builds do not compile in the dev channel by default (both crate feature and runtime configuration must enable it).
- A dev channel that can trigger hot replacement is a local code-execution entry point. Its trust model matches first-party dylib plugins (trusted local machine); log every dev operation for audit.
- Mark dev-loaded entries explicitly as `dev: true` in the registry to distinguish them from release artifacts; `unload-dev` acts only on dev entries.

## 8. Relationship to existing work

- **#45 / PR #56 (dylib SDK):** provides `Loader`, six-step loading, `swap`/`update`, and retention limits. This design builds on it and relaxes none of its identity checks.
- **#46–#48 (protocol plugins):** the dev channel also supports protocol plugins (`load` of an out-of-process plugin instructs the host to start a proxy fiber). #47 supplies protocol schema descriptions; §4.2 consumes them directly. **Protocol plugins also must be debugged against a real host; this design applies equally to both plugin types.**
- **D32 (config hot update) / D33 (dynamic event keys):** `swap` reuses update semantics; `describe` event surfaces rely on D33 qualifiers.
- **#41 (late registration):** prerequisite for safe hot replacement, especially important in dev where generations change frequently.
- **#12 / diagnostics:** extend `RuntimeDiagnostics` with probe and observability outputs; do not start a separate system.

## 9. Retention limit in dev

The per-plugin retained-version limit in dylib SDK design §9 (N versions, default 4) is too restrictive for development. Continuous debugging reaches the limit quickly; `RetentionExceeded` then forces a process restart, violating the core premise that the environment does not restart.

- Make the limit configurable in dev, defaulting to 32 (only counts dev-loaded entries; the registry still records them normally).
- Keep the production deployment limit unchanged.
- Show current retained count and remaining capacity in `status`; warn before reaching the limit instead of waiting to reject a swap.

## 10. Explicitly out of scope

- Remote/cloud dev channel (cross-machine debugging uses a shared dev environment with local tools connected to its socket; no extra design in the initial version).
- Event replay (§5.3; listed as later work).
- IDE/DAP debugger integration—use gdb/lldb against the dev host for breakpoints; first make logs and event observation useful.
- Hot changes beyond plugin replacement (changing the SDK or host itself): outside dylib design boundaries; host or SDK changes still require redeployment.

## 11. Implementation steps

1. Bus probe hooks + aggregated `RuntimeDiagnostics` output (event stream, service/dependency snapshot)—independent of dylib, can start first.
2. `rutis-dev` crate: local IPC skeleton with `hello` / `describe` / `status` / `watch`.
3. `load` / `swap`: synthesized dev manifest (share implementation with packager), direct-path load, preserve current version on failure.
4. Configurable dev retention limit and `dev` registry marker.
5. `cargo xtask dev`: handshake/alignment, source watch, build, swap, streaming output.
6. `cargo xtask describe` / `watch`.
7. Persist recordings (§5.3 initial release).
8. Extend commands to load protocol plugins through the dev channel (with #46).

Step 1 does not depend on #45 and can start immediately; step 3 onward depends on the dylib SDK `Loader` landing.

## 12. Acceptance criteria

- [ ] One command completes “edit code → effective in host”: from source save to running new version in the host, perceptible latency under 10 seconds including incremental compilation for a medium-sized plugin.
- [ ] If `swap` fails, old version continues serving; error includes failed step and reason and uses the same `LoadError` source as production.
- [ ] `xtask dev` catches SDK_ID/toolchain mismatch **before compilation** and reports the host's actual values.
- [ ] `describe` output is sufficient: a new team member can write their first plugin consuming an existing service and hot-swap it successfully using only describe + watch, without offline docs.
- [ ] Dynamic events (not officially defined; contributed by other plugins) are visible in describe/watch with at least key name, provider, and subscribers; protocol events include schema and version.
- [ ] Dev channel does not use the network, and production build contains no dev listener (verify feature and runtime config).
- [ ] Dev retention limit is configurable, `status` shows remaining capacity, and a warning appears near the limit.
- [ ] Event recordings can be inspected offline in order through a tool (not as raw JSON).
- [ ] Protocol plugins can be loaded and hot-swapped through the dev channel (depends on #46).

## 13. Open questions

1. **Shape of shared dev environments:** one persistent host shared by the team (with isolated swaps for each developer?) or a full local deployment per person? This affects `unload-dev` and registry isolation; decide before implementation.
2. **Scale of full event-payload capture:** watch defaults to summaries and recording captures full payload. Measure real-host event throughput and recording rotation needs on the first real host.
3. **Development path for `opaque` services (§4.2):** for dylib services whose types are absent from all shared crates, should the standard path be asking the provider to move types into an interface crate, or re-exporting them through the protocol surface? This extends the #52 layering decision.
