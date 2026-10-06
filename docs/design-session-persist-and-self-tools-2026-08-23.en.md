# Design: Session Persistence and Self-Control Tools

> 2026-08-23. Goal: give the agent the minimum capabilities to “keep its memory across restarts and control its own hot reload.”
> Position: infrastructure for self-evolution, with a minimal scope—add only two things: memory restoration and control tools.
> Based on [design-self-evolving-agent-2026-08-23.md](design-self-evolving-agent-2026-08-23.md) (persona design).

## 1. Session persistence

### Goal

Restore model-visible session history after a process restart or dependency reload, while keeping `agent.id()` unchanged.

### Current state (evidence)

- `Session` is `{ id: u64 (global counter), messages: Vec<ModelMessage> }`, in memory only, without persistence (`session.rs`).
- Restarting a fiber creates a new driver, then `Session::new()`, losing history.
- `SessionId` is used by `Agent::id()` and the `session` field in `agent/*` event payloads. Only `integration.rs:198` tests `agent.id()`.

### Design

1. **Format:** one JSON file (default `.rutis/session.json`), `SessionFile { version: 1, id: u64, messages: Vec<ModelMessage>, saved_at_ms: u64 }`. aimux messages already implement serde, so no conversion is needed.
2. **Generational `SessionId`:** `{ identity: u64 (stable, allocated once), generation: u32 (incremented on restart) }`. Persist `identity` unchanged across restarts; `generation` indicates which generation is active. Keep `as_u64()` (returns identity) for existing consumers.
3. **Save points:** (1) after each turn, before `followup` returns; (2) when the fiber unloads, by registering an effect disposer (registered later, so it runs earlier in LIFO cleanup). Write atomically using a temporary file and rename.
4. **Restore:** `AgentDriverPlugin::with_session_path(path)` calls `Session::restore(path)` during `apply`. On failure, silently start a new session and continue.
5. **Disabled by default:** `None` means no persistence, preserving current behavior.

### Validation

- `session_persist_roundtrip` (unit test)
- `corrupt_file_starts_fresh` (bad file starts a new session without hanging)
- `session_restored_after_driver_restart` (core case: two `ScriptedLlm` turns; the second prompt contains the first turn's history)
- `not_persisted_by_default` (no path keeps current behavior)

## 2. Self-control toolset

### Goal

Give the agent the “hands” to steer its own hot reload. The control loop consists of decisions (supervisor, later) and execution (tools, this design).

### Minimum toolset (6 tools)

| Tool | Purpose | Reuse / addition |
|---|---|---|
| `self_status` | Read session ID/generation/status/version | New; reads an `AgentDriver` status snapshot |
| `self_persist` | Persist the session manually | New; calls `Session::persist` |
| `self_build` | Run `cargo build -p rutis-agent` | Reuse bash |
| `self_check` | Run `cargo test` | Reuse bash |
| `self_reload` | Trigger a restart (cold: write intent and exit; hot: request the supervisor) | New |
| `self_rollback` | Roll back to the previous generation | New; uses a version ledger |

### Mount points

- Register tools as `ToolDef` entries in `ToolRegistry` (the `defs` list passed to `ToolsPlugin::new(defs)`).
- `self_status` / `self_persist` / `self_reload` need `AgentDriver` state. Use existing methods on the `Agent` trait (`id()` / `status()` / `session()`) rather than adding trait methods and polluting the interface. Inject the persistence path through `with_session_path`.
- Reuse bash for `self_build` / `self_check` rather than reimplementing it.
- Implement the cold-restart version of `self_reload` first (write a handoff intent and exit; the host restarts the process). The supervisor (hot restart) comes later.
- The tools themselves live in `ToolRegistry`, which can be hot-swapped: **adding/replacing agent tools means hot-swapping the registry.**

### Validation

One scripted test per tool; `self_reload` verifies “write intent document + request exit.”

## 3. Explicitly out of scope

- Persisting or replaying dsh's two event streams.
- Managing or switching multiple sessions.
- Dynamically loading new code (dylib/scripts). That is future work; this design only preserves memory across restarts and can trigger a restart.
- Automatic supervisor decisions (future work).

## 4. Evolution order

1. Session persistence (foundation, this design)
2. Self-control toolset (hands, this design)
3. Automatic supervisor decisions (mind, future work)
4. Dynamic loading of new code (ultimate goal, future work)
