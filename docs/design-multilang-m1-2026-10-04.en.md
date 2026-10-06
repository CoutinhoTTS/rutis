# M1 Design: Multilingual Plugin Interoperation

> Status: implemented. This document records the M1 design and its implementation adjustments. The overall design is in [design-multilang.md](design-multilang.md).
>
> Base commit: `main` as of 2026-10-04. PRs #129, #128, and #130.

## 1. Purpose and scope

M1 makes Cordis plugins in the Node runtime participate in rutis service dependency gating, and makes services provided by a Node plugin available to Rust plugins. It also implements cross-session reference forwarding, which M2 will need for calls between two runtimes.

### Goals

- Node plugin `inject` declarations participate in rutis lifecycle gating.
- Services provided by Node plugins can be used by Rust plugins, with the same lifecycle behavior as native rutis services.
- Host services are made available to a Node plugin only when a row needs them.
- References can be forwarded across sessions without losing call-chain information.

### Acceptance criteria

- All existing static mounts continue to work.
- Existing Cordis plugins need no changes; `package.json` metadata may be added to declare their provided services.
- Rutis does not gate dependencies that were not explicitly registered as shared.
- A service provided by a Node row can be consumed by another Node row without going through IPC.

### Out of scope

- Node-side leaf plugins (covered in §11 as a follow-on design).
- Sharing one runtime process between independent sessions.
- Forwarding object references to Node when the Node peer does not support Rust-exported objects.

## 2. Corrections to the overall design

| Topic | M1 decision |
| --- | --- |
| Service names shared across languages | Explicitly registered in `ServiceCatalog` with `register_shared(name)` |
| Scope of gating | Only required `inject` names registered as shared are gated by rutis; Cordis gates the others |
| Provider metadata | Existing Cordis plugins declare `provides` in `package.json`; new leaf plugins declare it in code |
| Host service lifetime | Register a host proxy when the first row needs it; withdraw it when the last row releases it |
| Row service lifetime | Project the row's provided services into its own rutis fiber |
| Runtime readiness | A second release stage reloads rows after runtime schema becomes available |
| Cross-session references | Forward them through local relay objects; do not add a new protocol frame |

## 3. Change inventory

| Area | Changes |
| --- | --- |
| `rutis-interop` protocol | Cross-session relay objects; session-tagged call paths; `rebase` |
| Node runner | New `rows.schema` format; row service slots; dynamic `hosts.provide` and `hosts.withdraw`; `features` in mount reply |
| `rutis-loader` | `register_shared`; row dependencies and service projection; per-row host-service registration; `RuntimeRowsPlugin` |
| Release | `rutis-interop` crate and npm package both move to 0.3.0 |
| Migration notes | Explain shared service registration, host-service gating, and `RuntimeRowsPlugin` |

## 4. Shared service keys

Use one dynamic key for the same named service in every language:

```rust
fn host_key(name: &str) -> TypeKey {
    TypeKey::keyed_dynamic::<dyn HostDispatch>(name)
}
```

`HostDispatch` represents a service that can be invoked through the interop runtime. It also exposes the method shape needed to create a host proxy.

Providers can be:

- The Rust application or a Rust plugin, as today, using `provide_as::<dyn HostDispatch>(host_key(name), ..)`.
- A row in a runtime, for which rutis-loader registers a `HostDispatch` forwarding to that runtime (§5.3).

Consumers can be:

- A Rust plugin: after the name is registered in the catalog with `register_shared`, it declares `inject: [weather]` and calls `require_as::<dyn HostDispatch>(host_key("weather"))` in `apply`.
- A row in a runtime: it calls `host:weather`, and Rust looks up the current provider by name and invokes it locally or forwards to another session.

### 4.2 Explicit registration

The application registers the names that rutis should gate:

```rust
let mut catalog = ServiceCatalog::new();
catalog.register_shared("llm").register_shared("weather");
```

Only registered names are looked up through `host_key(name)`. This keeps ordinary Cordis dependencies under Cordis's native rules and avoids making gating depend on which provider happens to be resolved first.

The tradeoff is that the application must list service names shared across languages. In return, the rule is static and independent of resolution order (see the alternative in §12).

### 4.1 Method shape travels with the service

When creating a host-service proxy on the Node side, the runtime needs to know whether each method is synchronous or asynchronous. This information currently lives in `CordisRuntimePlugin::host(name, methods)`. Once host services are registered per row, the shape must travel with the service:

```rust
pub trait HostDispatch: Send + Sync + 'static {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply;
    /// { method: "sync" | "async" }. None: filled from the runtime's `host` declaration.
    fn methods(&self) -> Option<Value> { None }
}
```

- The default implementation returns `None`, so existing implementations need no changes.
- `CordisRuntimePlugin::host(name, methods)` remains, but **only records the shape; it no longer makes the runtime depend on this service**. Document this behavior change in the 0.3.0 migration notes.
- A row service's `HostDispatch` (§5.3) always carries its shape.

### `CordisRuntimePlugin`

The runtime itself is provided under a fixed `CordisRuntime` key. `CordisRuntimePlugin::host(name, methods)` records only the method shape for the named host service; it does not make the runtime depend on that service. When a row needs the service, the row acquires a reference to it and the runner receives a proxy registration.

## 5. Node rows and service dependencies

### 5.1 `rows.schema`

Today `rows.schema(entry)` returns only the configuration JSON Schema. Change it to return:

```json
{
  "config": { "...": "JSON Schema, or null" },
  "inject": { "llm": { "required": true }, "cache": { "required": false } },
  "provides": { "weather": { "today": "async", "unit": "sync" } }
}
```

- `inject`: read the plugin module's `inject` export; normalize both array and object forms to an object.
- `provides`: Cordis plugins call `ctx.provide` at runtime, so their method shape is not available in metadata. M1 reads it from a field in the plugin package's `package.json`:

  ```json
  { "rutis": { "provides": { "weather": { "today": "async", "unit": "sync" } } } }
  ```

  This field can be written by hand or generated from `.d.ts` files by adding a shape-only mode to `generate.mjs`. Of the two options in §9 of the overall design, that document prefers this one; this design settles on it. Without the field, a plugin's services remain visible only in Cordis and are not projected into rutis, as today.
- Older runners return a bare Schema. Rust distinguishes the formats using `features` (§8), rather than guessing.

### 5.2 Which side gates a dependency

| Dependency | Source | Gated by |
| --- | --- | --- |
| `CordisRuntimeRows` (replacing `CordisRuntime`) | Fixed | rutis |
| `host_key(n)`, where `n` is required in `inject` and registered as shared in the catalog | `rows.schema` | rutis |
| Other `inject` names | `rows.schema` | Cordis, which waits using its existing native rules |
| `inject` in row configuration | Loader row | Cordis (`foreign_scope`), unchanged |

Optional dependencies (`required: false`) do not become rutis dependencies. If present at apply time they are exposed to Cordis; their later appearance or withdrawal does not restart the row. This matches how native rutis plugins handle optional services.

Because `Resolved::factory` captures injects at resolution time, a dependency declaration change requires the row to be resolved again. This is the reason for the second release stage (§6).

### 5.3 Projecting row-provided services into rutis

The runner already has an export-slot mechanism (`slots`, `exporter`, and `service` notifications), currently used only for build-time generated mounts. M1 uses it for rows too:

1. `rows.load(key, entry, config, isolate, inject, services)` gains a final argument listing the names and shapes declared in `provides`. The runner creates an export slot for every name; the exporter fiber is unloaded with the row's fiber.
2. Slot changes are reported to Rust through the existing `service(name, handle, version)` notification.
3. Rust adds `rows.rs`; `Process` forwards each notification to the row that registered the name.
4. When `JsRow` in rutis-loader receives a handle, it calls `provide_as::<dyn HostDispatch>(host_key(name), RowService { process, handle, methods })` on its own `Ctx`. When it receives `None`, it withdraws the service.
5. `RowService::invoke(method, args)` calls `process.invoke(handle, method, args)`.

The service is registered on the row's fiber, so unloading the row makes rutis withdraw it automatically. Rows and Rust plugins that depend on it stop according to native rutis rules.

If two rows declare the same name, Cordis has only one effective provider while rutis would see two. M1 does not resolve this conflict; it reports the overlap between the two rows' `provides` declarations as a diagnostic.

### 5.4 Host services become row-scoped

Today host services are registered in Node once at runtime startup (`Mount::hosts` → `args.provided`), and the runtime depends on them. M1 changes this:

- `CordisRuntimePlugin` has no injects and `Mount::hosts` is empty. The runtime depends only on what it needs to start.
- Add runtime-level control operations:
  - `hosts.provide(name, methods)`: the runner registers `ctx.provide(name, hostProxy(name, methods))` and stores the returned disposer.
  - `hosts.withdraw(name)`: invoke the stored disposer.
- Change the host-service table in `Process` from a fixed startup `HashMap` to `Mutex<HashMap<String, Entry>>`. Each `Entry` holds an `Arc<dyn HostDispatch>` and a reference count:
  - When the first row needs a service, insert it in the table and send `hosts.provide`.
  - When the last row releases it, send `hosts.withdraw` and remove it from the table.
  - A `host:<name>` call looks up this table and reports “host service not found” if absent, as it does today.
- Before `rows.load`, `JsRow::apply` does the following for every rutis-gated name from §5.2:
  1. Call `require_as::<dyn HostDispatch>(host_key(n))`.
  2. If the service is a row from this same runtime (`RowService` whose `process` is the same as this row's), skip it. Cordis already has the native provider, and registering another proxy would conflict.
  3. Otherwise, add one reference in `Process` (sending `hosts.provide` if needed) and register the release with this row's cleanup.

  Cleanup calls `rows.unload` before releasing host-service references, so the plugin does not see the service disappear before it unloads.
- If a provider is replaced, dependent rows restart under native rutis rules: release first (the count may reach zero, sending `withdraw`), then register again (sending `provide` with the new dispatch). There is no need to replace a proxy in place.

The runtime no longer waits on any host service. The overall design's concern about runtimes waiting on each other (§4) therefore does not arise in M1: all waits belong to rows.

## 6. Two-stage release

### 6.1 Why a second stage is needed

A row may be resolved before the runtime starts. At that point `InteropResolver` returns a result with no schema and a dependency only on the runtime (the behavior from #109); it does not cache this result. If the row starts as soon as the runtime appears, its `inject` declarations may not yet be dependencies, and the plugin could run before `llm` is ready.

### 6.2 `RuntimeRowsPlugin`

Place this plugin in the `interop` module of rutis-loader, where it can see both `Loader` and `CordisRuntime`:

```text
CordisRuntimePlugin   provides CordisRuntime                     (stage one)
RuntimeRowsPlugin     depends on CordisRuntime + Loader
                      refreshes row resolutions, then provides CordisRuntimeRows (stage two)
Row (JsRow)            depends on CordisRuntimeRows + shared services
```

`RuntimeRowsPlugin::apply`:

1. Get from `InteropResolver` the row names that need refreshing: those resolved previously without a schema (`meta.schema` was `unavailable`), and those whose `package.json` `version` differs from the previous resolution. The version is recorded in `meta.version`.
2. Clear those names from the resolver cache.
3. In a task, call `Loader::reload(id)` for each such row in sequence, then provide `CordisRuntimeRows` after all reloads finish. The task is registered for cleanup by the plugin: unloading or restarting the plugin cancels it, so the service is never provided by a cancelled task.

Use a task instead of waiting directly inside `apply` because `reload` needs the loader's operation lock, and the runtime may be restarting in the middle of a reconcile. Waiting inside `apply` could deadlock with that reconcile. With a task, `apply` returns immediately; rows wait until `CordisRuntimeRows` appears. Reloading does not start them, and rebuilding fibers after dependency declarations change does not start them either.

If refreshing one row fails to resolve, that row fails according to existing loader rules. Other rows are released as usual, and `CordisRuntimeRows` is still provided.

### 6.3 Application setup

```rust
let mut catalog = ServiceCatalog::new();
catalog.register_shared("llm").register_shared("weather");

root.provide_as::<dyn HostDispatch>(host_key("llm"), Arc::new(llm))?;
let runtime = CordisRuntimePlugin::new(node_package, anchor);
let resolver = Arc::new(InteropResolver::new(runtime.handle()));
root.plugin(runtime);
let options = LoaderOptions { catalog, ..LoaderOptions::default() };
root.plugin(LoaderPlugin::new(Chain::new().with_shared(resolver.clone()), options)).await?;
root.plugin(RuntimeRowsPlugin::new(resolver));
```

`RuntimeRowsPlugin` must share the same `InteropResolver` as the loader so it can use its cache. `Chain` therefore adds `with_shared(Arc<dyn Resolver>)`; internally, `Chain` already stores `Arc<dyn Resolver>`.

## 7. Session layer: cross-session forwarding

This is the only substantial session-layer change in M1 and is required for two runtimes to call each other in M2. M1 has only one runtime, and a Rust plugin that holds a JS callback passes it back to the same session. M1 covers forwarding with a second Node runtime started by the tests (§9).

### 7.1 Forwarding references with relay objects

Today `encode` rejects a remote reference from another session (`rpc.rs`: `cross-session reference forwarding is not implemented`). Change it as follows:

- When encoding a `Remote(import)` from another session, export a **local relay object** in this session with the same kind as `import.kind`:
  - Function: invoke `import` when called.
  - Future: await `import`.
  - Object: forward method calls and property reads to `import`.
- The relay object holds `Arc<Import>`. When the peer releases the relay (its reference count reaches zero), drop the relay and therefore the `Import`, sending `release` to the original session. Reference counts propagate naturally along the relay chain; no new frame is needed.
- If the same import is forwarded to the same session more than once, reuse one relay (tracked in this session's export table by `(session, import pointer)`). The peer sees the same reference, so identity comparisons still work.
- When forwarding back to the original session, do not create a relay: if the import belongs to the destination session, encode it with `home: true`, as today. The same applies to a chain of relays: unwrap to the innermost reference before checking.

### 7.2 Rewriting call paths

Give each `Connection` a process-unique session tag (for example, `s3`). Rewriting occurs only in cross-session forwarding code such as relays and `RowService`, using one function:

```text
rebase(path, from, to):
  untagged entry       → add the from-session tag ("s1/node:3", "s1/rust:7")
  entry tagged for to  → remove the tag, restoring its original call ID in to
  entry with other tag → keep unchanged
```

- Before forwarding, rewrite the current call chain (`current_path()`) from the source session to the destination session, then use it as the outgoing call's `path`.
- A tagged entry can never match an untagged call ID in a session, so it cannot be mistaken for a call in that session.
- On return to the original session, that session's entries are restored. It recognizes that the reverse call belongs to a call it is synchronously waiting for, and runs the callback on the waiting thread.

Example: Python synchronously calls `host:weather` (Python session's `node:3`), and Rust forwards the call to Node. While executing it, Node synchronously calls a function passed by Python (Node session's `node:9`). When the callback is relayed back to Python, its `path` is `["node:3", "s2/rust:5", "s2/node:9"]`. Python recognizes `node:3` as the call it is waiting for and executes the callback nested on the main thread, avoiding deadlock.

Also check `rpc.rs` code that relies on call-ID formatting. `related.sort_by_key(... strip_prefix("node:") ...)` sorts only calls received by this session, so those entries are untagged and unaffected. `receive` validates only the frame's `id`, not `path`, and is unaffected too.

## 8. Versions and compatibility

- The frame format is unchanged; `PROTOCOL` remains 2.
- Release the `rutis-interop` crate and npm package together as 0.3.0.
- The runner adds `features: ["rows.v2", "hosts"]` to the `mount` reply. In row mode, Rust checks for `rows.v2`; if missing, it reports that the Node runtime is too old and requires `@arcships/rutis-interop >= 0.3.0`. It does not guess the format of `rows.schema`.
- Build-time generated static mounts are unaffected: they do not use `rows.*` and can still register host services once through `Mount::hosts`. `Mount::hosts` remains available; `CordisRuntimePlugin` simply no longer uses it.
- Migration notes must explain these behavior changes:
  - `CordisRuntimePlugin::host` no longer makes the runtime wait for a host service; the row that uses the service waits for it.
  - For a JS row to use a rutis service, register its name with `register_shared` in the catalog and include it in the plugin's `inject`. A name present only in row configuration `inject` remains gated by Cordis.
  - The application must install `RuntimeRowsPlugin`; otherwise rows wait indefinitely for `CordisRuntimeRows`. Diagnostics identify this missing dependency.

## 9. Tests

Add these next to the existing tests. JS plugin fixtures go in `crates/rutis-loader/tests/fixtures`.

| Test | Location | Verifies |
| --- | --- | --- |
| Project row services into rutis | `rutis-loader/tests/interop_rows.rs` | A JS row declares `provides.weather` in `package.json`; a Rust row with `inject: [weather]` can call it synchronously and asynchronously; the Rust row stops when the JS row unloads |
| Plugin-declared dependencies participate in gating | Same | With plugin `inject = ['llm']` and `llm` registered as shared, the row stays Pending when the host does not provide `llm`, starts when it is provided, stops when withdrawn, and resumes when provided again |
| Resolve first, start runtime later | Same | Reconcile before runtime startup; after startup the row gets `inject` through the second-stage refresh and does not execute until `llm` appears |
| Unregistered names remain Cordis-gated | Same | Two JS rows: one provides unregistered `foo`, the other injects `foo`; both start and rutis does not gate `foo` |
| Avoid duplicate proxy registration within one runtime | Same | JS row A provides shared `weather`; JS row B injects it and receives A's native object; `host:weather` is not called |
| Host-service reference count | `rutis-interop/tests/host_services.rs` | Two rows use one host service: Node retains the proxy after the first row unloads and withdraws it after the second unloads |
| Reference forwarding | `rutis-interop/src/rpc/tests.rs` | A function, Future, and object from session A are forwarded through Rust to session B; B's calls, awaits, and property reads reach A; after B releases the reference, A receives `release`; forwarding the same reference twice preserves identity at B |
| Call-path rewriting | `rutis-interop/tests/rpc_callbacks.rs` | Start two Node runtimes with the same call ID (`node:1`) simultaneously waiting synchronously; A calls B through Rust, and B synchronously calls a function passed by A; the callback runs on A's waiting thread without deadlock or being mistaken for B's call |
| Old runner | `rutis-interop/tests/error_shape.rs` | If the reply has no `features`, row mode reports that the runtime version is too old |

The local Node version is 22.x, and several known tests (`node_sync_wait…`, etc.) also fail on main. Use CI's Node 26 as the baseline.

## 10. Split into pull requests

In dependency order, target each PR directly at `main` rather than stacking branches:

| PR | Contents | Dependency |
| --- | --- | --- |
| M1a | Session layer: session tags, `rebase`, relay objects, unit tests, and two-runtime tests (§7) | None |
| M1b | Runner and `Process`: new `rows.schema` format, `services` in `rows.load`, `hosts.provide` / `withdraw`, dynamic host-service table, `features`; `HostDispatch::methods`; version 0.3.0 | None; can proceed in parallel with M1a |
| M1c | rutis-loader: `register_shared`, row dependencies, row service projection, per-row host services, `RuntimeRowsPlugin`; remove host-service dependency from `CordisRuntimePlugin`; migration notes | M1b |

M1a is not on M1c's path: M1 has one runtime, and cross-session forwarding is not used in practice until M2. It is done first because it has the most risk and benefits from early testing.

## 11. JS/TS leaf plugins (alongside M2)

After M1, JS plugins are still Cordis plugins. Many JS/TS plugins only need to use a few services, provide one service, and clean up on exit; they do not need Cordis subplugins, events, or a local dependency graph. These plugins can use the same leaf style as Python, so authors do not need to know Cordis.

### 11.1 Authoring

```ts
import { definePlugin } from '@arcships/rutis-interop/plugin'

export default definePlugin({
  inject: ['llm'],
  provides: { weather: { today: 'async', unit: 'sync' } },
  config: { type: 'object', properties: { city: { type: 'string' } } },  // JSON Schema
  apply(ctx, config) {
    const llm = ctx.use('llm')
    ctx.provide('weather', new Weather(llm, config.city))
    return () => { /* cleanup */ }
  },
})
```

- `definePlugin` only marks the object with a `Symbol`; it does nothing else. The marker must be explicit because a Cordis function plugin also has the shape `apply(ctx, config)` plus `inject`, so shape alone cannot distinguish them.
- `apply` may be async; the cleanup function it returns may also be async, and returning nothing is allowed.
- Put `provides` in code; no `package.json` `rutis.provides` field is needed. That field is for existing Cordis plugins whose code cannot be changed.
- `config` is JSON Schema directly; schemastery is not needed.
- Load TS files as usual; the runner already starts with `node --import tsx`.

### 11.2 Implementation: use the existing Node runtime

Do not start another process. When loading a module, the runner detects the leaf marker and wraps it as a Cordis plugin:

```text
Cordis plugin {
  name, inject (copied as-is),
  apply(cordisCtx, config) {
    ctx = { use: name => cordisCtx.get(name), provide: (name, value) => cordisCtx.provide(name, value) }
    cleanup = await leaf.apply(ctx, config)
    cordisCtx.effect(() => cleanup)
  }
}
```

- `ctx` exposes only `use` and `provide`; the plugin cannot access Cordis `Context`.
- `use` returns either a native in-process object provided by another JS plugin or a host-service proxy registered in Cordis by M1 (provided by Rust or another runtime). The plugin does not need to distinguish them.
- `provide` is registered on the plugin's own fiber and withdrawn automatically when it unloads.
- Any configuration change restarts the plugin; leaf plugins have no volatile fields.
- For a leaf plugin, `rows.schema` reads `inject`, `provides`, and `config` directly from the object passed to `definePlugin`.

Leaf plugins and existing Cordis plugins therefore run in the same process. Calls between them do not use IPC, and rutis needs no change to support them.

### 11.3 Share consistency tests with Python

The M2 runtime consistency tests run the same plugin behaviors on each runtime. JS leaf plugins use the same authoring style as Python plugins, so this suite covers both:

- Service use, service provision, and cleanup order.
- Declared dependencies participating in rutis lifecycle gating.
- Cross-language service use (JS leaf ↔ Python).
- In-process calls avoiding IPC (JS leaf ↔ Cordis plugin).

### 11.4 Why not a separate pure-JS runtime

Another option is a separate Node process without Cordis, like Python, to run leaf plugins. It is cleaner in isolation, but adds a process, and calls between leaf plugins and Cordis plugins would use IPC. If isolation is needed, start another runtime instance as specified by the overall design's “add isolation when needed” rule; there is no need for a separate runtime type.

## 12. Open questions

- **Source of shared names:** This document uses explicit catalog registration (§4.2). An alternative is to treat every name declared in any runtime row's `provides` as shared. That avoids registration, but whether rutis gates a row then depends on whether the provider row has already been resolved, making behavior configuration-order-dependent. Do not choose this option. If explicit registration proves too cumbersome, consider having the loader resolve all rows in a reconcile first and then determine dependencies.
- **Optional dependencies:** M1 does not restart a row for them (§5.2). Add this only if a plugin needs `apply` to rerun when an optional service appears.
- **Conflicting service names:** M1 only reports a diagnostic (§5.3); it does not choose a winner.
- **Call-ID prefix:** Keep using `node:`, as in §9 of the overall design.

## 13. Implementation adjustments

| Original wording in this document | Implementation | Reason |
| --- | --- | --- |
| `rows.schema` distinguishes required and optional `inject` entries (§5.1, §5.2) | `inject` is a list of service names; all are treated as required | Cordis 4 has no optional injects (`Inject.resolve` produces only a map from names to interception config) |
| Only report a diagnostic when two rows declare the same service (§5.3) | The later row fails, with an error naming the row that already exported it | The failure is itself a diagnostic and prevents two rutis providers |
| `RuntimeRowsPlugin` uses `meta.schema` to determine which rows to refresh (§6.2) | The resolver records names resolved while the runtime was absent, plus names whose `package.json` version changed (`take_stale`) | Avoid depending on text in metadata |
| `CordisRuntimePlugin::host` records only shape information (§4.1) | Same; the shape reaches rows through `CordisRuntime::host_methods` | — |
| Forwarding interface in §7 | `Connection::tag`, `Connection::forward`, `Connection::forward_async`, `rpc::rebase` | — |

Two items remain for M2:

- `RowService::invoke` currently starts the call directly on the provider's session, without `Connection::forward`. That is correct when the caller is a Rust plugin. When the caller is another runtime (Python ↔ JS in M2), it must use the caller's session for `forward` so the call path is rewritten. This requires `HostDispatch::invoke` to receive the caller's session; M2 will settle the interface.
- When an object reference (with methods and properties) is relayed to Node, Node's `peer.mjs` still does not accept Rust-exported object references. Functions and Futures can be forwarded.
