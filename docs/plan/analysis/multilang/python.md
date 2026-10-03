# A Python runner for rutis-interop: design research

Date: 2026-10-03. Scope: what an analogue of `interop/node`'s runner needs in order to run real
min_cordis (Python) plugins in a Python process mounted into rutis, and how feasible it is.
Nothing in the rutis repo was modified. Experiments live in
`scratchpad/experiments/python/` (listed in the appendix).

Path abbreviations used below:

- `R/` = the rutis worktree `/Users/eric8810/Code/rutis/.claude/worktrees/multilingual-expansion-research-3f78c4`
- `MC/` = the min-cordis clone `scratchpad/min-cordis` (Python core in `MC/python/min_cordis`)
- `C404/` = the installed Cordis 4.0.4 that interop/node pins (`/Users/eric8810/Code/rutis/interop/node/node_modules/@deepseek-ai/cordis`)
- `X/` = `scratchpad/experiments/python`

---

## 0. Summary and recommendation

**Feasible, with moderate effort, and the protocol does not need to change.** Every Cordis hook the Node
runner relies on has a working equivalent in min_cordis Python. I checked this against the real library
(`X/experiment_runner_core.py`): exporter fibers gated by `inject` plus `_check`, `internal/service`,
`internal/set` after `next()`, identity through traceable views (`_original`), detecting unresolved
dependencies through `fiber.store`, host proxies provided before plugins load, event forwarding, and
disposal order. Node's synchronous-wait design also ports directly: the loop thread blocks and pumps an
inbox fed by an I/O thread. A prototype peer (`X/pypeer.py`) passed these cases against a scripted
fake-Rust peer:

- nested synchronous callback chains
- `SyncWaitCycle` detection
- cancellation through `Task.cancel()`
- deferral of unrelated calls
- calls from plugin worker threads
- roughly 25–60 µs per synchronous round trip (Python on both ends)

The work that is new compared with Node is mostly not in the runner loop:

1. **Type extraction.** Python has no `declare module 'cordis' { interface Context }` and no `interface Events`.
   Authors need small, zero-dependency conventions: a typing-only `Context` subclass, a seam class that declares
   `provide`, and an `EVENTS` table. A generator that imports the plugin modules in the mount's venv extracts a
   language-neutral IR. Rust is rendered from that IR, ideally by a renderer shared with TypeScript.
2. **Nominal data materialization.** TypeScript interfaces are structural, so the Node runner passes plain objects
   through. Python dataclass, Enum, NewType and pydantic values must be rebuilt from wire dicts using type hints.
   This applies to arguments, config, host results and callback arguments.
3. **Cancellation semantics.** Python cancels through `Task.cancel()`, which is pervasive and forced. JS uses an
   opt-in, cooperative `AbortSignal`.
4. **Deployment of virtualenvs.** A venv is not relocatable.
5. **min_cordis maturity.** It is not on PyPI, its version is 0.1.0, it fails on 3.11 despite claiming `>=3.11`,
   and it lags the TypeScript core's EffectRecord disposal contract.

Recommendation:

- Build `rutis-interop` for Python as a pure-Python PyPI package that is a dependency of a per-app uv project.
  Use a reader thread, keep all protocol state on the asyncio loop thread, and start runner-owned tasks eagerly.
- Require Python 3.12 or later.
- Use runtime introspection for bindings, with a static `ast` pre-pass for literal discovery.
- Factor the Rust emission in `generate.mjs` into a shared IR → Rust renderer in `rutis_interop::build`.

The largest strategic question is not technical. No published min_cordis Python plugins exist, so "mount real
plugins without changing their source" has no corpus yet. The design can, and should, specify Python plugin
conventions up front.

---

## 1. min_cordis Python: the author-facing API, concurrency, cleanup, errors, maturity

### 1.1 API surface

| Concept | Python API (file:line) | Notes vs TS/Cordis 4.0.4 |
| --- | --- | --- |
| Context | `Context(on_error=None)` `MC/python/min_cordis/_context.py:357-395` | The root installs `events`, `reflect`, `registry`, the root `fiber` (uid 0, immortal: `_disposables.clear()` at :395) and `logger`. |
| Plugin shapes | `Registry._normalize` `_registry.py:73-83` | (a) dict `{"apply","name","Config","inject"}`; (b) a class with an `apply` attribute; (c) any callable: a function `apply(ctx, config)`, or a class constructed as `Cls(ctx, config)`, such as a `Service` subclass. **A module is not a plugin.** The TS runner passes a module namespace (`runner.mjs:159`), so a Python runner must adapt `module:attr` entries itself. |
| Load a plugin | `ctx.plugin(plugin, config=None)` → `_FiberView` `_context.py:619-626`, `_registry.py:85-115` | The view is awaitable (`__await__` → `fiber.await_fiber()`, `_registry.py:42-43`). It delegates to the real fiber, which is audit fix C1. |
| inject | function attribute `fn.inject = [...]`, class attribute `inject: ClassVar = [...]`, dict key `"inject"`, or the `@Inject("name", config)` decorator `_service.py:166-201` | A list or a `name → config` map; a config becomes an intercept entry (`_fiber.py:126-130`). **All injects are required**, the same as 4.0.4 (`C404/src/registry.ts:296`). `ctx.inject(deps, cb)` is renamed **`ctx.inject_plugins`** (`_context.py:628-630`). |
| provide | `ctx.provide(name, value=None, check=None)` `_context.py:607-608` → `ReflectService.provide` :71-117 | It is an effect on the calling fiber. Its disposer is async: it returns a coroutine. |
| Service base | `class Service` `_service.py:86-163` | `super().__init__(ctx, name=None)`. The name comes from the `provide` ClassVar, or else **the class name** (`_service.py:100-101`). TS requires `name ?? constructor.provide` (`C404/src/service.ts:42-43`). Hooks: `_init` (awaited before ACTIVE), `_check` (the `Service.check` predicate), `_invoke` (callable service), `_extend`, `Config.merge`. |
| @Inject | `Inject(name, config)` on classes or methods `_service.py:166-201` | The method form delays the call until the dependencies exist (`collect_inject_hooks` :48-83). |
| Effects | `ctx.effect(execute, label)` → `Fiber.effect` `_fiber.py:191-307` | Accepts a disposer, a list of disposers, a sync or async generator, an async callable, or a coroutine. A setup barrier makes a dispose that arrives before setup finishes wait for it. The returned disposer returns a coroutine. |
| Plugin body | `_fiber.py:401-443` | The value returned by `apply` is collected like an effect: a disposer, a list, or a (async) generator. |
| Events | `ctx.emit` (sync), `await ctx.parallel`, `await ctx.serial`, `ctx.bail` (sync), `ctx.waterfall` (sync) `_events.py:44-123` | `emit` schedules coroutine results through `ensure_future` and routes failures to `on_error` (:50-55). `parallel` raises an `ExceptionGroup` (:73-79). `serial` and `bail` stop at the first value that is neither `None` nor `False`. `waterfall` uses the last argument as the innermost `next`. |
| Listener registration | `ctx.on(name, fn, options)`, `ctx.once` `_events.py:127-191` | `on` is an effect on the owning fiber. Its disposer must be awaited (documented gotcha, `MC/docs/design-python-traceable.md:116`). |
| Fiber states | `FiberState` `_fiber.py:38-44` | PENDING, LOADING, ACTIVE, FAILED, DISPOSED, UNLOADING. These are the six states. |
| Lifecycle | `fiber.dispose()`, `restart()`, `update(config)` `_fiber.py:513-556` | `update` validates before it stores (audit fix). |
| isolate / intercept | `ctx.isolate(name, label=None)`, `ctx.intercept(name, config)` `_context.py:429-441` | Labels are objects, and the store is keyed by label identity (C4). |
| accessor / mixin | `ctx.accessor(name, {"get","set"})`, `ctx.mixin(source, members)` `_context.py:178-228` | String sources behave like TS. Object sources reproduce the upstream quirk. |
| internal hooks | `internal/get` waterfall `(ctx, name, error, next)` `_context.py:486-495`; `internal/set` waterfall `(ctx, name, value, error, next)` :592-597; `internal/service (name, value)` :159-164; `internal/status`, `internal/plugin`, `internal/config`, `internal/update`, `internal/dispatch` | The signatures match 4.0.4 (`C404/src/events.ts:331-351`). `internal/service` is emitted without the 4.0.4 filter `this` (`C404/src/reflect.ts:331-334`). |
| Traceable views | `get_traceable` / `_TraceableView` `_traceable.py:76-91, 197-320` | `ctx.foo` returns a **new view per read**. `view._original` is the raw target (:238-241). `isinstance` works through a `__class__` property (:209-211). `==` and `hash` delegate to the target (:144-151). |
| Logger | `ctx.logger('name')` `_logger.py:60-100` | The minimal logger. **info and debug go to stdout** (:40-42), so protocol frames must not share stdout. |
| Config validation | `Fiber._resolve_config` → `resolve_config(runtime, config)` `_fiber.py:61-74, 480-482` | `Config` is a **callable transform**. It returns the normalized value, an `Exception`, or `(ok, value)`. 4.0.4 uses Standard Schema `Config['~standard'].validate` (`C404/src/fiber.ts:50-53`). Python has no Standard Schema equivalent. |

### 1.2 Concurrency model

- **asyncio only and loop-affine.** Construction and dependency transitions call `asyncio.ensure_future`
  (`_fiber.py:146, 371, 373, 396, 498`). So `ctx.plugin()`, `provide()` and every state transition must run
  on the thread that runs the loop. I probed this in `X/loop_probe.py`:
  - From a worker thread, `ctx.plugin(...)` raises `RuntimeError: There is no current event loop in thread …`.
    The half-registered fiber is left with a dangling `_reload` coroutine.
  - Outside a running loop, it silently schedules on a non-running loop. A DeprecationWarning is emitted and the
    plugin never runs.
  - Consequence: the runner owns one loop thread, and *every* min_cordis call must be marshalled to it, exactly
    like Node's single JS thread.
- `apply`, `_init`, effects and disposers can be sync or async. A checkpoint `await asyncio.sleep(0)` in
  `_reload` (`_fiber.py:382`) stands in for the JS microtask boundary. Its documented timing caveat: the Python
  `ensure_future` chains need extra loop ticks in some tests (`design-python-traceable.md:89`).
- There are no locks in min_cordis. It is safe only under a single thread.

### 1.3 Cleanup and dispose

- LIFO effect ledger per fiber (`DisposableList`, `_utils.py:40-75`). A child fiber's disposal is an effect on
  its *parent's* ledger (F1 fix, `_fiber.py:134-159`), so unloading a parent tears down the whole subtree.
- `_unload` runs every disposer and awaits coroutines. Each failure is contained through `on_error`
  (`_fiber.py:484-501`). There is no aggregate error. This is the "old chain": HANDOFF item 4
  (`MC/HANDOFF.md:87`) records that the TS core moved to the EffectRecord contract (every cleanup runs, failures
  are combined in a single AggregateError, disposal-task identity is preserved) while Python did not.
- Disposers are coroutines. Calling one without awaiting it does nothing and produces a "never awaited" warning.

### 1.4 Error handling

- Errors thrown while loading a plugin set `FAILED`, go to `on_error`, and re-raise from `await view`
  (`_fiber.py:386-390, 503-508`).
- Errors from listeners:
  - Synchronous `emit` listener errors propagate (E1 fix).
  - Async `emit` failures go to `on_error`.
  - `parallel` raises `ExceptionGroup("parallel dispatch failed", errors)`.
- `AggregateError` corresponds to `ExceptionGroup`, and `cause` corresponds to `__cause__`. The translation map is
  in `MC/python/README.md:9-23`. `composeError` stack splicing is intentionally not ported.

### 1.5 Maturity and parity vs Cordis 4.0.4

**Lineage.** `@deepseek-ai/cordis` 4.0.4 *is* the vendored deepseek-harness Cordis that min-cordis was trimmed from.
The TS sources are nearly identical:

- context, reflect, service and registry differ by only 6–30 lines.
- fiber.ts differs by about 466 lines, because min-cordis TS has the EffectRecord contract.
- logger.ts differs because 4.0.4 has the full logger.
- The diff was run in this session between `MC/src/*.ts` and `C404/src/*.ts`.

The Python port mirrors the min-cordis TS core at an earlier fiber stage.

**Differences that matter to a runner or to plugin authors** (Python vs 4.0.4):

| Area | Python min_cordis | Cordis 4.0.4 |
| --- | --- | --- |
| Config | callable transform (`resolve_config`) | Standard Schema validator (+ `Plugin.Transform`) |
| Attribute reads | `ctx.foo` requires an inject on the ancestor chain **even on root** (`_context.py:475-484`); `ctx.get` is the escape hatch | root reads are lenient |
| Root property write | `root.foo = x` is a **plain instance attribute** (`_context.py:588-590`): it bypasses `reflect.set`/`internal/set` and *shadows* the service on root, because `__getattr__` only fires when lookup fails | root writes of a declared service go through `internal/set` (`C404/src/reflect.ts:173-194`) |
| `inject(deps, cb)` | `inject_plugins` | `inject` |
| Service name default | class name | required / `provide` |
| Effect disposal | old chain, errors contained one by one | intermediate `disposeAfter` / `finalizeDisposal` design |
| `internal/service` | emitted on the root bus, no scope filter | emitted with a filter `this` |
| Loader/include/HMR/schemastery/full logger | absent (TS min-cordis also removed them) | loader and include are optional peer dependencies |
| Callable services | `_invoke` / `__call__` | `symbols.invoke` |
| Stack splicing (`composeError`) | not ported | present |

**Tests.**

- `uv run pytest -q`: **96 passed** on CPython 3.12.2 and 3.13.6. The 96 are 85 core tests and 11 example tests.
  The suite also passes with `-W error::RuntimeWarning -W error::DeprecationWarning`.
- **On 3.11.11, one test fails**: `tests/test_parity.py::test_f4_generator_effect_and_plugin_body`, with
  `TypeError: object generator can't be used in 'await' expression` at `_fiber.py:435`. On 3.11,
  `asyncio.iscoroutine(<generator>)` is `True` (`X/eager_probe.py` session output; fixed in 3.12).
- So **`requires-python = ">=3.11"` (`MC/python/pyproject.toml:7`) is wrong**: generator plugin bodies break on
  3.11. The effective floor is 3.12.
- Coverage is the upstream-spec-derived suite (fiber, events, isolate, service, traceable, inject, logger, hooks).
  Per HANDOFF, reentrant lifecycle adversarial tests exist only in TS (27 tests). Python has its own 14 lifecycle
  tests, but not the full reentrancy suite.

**Publication.** Neither `min-cordis` nor `min_cordis` is on PyPI: `https://pypi.org/pypi/min-cordis/json` returns
404. `cordis` on PyPI is an unrelated 0.0.0 placeholder, and `rutis-interop` is free. The package has zero runtime
dependencies and builds with hatchling. Note that `MC/python/uv.lock` points at a mirror
(`mirrors.aliyun.com`), which matters for anyone reproducing a lock.

**Ecosystem.** Unlike the Node case, where published dsh plugins are mounted as they are, **there is no corpus of
existing min_cordis Python plugins**. The only "real" one is the reference application
`MC/python/examples/agent_loop.py`, which is `LLMClient` + `AgentLoop` with `inject = ["llm"]`.

---

## 2. What the Node runner relies on, mapped to min_cordis Python

Each row below was exercised against the real library in `X/experiment_runner_core.py`; the output is reproduced
in the appendix.

| Node runner reliance (file:line) | Purpose | min_cordis Python equivalent | Status |
| --- | --- | --- | --- |
| Resolve the plugin's own Cordis (`createRequire(entry).resolve('@deepseek-ai/cordis')`, `R/interop/node/src/runner.mjs:9-13, 150-153`) | Service classes and Context must come from one module instance | A venv has one `site-packages`, so `import min_cordis` is a single module object. The runner must live in the **same venv** as the plugins. The check becomes "the runner and the plugins share an interpreter", plus a version range on `min-cordis`. | simpler |
| `import(pathToFileURL(entry))`; `module.apply` or `module.default` (`runner.mjs:155-160`) | Load a plugin entry | `importlib.import_module(mod)` + `getattr(mod, attr)` for `module:attr`; entry points (`importlib.metadata.entry_points(group=…)`); `spec_from_file_location` for path mounts. Wrap a module that has an `apply` function into a dict plugin (`{"apply", "inject", "name", "Config"}`), because `_normalize` rejects modules. | adapt |
| `ctx.plugin(plugin, config)`; `fiber.await()` (`runner.mjs:160, 163`) | Load and wait | `ctx.plugin(...)` → `_FiberView`; `await view` / `view.await_fiber()` (`_fiber.py:503-511`) | ✓ |
| Exporter fibers `ctx.plugin({name, inject:[name], apply(scope){…}})` (`runner.mjs:39-53`) | Read the service like a native consumer; gating through the provider state and `Service.check()`; effects created by service methods belong to the exporter | A dict plugin `{"name", "inject": [name], "apply": fn}` is supported (`_registry.py:74-75`). `_check` gating goes through `Fiber._check_impl` (`_fiber.py:338-349`). Experiment: `watch()` called `self.ctx.effect(...)` and the effect was owned by `interop-export:counter`. It was disposed when `_check` turned false (`watch:dispose`). | ✓ |
| `scope.get(name)` (`runner.mjs:55-58`) | Read the slot through the exporter scope | `scope.get(name)` (`_context.py:601-602`) → traceable view bound to the scope | ✓ |
| `Symbol.for('cordis.original')` (`runner.mjs:30-33`) | Unwrap per-read proxies to compare identity | `view._original` (`_traceable.py:238-241`), or the internal `_traceable._unwrap` (:69-73). `_utils.ORIGINAL = sym("cordis.original")` (`_utils.py:33`) exists but views do **not** use it. The real key is the underscore name `_original`. Experiment: two reads are different objects, compare `==`, have the same `_original`, and pass `isinstance(view, Counter)`. | ✓ (not public; ask upstream for `min_cordis.original(value)`) |
| `ctx.on('internal/service', …)` (`runner.mjs:97`) | provide, withdrawal, provider (in)activation | Emitted in `ReflectService.notify` (`_context.py:159-164`) with listener arguments `(name, value)`. Experiment: notifications on provide, withdraw and `_check` flips. | ✓ |
| `ctx.on('internal/set', (ctx,name,value,error,next) => { r = next(); refresh(); return r })` (`runner.mjs:98-102`) | Property-assignment swaps | Same waterfall and signature (`_context.py:592-597`, `_events.py:106-123`). Experiment: `c.store = make(3)` → new handle `store#3`. **Gap:** a root-context `root.foo = x` bypasses it (`_context.py:588-590`). Plugins write through plugin contexts, so this only matters to code running on root. | ✓ (root caveat) |
| Re-read after every call, including after a throw (`runner.mjs:221-229`) | Direct `ctx.set` emits nothing (boundary rule 1) | Identical: `ctx.set` notifies nothing (`_context.py:125-133`). Experiment: `swap` became visible after the call. | ✓ |
| Unresolved deps: `fiber.store ? [] : fiber.inject…`, `ctx.get(name,false)` (`runner.mjs:166-169`) | Fail the mount with the names | `fiber.store is None` until active (`_fiber.py:113, 379, 492`); `fiber.inject` dict; `ctx.get(n, False)`. Experiment: `[('needs_missing', ['missing'])]`. | ✓ |
| Host proxies `ctx.provide(name, hostProxy(...))` before plugins (`runner.mjs:117-131, 147`) | rutis-provided services resolve natively | `root.provide(name, proxy)`. Experiment: `needs_llm` (inject `llm`) activated against a proxy. Python can do better than JS here: a proxy class can *subclass or register with* the seam class, so `isinstance` holds (relaxing boundary rule 7). Every public method must be overridden so that seam code never runs on an uninitialized instance. | ✓+ |
| Event forwarding `ctx.on(name, (...v) => peer.callAsync(...))` (`runner.mjs:176-182`) | Cordis → rutis notifications | The listener should send immediately and return a **coroutine** wrapping the reply future. `emit` schedules coroutines (`_events.py:53-55`). `parallel` and `serial` await them only if `asyncio.iscoroutine(result)` (`_events.py:81-86, 91-93`): **a plain Future would not be awaited by `parallel`.** Experiment: emit was fire-and-forget and parallel waited. | ✓ (coroutine, not Future) |
| rutis → Cordis `ctx.parallel(name, ...values)` (`runner.mjs:195-200`) | Emit into Cordis | `await ctx.parallel(name, *values)`. Errors become an `ExceptionGroup`, which is encoded with `errors`. | ✓ |
| Dispose: exporters first, then plugins in reverse (`runner.mjs:106-111`) | Unwinding order | `await view.dispose()` in the same order. Experiment: slots were withdrawn, then the effects disposed. | ✓ |
| `fiber.dispose()` + `peer.drain()` concurrently (`runner.mjs:192-194`) | Cleanup first, drain second | `asyncio.gather(dispose(), peer.drain())` | ✓ |
| `AbortController` / `AbortSignal` (`peer.mjs:63-66, 133-139, 235-240`) | Cancel an incoming call | **No equivalent type** in min_cordis or the stdlib. Mapping: `cancel` → `task.cancel()` on the *runner-owned* task (§3.3). `WireValue::Signal` (`R/crates/rutis-interop/src/protocol.rs:30-32`) is unused for Python. | design change |
| Promise detection `value instanceof Promise` (`peer.mjs:98, 296-303`) | Future references; settled hooks | `asyncio.iscoroutine` (wrap in a runner-owned Task), `asyncio.isfuture`, and `inspect.isawaitable` (objects with `__await__`). Views are never awaitable, because dunders are resolved on the type. | ✓ |
| `isLive` / `holdsReference` / `checkData` (`peer.mjs:9-40`) | Data vs live classification | §4: a Python value-type list (dataclass, Enum, TypedDict-as-dict, NamedTuple, pydantic model) is data; other instances are live; functions, bound methods and partials are functions; awaitables are futures. | design |
| `AsyncLocalStorage` call path (`peer.mjs:61, 83, 278`) | Route nested calls to the sync waiter | `contextvars.ContextVar` (tasks copy the context on creation). `asyncio.to_thread` propagates it; `run_in_executor` does not. | ✓ |
| `WeakRef` / `FinalizationRegistry` imports (`peer.mjs:73, 150-164`) | Release Rust refs on GC | `weakref.ref` + `weakref.finalize`. CPython refcounting makes the release **deterministic** (experiment: 3–5 releases seen by Rust right after the calls). Finalizers can run on any thread, so they should only enqueue work. | ✓+ |
| `queueMicrotask` (`peer.mjs:206, 263`) | Defer unrelated jobs | `loop.call_soon` | ✓ |
| Worker + `Atomics.wait` + `receiveMessageOnPort` (`R/interop/node/src/client.mjs:19-29`, `io-worker.mjs`) | Block the JS thread and pump frames | A reader thread + `threading.Condition` inbox; the loop thread blocks in `cv.wait()` (GIL released) and drains. Proven in `X/pypeer.py`. | ✓ |
| Uncaught exception kills Node (boundary rule 8) | Failure model | asyncio *logs* unhandled task exceptions and keeps going; min_cordis routes listener failures to `on_error`. A policy decision is needed (§8). | differs |

**Net:** apart from the missing `AbortSignal` type, nothing the Node runner uses is missing. The remaining gaps are
API spelling (`_original`, `inject_plugins`, modules not being plugins) and the root-write quirk. The Python runner
can be a near line-by-line port of `runner.mjs` and `peer.mjs`.

---

## 3. Sync vs async

### 3.1 The contract to preserve (`R/docs/design-protocol-plugin-mount.md:97-103`, requirements §3 "方法形状")

- A synchronous method stays synchronous: the caller blocks, and reverse calls in the same chain (selected by
  `path`) run on the waiting thread.
- An async method returns a future reference that the caller `await`s separately.
- Awaiting something that only the blocked executor could advance returns `SyncWaitCycle`.
- Dropping the Rust future sends `cancel`.

### 3.2 Mapping onto Python, as prototyped

```text
main thread: asyncio loop (all min_cordis + all protocol tables + all plugin code)
reader thread: blocking readline() on the socket → inbox (deque + Condition)
               → wakes the loop (call_soon_threadsafe, coalesced) and any sync waiter
writes: json.dumps(..., allow_nan=False) + sendall under a lock (any thread)
```

- **Idle loop:** a frame becomes a job; `call_soon(_run, job)` runs it; the dispatch result is either a value or a
  coroutine, which becomes an owned `Task` and is exported as a future reference.
- **Synchronous request from Python** (calling a Rust callback, a sync host method, or a remote function):
  1. Push the call id onto `waiting`.
  2. Run any queued jobs whose `path` contains a waiting id.
  3. Loop: `cv.wait()`, drain the inbox, and handle each frame, executing related invocations *inline on the loop
     thread*.
  4. Unrelated invocations stay queued. When the outermost wait ends, they are rescheduled with `call_soon`.

  This is `peer.mjs#requestSync`/`#run` (`peer.mjs:196-210, 266-271`) unchanged.
- **`await` frame for a Python future:** if the future is done, reply at once. If the loop thread is inside a
  synchronous wait, reply `SyncWaitCycle`. Otherwise reply from `add_done_callback`. This mirrors
  `peer.mjs:279-284`.
- The Rust side is unchanged. Its own `SyncWaitCycle` detection for `current_thread` executors
  (`R/crates/rutis-interop/src/rpc.rs:238-241, 324-357, 800-839`) still applies when Python makes a synchronous
  call into a Rust host whose work needs the blocked runtime.

**Experiment results** (`X/experiment_sync.py`, CPython 3.12.2, AF_UNIX socketpair, fake Rust written in Python):

| Scenario | eager start | lazy tasks |
| --- | --- | --- |
| Rust → Py `compute(cb)`, where `cb` (Rust) → Py `double` nested | `70`; `double` ran on the loop thread at depth 1 | same |
| Rust callback awaits a *pending* Python future inside the sync chain | `SyncWaitCycle` | same |
| The same future awaited after the chain | `slept` | same |
| Rust callback invokes `async def quick(): return 42` and awaits it inside the chain | **`42`** | **`SyncWaitCycle`** |
| `cancel` of an awaited `slow(5)` | task cancelled, `finally` ran, reply `CancelledError` | same |
| An unrelated `invoke` sent while Python is blocked in a sync wait | deferred (`sleeper_wait:end` before `double:21`) | same |
| A Rust function called from `run_in_executor` / `to_thread` worker threads | `[500, 600]` (marshalled to the loop) | same |
| Rust → Py sync invoke | 49 µs/call | 59 µs/call |
| Py → Rust sync call | 26 µs/call | 26 µs/call |

Latencies include the Python fake peer's own overhead, so a real Rust peer will be faster. There is no published
Node baseline to compare against; `R/interop/node/bench/sync-call.mjs` prints p50/p95 but nothing is recorded in
the repo.

### 3.3 Hard problems (Python-specific unless noted)

1. **Blocking the loop.** A synchronous chain freezes every asyncio task, timer and socket of the plugin process
   (same as Node). This is boundary rule 6 again. asyncio's debug mode will report slow callbacks. It is a
   correctness non-issue but a latency hazard.
2. **Lazy coroutines.** A Python coroutine does not run until it is scheduled; a JS async function runs its
   synchronous prefix immediately (requirements §4 already says this is "not guaranteed"). Python 3.12 added
   `asyncio.Task(coro, loop=loop, eager_start=True)` (probed: it works on 3.12 and 3.13, `X/eager_probe.py`).
   - Use it **only for tasks the runner creates from returned coroutines**. That makes `async def` methods behave
     like JS: the prefix runs before the reply, and coroutines that never suspend are already done. The table
     above shows this removes a whole class of false `SyncWaitCycle`.
   - Do **not** install `eager_task_factory` globally. min_cordis's internal `ensure_future` chains and their
     `sleep(0)` checkpoint ordering assume lazy tasks.
3. **Cancellation semantics.**
   - `AbortSignal` is opt-in and cooperative: only methods that declare it can observe cancellation.
     `Task.cancel()` injects `CancelledError` at the next `await` of *any* async method. That is idiomatic in
     Python, since `asyncio.timeout` and `wait_for` work the same way, but it is stronger than the TS behaviour.
   - Rules:
     - cancel only **runner-owned** tasks, created from a coroutine the call returned.
     - If a method returned an existing `Future`/`Task` (possibly shared, such as a cache), only stop awaiting.
       Never cancel a future the call did not create.
     - A coroutine that swallows `CancelledError` completes normally. Its late reply is an orphan, which the Rust
       side already counts (`Connection::orphans`).
     - Work offloaded to threads (`to_thread`) is not interrupted, the same as in Python itself.
     - Sync methods cannot be cancelled (same as TS).
   - The generator should document each `async fn` as "dropping the future cancels the Python task" instead of
     relying on a signal parameter.
   - Python can additionally **send** `cancel`, which Node never does. When a plugin task that awaits a Rust future
     is cancelled, the runner can send `cancel {id}`. Rust drops its `Awaiting` entry (`rpc.rs:1097-1106`), and the
     proxy release then aborts the Rust task (`AsyncResult::drop`, `rpc.rs:300-306`). The prototype wires this
     (`pypeer.py` `_request_async`).
4. **Reentrancy plus locks.** Nested callbacks run on the loop thread while an outer plugin frame is suspended
   mid-call (same as Node). Python adds `threading.Lock`: if a plugin holds a non-reentrant lock around a call into
   Rust, and Rust calls back into code that takes the same lock, the thread deadlocks on itself. JS has no such
   primitive. This needs a boundary rule ("do not hold a `threading.Lock` across a call into rutis") or the use of
   `RLock`.
5. **Foreign threads.**
   - Plugins and libraries use threads (`to_thread`, executors, client libraries with background threads).
   - A Rust proxy (function, sync host method) called from a non-loop thread must be marshalled to the loop and
     must block only that thread. The prototype's `_call_from_any_thread` uses `run_coroutine_threadsafe`, and
     scenario 6 passes.
   - min_cordis itself must never be touched from such threads (§1.2).
   - Remaining deadlock: plugin code on the loop thread blocks on a worker's `concurrent.futures.Future`
     (`.result()`) while that worker calls a Rust proxy. That is a plugin bug (it blocks the loop).
   - Routing foreign submissions through the same inbox that the pump drains would at least let *runner* sync
     waits make progress.
6. **GIL.** Correctness is unaffected: the waiter releases the GIL in `cv.wait()`, and the reader holds it only
   to decode. A CPU-bound plugin delays frame admission by up to the switch interval (5 ms default). The
   free-threaded 3.13t/3.14t builds give no benefit, because min_cordis is loop-affine.
7. **Process exit.**
   - `asyncio.run` waits for the default executor at shutdown, and the interpreter joins non-daemon threads. A
     plugin's stray worker can hang exit, and `Process::dispose` waits for the exit status
     (`R/crates/rutis-interop/src/process.rs:470-480`).
   - The runner should flush stdio and `logging.shutdown()`, then `os._exit(0)` after a clean dispose, or after a
     bounded grace period.
8. **No `undefined`.**
   - A wire `undefined` argument must become *an omitted parameter*, so the Python default applies. This means
     binding by name through `inspect.signature`, not positional `*args`.
   - An `undefined` record field must become an omitted key.
   - An `undefined` result must become `None`.
   - The sentinel must never leak into plugin code.

---

## 4. Object identity and live objects

**Exports (Python → Rust):**

- Exports use a table `ref → entry`, which strongly pins the object, and an identity map keyed by `id(obj)`.
  Holding the pin while mapped makes `id()` reuse impossible. Both are removed together when grants reach 0. That
  mirrors `peer.mjs` `#exports`/`#identities` and the Rust `Exports` (`rpc.rs:466-475, 940-975`).
- A `WeakKeyDictionary` is unsuitable. Many objects are unhashable or not weak-referenceable (dicts, lists, many
  builtins), and `__eq__` overrides break key semantics.
- **Bound methods are fresh objects on each attribute access** (`obj.m is obj.m` is `False`, but `==` is `True`).
  Key their identity by `(id(m.__self__), id(m.__func__))`. Otherwise passing `self.on_change` twice yields two
  references, which breaks "unsubscribe with the same function" patterns.
- Traceable views are new per read, so service objects returned *as values* get a new reference per read. This
  matches Node, which keys its WeakMap by the proxy. Service *handles* are tracked by `_original`, as in §2. The
  runner can keep calling through the latest view, as `runner.mjs:76-79` does.

**Classification (proposed, as implemented in the prototype `is_live`):**

| Python value | Crosses as |
| --- | --- |
| `None`, `bool`, `int`, `float` (finite), `str` | data (`json.dumps(..., allow_nan=False)`; check `bool` before `int`) |
| `list`, `tuple` | list; tuple identity is lost, which is fine for data |
| `dict` with `str` keys | data, or a record if it holds references. **Reject non-str keys**: `json` would silently stringify them. |
| dataclass instance, NamedTuple, pydantic `BaseModel`, `Enum` (→ `.value`) | data; encode field by field so that nested live values become a record (never `dataclasses.asdict`, which deep-copies) |
| function, lambda, bound method, builtin, `functools.partial` | function reference |
| coroutine | a runner-owned Task, then a future reference |
| `asyncio.Future` / `Task` / objects with `__await__` | future reference (not owned) |
| `BaseException` as a value | data `{name, message, stack}` (reuse `JsError`, ideally renamed) |
| other class instances, including service views | object reference: live property reads through `get`, method calls through `call {method}` |
| `bytes`, `datetime`, `set`, generators, async generators, classes, modules | unsupported, the analogue of the TS built-ins and streams |

**Imports (Rust → Python):**

- Rust never exports objects (`rpc.rs:1142-1146`), so Python needs two proxies:
  - `RemoteFunction.__call__` performs a synchronous request from any thread.
  - `RemoteFuture.__await__` sends `await` lazily, like `RemotePromise` in `peer.mjs:42-51`. It must **memoize** the
    `await` future; the prototype does not yet.
- There is one proxy per import id, held through a weak map, and repeated grants increment a per-record count.
- `weakref.finalize` sends `release` with that record's count. This preserves the "old release crossing a new
  grant" safety: Rust subtracts per-record counts.
- CPython refcounting gives immediate release, so Rust exports are reclaimed far more promptly than with JS GC.
  Proxies inside reference cycles are released at the next cyclic GC.

**Live properties** behave like `getattr(obj, prop)` at the moment of the `get` frame. Through a service view,
`@property` getters run with the shadow receiver (`_traceable.py:262-264`). Instance attributes are read directly.

**Nominal materialization (new versus Node).**

- Rust sends `{"city": "oslo", "days": [...]}` for a parameter annotated `Forecast` (a dataclass). Python code
  would then fail on `forecast.days[0].high` with a dict.
- The runner must rebuild declared types from hints for:
  - service-method arguments
  - configs (min_cordis passes `config` through unchanged, so a dataclass-typed config must be constructed)
  - host-method results, by importing the seam's return annotation
  - arguments of Python callbacks passed to host methods
  - forwarded event arguments
- `X/materialize.py` (about 50 lines) handles dataclass, TypedDict, Enum, NewType, Literal, Optional, list and dict,
  including the TYPE_CHECKING-aware hint resolution of §5. Example: it rebuilt
  `Forecast(city='oslo', days=[DayForecast(...)], unit=<Unit.IMPERIAL>, …)` and rejected
  `Literal` violations.
- Unions of several dataclasses are ambiguous without a discriminator; pydantic's "smart" union mode solves this.
  Recommendation: a stdlib converter, delegating to `pydantic.TypeAdapter` when the hint is or contains pydantic
  types and pydantic is installed.

---

## 5. Build-time type extraction

### 5.1 What `generate.mjs` does (`R/interop/node/src/generate.mjs`)

- It builds a full TS `Program` with the checker (:56-69).
- It discovers services:
  - from `declare module '@deepseek-ai/cordis' { interface Context {…} }` augmentations anywhere in the program
    (:360-376);
  - through the lineage of a default-exported `Service` class, matched against augmentation types (:390-409);
  - from literal `ctx.provide('name', value)` calls in source plugins (:418-429);
  - from package-declared augmentations for declaration-only packages (:430-432).
- It gets config from the constructor or `apply`'s second parameter, or from a callable `Config` export (:407-445).
- It gets events from `interface Events` (:370, 749-793).
- It maps types to Rust (:164-353), binds members (:488-557) and callbacks (:562-617), and emits the host traits
  (:795-903) and the Plugin impl (:980-1031).
- It **emits Rust source directly as strings**.

### 5.2 What a Python plugin can declare today

- min_cordis typing is dynamic. `ctx.llm` is `Any` (`Context.__getattr__`), and **there is no Context augmentation
  and no Events interface.** The only declarative facts are:
  - the `provide` ClassVar on Service classes;
  - `inject` lists;
  - parameter and return annotations;
  - the `Config` attribute.
- Example authors already annotate well (`MC/python/examples/agent_loop.py:101-113, 137-244` uses dataclasses and
  `async def complete(...) -> LLMResponse`).
- The types of `ctx.llm` and of the `agent/step` events are undeclared.

### 5.3 Proposed conventions (zero new dependencies; all of them work in IDEs and pyright today)

1. **The seam class names the service.** In a `Service` subclass's MRO, the class that declares
   `provide = "name"` in its own `__dict__` *is* the interface. Members declared on implementation subclasses are
   not part of it, the analogue of the TS seam rule (`generate.mjs:391-406`). Example: `LLMClient(provide="llm")`
   and `ScriptedLLM(LLMClient)`.
2. **A typed Context declaration** is the analogue of `declare module … interface Context`. It is a typing-only
   subclass with annotations:
   ```python
   class WeatherContext(Context):
       weather: Weather
       llm: LLMClient          # may come from an `if TYPE_CHECKING:` import
   ```
   - Plugins annotate `ctx: WeatherContext` to get real IDE types for `self.ctx.llm`.
   - The generator merges the annotations of *all* `Context` subclasses in the imported modules, as `generate.mjs`
     merges augmentations.
   - It is harmless at runtime: class annotations never touch `Context.__setattr__`.
   - It serves both `provide` (exported services of function plugins) and host-provided services
     (`provide = ["llm"]`).
3. **An events table** is the analogue of `interface Events`. Event names contain `/`, so they cannot be
   identifiers. Use a module-level dict of signature stubs:
   ```python
   def _weather_updated(city: CityId, forecast: Forecast) -> None: ...
   EVENTS = {"weather/updated": _weather_updated}
   ```
   It is statically readable (a dict literal) and introspectable (it keeps parameter names). A tiny upstream helper
   such as `min_cordis.declare_event("weather/updated")` would be nicer, but it is optional.
4. **Annotations:**
   - fully annotate public members of seams and live classes;
   - write async methods as `async def` or `-> Awaitable[T]`;
   - use dataclass, TypedDict (with `NotRequired`), Enum (str-valued), `NewType`, `Literal`, `X | None`, and pydantic
     for data;
   - use `Callable[[...], R]` for callbacks;
   - write `-> Callable[[], None]` for returned disposers;
   - annotate `config` on `__init__(self, ctx, config: Cfg | None = None)` or on `apply(ctx, config: Cfg)`, or give
     the input type on a `Config` transform's first parameter.
   - Unannotated members become `serde_json::Value` with a `cargo:warning`, like TS `any`.

### 5.4 Runtime introspection vs static analysis

| | Runtime introspection (import in the mount's venv; never apply) | Static: `ast` only | Static: griffe | Static: mypy / pyright as a library |
| --- | --- | --- | --- | --- |
| Fidelity | Highest. It uses Python's own semantics: `typing.get_type_hints(include_extras=True)`, `inspect.signature`, `dataclasses.fields` (including **default values**), `__required_keys__`, Enum members and values, `NewType.__supertype__`, `get_overloads`, pydantic `model_fields` and `model_json_schema()`. | Names only. Resolving imports, aliases, re-exports, inheritance and generics means writing a type checker. | Good API structure: signatures, async labels, annotation expression trees, alias targets (it resolves TYPE_CHECKING imports to `llm_seam.LLMClient`), dataclass label, Enum values as *source strings*, a TypedDict base. Semantics such as Enum `auto()`, `field(default_factory)`, pydantic aliases and NewType calls still have to be reinterpreted. | Full types, but mypy's `build` API is internal and unstable. pyright has no library API; only `--verifytypes --outputjson` gives string-rendered types. ty and pyrefly have no stable API yet. |
| Needs the venv installed at build | yes, like `node_modules` for TS today | no | no (needs sources) | yes (stubs) |
| Side effects | **Module top-level code runs.** The demo's `print("weather_impl: import-time side effect ran")` executed during generation. | none | none | none |
| `if TYPE_CHECKING:` imports | `get_type_hints` raises `NameError`. Workaround: execute the typing-only import statements into a `localns`. This works in the prototype; the diagnostic at `weather_api.py:75` resolved `LLMClient`. | n/a | handled | handled |
| Known CPython traps | `TypedDict` + `NotRequired` under `from __future__ import annotations` gives **wrong `__required_keys__`** on 3.11–3.14a5 (`X/td_probe.py`). Derive requiredness from `get_origin(hint)`; the prototype was fixed this way. | | | |
| Cross-platform builds | Platform-specific wheels must import on the build host. | fine | fine | stubs needed |

**Recommendation:** use runtime introspection as the source of truth, run by the mount's own interpreter
(`<venv>/bin/python -m rutis_interop.generate`), with the following around it:

- An **import-safety boundary rule**: plugin modules must not perform work at import. This is the Python
  counterpart of "it must be a Cordis plugin".
- A static `ast` pre-pass for the things that need *source* rather than objects: literal
  `ctx.provide("name", …)` calls inside `apply`, like `generate.mjs:418-429`, and the locations used in diagnostics.
- The TYPE_CHECKING fallback, and requiredness computed from hints.
- Emitting a **language-neutral IR (JSON)**, not Rust:
  - services → members (params with name, kind, type, nullable, omissible, default; `async`; result), getters,
    and unbound members with reasons;
  - named types (struct, enum, newtype, union, object) with their Python `module:qualname`, so that the runner can
    import them for materialization;
  - config, hosts, events, inputs (module files for `cargo:rerun-if-changed`), diagnostics, the protocol, and the
    installed distribution versions.
- Rendering Rust in `rutis_interop::build`, shared across languages. Today all emission lives in `generate.mjs`
  (`:595-1031`). Porting it to Python would duplicate about 450 lines of Rust-string templating.

`X/introspect.py` (about 300 lines) already produces this IR for a seam/implementation pair (`X/typedemo/*`):

- the `weather` service with 4 methods (async `forecast`, `subscribe` with a callback returning `remote-function`,
  `watch` returning a live `Subscription`, unannotated `lookup` → json) and a `calls` getter;
- `LLMClient` as a host service (`complete` async, with an omissible `max_tokens`);
- `WeatherConfig` (total=False);
- the `weather/updated` event;
- `CityId` (newtype), `Unit` and `DayForecastKind` (enums), and `DayForecast`, `Forecast` and `Options` (structs
  with correct requiredness).

The output is in `X/introspect-output.json`; the griffe comparison is in `X/griffe-weather_api.json`.

### 5.5 Python → Rust type mapping (extends `design-protocol-plugin-mount.md` §2)

| Python | Rust |
| --- | --- |
| `bool` / `int` / `float` / `str` / `None` return | `bool` / **`i64`** (TS has only `f64`; reject ints that do not fit) / `f64` / `String` / `()` |
| `NewType("CityId", str)` | `pub struct CityId(pub String)` transparent, `From<&str>`. The brand analogue, and simpler to detect. |
| `Literal["a","b"]`, str-valued `Enum` | enum with serde renames |
| int `Enum` / `Literal[1,2]` | `i64` (or serde_repr later) |
| dataclass / TypedDict / NamedTuple / pydantic model | struct; field names are already snake_case, so `r#` is needed only for keywords |
| `T \| None` without default | `Option<T>` (sends `null`) |
| parameter with default, dataclass field with default, `NotRequired[T]` | `Option<T>` (omitted → Python default). With `default=None` and nullable, collapse to `Option<T>` (both mean None). Otherwise `Option<Option<T>>`, as in TS. |
| `list[T]` / `Sequence[T]` / `tuple[T, ...]` | `Vec<T>` (`&[T]` in parameters) |
| `tuple[A, B]` | `(A, B)`; serde tuples are JSON arrays (TS rejects tuples) |
| `dict[str, T]` / `Mapping[str, T]` | `BTreeMap<String, T>` |
| `Any`, `object`, unannotated, data unions | `serde_json::Value` + warning |
| class with methods / `Protocol` / Service seam | live proxy struct over `ObjectRef` (getters and methods) |
| `Callable[[A], R]` param | `impl Fn(A) -> Result<R>`; `Callable[..., Awaitable[R]]` → `BoxFuture` |
| `-> Callable[...]` | `RemoteFunction` |
| `async def` / `-> Awaitable[T]` / `-> Coroutine[..., T]` | `async fn`, documented "dropping cancels the Python task" |
| `BaseException` value | `JsError { name, message, stack }` (rename to something neutral) |
| `bytes`, `datetime`, iterators, generators, `*args`/`**kwargs`, `@overload`, generic methods (TypeVar) | not bound: warning and skipped |

Python makes three things possible that TS does not: real `i64`, tuples, and **known default values**. Dataclass
and TypedDict-less defaults are runtime values, so `impl Default for Config` could carry the plugin's real defaults.

### 5.6 Precedents for deriving schemas from hints

- **FastMCP / the MCP Python SDK** build a pydantic model from a tool function's `inspect.signature` and
  annotations (`func_metadata`) and publish its `model_json_schema()` as the tool input schema. They also derive
  structured output schemas from return annotations.
- **FastAPI** derives request bodies and dependencies from hints. pydantic's `TypeAdapter(T).json_schema()` /
  `validate_python()` covers dataclasses, TypedDict, Enum, Literal and NewType.
- **msgspec** (`msgspec.convert(obj, type=T)`) and **cattrs** do hint-driven materialization; this is §4's runner
  side.
- **Strawberry** builds GraphQL schemas from type hints, and **Typer** builds CLIs from them.
- **griffe**, behind mkdocstrings, is the established static extractor.

All the runtime-hint precedents import user modules. That is the accepted practice in the Python ecosystem, which
supports the runtime-introspection recommendation. For the loader's P6 config-schema export
(`R/docs/design-rutis-loader-2026-10-02.md:355`), Python gets a JSON Schema almost for free: from pydantic if it is
present, or from the IR.

---

## 6. Environment and deployment

| Concern | Node today | Python proposal |
| --- | --- | --- |
| Project | one npm project per app (`npm = "cordis"`), one Node process per mount (`R/crates/rutis-interop/src/build.rs:238-364`) | One **uv project** per app (`python = "python"`: `pyproject.toml`, `uv.lock`, `.venv`). It lists the plugin distributions, `rutis-interop` (runtime) and `min-cordis`. One process per mount, all sharing the venv. Mounts whose dependencies conflict get their own uv project (a per-mount `python = "…"` override). This matches the old roadmap ("每组使用锁定的依赖环境，冲突时拆组，不能靠修改 sys.path 声称隔离", `R/docs/roadmap-protocol-plugin-languages-2026-09-26.md:31-32`). |
| Lock | `package-lock.json`; the build checks installed versions against `version` | `uv.lock` is the single source. The build does **not** install. A missing `.venv` fails with "run `uv sync --frozen --project python`". Versions are checked by the generator inside the venv (`importlib.metadata.version(dist)` against the pinned `version`). Comparing all of `uv.lock` with the installed `dist-info` naively is wrong, because platform-marker packages such as `colorama` are locked but not installed (seen in `MC/python`). Check only the declared distributions. `cargo:rerun-if-changed`: `uv.lock`, `pyproject.toml`, `.venv/pyvenv.cfg`, and the module files reported by the generator. |
| Interpreter at build | `node` on PATH (`build.rs:186`) | `<project>/.venv/bin/python` (honour `UV_PROJECT_ENVIRONMENT`), never PATH. Requires 3.12+. Record the minor version; a change means re-sync and rebuild. |
| Interpreter at runtime | `node --import tsx runner.mjs <socket> <entry>` (`process.rs:330-342`) | `<root>/.venv/bin/python -u -s -m rutis_interop.runner <socket> <entry>`. `-s` excludes user site; `-I` is tempting but also ignores `PYTHONASYNCIODEBUG`/`PYTHONPATH`, which is an open question. Path mounts add their directory to `sys.path` explicitly. Frames use the socket; stdout and stderr are inherited, which matters because the min_cordis logger prints to stdout. |
| Relocation | `RUTIS_INTEROP_ROOT` replaces the npm project root (`R/crates/rutis-interop/src/lib.rs:15-25`); `cp -RL` the project (`README.md:105-114`) | **A venv is not relocatable.** `pyvenv.cfg` stores an absolute `home`, `bin/python` is an absolute symlink to the base interpreter (seen: `-> /Users/eric8810/miniconda3/bin/python3.12`), and editable installs store absolute `.pth` paths. `uv venv --relocatable` only makes scripts relative. Supported deployments: (a) ship the uv project and run `uv sync --frozen --no-dev --no-editable` on the target (uv can provision the exact managed CPython); or (b) bundle a relocatable python-build-standalone interpreter plus `uv pip install --target` site-packages, with the runner putting that directory on `sys.path`. One env var cannot point to both an npm and a uv root: add `RUTIS_INTEROP_PYTHON_ROOT`, or make the root a parent with `node/` and `python/` subdirectories. |
| Runtime distribution | npm `@arcships/rutis-interop`, same version as the crate (`R/interop/node/package.json`) | PyPI **`rutis-interop`** (name free), pure Python, the same version as the crate, depending on `min-cordis` within a range. **Prerequisite: publish `min-cordis` to PyPI** (name free), or use a git dependency (`min-cordis @ git+https://github.com/eric8810/min-cordis#subdirectory=python`). |
| Protocol check at build | `package.json` `rutisProtocol` vs `PROTOCOL` (`build.rs:367-388`) | `rutis_interop.PROTOCOL = 1`, reported in the generator's JSON output (the build runs the venv interpreter anyway) and also shipped as package data `rutis_interop/protocol.json` for a no-exec check. The `hello` handshake checks it again at runtime. |

---

## 7. Python runner architecture

### 7.1 Package `rutis_interop` (PyPI), proposed modules

| Module | Role | Node counterpart |
| --- | --- | --- |
| `wire.py` | framing; `json.dumps(allow_nan=False)`; reject non-str keys and out-of-range ints | `wire.mjs` |
| `errors.py` | exception ↔ error graph (`__cause__`, `ExceptionGroup.exceptions` ↔ `errors`, `__notes__`, shared and cyclic nodes); `RemoteError`, `SyncWaitCycle` | `errors.mjs` |
| `io.py` | reader thread, inbox + Condition, coalesced `call_soon_threadsafe` wakeups, locked writer | `io-worker.mjs` + `client.mjs` |
| `peer.py` | tables, sync pump, async requests (Future), exports and imports, owned tasks, cancel mapping, `ContextVar` path, foreign-thread marshalling, finalizer → release | `peer.mjs` |
| `values.py` | classification (§4), encoding of dataclass/Enum/pydantic, hint-driven materialization, binding of `undefined` → omitted kwargs | `isLive`/`checkData` |
| `runner.py` | `python -m rutis_interop.runner`: connect, `Context(on_error=…)`, slots/handles/exporters/refresh, mount (host proxies → plugins → exporters → await → unresolved check → forwarding listeners), control ops `mount`/`dispose`/`emit`/`get`/`release`, `invoke` dispatch with refresh after the call (or on task completion), shutdown via `os._exit` | `runner.mjs` |
| `hosts.py` | host proxies; optionally subclasses of the imported seam class with every public method overridden | `hostProxy` |
| `generate.py` | `python -m rutis_interop.generate`: import, discover (+ `ast` pre-pass), IR JSON | `generate.mjs` (analysis half) |

Rough size: about 1,300–1,800 lines of Python, comparable to the roughly 1,800 lines of `interop/node/src`. The
prototypes total about 900 lines.

### 7.2 Rust-side changes (`crates/rutis-interop`)

1. **Launcher abstraction.** `Process::mount` hard-codes `node --import tsx …/runner.mjs`
   (`process.rs:330-342`). Introduce `Runtime::{Node{package}, Python{interpreter, project}}`, which supplies the
   command, cwd and env. The rest of `Process`, `rpc`, `Projection` and `events` is language-neutral. The error
   text "Cordis process exited" should become "plugin process".
2. **Manifest.** Add `python = "<uv project>"` and per-mount `language = "python"` (or a separate
   `python-mounts` table). Plugin selection is one of:
   - `plugin = "<distribution>"`, resolved through its `min_cordis.plugins` entry point, plus `entry = "<name>"`
     when there are several;
   - `module = "pkg.mod:Attr"`;
   - `path = "python/plugins/x.py"`.

   `provide`, `events` and `emits` stay as they are.
3. **IR → Rust renderer** in `build`, reusing the existing rules: absence (`Option<Option<T>>`), live objects,
   callbacks, host traits, events, Plugin impl. In the longer term `generate.mjs` emits the same IR.
4. **Protocol v1 Node-isms that Python must live with, or that a v2 should neutralize:**
   - call-id prefixes `node:`/`rust:` are hard-coded (`rpc.rs:822, 1165-1167, 1192-1197`;
     `peer.mjs:146, 247-249`), so the Python peer must allocate `node:n` ids, as the prototype does;
   - `WireValue::Signal` is JS-specific;
   - `JsError` naming.

   None of these block v1.

### 7.3 Author-facing experience

A Python plugin package that is published or path-mounted, written as plain min_cordis code with annotations:

```python
# weather_api.py  (seam: interface + data)
from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Callable, Literal, NewType, NotRequired, TypedDict
from min_cordis import Context, Service
if TYPE_CHECKING:
    from llm_seam import LLMClient

CityId = NewType("CityId", str)
class Unit(str, Enum):
    METRIC = "metric"; IMPERIAL = "imperial"

@dataclass
class DayForecast:
    high: float; low: float; kind: Literal["sun", "rain", "snow"]

@dataclass
class Forecast:
    city: CityId; days: list[DayForecast]; unit: Unit = Unit.METRIC; note: str | None = None

class Options(TypedDict):
    days: int
    unit: NotRequired[Unit]

class Weather(Service):
    provide = "weather"                         # the seam names the service
    async def forecast(self, city: CityId, options: Options | None = None) -> Forecast: ...
    def subscribe(self, city: CityId, listener: Callable[[Forecast], None]) -> Callable[[], None]: ...

class WeatherContext(Context):                  # typing-only: what `ctx` carries
    weather: Weather
    llm: "LLMClient"

def _updated(city: CityId, forecast: Forecast) -> None: ...
EVENTS = {"weather/updated": _updated}

# weather_impl.py  (entry point "weather_impl:OpenMeteo")
class WeatherConfig(TypedDict, total=False):
    api_key: str; timeout: float

class OpenMeteo(Weather):
    inject = ["llm"]
    ctx: WeatherContext
    def __init__(self, ctx: WeatherContext, config: WeatherConfig | None = None) -> None:
        super().__init__(ctx); self.config = config or {}
    async def forecast(self, city, options=None):
        text = (await self.ctx.llm.complete(f"weather in {city}")).text
        self.ctx.emit("weather/updated", city, result := Forecast(city, [DayForecast(20, 10, "sun")], note=text))
        return result
```

The plugin distribution's `pyproject.toml`:
`[project.entry-points."min_cordis.plugins"] openmeteo = "weather_impl:OpenMeteo"`.

The application:

```toml
# Cargo.toml
[package.metadata.rutis-interop]
python = "python"                         # uv project: pyproject.toml + uv.lock (+ .venv after `uv sync --frozen`)

[package.metadata.rutis-interop.mounts.weather]
language = "python"
plugin = "weather-openmeteo"              # distribution; entry point in group min_cordis.plugins
version = "0.3.1"                         # must match the installed distribution
provide = ["llm"]                         # rutis implements LlmClientHost
events = ["weather/updated"]              # forwarded to rutis listeners
```

```toml
# python/pyproject.toml
[project]
name = "my-app-plugins"
version = "0"
requires-python = ">=3.12"
dependencies = ["rutis-interop==0.2.0", "weather-openmeteo==0.3.1"]
[tool.uv]
package = false
```

The Rust usage is identical to Node mounts (`build.rs` → `from_manifest()`, `include_mounts!()`):

```rust
weather::provide_llm(&ctx, MyLlm)?;                       // impl weather::LlmClientHost
let view = ctx.plugin(weather::Plugin::new(weather::Config { api_key: Some("k".into()), ..Default::default() }));
(&view).await?;
let w = ctx.require::<weather::Weather>()?;
let f: weather::Forecast = tokio::time::timeout(secs(5), w.forecast(&"oslo".into(), None)).await??; // drop = Task.cancel()
let unsubscribe = w.subscribe(&"oslo".into(), |f| { println!("{f:?}"); Ok(()) })?;               // RemoteFunction
```

The generated items follow §5.5:

- `CityId(String)`, `Unit` and `DayForecastKind` enums
- `DayForecast` and `Forecast` structs
- `Options { days: i64, unit: Option<Unit> }`
- a `Weather` proxy with `async fn forecast`, `fn subscribe(…, impl Fn(Forecast) -> Result<()>) -> Result<RemoteFunction>`
  and `fn calls() -> Result<i64>`
- `trait LlmClientHost { fn complete(&self, prompt: String, max_tokens: Option<i64>) -> BoxFuture<…Completion…> }`
- an event struct `WeatherUpdated { city, forecast }`.

---

## 8. Risks and open questions

1. **Purpose and corpus.** There are no existing min_cordis Python plugins. Is the goal parity with a non-existent
   ecosystem, or a first-class "write rutis plugins in Python" story? If the latter, the conventions of §5.3 can be
   required up front, and requirement §3's "插件源码不改写" is easy to satisfy.
2. **min_cordis readiness:**
   - not on PyPI, version 0.1.0;
   - `requires-python` is wrong (fails on 3.11);
   - the Python EffectRecord contract lags (HANDOFF item 4);
   - `_original` is underscore-only, so a public `original()` would help;
   - the root-write quirk (`_context.py:588-590`);
   - Standard Schema is absent.

   Who maintains it? The runner would be its first heavy consumer. A few small upstream changes would help (a
   public identity helper, typing helpers for the Context and events declarations, a fix for `requires-python`).
   The Cordis constraint "do not change the framework" (requirements §3) is presumably looser for the user's own
   min_cordis. Confirm this.
3. **Import side effects at build time.** Runtime introspection executes module top-level code. Mitigations: the
   import-safety rule, generating in a subprocess, and a static fallback (griffe or `ast`) that warns. Decide
   whether a no-import mode is required, for example for cross-compiling with platform-specific wheels.
4. **Declaration conventions** (seam `provide`, typed `Context` subclass, `EVENTS` table) need sign-off. Each is a
   new convention for Python authors.
5. **Cancellation semantics** (§3.3.3). Is forced `Task.cancel()` acceptable as "the" cancellation? Should a
   cooperative token type be added to min_cordis so that thread-offloaded work can observe cancellation?
6. **Failure policy.** Node dies on an unhandled rejection (boundary rule 8). asyncio logs and continues. Pick one:
   keep the Python behaviour and document it, or make the runner exit on `loop.set_exception_handler` for
   unretrieved task errors so that rutis sees a crash and withdraws services.
7. **Nominal materialization scope** (§4): which positions are materialized, whether pydantic is optional or
   required, and union disambiguation.
8. **Deployment model** for venvs (§6). Does an application ship `uv` plus network access, or a bundled
   interpreter? Also `RUTIS_INTEROP_*` naming for two ecosystems.
9. **Generator architecture.** A shared IR → Rust renderer (recommended) means refactoring `generate.mjs`'s
   emission into the crate. Otherwise about 450 lines of templating are duplicated in Python.
10. **Protocol hygiene.** The `node:` id prefix, `Signal` and `JsError` naming. Keep v1, or define v2 when the
    second language lands.
11. **Threads.** Plugins whose code touches `ctx` from threads break min_cordis (RuntimeError). A runner-level guard
    could detect off-loop access, for example by asserting the thread in proxies.
12. **Performance.** Expect tens of microseconds per synchronous round trip. CPU-bound plugins contend for the GIL.
    A no-thread design is possible as a later optimisation: the loop thread owns a raw non-blocking socket and the
    pump uses `selectors`. It avoids cross-thread wakeups, but it must not use asyncio transports, whose buffered
    writes stall while the loop is blocked.
13. **Exit hangs** from non-daemon plugin threads and executor shutdown. The runner should `os._exit(0)` after a
    clean dispose (§3.3.7).
14. **asyncio only.** Trio-native plugins are out of scope. anyio-on-asyncio works.

---

## Appendix: experiments (`X/` = `scratchpad/experiments/python`)

| File | What it shows | How to run |
| --- | --- | --- |
| `MC/python` test suite | 96/96 on 3.12.2 and 3.13.6 (also with warnings as errors); 95/96 on 3.11.11 (`asyncio.iscoroutine(generator)`) | `cd MC/python && uv run --python 3.12 pytest -q` |
| `X/pypeer.py`, `X/fakerust.py`, `X/experiment_sync.py` | Prototype Python peer for protocol v1 plus a scripted fake Rust peer: nested sync chains, SyncWaitCycle, eager vs lazy tasks, cancel, deferral, foreign threads, latency, deterministic release | `uv run --no-project --python 3.12 python experiment_sync.py` |
| `X/experiment_runner_core.py` | The `runner.mjs` slot projection against real min_cordis: exporter + `_check` gating, `internal/service`, `internal/set`, `_original`, unresolved detection, host proxy, event forwarding, dispose | `uv run --no-project --python 3.12 python experiment_runner_core.py` |
| `X/typedemo/*.py`, `X/introspect.py`, `X/introspect-output.json` | Runtime-introspection generator producing the IR; TYPE_CHECKING fallback; import side effect observed | `cd typedemo && PYTHONPATH=.:../../../min-cordis/python uv run --no-project --python 3.12 python ../introspect.py weather_impl:OpenMeteo --provide=llm` |
| `X/griffe-weather_api.json` | Static extraction with griffe 2.3.0 | `uvx --python 3.12 griffe dump weather_api -s . -s <min_cordis>` |
| `X/materialize.py` | Hint-driven materialization (dataclass, Enum, NewType, Literal, TypedDict) | `uv run --no-project --python 3.12 python materialize.py` |
| `X/eager_probe.py`, `X/td_probe.py`, `X/loop_probe.py` | `eager_start` on 3.12+; the TypedDict `NotRequired` + PEP 563 bug (3.11–3.14a5); min_cordis loop/thread affinity | `uv run --no-project --python 3.12 python <file>` |

Runner-core experiment output (abridged):

```text
unresolved: [('needs_missing', ['missing'])]
initial notifications: [('counter', 'counter', 1), ('store', 'store', 2)]
two reads: same object? False | equal? True | same _original? True | isinstance Counter: True
counter.watch -> interop-export:counter ['watch:setup']
after ctx.set swap (re-read after call): [('store', 'store#2', 3)] 2
after property assignment (internal/set): [('store', 'store#3', 4)]
after _check -> False: [('counter', None, 5)] exporter state: PENDING ['watch:dispose']
after _check -> True: [... ('counter', 'counter#2', 6)]
after dispose: [('counter', None, 7), ('store', None, 8)]
```
