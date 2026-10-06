# Agent Verification Strategy and TUI Interaction Design

> 2026-08-18. Companion to [design-min-agent-2026-08-18.md](design-min-agent-2026-08-18.en.md), the minimal agent framework. This document defines two things: **how to verify it** (three test layers plus real end-to-end) and **the simplest useful TUI interaction**.

## 1. Verification strategy (three layers plus real-world testing)

| Layer | Backend | What it verifies | When to run |
|---|---|---|---|
| **Unit** | Scripted backend (returns `LlmResponse` values in order) | Loop logic: no tools → final answer; tools → execute → feed result back → final answer; history continuity across turns; `max_steps` cutoff; cancellation between steps; tool failure/panic is fed back without crashing | Every CI run |
| **Integration** | aimux `MockReplayModel` (uses the real `LanguageModel` interface with recorded replay) | Plugin assembly: dual gates (llm + tools); unloading llm automatically evicts driver; fiber unload → `ctx.cancelled()` → loop stops; observe `agent/step` and `agent/tool` events; listeners unload with fiber | Every CI run |
| **Real end-to-end** | Real provider (DeepSeek cloud or local Ollama) | Multiple followups against a real model, tool calls, final-answer quality, history continuity | Manual/demo, **not in CI** |

### Unit layer: scripted backend

The existing `ScriptedLlm`, which returns responses in order, implements `LanguageModel`. For history continuity, assert that on the second `followup` the backend receives a prompt containing all messages from the first turn. This proves that the session is the source of truth for the continuous loop.

### Integration layer: MockReplayModel

`MockReplayModel::new(provider, model_id, recordings)` implements `LanguageModel`, matches recordings by input, and makes no real API calls (`aimux/replay.rs`). **The eviction assertion is mandatory:** dispose the llm service and assert that the driver fiber returns to Pending. This is a framework differentiator. Control ordering with synchronization points (`Notify`/gates), not `sleep`.

### Real end-to-end layer: the demo is the test

```rust
// examples/demo.rs (or an #[ignore] test), with a real backend
let model = aimux_providers::provider("deepseek", None, "deepseek-chat", None)?;  // None → read DEEPSEEK_API_KEY
let root = Ctx::root()?;
root.provide_as(llm_key(), Arc::from(model))?;
root.plugin(ToolsPlugin::new(vec![weather_tool]));
let agent_view = root.plugin(AgentDriverPlugin::new(16));
(&agent_view).await.expect("gated on llm+tools");
let agent = root.get_as::<dyn Agent>(agent_key()).unwrap();
let a1 = agent.followup("weather in Oslo?").await?;
let a2 = agent.followup("and in Bergen?").await?;   // continuous history
// Unloading llm → driver is automatically evicted (dependency-driven reload)
```

Implement the real layer as an `#[ignore]` test plus a runnable demo. Trigger it manually when an API key or local service is available: `cargo test -p rutis-agent -- --ignored`. CI does not fail when no key is configured.

## 2. TUI interaction design (the simplest user interaction)

### Goal

The simplest interaction: user input → see the agent's reasoning/tool calls/answer stream in → enter another turn. **The TUI is the vehicle for real end-to-end verification:** it turns the demo's scripted two turns into live, multi-turn human interaction.

### Technology choices

- **ratatui + crossterm:** the de facto Rust TUI stack; cross-platform and mature with Tokio. Add these two dependencies to the crate, which currently has no TUI dependency.
- **aimux `do_stream`:** inside the driver loop, `do_stream` yields `TextDelta`; emit each chunk as an `AgentTextDelta` event (see design §3/4.1). The TUI subscribes and prints each chunk. **Streaming is required in the first release** and is broadcast through EventBus.

### Interaction model: a TUI frontend plugin plus an agent service

The TUI is a frontend fiber that **consumes `agent/*` events**. It does not connect directly to the `followup` stream. It subscribes to events for rendering; input triggers `followup`:

```
┌─ TuiPlugin(fiber)──────────────────────────────┐
│ Input line: Enter → agent.followup(text) (turn) │
│ Display: on(AgentTextDelta/AgentToolCall/        │
│          AgentToolResult), render chunks        │
│ Status bar: agent.status(idle/running)          │
└────────────────────────────────────────────────┘
        ↓ dependency (injects)
  agent service (AgentDriverPlugin)
```

- **Input** → `agent.followup(input)` triggers a turn and returns its final result, not intermediate events.
- **Display** → subscribe to `AgentTextDelta` (incremental text), `AgentToolCall`, and `AgentToolResult`; **all progress arrives over EventBus broadcast, not an exclusive stream**.
- **Cancel** → Esc/Ctrl+C invokes `agent.cancel()` (interrupt current turn but retain history).
- **Exit** → unload the TUI fiber; the driver stops through cascading unload. Listeners unload with the fiber (D28).

**Why events instead of a stream:** output is broadcast, not exclusive. Multiple consumers can observe one turn (TUI, logs, future frontend); the TUI can subscribe late and observe without controlling the stream. This decouples it from the driver, and fiber lifecycle owns listener cleanup.

### Minimal TUI layout (three sections)

```
┌──────────────────────────────────────────────┐
│ Conversation (scroll): user/assistant/tool    │
│   you> weather in Oslo?                      │
│   ⚙ get_weather({"city":"Oslo"}) → 18° clear│
│   agent> Oslo: 18 degrees, clear sky.        │
├──────────────────────────────────────────────┤
│ Status: idle | running (step 2) | [Esc cancel]│
├──────────────────────────────────────────────┤
│ Input> _                                      │
└──────────────────────────────────────────────┘
```

### Minimal interaction loop (trimmed from dsh turn flow)

```
User input → agent.followup(input) (trigger; returns final result)
  → session.push(user)
  → loop: agent/pre-step waterfall (rewrite/reject messages) → llm stream
        → emit each TextDelta as AgentTextDelta → TUI renders incrementally
        → ToolCall through three stages (pre-execute gate → execute → post-execute decision)
        → emit AgentToolCall/AgentToolResult → TUI displays them
        → no tool_call → final answer → emit AgentTurnEnd
  → status: running → idle
  → wait for next input
```

This follows dsh's turn flow: `agent/pre-step` and the three tool stages (`pre-execute`/`execute`/`post-execute`) are waterfalls that can rewrite, veto, or decide; progress is broadcast incrementally through `agent/*` events. Omit `agent/request` model routing (M4), inbox queuing, and steer. The minimal TUI needs input, streamed text, visible tools, and cancellation; **incremental streaming is a first-release requirement and uses event broadcasts**.

### Out of scope

- No switching between multiple agents, persisted-session recovery, command system (`/clear`, etc.), Markdown rendering/syntax highlighting, or mouse support.
- Streaming is included in the first release and broadcast through EventBus. `agent/*` events are the single observation channel shared by the TUI and other observers; `followup` does not expose a private stream.

## 3. Implementation order

1. **Unit and integration tests** (scripted backend + MockReplayModel), added alongside agent crate changes and run in CI.
2. **Switch the demo to a real backend:** `provider("deepseek", None, ...)`, then verify with a key.
3. **TuiPlugin:** ratatui + crossterm; first release supports multiple turns, visible tool calls, and cancellation. Add streaming afterward.
