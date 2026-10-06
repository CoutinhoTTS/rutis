# Design: Compatibility Layer for Mounting Cordis Plugins in rutis

Based on the [requirements](requirements-protocol-plugins.en.md). Baseline: Cordis `4.0.4` (pinned in `interop/node/package-lock.json`). Delivery plan: [roadmap](roadmap-native-plugin-mount.en.md).

**The original TS plugin runs in real Cordis; the rutis side receives Rust types generated at build time and registers them as ordinary rutis services. All mapping happens in the compatibility layer using only public rutis and Cordis APIs.**

## 1. Components

```text
rutis application process                           Node process
+------------------------------+                    +---------------------------+
| ctx.plugin(bindings::Plugin) |                    | runner.mjs                |
|   Projection                 |                    |   real Cordis Context     |
|   -> provide_mut_as / replace|  Unix socket       |   original TS plugin      |
|   Process + rpc::Connection  | <================> |   Peer (client.mjs)        |
|                              |  newline JSON frames|   I/O Worker               |
+------------------------------+                    +---------------------------+
```

| Component | Location | Responsibility |
| --- | --- | --- |
| Generator | `interop/node/src/generate.mjs`, called by `rutis_interop::build::cordis_plugin` from `build.rs` | Use the TypeScript checker on the original plugin; generate Rust Config, service proxy types, and mount plugin |
| Mount plugin | Generated code | Start Node, register cleanup effect, and publish services through `Projection` |
| `Projection` | `crates/rutis-interop/src/projection.rs` | Map Cordis service-slot changes to rutis service registration, replacement, and revocation |
| `Process` / `rpc` | `crates/rutis-interop/src/{process,rpc,protocol}.rs` | Process management, wire protocol, calls, and reference table |
| runner / Peer | `interop/node/src/{runner,client,peer,io-worker,errors}.mjs` | Load one or more original plugins in order, track service slots, and execute calls |

Neither the rutis core nor Cordis is modified for compatibility.

## 2. Build-time generation

**Package-level integration:** The application declares its npm project and mounts (plugin package or TS source, version, group members, host services, events) in `[package.metadata.rutis-interop]` in `Cargo.toml`. `build.rs` only calls `rutis_interop::build::from_manifest()`, and code uses `rutis_interop::include_mounts!()` to include every mount module. The npm project's lockfile is the only source of plugin versions. Builds do not install npm dependencies; a missing package, version mismatch, or runtime-protocol mismatch fails with a remediation command. The runtime package declares `rutisProtocol` in `package.json`, which must equal the crate's `PROTOCOL`; the runtime handshake checks it again. See the [integration guide](../crates/rutis-interop/README.md).

Low-level API: `Bindings::new(module_name, runtime_dir)` plus `.plugin` / `.member` / `.provide` / `.event` / `.emit`. Generated output goes to `OUT_DIR/{module_name}.rs`; `cordis_module`, `cordis_group`, and `cordis_plugin` are shorthand helpers. A plugin may be a TS source file or installed npm package directory. Packages are analyzed through `package.json` `types` and loaded through their runtime entry.

**Combined mounts:** Published plugins are often designed to be composed. For example, `dsh-workspace` depends on services from `dsh-storage`, `dsh-storage-domain`, and a session-persistence implementation. `cordis_group(module_name, &[(name, plugin), ...], interop/node)` generates one binding for the group. Plugins load in the given order into the same Node process and Cordis Context; Cordis resolves in-group dependencies natively. Every member's services are exported to rutis; duplicate service names fail at build time. Each member's config is a field in the combined `Config`, named after the given member name.

**Service discovery**

| Plugin form | Service source |
| --- | --- |
| Default export is a `Service` subclass (common for published plugins) | Members in `declare module '@deepseek-ai/cordis' { interface Context { ... } }` whose type is that class or its base class. An interface may declare `credentials: CredentialProvider`; an implementation package inheriting it can then be discovered. |
| Exported `apply` function plugin, TypeScript source | Literal service names in `ctx.provide('name', value)`; prefer the Context declaration for type. |
| Exported `apply` function plugin, declaration file only | Context members declared by the package itself. |

Configuration type comes from the `Service` subclass constructor or the second parameter of `apply`. If every field is optional, `Config` implements `Default`.

**Type mapping**

| TypeScript | Rust |
| --- | --- |
| `number` / `string` / `boolean` / `void` | `f64` / `String` / `bool` / `()` |
| Branded `string & { __brand }` (with alias) | Same-name newtype, `#[serde(transparent)]`, convertible from `&str` |
| String literal union | Same-name enum with variants renamed to original strings |
| Data interface / object literal | Same-name struct; snake_case fields, serialized under original names; defaults follow the next rows |
| Required `T \| null` | `Option<T>`; `None` sends `null` |
| `T \| undefined`, optional parameter/field `x?: T` | `Option<T>`; `None` sends `undefined` (field omitted) |
| Optional and nullable `x?: T \| null` | `Option<Option<T>>`: `None` omits, `Some(None)` sends `null`; decoding distinguishes absent and null |
| Array / readonly array | `Vec<T>`; borrow as `&[T]` in argument position |
| `Record<string, T>` | `BTreeMap<String, T>` |
| `any` / `unknown` and other pure-data unions (such as discriminated object unions) | `serde_json::Value`: data passes through without generated static structure. If it contains a live object/function, decoding errors; internal reference markers are never returned as data. |
| Union of live objects (`Left \| Right`) | `ObjectRef`: retain reference and wrap as a member proxy on demand (`Left(object)`); generate each member proxy. |
| Union of live object and data (`Session \| SessionId`) | Same-name `untagged` enum; reference variants first; multiple live-object members share one `Object(ObjectRef)` variant. |
| Live object: interface/object with methods or class instance (e.g. `Workspace`, `SessionHandle`) | Same-name proxy struct around `ObjectRef`; data properties generate getters that read current values; methods follow service-method rules; usable inside data structures, arrays, and parameters; proxies for the same object compare equal; passing back to Cordis restores the original object. |
| Function parameter (callback) | `impl Fn(args...) -> Result<T>`; Promise-returning callback uses `BoxFuture`, and `void \| Promise<void>` is treated as async. Cordis may call immediately or retain it for later. Callback receives the original function and `AbortSignal` protocol values. Optional callback parameters are not yet supported. |
| Returned function (e.g. unsubscribe function) | `RemoteFunction`: `call` invokes synchronously; `call_async` awaits the returned Promise; function stays on Cordis side. |
| JS `Error` value (e.g. an error received by a callback) | `JsError { name, message, stack }` |
| JS built-ins (`Map`, `Set`, iterators, etc.) | Not bound |

Methods: synchronous methods stay synchronous; Promise-returning methods become `async fn`; all return `Result<T, rutis_interop::Error>`. Optional arguments pass `None` as JS `undefined`; required nullable arguments pass `None` as `null`. Generated internal variables use the `__rutis_` prefix so plugin argument names cannot collide. Rust signatures omit `AbortSignal`: Cordis receives a real signal, and dropping the returned future (for example on `tokio::time::timeout`) cancels the call and aborts it. A sync method cannot be cancelled mid-call, so its signal never aborts. `AbortSignal` fields inside options objects are unsupported and are not passed.

Service properties also generate getters and are read live through the control operation `get(handle, property)`.

**Unsupported members:** overloads, generic methods, optional callback parameters, `Uint8Array`, streams, JS built-ins, etc. do not generate corresponding methods. Each is reported at build time with `cargo:warning`, source location, and reason, and listed in the service type's doc comment. Other plugin members are still generated; one unsupported member does not fail the entire plugin.

## 3. Wire protocol

One bidirectional Unix socket, newline-delimited JSON, protocol version 1.

| Frame | Meaning |
| --- | --- |
| `hello { version }` | Handshake; no other frames accepted before completion |
| `invoke { id, path, target, method, args }` | Call a service method (`target` is a handle) or a control operation (`target` is empty) |
| `call { id, path, reference, method?, args }` | Call a function reference received from the peer; when `method` is present, call a method on an object reference |
| `get { id, path, reference, property }` | Read a live property from an object reference |
| `await { id, path, reference }` | Await an async result received from the peer |
| `return { id, value }` / `throw { id, error }` | Return value / error |
| `release { reference, count }` | Return a number of reference grants |
| `cancel { id }` | Caller abandons invocation `id`: abort callee's `AbortSignal` or stop waiting for async result; discard and count late responses |

- **Values:** `undefined`, JSON data, lists, records (ordinary objects whose fields may contain references), and references (functions, async results, objects). Functions, Promise/Future values, and live objects (objects with methods or class instances) pass by reference, retaining identity instead of becoming snapshots. The same object is the same reference wherever it appears. Currently only Node exports object references; Rust does not export objects.
- **Rust decoding:** generated types use serde. On decode, object references are first replaced by markers and then recovered by `ObjectRef` deserialization; encoding arguments does the reverse.
- **Reference counting:** sender increments once per grant. Receiver registers ownership on arrival before scheduling execution, and sends `release` for the received grant count after all local proxies are dropped. An old release racing with a new grant cannot delete the new one. Clear all tables on session close; do not wait for GC.
- **Errors:** transmit as object graphs, preserving `name`, `message`, `cause`, and `AggregateError.errors` order, sharing, and cycles.
- **Call chains:** IDs are `rust:n` / `node:n`; `path` records nested calls so reverse calls can run on the side currently waiting synchronously.

### 3.1 Synchronous and asynchronous calls

- A generated sync method waits synchronously for its result on the calling thread. During the wait, reverse calls in the same call chain (for example, JS synchronously calling a Rust callback) run on the waiting thread, so nested sync callbacks work.
- An async method returns a Future; Node returns a Promise. Invoke and await are separate operations: `invoke` returns the async result as a reference, which can later be `await`ed.
- **Wait cycles:** if a synchronous call needs a result that can only be advanced by an executor it is blocking (a timer/Promise on Node's main thread or a Future on a Rust `current_thread` runtime), return `SyncWaitCycle` instead of hanging.
- Adapters may use `Connection::independent_future` to run async work known to be independent of the caller's executor on one background executor per connection. `Send` alone does not mean work may be moved; only explicitly designated work is moved.
- **Cancellation/timeouts:** dropping an async-call future before completion sends `cancel`. Node creates an `AbortController` for calls with a `signal` argument and keeps it until the returned Promise finishes, so cancellation while awaiting also aborts the original signal. Rust stops the corresponding async wait on `cancel`. Discard and count late responses through `Connection::orphans()` instead of treating them as protocol errors that disconnect. Callers implement timeouts with `tokio::time::timeout`; the protocol has no default timeout.

## 4. Service projection

### 4.1 Handles

On Node, maintain a sequence of **object handles** for each exported service slot:

- The first object in a slot uses the service name as its handle (for example `counter`); each replacement gets a new handle (`counter#2`, `counter#3`, …).
- A handle always points to the object it was created for. Method calls locate the object by handle and do not reread the slot.
- When the slot changes, Node notifies Rust through the control call `service(name, handle | null, version)`; `version` prevents stale notifications from overwriting newer ones.

**Reading:** the runner mounts an export fiber for each exported service. It injects the service like an ordinary native consumer and reads it in its own scope.

- Cordis's native gating determines availability, including provider state and `Service.check()`; a failed check means the service is not exported.
- Effects created by service methods through the caller's Context (for example `this.ctx.effect(...)`) belong to the export fiber and clean up with it.
- Cordis creates a new tracking proxy for every read of a `Service` instance. To detect replacement, compare the original object obtained with `Symbol.for('cordis.original')`; read-only calls do not create new handles.

Detect slot changes only through public Cordis entry points:

| Change | Detection |
| --- | --- |
| `ctx.provide` publishes/revokes, provider enters/leaves ACTIVE | `internal/service` event |
| Property assignment `ctx.counter = x` | `internal/set` hook, reread after `next()` |
| Direct `ctx.set('counter', x)` | Cordis sends no notification; after every call to a service method in this process, the compatibility layer rereads the slot, including when the method throws (boundary rule 1). |

### 4.2 rutis side

`Projection` uses only public rutis APIs:

| Slot state | rutis action |
| --- | --- |
| First becomes available | Register proxy with `ctx.provide_mut_as` and retain returned `ServiceWriter` |
| Replaced by new object | Replace through `ServiceWriter::set`; already obtained `Arc` snapshots still point to old object |
| Becomes unavailable | Revoke registration; dependent rutis plugins stop under native dependency rules |
| Available again | Re-register; dependents restart under native rules |

When a notification arrives in a synchronous call chain, process it before the call returns, so after `counter.swap(2)`, `ctx.get::<Counter>()` already returns the new proxy. Async paths may briefly return the old proxy (requirement §4).

Publication rules:

- Publish outside locks; a publication loop already in progress handles reentrant/concurrent changes.
- Do not re-register until revocation completes, avoiding conflict when a service is re-provided immediately after revocation.
- A failed publication is not marked applied; keep the slot pending and retry on the next change.
- Do not release a handle currently being published due to a reentrant replacement.

### 4.3 Handle reclamation

Each generated proxy corresponds to a handle. When its last `Arc` is dropped, send `release(handle)`. Node deletes a handle only when it is released and no longer the slot's current object. If a handle is replaced before a proxy is generated, `Projection` releases it directly.

When a mount plugin unloads, `Projection::close()` drops the publisher closure holding `ServiceWriter`, breaking the reference cycle `Process → Projection → ServiceWriter → proxy → Process` so `Process` can be reclaimed.

## 5. Host provides services to Cordis plugins

A mounted Cordis plugin may depend on services supplied by the rutis application—for example, host-provided `systemPrompt`, LLM, or storage. This is the reverse of §4's service-projection direction but still has a rutis application as host; it is not the frozen reverse integration in §8.

**Declaration:** In `build.rs`, use `Bindings::new(module, interop/node).plugin(plugin).provide("systemPrompt").generate()` (for a combined mount use `.member(name, plugin)`). The service interface comes from the Context declarations visible to the plugin: `dsh-persona` depends on `systemPrompt`, whose interface is `systemPrompt: SystemPrompt` from its imported `dsh-system-prompt`. Build fails if the declaration is missing or conflicts with an in-group service.

**Generated items:**

| Generated item | Purpose |
| --- | --- |
| Trait `{Interface}Host` (e.g. `SystemPromptHost`) | Implement in the application. Sync methods are `fn`; Promise-returning methods return `BoxFuture<'static, …>`. Methods default to “host not implemented” errors, so implement only those used by the plugin. |
| `provide_{service}(ctx, host)` | Register implementation as an ordinary rutis service keyed by `dyn {Interface}Host`. |
| `{Interface}HostDispatch` | Route calls from Node to the registered application implementation. |
| Mount plugin's `injects` | Includes these host services; if the rutis side is not ready, the mount waits under native rules. |

**Types:** data arguments and results use §2 mappings. Function parameters arrive by reference as `rpc::Value` and can be called by the host. To return a function (such as an unsubscribe function common in Cordis services), the host returns `rpc::Value::callback(...)`; Cordis receives a callable function. Function members inside unions (for example `text: string | ((ctx) => string)`) are not bound; bind only data members and emit a build warning. Other unsupported members follow §2: warn at build time, omit the trait method, and return a clear “not provided by the rutis host” error if Cordis calls them.

**Node side:** before loading plugins, the runner registers a proxy object with `ctx.provide(name, proxy)`. Proxy methods call Rust through the protocol (control target `host:{service-name}`); Cordis resolves dependencies natively. This proxy is not an instance of the class declared by the plugin (boundary rule 7).

**Changes and cleanup:**

- When a rutis-side service is revoked, the mount plugin stops as a dependent under native rutis rules and its Node process closes. Re-providing restarts the mount. The first version does not replace individual host services in place.
- Native dependency rules on both sides ensure cleanup order: the host provider remains until the dependent mount cleans up. During mount cleanup, Cordis plugins unload first and may still call the host's unsubscribe function.
- A JS synchronous call to a host service blocks the Node main thread while Rust responds; use §3.1 sync rules and `SyncWaitCycle` detection.
- Known difference: if a plugin registers outside an effect and discards the returned unsubscribe function (for example, `dsh-persona` calls `suppressRuntimeContext()` while closing a runtime context), native Cordis ties it to the plugin lifecycle and automatically revokes it. A host implementation does not know the caller plugin's lifecycle and must handle it itself, at latest by revoking all registrations on mount unload.

Acceptance: `examples/dsh-baseline/tests/host.rs` uses `dsh-persona`: mount waits until host service is provided; the plugin registers two prompt fragments; revoking the service stops the plugin and removes fragments; re-providing recovers; on unload Cordis calls the host's returned unsubscribe function. Protocol test: `crates/rutis-interop/tests/host_services.rs`.

## 6. Lifecycle and failures

- **Startup:** the mount plugin starts Node, handshakes, then sends `mount`. Node loads original plugins and waits for `fiber.await()`. The runner uses the Cordis instance resolved by the plugin itself (the plugin's `Service` subclass and `Context` must come from the same module instance). A plugin entry may export `apply` or default-export a `Service` subclass. Original plugin startup failure fails the mount and registers no services. One Node process runs only the plugins in this mount (one or a group). rutis cannot yet provide services back to them during startup, so unsatisfied required in-group dependencies never become ready: fail the mount directly and list which plugin lacks which services. After mount, a service that becomes unavailable (for example, revoked by its plugin) is revoked according to §4.2.
- **Cleanup order:** mount plugin registers its cleanup effect before registering services. rutis cleans effects in reverse order: revoke services first, run consumer disposers while remote services are still callable, then close Node.
- **Cleanup before drain:** `dispose` starts Cordis plugin unload and in-flight-call drain together; do not wait for calls first, because a disposer may be what releases an in-flight wait.
- **Failure:** after Node exits or connection closes, all in-flight and later calls return `Transport` with how the process ended (for example `Cordis process exited with exit status: 17` or `… signal: 9 (SIGKILL)`; `Process::exit_status()` is also available). Do not retry or return default values; a call sent without a response has an unknown outcome.
- **Services after crash:** when the connection closes, revoke all projected services as in §4.2. rutis dependents stop and wait under native gating. The mount plugin itself remains Active but provides no services; the application decides whether to remount (unload, then mount again; services recover and consumers restart). The compatibility layer does not restart automatically. A separate thread reads process exit status without reaping the process, so errors include exit details even if a `current_thread` runtime is blocked in a synchronous call.

## 7. Events

**Cordis → rutis (implemented):** in `build.rs`, use `Bindings::event("name")` to select forwarded events.

- **Generation:** generate one Rust struct per selected event (fields are event parameters, types follow §2), implementing `rutis::Event` (`NAME` is event name, `Value = ()`) and `from_args`. rutis plugins subscribe natively, e.g. `ctx.events().on(&ctx, &EventKey::<CredentialsRecordUpdated>::of(), listener)`.
- **Notifications only:** accept only events returning `void`. Reject waterfall, bail, and other return-valued events at build time: forwarding listeners answering for rutis would change the original event chain. The removed `rutis-cordis` bridge was tested with dsh; passive waterfall subscription interrupted the whole chain. Do not forward `internal/*` events.
- **Node side:** after original plugins start, the runner registers a forwarding listener in Cordis for each selected event, sends arguments to Rust, and returns a Promise resolved after rutis-side handling. Cordis `emit` ignores the Promise (fire-and-forget, boundary rule 2); `parallel` / `serial` wait for it. Ignored rejections do not become unhandled rejections.
- **Rust side:** decode arguments as the event type, then emit through rutis `parallel` from the mount plugin's Context.
- **Ordering:** forwarding listener registers after original plugin startup. Cordis order is therefore startup-registered listeners → rutis group → later listeners; the group uses native rutis order (boundary rule 3). Events emitted during original plugin startup are not forwarded.

**rutis → Cordis (implemented):** use `Bindings::emit("name")` to select notification events sent from rutis to Cordis. This is needed for full host services: Cordis plugins depending on a rutis-provided service often listen to its events (for example `system-prompt/change`, `credentials/record-updated`).

- **Generation:** event type is as above, with `to_args`. The mount plugin registers an `EmitToCordis` listener on the rutis bus; it belongs to the mount and unloads with it.
- **Node side:** runner receives the event and emits through Cordis Context `parallel`; only event names declared at mount are accepted.
- **Semantics:** rutis `parallel` waits for Cordis listeners; rutis `emit` is fire-and-forget under native rutis queue semantics.
- **Prevent loops:** an event name may be selected in only one direction; selecting both fails the build, so forwarding cannot loop.

## 8. Reverse direction: Cordis app mounts rutis plugin (frozen)

The old bridge where dsh hosted Rust (`rutis-cordis` + `host/`) has been removed; the dsh interface now runs under a rutis host through this compatibility layer (`crates/rutis-dsh`). Keep this implementation and its tests, but add no more capabilities:

- `rutis_interop::build::rutis_plugin` uses syn to parse one entry source file and generates Rust export dispatch, a Cordis mount plugin (`rutis.mjs`), and Context declarations (`rutis.d.mts`).
- Supports only concrete public plugins in one file, `&self` methods, and basic types; module declarations, macros, generics, borrows, and public fields fail.
- Exported objects are captured at mount and do not follow replacements.

## 9. Engineering safeguards

- **Complete:** cancellation propagation and timeouts (§3.1), discard/count late responses, frames use a dedicated socket (plugin stdout cannot corrupt them), and all in-flight calls fail clearly on disconnect.
- **Protocol version:** runtime package declares `rutisProtocol`; build compares it with crate `PROTOCOL`, and runtime handshake checks again. There is only one protocol version; no multi-version negotiation.
- **Deferred:** log method names for each in-flight call on disconnect; add when needed for debugging.

## 10. Unsupported or impossible capabilities

| Capability | Reason | Handling |
| --- | --- | --- |
| Immediate visibility after direct `ctx.set` replacement | Cordis sends no notification | Visible after next call (boundary rule 1) |
| `instanceof` host-provided service on Cordis side | Proxy is not an instance of plugin-declared class | Calls through declared interface are consistent (boundary rule 7) |
| Sync call waits for result requiring the blocked thread | Single-thread event loop / `current_thread` cannot reenter | Return `SyncWaitCycle` (boundary rule 6) |
| rutis listeners finish before Cordis `emit` returns | Cross-process language-stack difference | Fire-and-forget (boundary rule 2) |
| Individual interleaving of listeners across sides | Each side has a separate listener table | Forward as groups (boundary rules 3, 4) |
| Same-name event contracts with different return meanings | Framework contracts differ | Convert explicitly by signature; report incompatibility if impossible |
| Unsupported members (binary, streams, generic methods, optional callbacks, JS built-ins, etc.) | Generator or protocol not implemented | Warn at build time and skip member; generate the rest; track on roadmap |
| Rust-side object passed to Cordis by reference | Rust does not export object references | Not supported; host services may return objects obtained from Cordis |

Do not build: shared-memory version pages, commit-boundary auditing, a unified cross-framework event queue ([#74](https://github.com/arcships/rutis/pull/74), no longer pursued), rustdoc/compiler extraction of complete interfaces from Rust source, or distributed GC.
