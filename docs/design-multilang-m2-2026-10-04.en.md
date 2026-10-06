# Multilingual Plugins M2: Python Runtime and Leaf Plugins (Implementation Notes)

Status: implemented. Date: 2026-10-04.
Based on [Multilingual Plugins: One Runtime Plugin per Language](design-multilang-runtimes-2026-10-03.en.md) (§11 M2) and [Multilingual Plugins M1](design-multilang-m1-2026-10-04.en.md) (§11 JS/TS leaf plugins, §13 items deferred to M2).

## 1. What changed

| Area | Details |
| --- | --- |
| Named runtimes | `RuntimePlugin::named`, with service keys `Runtime::key(name)` and `RuntimeRows::key(name)`. Node defaults to `"node"`, Python to `"py"`. An application can use multiple runtimes. |
| Configurable launch command | `Mount::launcher` / `Launcher { program, args, env, cwd }`; the final two arguments remain the socket path and project location. |
| Python runtime | `interop/python/rutis_runtime`: protocol layer `peer.py` (ported from `peer.mjs`), leaf-plugin runner, and plugin SDK. `RuntimePlugin::python(sdk, project)` starts it. |
| Python rows | `InteropResolver::modules`: row names use `py:<module-name>`. |
| JS/TS leaf plugins | `definePlugin` from `@arcships/rutis-interop/plugin`; the runner recognizes the marker, wraps the plugin as a Cordis plugin, and loads it into the existing Node process. |
| Cross-language forwarding | `RowService` uses the caller's session (`rpc::caller`) for `Connection::forward`, rewriting synchronous call chains across sessions. |
| Compatibility tests | `crates/rutis-loader/tests/multilang.rs`: equivalent leaf plugins written in Python and JS run together in both runtimes. |

## 2. Decisions made during implementation

**rutis gates every dependency of a leaf runtime.** The Python runtime reports the `leaf` capability when mounted. It has no dependency resolver of its own, so the resolver adds every name in the plugin's `inject` declaration to the row's rutis dependencies; `register_shared` is not required. The Node runtime still gates only names registered as shared and delegates the rest to Cordis.

**The Python runtime is reentrant.** During cold start, a JS plugin can synchronously call a Python service while a Python plugin synchronously calls a JS service. Both processes wait synchronously and defer the other's call as unrelated, causing deadlock; the compatibility test reproduced this immediately. Node's rule remains unchanged (run only incoming calls on the current call chain). Python runs all incoming calls during a synchronous wait. If either side is reentrant, the crossed wait can complete. The tradeoff is that a Python service may be called during its own synchronous call, so the README says not to hold locks while calling rutis services. Two Node runtimes can still deadlock on crossed synchronous calls; the docs recommend async methods for such calls.

**Providers stop after consumers.** When a row unloads, it first withdraws the service projected into rutis (`Projection::withdraw`), letting the core stop consumers, and then unloads the plugin from the runtime. Previously the plugin unloaded first and the service was revoked asynchronously; the compatibility test caught provider cleanup running before consumer cleanup.

**Python plugin configuration changes restart the plugin.** Leaf plugins have no volatile fields, so `rows.update` is equivalent to unloading and loading again.

**Python plugin code can be reloaded.** A runtime that loads by module name has no package version to compare, so the resolver does not cache its resolution and asks the runtime each time. The runtime detects changes to the module file's modification time or size and reimports it. `Loader::reload` therefore gets fresh code and declarations. Only the plugin module itself is reimported.

**Languages remain decoupled.** Each language is a Cargo feature (`node` and `python` for rutis-interop and rutis-loader; `interop` enables both in rutis-loader). An application compiles only enabled languages, and no process starts unless a runtime plugin is mounted. Shared components (protocol, process management, service projection, `RuntimePlugin`, `Launcher`) are language-independent. Types use language-independent names (`RuntimePlugin`, `Runtime`, `RuntimeRows`); old `Cordis*` names remain as deprecated aliases. CI checks builds with only one language enabled.

## 3. Behavior covered by compatibility tests

Both runtimes provide services, have cross-language consumers and same-process consumers, and start together:

1. All start without runtimes waiting on each other. Synchronous and asynchronous cross-language calls reach the other side, while same-process consumers receive the provider's original object.
2. Removing any provider stops only its consumers in both languages, and all consumers stop before provider cleanup. Other rows keep running.
3. A Rust-provided injected service (`llm`) gates plugins in both languages: they wait until it is ready, start when provided, and stop when revoked.
4. If the Python process exits unexpectedly, only Python rows and JS rows using Python services stop. The Node runtime and other JS rows continue.

## 4. Deferred work

- Swift (M3) and Go (M4), following the main design once requirements are clear.
- Passing object references (objects with methods and properties) to Node: `peer.mjs` still rejects Rust-exported object references; Python rejects them too. Functions and async results can be passed.
- Python plugins do not update configuration in place or generate configuration schemas from type annotations (see the convention in `python.md` §5); `Config` is currently written as JSON Schema.
- Reloading reimports only the plugin module, not modules imported by it.
