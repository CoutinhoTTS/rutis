# Minimal Mode Supplementary Design (`replace_text` + `bash`)

> 2026-08-18. Goal: add the deepseek-harness minimal mode (only two tools, `replace_text` + `bash`) to our minimal agent MVP, making it a coding agent that can do real work.
> Basis: [design-min-agent-2026-08-18.md](design-min-agent-2026-08-18.en.md) (minimal agent framework), dsh [tool catalog](../deepseek-harness/docs/tool-catalog.md), and the headless bundle.
> Position: an MVP increment—from “can chat” to “can edit files and run commands.”

## 1. What is minimal mode?

dsh minimal mode is a coding agent with only two tools:

| Tool | dsh equivalent | Function |
|---|---|---|
| **bash** | `dsh-tool-bash` | Executes a bash command (`bash -c`) and returns stdout/stderr. Each call starts a new process, so state does not persist (cwd/variables do not carry over); use `workdir` instead of `cd`. |
| **replace_text** | `dsh-tool-str-replace-editor` | View/create files and make local edits. Core command `str_replace` exactly replaces the unique matching contiguous lines specified by `old_str` with `new_str`. |

Together, these tools provide the minimum coding-agent capabilities: **read/edit files (`replace_text`) + run commands (`bash`).**

## 2. Features to add (in dependency order)

### 1. `bash` tool plugin (standalone plugin providing `ctx.shell` capability)

```rust
pub struct BashTool;  // implementation of ToolDef
// schema:
//   command: string (required)
//   description: string (required; one sentence saying what the command does, for UI; follow dsh)
//   workdir: string (optional; state does not persist between calls, use workdir instead of cd)
//   timeout_ms: number (optional, bounded by a default)
// runner: tokio::process::Command starts bash -c and captures stdout/stderr
```

Details (match dsh):

- **Start a new process for every call**, with no persistent shell state. Document “use `workdir`, not `cd`.”
- **A non-zero exit is not an error:** return `[exit code: N]` + stderr so the model sees the real result instead of crashing.
- **Truncate long output from the tail** (dsh behavior). **Intentional reduction:** dsh stores full output in a file and reports its path; the minimal version only truncates and does not add file storage.
- **Timeout:** bounded by default; kill the process on timeout.
- **Not in this version:** `run_in_background` + jobs (available in dsh but removed here); also remove the sandbox escalation flow (`sandbox_permissions` / `justification`).

### 2. `replace_text` tool plugin (standalone file-editing plugin)

dsh's `str_replace_editor` has four commands: view/create/str_replace/insert. **Keep only three** (view/str_replace/create; create makes a new file precisely and is more controllable than bash redirection):

```rust
// schema:
//   command: "view" | "create" | "str_replace"
//   path: string
//   file_text: string (required for create; new file contents)
//   old_str: string (required for str_replace; unique contiguous lines in the file)
//   new_str: string (required for str_replace; replacement text)
//   view_range: [start, end] (optional for view; inspect a line range)
```

Details (match the actual `replaceInFile` behavior in dsh):

- **view:** for a file, output `cat -n` (line numbers, right-aligned to six characters); for a directory, list non-hidden files up to two levels deep. **Directories support only view**; other commands return “only the `view` command can be used on directories.” Validate `view_range` start/end; `-1` means through EOF. Truncate long output with `<response clipped>` and a hint to retry with “use grep -n to find the line number” (teach the model to recover itself).
- **str_replace** (exact dsh `replaceInFile` semantics):
  - If `old_str` is absent, return `old_str ... did not appear verbatim in <path>`.
  - If it matches multiple places, return `Multiple occurrences of old_str ... in lines [line numbers]`, **including the line numbers**—the model uses these to add context, so this must be preserved.
  - If `new_str` is omitted, default to an empty string; str_replace can therefore delete text.
  - On success, return `The file <path> has been edited successfully.`
- **create:** if the file already exists, return `Cannot overwrite files using command `create``; on success return `New file created successfully at: <path>`.
- **Not in this version:** `insert`, `undo_edit`, and read-before-write policy gating (dsh's fs-observation-policy).

### 3. System prompt + tool descriptions (copy dsh wording)

dsh's prompts are mainly **not in the system prompt**; they live in tool schema descriptions. The model sees descriptions when it reads the tools. Split the minimal prompt into two layers:

**Persona (static, interpolate `{{cwd}}`)** — match the dsh headless bundle persona:

```text
You are a coding agent powered by the {{model}} model. Your working directory is {{cwd}}.
```

**bash tool description (copy dsh `bashDescription`, removing sandbox/background sections):**

```text
Execute a bash command (`bash -c`) and return its stdout/stderr. Each call runs in a fresh shell: no state (cwd, variables, functions) persists between calls — pass `workdir` instead of using `cd`. Non-zero exits are reported as `[exit code: N]`. Long output is truncated to its tail.
```

The `description` parameter is also required on bash calls (for UI display; follow dsh): “Clear, concise description of what this command does in active voice, 5-10 words.”

**replace_text tool description (copy dsh `DEFAULT_DESCRIPTION`, removing insert/undo sections):**

```text
Custom editing tool for viewing, creating and editing files
* State is persistent across command calls and discussions with the user
* If `path` is a file, `view` displays the result of applying `cat -n`. If `path` is a directory, `view` lists non-hidden files and directories up to 2 levels deep
* The `create` command cannot be used if the specified `path` already exists as a file
* If a `command` generates a long output, it will be truncated and marked with `<response clipped>`

Notes for using the `str_replace` command:
* The `old_str` parameter should match EXACTLY one or more consecutive lines from the original file. Be mindful of whitespaces!
* If the `old_str` parameter is not unique in the file, the replacement will not be performed. Make sure to include enough context in `old_str` to make it unique
* The `new_str` parameter should contain the edited lines that should replace the `old_str`
```

**Principle:** copy the original dsh tool descriptions (tuned and production-tested), removing only feature sections we explicitly exclude (sandbox/background/insert/undo). Use the minimal dsh headless persona. Do not implement dsh's system-prompt section assembly registry; that exceeds the minimum.

### 4. Register in the tool set

Add both tools to `ToolsPlugin`'s `ToolRegistry` (the tool registry plugin in design §3.2). MVP tools = `bash` + `replace_text` (+ optionally retain demo `get_weather` as an example). **No new plugin kind is needed**: these are two new `ToolDef` implementations installed in the existing `ToolsPlugin`.

## 3. Which plugin owns the tools, how to register them, and how to consume them

### dsh approach (reference design)

In dsh, **each tool is a standalone Cordis plugin package**, registered in the central tool service through `ctx.tools.register(defineTool({...}))`:

```ts
// packages/shell/tool-bash/src/index.ts
export const name = 'tool-bash'
export const inject = ['tools', 'shell', 'systemPrompt', 'shellEnv']  // dependency gates
// in apply:
ctx.tools.register(defineTool({ name: 'bash', description, parameters, execute }))
// → returns a disposer; automatically unregistered when the fiber unloads

// packages/fs/tool-str-replace-editor/src/index.ts
export const name = 'tool-str-replace-editor'
export const inject = ['tools', 'fs']
```

- **Registration:** `ctx.tools.register(def)` returns a disposer owned by this plugin fiber (D28); unloading unregisters it. The schema is included in prompt assembly (`ctx.systemPrompt`).
- **Consumption:** a model tool_call in the driver loop → look up by name in `ctx.tools`, call `execute`, then feed the result back.
- **Key point:** dsh's `ctx.tools` is a **central tool service** provided by the `core/tools` plugin. Tool plugins register there and inject their own capability seams (`ctx.shell` for bash, `ctx.fs` for the editor).

### Our minimal shape (trimmed)

We **do not implement dsh's two layers of central tool service + capability seams**. The minimum version is:

- **The two tools are not standalone plugins.** They are `ToolDef` values installed in **one `ToolsPlugin`** (the tool registry plugin in design §3.2): `ToolsPlugin::new(vec![bash_tool(), replace_text_tool()])`.
- **Registration:** `ToolsPlugin.apply` provides a `ToolRegistry` (containing both tools) as the `ctx.tools` service. Unloading this fiber removes the service.
- **Consumption:** the `AgentDriver` loop obtains `ToolRegistry` from `ctx.tools`; model tool_call → `registry.execute(&call, &token)` → feed the result into the session.
- **Remove capability seams:** bash calls `tokio::process::Command` directly; replace_text calls `std::fs` directly. The minimum needs runners, not a “replaceable backend” abstraction.

### Comparison

| | dsh | Our minimal version |
|---|---|---|
| Tool representation | One standalone plugin package per tool | `ToolDef` values installed in one `ToolsPlugin` |
| Central tool service | `core/tools` provides `ctx.tools`; `register` returns a disposer | `ToolRegistry` is `ctx.tools`; provided by `ToolsPlugin` |
| Capability seams | bash → `ctx.shell`, editor → `ctx.fs` (replaceable backend) | Direct `tokio::process` / `std::fs`; no abstraction layer |
| Dependency gates | Tool plugin injects its own seam | `AgentDriverPlugin` injects `[llm, tools]` |
| Unload | Registration disposer runs on fiber unload | ToolRegistry service is removed with the ToolsPlugin fiber |

**Rationale:** dsh's “central tool service + capability seams” supports multiple deployment modes (local/sandbox/remote backends). The minimal MVP runs locally in one process and does not need backend replacement, so it is reduced to “one ToolsPlugin contains all tools, with direct runners.” If multiple backends become necessary, extract bash/editor into their own seam plugins later.

## 4. Explicitly out of scope

- **No new plugin kinds:** bash/replace_text are `ToolDef` values in existing `ToolsPlugin`; no dsh-style `ctx.shell` / `ctx.fs` capability seams. The minimum needs runners, not an abstraction layer.
- **No separate central tool-service package** (dsh `core/tools`): in the minimal version, `ToolRegistry` is the tool service and `ToolsPlugin` provides it directly.
- **No system-prompt assembly registry:** static persona + cwd interpolation is enough.
- **No sandbox/approval/permission flow** (dsh production security): the minimal MVP runs locally and trusts the user; security can be added later.
- **No jobs/background**, **read-before-write gate**, or **Code Mode** (dsh worker execution); these exceed the minimum.
- **No persistence/session log:** retain the in-memory session specified by the design.

## 5. Complete MVP shape (with this supplement)

```text
rutis framework (fiber/events/registry)
  └─ rutis-agent
       ├─ LLM service (aimux, provided directly)
       ├─ ToolsPlugin: bash + replace_text [+ get_weather example]
       ├─ AgentDriverPlugin (injects=[llm, tools])
       └─ TuiPlugin (consumes Agent; streaming interaction)
```

User: “Change the timeout in `config.rs` to 30 and run the tests.” → driver loop: LLM calls replace_text to edit the file → calls bash to run `cargo test` → receives the result → gives a final answer. **This is a minimal coding agent that can do real work.**

## 6. Acceptance

The goal is **a real person completing “edit a file + inspect a file + run a script” in the TUI**, not merely green tests.

### TUI end-to-end (real acceptance, `cargo run -p rutis-agent --example tui`)

Use a real key and give the agent a real coding task in the TUI. Verify visually:

1. **Create a file:** “Create `hello.rs` in the current directory with a main function that prints hello.” The agent calls `replace_text` create; use local `cat hello.rs` to confirm the file exists.
2. **View a file:** “Show me what's in hello.rs.” The agent calls `replace_text` view; the TUI shows numbered lines.
3. **Edit an existing file:** “Change hello to world.” The agent calls `str_replace`; confirm locally that the exact edit took effect.
4. **Run a script:** “Use bash to run `rustc hello.rs && ./hello`.” The agent calls bash; the TUI displays `world`.
5. **Continuous multi-turn session:** do the steps above in one session to verify history continuity (in step 2 the agent knows `hello.rs` was created in step 1).
6. **Tools are visible:** the TUI conversation shows `⚙ replace_text(...)` / `⚙ bash(...)` and their streaming results.
7. **Eviction:** from another operation, dispose the llm service mid-run and verify that the driver is automatically evicted back to Pending.

### Automated tests (CI)

- **Unit:** bash (command execution / non-zero `[exit code: N]` / workdir / timeout / tail truncation); replace_text (view file with line numbers and list directory; create does not overwrite; str_replace exact match / multiple matches with line numbers / empty deletion / successful replacement).
- **Integration:** install both tools in `ToolsPlugin`; drive a multi-step “create file + edit file + run command” turn with a scripted backend; unloading llm evicts the driver.
- **Real backend** (`#[ignore]`): replay a recorded coding turn with MockReplayModel.

Minimal mode is complete only when all seven TUI end-to-end checks pass.
