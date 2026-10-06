# Minimal Agent Framework Design (Based on rutis + aimux)

> 2026-08-18. Goal: define the **minimal agent framework** built on rutis's five pillars—the required elements, their forms, which ones are plugins, and where the boundaries lie.
> Basis: [deepseek-harness architecture](../deepseek-harness/docs/architecture.md) (plugin decomposition), [aimux](../aimux/) `LanguageModel` / `CallOptions` / `Tool` (unified LLM access layer), and v5 of [design-rust-port.md](design-rust-port.en.md) (five pillars).
> Position: design basis for the M3 agent crate. **Define first, implement afterward.**

## 1. Core insights

1. **dsh separates service (resource) from loop (behavior).** `core/agent` provides the `Agent` interface; `core/agent-loop` provides the driver that implements it. The loop is a driver plugin that implements the interface, not a container holding a `run` method.
2. **aimux has already implemented the LLM seam.** 329 providers are unified behind one `dyn LanguageModel`, with `CallOptions` / `GenerateResult` / `FunctionTool`. **Consume it directly; do not invent another trait.**
3. **A session carries a continuous loop; it is not a persistence log.** Multi-turn history lives in the session. dsh needs event-log + surface-projection layers for persistence/compaction/replay; a minimal in-memory version **stores model-visible messages directly in one layer.**
4. **The framework event system can observe (emit) and intercept (waterfall) loop progress.** Key dsh turn-flow points are waterfalls: `agent/pre-step` (rewrite/reject messages), `agent/request` (replace model config), and three tool stages `tools/pre-execute` (gate), `execute` (run), and `post-execute` (decide results, including failures). Progress facts are broadcast as events, and the UI consumes those events. **The framework eats its own dog food:** the loop is not a black box; key points use waterfalls, incremental progress uses emit, and both observation and interception go through EventBus.

## 2. Element inventory and form decisions

The minimal agent has six elements. Plugins appear only **where resources need managing and a lifecycle needs to be attached** (Cordis paradigm: plugin = assembly unit).

| Element | Form | Plugin? | Reason |
|---|---|---|---|
| **LLM backend** | `Arc<dyn LanguageModel>` service | **Service**, provide directly | aimux already implements it. Make it a plugin only when there is real assembly logic (connections/credentials/cleanup); otherwise one line of `provide_as`. |
| **Tool set** | `ToolRegistry` service | **Yes, a plugin** | Unified registration, gating, hot replacement, and adding schemas to prompts. It is a real assembly unit. |
| **Session** | In-memory ordered message log | Not a plugin | Source of truth for the continuous loop; held by driver and released with the fiber. |
| **Agent loop** | Driver implementing `Agent` | **Yes, a plugin** | The loop itself; fiber owns its lifecycle. |
| **Event observation** | `agent/*` events (step/tool) | Attached to driver fiber | Listeners belong to driver fiber (D28). |
| **Stop/cancel** | Fiber `CancellationToken` | Not a plugin | D27; inside the driver, await `ctx.cancelled()`. |

**Two real plugins:** `ToolsPlugin` and `AgentDriverPlugin`. **One directly provided service:** LLM (aimux). **Do not write an empty shell:** `LlmPlugin`. **In-memory object:** session.

## 3. Element definitions

### 1. LLM service — provide directly

```rust
use aimux_core::LanguageModel;
pub fn llm_key() -> TypeKey { TypeKey::of::<dyn LanguageModel>() }

// Consumer:
ctx.provide_as(llm_key(), Arc::new(my_aimux_model))?;
```

### 2. Session — source of truth for the continuous loop (in-memory, one layer)

```rust
/// Carrier for continuous conversation: ordered messages. No persistence/replay/compaction.
pub struct Session {
    id: SessionId,
    messages: Vec<ModelMessage>,  // Store model-visible messages directly; no event → projection layers
}
impl Session {
    pub fn id(&self) -> SessionId;
    pub fn push(&mut self, msg: ModelMessage);       // user / assistant / tool result
    pub fn messages(&self) -> &[ModelMessage];        // read-only snapshot, prevents external mutation
}
```

dsh's `deriveMessages()` is just `messages()` here—no surface, generation, or deep freeze.

### 3. Agent trait — multi-turn, observable, cancellable

```rust
pub trait Agent: Send + Sync + 'static {
    fn id(&self) -> SessionId;                        // shares identity with the session
    fn status(&self) -> AgentStatus;                  // idle | running
    fn session(&self) -> &Session;                    // holds the session, the continuous-loop carrier
    /// Submit a user message: push into session and drive one turn.
    /// Returns the turn's terminal result; **incremental progress (text/tool/status) is broadcast
    /// through EventBus `agent/*` events**, not exclusively returned. Observers (TUI/logs/other
    /// frontends) subscribe to events; they do not call followup.
    fn followup<'a>(&'a self, input: &'a str) -> BoxFuture<'a, Result<String, AgentError>>;
    /// Interrupt the current turn but retain session history; the next followup continues it.
    fn cancel(&self);
}
```

**Turn progress uses EventBus, not an exclusive stream.** The driver emits each increment as an `agent/*` event:

```rust
pub struct AgentTextDelta { pub session: SessionId, pub step: usize, pub delta: String }   // impl Event
pub struct AgentToolCall  { pub session: SessionId, pub name: String, pub args: Value }  // impl Event
pub struct AgentToolResult{ pub session: SessionId, pub name: String, pub ok: bool, pub output: String }  // impl Event
pub struct AgentTurnEnd   { pub session: SessionId, pub result: Result<(), String> }     // impl Event (summary for non-Clone result)
```

Rationale (dsh pattern + framework consistency): **output is broadcast, not exclusive.** Multiple parties can observe one turn (TUI + logs + future frontend); observers may subscribe late and only watch. Listeners unload with the fiber (D28), and TUI is decoupled from the driver: it subscribes to `agent/*` events and does not call `followup` to get a stream. `followup` only “starts a turn + returns its terminal result”; events carry all progress.

**Omitted** (available in dsh but not in this minimal version): inbox/multi-boundary queuing, steer/inject (human in the loop), fork/resume (requires persistence), runMaintenance/whenIdle (maintenance scheduling), reset (create a new session/agent instead).

### 4. AgentDriver — the loop itself

```rust
pub struct AgentDriver {
    llm: Arc<dyn LanguageModel>,
    tools: Arc<ToolRegistry>,
    session: Mutex<Session>,       // sole source of truth; history spans turns
    status: AtomicUsize,           // AgentStatus
    cancel_token: CancellationToken,
    max_steps: usize,
}

impl Agent for AgentDriver {
    fn followup<'a>(&'a self, input: &'a str) -> BoxFuture<'a, Result<String, AgentError>> {
        Box::pin(async move {
            self.session.lock().unwrap().push(ModelMessage::user(input));
            self.set_status(AgentStatus::Running);
            let out = self.run_loop().await;          // see 4.1
            self.set_status(AgentStatus::Idle);
            out
        })
    }
    fn cancel(&self) { self.cancel_token.cancel(); }
    // id / status / session accessors omitted
}
```

#### 4.1 Loop (perceive → think → act → observe): waterfalls at key points, event-based output

The framework event system can **observe loop progress (`emit`) and intercept it (`waterfall`)**. This is the essence of the dsh architecture and the framework eating its own dog food. The driver does not hard-code a linear flow; it dispatches key points through waterfalls so plugins can add middleware to rewrite or veto behavior.

**Exact mapping to dsh waterfall points** (corrected after checking dsh source):

| dsh | Semantics | Minimal version |
|---|---|---|
| `agent/pre-step` | Rewrite/reject the **messages** entering this step (`next()` preserves them) | ✅ Included (message rewriting is a common extension point) |
| `agent/request` | Replace **frozen call config** (provider/model/maxTokens; **not messages**) | ⏸ M4 (when model routing is needed) |
| `tools/pre-execute` | Gate **before** tool execution: reject or allow (approval/permission) | ✅ Included |
| `tools/execute` | **Wrap execution itself:** timeout/retry/metrics; `next()` returns normalized result | ✅ Included |
| `tools/post-execute` | After execution: accept/replace/enrich/block result; **also runs on failure** (thrown errors arrive here for retry decisions) | ✅ Included |

```rust
impl Agent for AgentDriver {
    fn followup<'a>(&'a self, input: &'a str) -> BoxFuture<'a, Result<String, AgentError>> {
        Box::pin(async move {
            self.session.lock().unwrap().push(ModelMessage::user(input));
            self.set_status(AgentStatus::Running);
            let out = self.run_loop().await;
            self.set_status(AgentStatus::Idle);
            self.emit(AgentTurnEnd { session: self.id(), result: out.as_ref().map(|_|()).map_err(|e| e.to_string()) });
            out
        })
    }
    fn cancel(&self) { self.cancel_token.cancel(); }
}

async fn run_loop(&self) -> Result<String, AgentError> {
    for step in 0..self.max_steps {
        if self.cancel_token.is_cancelled() { return Err(AgentError::Stopped); }

        // ── agent/pre-step waterfall: rewrite/reject messages entering this step (default next = keep as-is) ──
        let prompt = convert_to_language_model_prompt(self.session.lock().unwrap().messages(), None);
        let tools = self.tools.schemas();
        let (prompt, tools) = self.ctx.events()
            .waterfall(&self.ctx, &AgentPreStep { prompt, tools, step }, next::identity()).await?;

        // Think: call aimux as a stream
        let mut result = self.llm.do_stream(&CallOptions { prompt, tools: Some(tools), ..Default::default() })
            .await.map_err(|e| AgentError::Llm(e.to_string()))?;

        // Observe: collect TextDelta chunks and broadcast agent/text-delta events (not an exclusive stream)
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        while let Some(part) = result.stream.next().await {
            match part {
                Ok(StreamPart::TextDelta { delta, .. }) => {
                    text.push_str(&delta);
                    self.emit(AgentTextDelta { session: self.id(), step, delta });  // broadcast event
                }
                Ok(StreamPart::ToolCall { tool_name, args, .. }) => calls.push(/* ... */),
                Ok(StreamPart::Finish { .. }) => break,
                Ok(StreamPart::Error { error }) => return Err(AgentError::Llm(error.to_string())),
                _ => {}
            }
        }
        self.session.lock().unwrap().push(assistant_message(text.clone(), &calls));
        if calls.is_empty() { return Ok(text); }  // final answer

        // ── Act: each tool call goes through three stages (pre-execute gate → execute → post-execute decision) ──
        for call in calls {
            self.emit(AgentToolCall { session: self.id(), name: call.tool_name.clone(), args: call.input.clone() });

            // ① tools/pre-execute: gate before running (reject or allow; default next = allow)
            let decision = self.ctx.events()
                .waterfall(&self.ctx, &ToolPreExecute { call: call.clone() }, next::allow()).await?;
            if decision.is_reject() { /* reject: result = reason; skip execution */ }

            // ② tools/execute: wrap execution (timeout/retry/metrics; default next = local execution; panic-safe inside)
            let out = self.ctx.events()
                .waterfall(&self.ctx, &ToolExecute { call: call.clone() },
                    next::run(|c| self.tools.execute(&c.call, &self.cancel_token))).await?;

            // ③ tools/post-execute: decide result (accept/replace/retry; failure also arrives; default next = unchanged)
            let out = self.ctx.events()
                .waterfall(&self.ctx, &ToolPostExecute { call: call.clone(), result: out }, next::accept()).await?;

            self.emit(AgentToolResult { session: self.id(), name: call.tool_name.clone(), ok: out.ok, output: out.output.clone() });
            self.session.lock().unwrap().push(tool_result_message(&call, out));
        }
    }
    Err(AgentError::MaxSteps(self.max_steps))
}
```

**Three key design choices:**

1. **Waterfalls at key points (matching dsh pipeline):** `agent/pre-step` (rewrite/reject messages) plus the three tool stages `pre-execute` (gate), `execute` (run), `post-execute` (decide result, including failure). The default `next` is the original behavior; plugins attach `on_waterfall` middleware to wrap it and can rewrite or veto (do not call next). This is a proper use of EventBus's fourth dispatch mode in the loop; the framework eats its own dog food.
2. **Emit progress increments as `agent/*` events:** broadcast `text-delta` / `tool-call` / `tool-result` / `turn-end` to any observer. There is no exclusive stream.
3. **Session remains the sole source of truth:** emit each increment to observers and accumulate it back into the session; multiple `followup` turns are naturally continuous. **Use `do_stream`** (not `do_generate`) for the streaming path.

### 5. ToolsPlugin / AgentDriverPlugin — two real plugins

```rust
impl Plugin for AgentDriverPlugin {
    fn name(&self) -> &str { "agent-driver" }
    fn injects(&self) -> &[TypeKey] { &[llm_key(), tools_key()] }   // dual gate
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            let llm = ctx.get_as::<dyn LanguageModel>(llm_key())
                .ok_or_else(|| CordisError::InjectUnsatisfied(vec!["llm".into()]))?;
            let tools = ctx.get_as::<ToolRegistry>(tools_key())
                .ok_or_else(|| CordisError::InjectUnsatisfied(vec!["tools".into()]))?;
            let driver = Arc::new(AgentDriver::new(llm, tools, ctx.clone(), self.max_steps));
            ctx.provide_as(agent_key(), driver)?;
            // Fiber unload → cancel (ctx.cancelled() inside driver cascades shutdown)
            Ok(Effect::Done)
        })
    }
}
```

`ToolsPlugin` follows the same pattern; `apply` provides `ToolRegistry` (schemas produce aimux `Tool` values, runner failures are fed back, and panics are contained at the task boundary).

## 4. Minimal demo

```rust
let root = Ctx::root()?;
root.provide_as(llm_key(), Arc::new(aimux_model))?;        // LLM service; no empty plugin wrapper
root.plugin(ToolsPlugin::new(vec![weather_tool]));
let agent_view = root.plugin(AgentDriverPlugin::new(16));
(&agent_view).await.expect("driver loads (gated on llm+tools)");

// Observer (TUI/logs): subscribe to agent/* events; do not call followup
root.events().on(&root, |_, e: &AgentTextDelta| /* render incrementally */);
root.events().on(&root, |_, e: &AgentToolCall|   /* display ⚙ tool */);

let agent = root.get_as::<dyn Agent>(agent_key()).unwrap();
let a1 = agent.followup("weather in Oslo?").await?;         // first turn (terminal result)
let a2 = agent.followup("and in Bergen?").await?;           // second turn; continuous history

// Dependency-driven reload: unloading the llm service automatically evicts the driver; no manual agent_view dispose
```

**The TUI is an `agent/*` event listener** (owned by its own fiber, D28). It renders `AgentTextDelta` / `AgentToolCall` / `AgentToolResult`; input triggers a turn through `agent.followup`. **Progress increments use events, decoupling TUI from the driver**—late subscriptions, multiple observers, and read-only observers all work.

## 5. Comparison with dsh

**Core alignment:** service/loop separation, Agent interface, tool registry, event observation, and cancellation path all match. **The agent model is intentionally scoped down:** dsh's Agent is a long-lived entity with inbox/status/session logs/steer (supporting multi-turn sessions, human-in-the-loop, persistence, and UI). We retain its multi-turn kernel (id/status/session/followup/cancel) and remove persistence and human scheduling.

| dsh | Minimal version | Why omitted |
|---|---|---|
| Session = event log + surface projection (two layers) | Store messages directly in one layer | Two layers serve persistence/compaction/replay; in-memory continuous loops do not need them. |
| Append-only + deep freeze + cache | `Vec` push + read-only snapshot | `&[ModelMessage]` is enough to prevent external mutation. |
| Compaction / persistence seam / session events | Omitted | Beyond the minimum; if persistence is needed later, messages can become an event log. |
| Inbox / steer / inject / fork / resume / runMaintenance | Omitted | Human scheduling and persistence are beyond the minimum. |

## 6. Changes required in the current implementation

| Current | Change to | Reason |
|---|---|---|
| Empty `LlmPlugin` wrapper | Remove; call `ctx.provide_as` directly | No assembly logic. |
| Invented `LlmService` trait | Consume `aimux::LanguageModel` | Seam already exists with 329 providers. |
| `AgentLoopPlugin` + public `run` | `AgentDriverPlugin` implements `Agent`; `followup` triggers a turn | dsh pattern; the loop is internal driver behavior. |
| `Agent` permanently owns `messages` / `steps` / `stopped` | Session owns history; turn state is independent | Fixes sol #12; session carries continuous loop state. |
| `Agent::inert` | Remove | Odd shape; dependency gate already guarantees llm availability. |
| Invented `ToolSpec` | aimux `FunctionTool` + runner | Schema matches and goes directly into `CallOptions`. |
| No session (single turn) | Add in-memory session | Source of truth for continuous loops. |
| Demo manually disposes in reverse order | Dispose only llm and demonstrate automatic eviction | Dependency-driven reload is a differentiating capability. |

## 7. One-sentence definition

**The minimal agent framework = one aimux `LanguageModel` service + one `ToolRegistry` plugin + one driver plugin implementing `Agent` + one in-memory session (source of truth for a continuous loop).** The loop lives inside `AgentDriver`: perceive (`session.messages()`) → think (`llm.do_stream`, with a pre-call `agent/request` waterfall available for rewriting/interception) → act (`tools.execute`, intercept/replace through waterfalls) → observe (emit incremental `agent/*` broadcasts and write back to session), checking cancellation at each step. **Key loop points use waterfalls; progress increments use events**—the framework eats its own dog food, so the loop can be observed (emit) and intercepted (waterfall). Session history enables multiple continuous turns; fiber owns the driver lifecycle, and `ctx.cancelled()` stops it. Plugins appear only where a resource needs ownership and a lifecycle.
