# Design: Rewriting the Agent TUI Baseline with rutui

> 2026-08-23. Companion documents: [design-agent-verification-tui-2026-08-18.md](design-agent-verification-tui-2026-08-18.md) (current TUI design) and [design-min-agent-2026-08-18.md](design-min-agent-2026-08-18.md) (agent framework). This document defines the rewrite of `rutis-agent`'s `TuiPlugin` using [rutui](https://github.com/eric8810/rutui): replace the entirely handwritten conversation/input/wrapping logic with rutui's block-based scrollback and multiline prompt editor, without changing the agent framework core.

## 1. Motivation: Limitations of the Current Baseline

Current [`crates/rutis-agent/src/tui.rs`](../crates/rutis-agent/src/tui.rs) is an entirely handwritten TUI (about 460 lines, ratatui 0.30 + crossterm 0.29):

| Area | Current implementation | Limitation |
|---|---|---|
| Conversation | `App.transcript: Vec<Line<'static>>` + handwritten [`wrap_line`](../crates/rutis-agent/src/tui.rs#L294) (about 50 lines plus 4 tests for CJK full-width columns) | No scrollbar, folding, selection/copy, search, or sticky header |
| Input | `App.input: String` + handwritten single-line editing (only `Char`/`Backspace`) | No cursor movement, multiline input, readline shortcuts, undo, paste, or history |
| Assistant streaming | Append spans to the current line on `AgentTextDelta` | Plain text, **no Markdown rendering or code highlighting** |
| Tool display | Handwritten `* name(args)` / `-> [name] output` lines | No folding, diff, or status colors |
| Status bar | Handwritten `status_line()` | No shortcut hints |
| Terminal | Handwritten `setup/restore_terminal` | No terminal capability detection (truecolor/sixel/kitty/OSC8/tmux) |

**Core pain points:** no Markdown/code highlighting, weak input editing, and handwritten wrapping/scrolling machinery. rutui is a decoupled TUI toolkit extracted from a production coding agent and addresses these directly.

## 2. What Is rutui?

[rutui](https://github.com/eric8810/rutui) (Apache-2.0) was extracted from the production agentic coding CLI `xai-grok-pager`. It is based on ratatui + crossterm and uses Elm-style data flow (input → intent → synchronous state update → async effect). It contains **no application-specific business logic**: callers inject state, protocol, and product behavior through generics and `install_*` seams.

### Crates relevant to this rewrite

| Crate | Role | Use here |
|---|---|---|
| `rutui-core` | Block-based scrollback rendering pipeline: content blocks, scrolling, folding, selection, search, sticky header, timeline rail | **Conversation area** |
| `rutui-prompt` | Multiline prompt editor: textarea + paste/image elements + ghost text + completion + fuzzy history search | **Input** |
| `rutui-input` | Key abstraction + modifier normalization + `ActionRegistry<A,C>` key-binding registry (three-level bubbling) | **Key bindings** (P2) |
| `rutui-theme` | Five built-in themes + terminal capability detection + color quantization + appearance config | **Theme/terminal detection** (P2) |
| `rutui-widgets` | Modal/overlay/progress bar/shortcuts bar | **Shortcut bar** (P2) |
| `rutui-markdown` | Streaming Markdown renderer (pulldown-cmark + syntect highlighting) | Used directly by agent message blocks |
| `rutui-foundations` | Umbrella crate: textarea + inline viewport + TTY-safe subprocess | Indirectly, through core/prompt |

### Availability check

Ran `cargo check -p rutui-core -p rutui-prompt -p rutui-input -p rutui-theme -p rutui-widgets` for the five core crates; **all passed**, including the `nucleo` git dependency of `rutui-prompt`. Compilation took about 15 seconds.

## 3. Mapping the Core APIs

### 3.1 Conversation area: `ScrollbackState` + `ScrollbackPane`

[`ScrollbackState`](https://github.com/eric8810/rutui/blob/main/rutui-core/src/scrollback/state/mod.rs) is the conversation state machine and stores `IndexMap<EntryId, ScrollbackEntry>`. [`ScrollbackPane`](https://github.com/eric8810/rutui/blob/main/rutui-core/src/scrollback/scrollback_pane.rs) is a `StatefulWidget` whose `State = ScrollbackState`.

Streaming APIs (directly matching our agent event semantics):

```rust
// ScrollbackState
pub fn push_block(&mut self, block: RenderBlock) -> EntryId;        // Append a block and return its id
pub fn push_chunk_to_agent(&mut self, id: EntryId, chunk: &str) -> bool;  // Append to a streaming agent message
pub fn set_entry_running(&mut self, id: EntryId, running: bool);    // Mark a block as running (animated bullet)
pub fn finish_running(&mut self, id: EntryId);                      // Mark complete (stop animation)
pub fn tick(&mut self) -> bool;                                     // Advance animation; return whether redraw is needed
pub fn set_appearance(&mut self, appearance: AppearanceConfig);     // Appearance (fold/raw, etc.)

// ScrollbackPane (rendering)
impl StatefulWidget for ScrollbackPane {
    type State = ScrollbackState;
    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State);
}
// Or render_with_scratch(area, buf, &state, &mut scratch) to reuse a scratch buffer
```

### 3.2 `RenderBlock` variants ↔ agent events

[`RenderBlock`](https://github.com/eric8810/rutui/blob/main/rutui-core/src/scrollback/block.rs#L371) variants (only those used here are listed):

| `RenderBlock` variant | Agent event | Block type |
|---|---|---|
| `UserPrompt(UserPromptBlock)` | User presses Enter to submit | User message |
| `AgentMessage(AgentMessageBlock)` | `AgentTextDelta` (streaming) | **Markdown + code highlighting** |
| `ToolCall(ToolCallBlock::Other(OtherToolCallBlock))` | `AgentToolCall` + `AgentToolResult` | Generic tool block (bash/replace_text) |
| `SessionEvent(SessionEventBlock)` | `AgentTurnEnd{ok:false}` | Turn-failure event |
| `System(SystemMessageBlock)` | Intro line | System information |

Streaming usage of `AgentMessageBlock`:

```rust
let id = state.push_block(RenderBlock::AgentMessage(AgentMessageBlock::streaming()));
state.set_entry_running(id, true);
for delta in stream { state.push_chunk_to_agent(id, &delta); }
state.finish_running(id);
```

`AgentMessageBlock` is backed by [`MarkdownContent`](https://github.com/eric8810/rutui/blob/main/rutui-core/src/scrollback/blocks/markdown_content.rs) and uses `rutui_markdown::StreamingMarkdownRenderer` directly—**it does not use the `install_renderer` seam**. Markdown rendering and code highlighting work out of the box (highlighting lazily builds through `get_syntect()` from bundled `.tmTheme` assets; no host injection is needed).

`OtherToolCallBlock` (generic tool block, suitable for bash/replace_text):

```rust
let id = state.push_block(RenderBlock::ToolCall(ToolCallBlock::Other(
    OtherToolCallBlock::new(name, args)
)));
state.set_entry_running(id, true);
// When the result arrives:
//   finish_running(id) + (replace with a version containing output/error, or use replace_tool_block)
```

### 3.3 Input: `PromptWidget`

[`PromptWidget`](https://github.com/eric8810/rutui/blob/main/rutui-prompt/src/prompt_widget/mod.rs) is a multiline editor:

```rust
pub struct PromptWidget { /* textarea + elements + ghost text + completion */ }
impl PromptWidget {
    pub fn new() -> Self;
    pub fn handle_key(&mut self, key: &KeyEvent) -> PromptEvent;  // Edited | Ignored
    pub fn route_enter(&mut self, key: &KeyEvent) -> EnterOutcome; // NewlineInserted | Submit | PassThrough
    pub fn text(&self) -> &str;
    pub fn set_text(&mut self, text: &str);
    pub fn desired_height(&self, ...) -> u16;  // Adaptive height
}
```

Built-in readline shortcuts (Ctrl-A/E/W/U/K/Y), undo/redo, paste elements, ghost text, completion dropdown, and fuzzy history search are included. When `handle_key` returns `PromptEvent::Ignored`, the caller handles the key (Esc/Tab/Ctrl-D, etc.), which fits our key-dispatch requirements.

### 3.4 Seams: Nothing Is Mandatory

All `install_*` seams in rutui-theme use optional `OnceLock` injection:

| Seam | Behavior when not installed |
|---|---|
| `md_style::install_renderer` | Returns `None`—but `AgentMessageBlock` does not use this path (it calls `rutui_markdown` directly), so **no effect** |
| `clipboard::install_host` | Copy returns Err, **does not panic**, and degrades gracefully |
| `appearance::install_pager_config_path` | Uses the default path |

`Theme::current()` includes a default theme (if reading `~/.grok/config.toml` fails, falls back to GrokNight), and `AppearanceConfig::default()` is usable. **There are no mandatory bootstrap seams**; basic rendering works out of the box.

## 4. Event-to-Block Mapping

```text
User Enter (submit)   → push_block(UserPromptBlock::new(text)) + agent.followup(text) + running=true
AgentTextDelta (first)→ id=push_block(AgentMessageBlock::streaming()); set_entry_running(id,true); save cur_agent=id
AgentTextDelta (later)→ push_chunk_to_agent(cur_agent, &delta)
AgentToolCall         → id=push_block(OtherToolCallBlock::new(name, args)); set_entry_running(id,true); save cur_tool=id
AgentToolResult{ok}   → fill tool block output + finish_running(cur_tool)
AgentToolResult{err}  → fill tool block error + finish_running(cur_tool)
AgentTurnEnd{ok}      → finish_running(cur_agent) (if any) + running=false
AgentTurnEnd{err}     → push_block(SessionEvent::failed(error)) + running=false
intro                 → push_block(SystemMessageBlock::new(line)) (during apply startup)
```

This matches the semantics of the current `UiEvent` enum and `App::on_ui_event`; only the state container changes from `Vec<Line>` to `ScrollbackState`, with wrapping, scrolling, and animation handled by rutui.

## 5. Rewrite Scope and Boundaries

### Change

- [`crates/rutis-agent/src/tui.rs`](../crates/rutis-agent/src/tui.rs) — rewrite.
- [`crates/rutis-agent/Cargo.toml`](../crates/rutis-agent/Cargo.toml) — adjust dependencies (see §6).

### Leave unchanged

- Agent framework core: [agent.rs](../crates/rutis-agent/src/agent.rs) / [driver.rs](../crates/rutis-agent/src/driver.rs) / [events.rs](../crates/rutis-agent/src/events.rs) / [session.rs](../crates/rutis-agent/src/session.rs) / [minimal.rs](../crates/rutis-agent/src/minimal.rs) / [scripted.rs](../crates/rutis-agent/src/scripted.rs) / tools.
- The `TuiPlugin` **plugin skeleton**: `Plugin` trait, `injects = [agent]`, `apply` as the main loop, `with_intro`, cancellation on fiber unload (`ctx.cancelled()`), and listener cleanup on fiber unload (D28)—all preserved.
- [examples/](../crates/rutis-agent/examples) (`tui.rs` / `tui_scripted.rs` / `demo.rs`) and [rutis-cli](../crates/rutis-cli/src/main.rs)—API-compatible and benefit automatically.

### New `TuiPlugin` structure (illustrative)

```rust
pub struct TuiPlugin {
    inject_keys: Vec<TypeKey>,   // vec![agent_key()]  unchanged
    intro: Vec<String>,          // unchanged
}

struct App {
    scrollback: ScrollbackState,        // replaces Vec<Line>
    prompt: PromptWidget,               // replaces input: String
    cur_agent: Option<EntryId>,         // current streaming agent block
    cur_tool: Option<EntryId>,          // current running tool block
    running: bool,
    scratch: ScratchBuffer,             // reusable rendering scratch
}
```

The four `Listener`s (Delta/ToolCall/ToolResult/TurnEnd) still forward `agent/*` events into an `mpsc::channel`. The main loop consumes them with `select!` and calls `App` methods that operate on `ScrollbackState`. **The event-bridge architecture is unchanged**; only the path becomes `UiEvent` → `App` method → `ScrollbackState` operation.

## 6. Dependency and Version Alignment (Largest Risk)

### Conflict

| Crate | Current rutis-agent | rutui pinned version |
|---|---|---|
| ratatui | 0.30.2 | **0.29** |
| crossterm | 0.29.0 | **0.28** |
| unicode-width | 0.2.2 | 0.2 (compatible) |

rutui exposes ratatui types in its **public API** (`StatefulWidget`, `handle_key(&KeyEvent)`, `ScrollbackPane::render`), so consumers **must align versions** or type mismatches will fail compilation.

### Solution: downgrade rutis-agent

Downgrade rutis-agent's ratatui to 0.29 and crossterm to 0.28. `tui.rs` uses only basic types (`Frame`/`Terminal`/`Block`/`Paragraph`/`Layout`/`Color`/`Style`/`Span`/`Line`); these APIs have no breaking changes between 0.29 and 0.30, so the downgrade is small and can happen naturally during the rewrite.

> Alternative: wait until rutui upgrades to ratatui 0.30. rutui has just been extracted (one commit) and says “Expect breaking changes,” so its short-term schedule is unpredictable. Downgrading is the workable option now.

### How to depend on rutui

rutui is **not on crates.io** (one commit), so it can only be a git dependency for now:

```toml
# crates/rutis-agent/Cargo.toml
[dependencies]
rutui-core   = { git = "https://github.com/eric8810/rutui.git", rev = "<pin>" }
rutui-prompt = { git = "https://github.com/eric8810/rutui.git", rev = "<pin>" }
# Add in P2: rutui-input / rutui-widgets / rutui-theme
```

- **Pin `rev`:** rutui's API is unstable; a commit pin avoids unexpected breakage.
- `rutui-prompt` pulls [nucleo](https://github.com/helix-editor/nucleo) as a git dependency for fuzzy history search; compilation has been verified.
- If rutui is published to crates.io later, switch to a version dependency.

### rust-version

rutui crates use `edition = "2024"` and require Rust 1.85+; the rutis workspace has `rust-version = "1.85"`, so this is satisfied.

## 7. Phased Implementation

### P1: core replacement (conversation, streaming, tools, input)

1. Cargo.toml: downgrade ratatui/crossterm and add rutui-core/rutui-prompt git dependencies.
2. Rewrite tui.rs:
   - `App`: `ScrollbackState` + `PromptWidget` + `cur_agent`/`cur_tool`/`running` + `ScratchBuffer`.
   - Four listeners: translate agent events into channel commands (same architecture as today).
   - Main loop: `select!` over cancellation / tick / input / `ui_rx`; rendering uses `ScrollbackPane::render_with_scratch` + `PromptWidget` + a handwritten status line.
   - Key dispatch: `PromptWidget::handle_key` → `Edited`/`Ignored`; when ignored, handle Enter(submit)/Esc/Ctrl+C(cancel)/Ctrl+Q(quit) with a handwritten match.
   - Preserve `setup/restore_terminal`, fiber-unload fallback, and `with_intro`.
3. Verify with `cargo run -p rutis-agent --example tui_scripted` (offline) and `--example tui` (real, requires key).

### P2: enhancements (key bindings, theme, terminal detection)

1. Define send/cancel/quit actions in `ActionRegistry`, replacing the handwritten key match.
2. Render a `ShortcutsBar` for shortcut hints, replacing the handwritten status line.
3. Use `rutui-theme` terminal capability detection (truecolor/sixel/kitty/OSC8/tmux/keyboard protocol) and theme selection to enhance setup.
4. Optionally wire up folding/raw mode, text selection/copy, and search—already built into rutui.

## 8. Verification

| Layer | Method |
|---|---|
| Compile | `cargo check -p rutis-agent` (dependency alignment + fetch git dependencies) |
| Offline TUI | `cargo run -p rutis-agent --example tui_scripted` (scripted backend, no key required) |
| Real TUI | `cargo run -p rutis-agent --example tui` (requires `DEEPSEEK_API_KEY`) |
| CLI | `rutis-cli --scripted` / `rutis-cli` (real) |
| Existing tests | `cargo test -p rutis-agent` — the four `wrap_line` unit tests in tui.rs are removed with the rewrite (rutui handles wrapping internally); other tests are unaffected |

## 9. Risks and Mitigations

| Risk | Mitigation |
|---|---|
| rutui API unstable (one commit; warns of breaking changes) | Pin `rev`; depend only on stable core APIs (`ScrollbackState`/`ScrollbackPane`/`PromptWidget`/`RenderBlock`) during rewrite; avoid obscure blocks |
| Version downgrade (0.30→0.29) affects other code | Only rutis-agent uses ratatui; rutis-cli consumes it indirectly and will use the same version; core crates have no TUI dependency |
| git dependency (nucleo) increases build complexity | Compilation verified; pin rev; switch to crates.io later |
| rutui extracted from Grok agent and may carry domain assumptions | Used variants (UserPrompt/AgentMessage/OtherToolCall/SessionEvent/System) are generic semantics; ignore Grok-specific blocks (ContextInfo/CreditLimit/Btw, etc.) |
| `OtherToolCallBlock` running→finished flow | rutui state has `set_entry_running`/`finish_running`/`replace_tool_block` with matching semantics; verify in P1 |

## 10. Implementation Record (2026-08-23, P1 Completed)

### Dependency method: path instead of git

Implementation uses **path dependencies** (rutui cloned beside rutis at `../rutui`) rather than the planned git dependencies. Reasons:

- rutui was just extracted (one commit) and its API is unstable; publishing to crates.io now would be premature.
- P1 is the phase with the most frequent rutui changes. Path dependencies apply edits immediately; git dependencies would require a push and rev update each time.
- Once the rutis-agent rewrite is complete and rutui's API has stabilized in practice, switch in the order `path → git → crates.io`.

Actual Cargo.toml (`crates/rutis-agent/Cargo.toml`):

```toml
ratatui = "0.29"     # downgraded from 0.30.2 (rutui pins 0.29)
crossterm = { version = "0.28", features = ["event-stream"] }  # downgraded from 0.29.0
rutui-core   = { path = "../../../rutui/rutui-core" }
rutui-prompt = { path = "../../../rutui/rutui-prompt" }
rutui-theme  = { path = "../../../rutui/rutui-theme" }
```

### Key finding 1: `ScrollbackState` is not Send, so use a dedicated OS thread

`ScrollbackState` contains `MarkdownContent` → `StreamingMarkdownRenderer`, which contains an onig_sys `*mut` and is not Send. Since `App` owns `ScrollbackState`, it is not Send either; rutis `BoxFuture` requires Send, so it cannot be held across awaits in `tokio::select!`.

**Solution:** a TUI is naturally single-threaded. Put `App` + `Terminal` on a dedicated OS thread (`std::thread::spawn`) running a synchronous render loop. Async `apply` holds only Send channel handles for I/O multiplexing:

```
async apply (Send)                     OS thread "rutis-tui" (non-Send is OK)
┌─────────────────────┐                ┌──────────────────────────────┐
│ EventStream → key   │──ThreadMsg──▶  │ loop { draw; recv_timeout }  │
│ agent events → UiCmd│──ThreadMsg──▶  │   App(ScrollbackState)       │
│ ctx.cancelled()     │──Cancel────▶   │   PromptWidget               │
│                     │◀─outcome─────  │   Terminal                   │
│ handle.spawn(       │                │ handle_key → HandleOutcome   │
│   agent.followup)   │                │   Quit/Cancel/Submit         │
└─────────────────────┘                └──────────────────────────────┘
```

- The render thread owns `Arc<dyn Agent>` (`Agent: Send + Sync`). `Submit`/`Cancel` use `Handle::spawn` to run `agent.followup`/`agent.cancel` on the tokio runtime.
- The render thread uses `std::sync::mpsc::recv_timeout(50ms)` to balance frame rate with message responsiveness.
- Fiber unload (`ctx.cancelled()`) sends `ThreadMsg::Cancel` to stop the render thread.

### Key finding 2: call prepare_layout before rendering

`ScrollbackPane::render_with_scratch` requires callers to call `ScrollbackState::prepare_layout(width, height)` **first** (to calculate entry heights and cache layout); otherwise it panics (“layout cache must be valid - was prepare_layout() called?”). The default `StatefulWidget::render` implementation in rutui does not call prepare_layout (the comment explicitly says: “All layout preparation is now done by state.prepare_layout() BEFORE render is called”).

```rust
app.scrollback.prepare_layout(conv.width, conv.height);  // must be called first
ScrollbackPane::new().render_with_scratch(conv, buf, &app.scrollback, &mut scratch);
```

### Verification results

| Check | Result |
|---|---|
| `cargo check --workspace --examples` | ✅ Entire workspace compiled |
| `cargo test -p rutis-agent` | ✅ 15 unit_loop tests + integration tests + doc tests passed |
| `tui_scripted` offline PTY run | ✅ Rendered get_weather/Oslo/scripted/status/idle/running; no panic, exit 0, terminal restored |
| `rutis-cli --scripted` offline PTY run | ✅ Rendered bash/replace_text/status; no panic, exit 0, terminal restored |

### Actual event-to-block mapping

| Event | App operation |
|---|---|
| intro | `push_block(System)` |
| Enter(submit) | `push_block(UserPrompt)` + `agent.followup` |
| `AgentReasoning` (first) | **First call `finish_running(cur_thinking)`**; `push_block(Thinking::streaming())` + `set_entry_running(id,true)` + `cur_thinking=id` |
| `AgentReasoning` (later) | `push_chunk_to_thinking(id, &delta)` |
| `AgentTextDelta` (first) | **First call `finish_running(cur_thinking)`**; `push_block(AgentMessage::streaming())` + `set_entry_running(id,true)` |
| `AgentTextDelta` (later) | `push_chunk_to_agent(id, &delta)` |
| `AgentToolCall` | **First `finish_running(cur_thinking)` + `finish_running(cur_agent)`**, then `push_block(ToolCall::Other(new(name,args)))` + `set_entry_running`; save `cur_tool=(id,args)` |
| `AgentToolResult` | `replace_tool_block(id, Other with_output/with_error, started_at)` + `finish_running` (do not force expansion) |
| `AgentTurnEnd{ok}` | `finish_running(cur_thinking)` + `finish_running(cur_agent)` + idle |
| `AgentTurnEnd{err}` | `finish_running(cur_thinking)` + `finish_running(cur_agent)` + `push_block(SessionEvent::TurnFailed)` + idle |

### Key finding 3: text and tool calls must alternate within a turn

**Bug found during real use:** initially, `cur_agent` pointed to one text block for the whole turn. On `ToolCall`, it was not closed, so text arriving **after** the tool call was appended to the **first** text block and appeared before the tool.

Driver event order per step: `AgentTextDelta` × N → `AgentStepEvent` → `AgentToolCall` → `AgentToolResult`. Correct display alternates in time: `[text1][tool1][text2][tool2][text3]`.

**Fix:** when handling `UiCmd::ToolCall`, first call `finish_running(cur_agent)` (close and freeze the current text block, set `cur_agent=None`), then append the tool block. A later `TextDelta` naturally opens a new block because `cur_agent=None`. Also retain tool arguments in `cur_tool: Option<(EntryId, String)>`; when the result arrives, preserve the summary and populate output/error separately rather than treating the result as the summary.

Regression test `tui::tests::text_and_tool_calls_interleave_in_order` locks in this ordering.

### Key finding 4: tool results are folded (data is merged, but hidden by default)

**Observed in real use:** after a tool call, the result was not visible on the tool-call block.

**Cause:** `replace_tool_block` resets the entry's `display_mode` to `block.default_display_mode()`, and `OtherToolCallBlock::default_display_mode()` is `Collapsed`. `Collapsed` renders only the header (name + summary), **not the output**. `OtherToolCallBlock::finished_display_mode()` uses the default (`None`), so `finish_running` does not expand it. The result data had been written into the block (unit test `tool(get_weather,city=Oslo,out=18°)` proves this), but was hidden by folding.

**Ground-truth validation:** `tui::tests::tool_result_output_visibility` once rendered the ToolCall + ToolResult sequence through the real `ScrollbackPane::render_with_scratch` into a `Buffer`, confirming that before the fix only `◆ get_weather  city=Oslo` appeared and no output was rendered (Collapsed mode). It has since been replaced with the data-level check `tui::tests::tool_result_merged_into_block`.

**Conclusion and handling:** this is rutui's design: tool blocks are collapsed by default. Result data is merged into the same block, but remains hidden until manually expanded (Ctrl+E/click). At the user's confirmation that default expansion was unnecessary, we **did not force Expanded** and retained rutui's default collapsed behavior; we only preserve `started_at` timing so the correct duration appears when manually expanded. If direct visibility is preferred later, set `display_mode = DisplayMode.Expanded/Truncated` on the entry in `ToolResult` (or change rutui so `OtherToolCallBlock` defaults to `Truncated`).

### Key finding 5: reasoning was not displayed at all

**Observed in real use:** the model emitted reasoning/chain-of-thought, but the TUI displayed none of it.

**Two causes:**

1. **The driver discarded it:** `aimux_core::StreamPart` has `ReasoningStart/ReasoningDelta/ReasoningEnd` variants, but the streaming loop in `driver.rs` handled only `TextDelta/ToolCall/Error`; a catch-all `Chunk::Part(Ok(_)) => {}` silently discarded the rest, including `ReasoningDelta`, with no reasoning event.
2. **The TUI did not listen for it:** the original `TuiPlugin::apply` subscribed only to `AgentTextDelta/AgentToolCall/AgentToolResult/AgentTurnEnd`, so there was no reasoning event to consume.

**Fix across three files:**

- `events.rs`: add an `AgentReasoning { session, step, delta }` event, parallel to `AgentTextDelta` (`#[allow(dead_code)]` because the listener reads only `delta`).
- `driver.rs`: add a `Chunk::Part(Ok(StreamPart::ReasoningDelta { delta, .. }))` branch in the streaming loop and broadcast `AgentReasoning` through a new `emit_reasoning(...)`. Reasoning is for display only; it is not added to assistant text or written back to the session.
- `tui.rs`: add `UiCmd::ReasoningDelta(String)` and a `ReasoningL` listener, subscribing in `apply()`. Add `cur_thinking: Option<EntryId>` to `App`. In `on_ui_cmd`, render reasoning as rutui `ThinkingBlock` (`streaming()` + `push_chunk_to_thinking` + `finish_running`), and first finish `cur_thinking` on `TextDelta`/`ToolCall`/`TurnEnd` to preserve temporal order `[reasoning][text][tool]`.

**Result:** reasoning appears in a separate folded “Thought” block (collapsed by default; users can expand with Ctrl+E/click), before the body text/tool call.

Regression test `tui::tests::reasoning_rendered_before_text` locks in this order and merging behavior.
