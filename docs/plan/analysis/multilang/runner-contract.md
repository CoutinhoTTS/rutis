# rutis-interop: the runner contract as implemented (protocol 1)

Research note for adding Python / PowerShell / Bash / AppleScript runners. Status: describes `main` at
`781c109` (worktree `multilingual-expansion-research-3f78c4`). Nothing in the repo was modified.

## 0. Scope, sources and method

"Runner" = the child process that the Rust side (`rutis_interop::Process`) spawns and talks to over a Unix
socket; today that is `interop/node/src/runner.mjs` (+ `client.mjs`, `peer.mjs`, `io-worker.mjs`,
`errors.mjs`, `wire.mjs`). This document specifies what the Rust side *actually* requires of that process,
derived from the code, and separates language-neutral parts from Node/JS/Cordis-specific ones.

Path abbreviations used below:

| Short | File |
| --- | --- |
| `lib.rs`, `protocol.rs`, `rpc.rs`, `objects.rs`, `process.rs`, `projection.rs`, `events.rs`, `server.rs`, `build.rs`, `build/rust.rs` | `crates/rutis-interop/src/…` |
| `runner.mjs`, `peer.mjs`, `client.mjs`, `io-worker.mjs`, `errors.mjs`, `wire.mjs`, `generate.mjs` | `interop/node/src/…` |

Verification beyond reading:

1. **Live trace.** A ~200-line Python stand-in for the Rust side (`scratchpad/fake_host.py`) drove the real,
   unmodified `runner.mjs` (copied from this worktree into the scratchpad, `node_modules` symlinked from the
   main checkout, Node v22.13) through every frame type: mount, sync/async service calls, a slot replacement,
   handle release, a Rust callback, sync + async host-service calls, cancellation, forwarded events in both
   directions, live objects, records, error graphs, dispose. Mount failure, bad handshake version, bad id
   prefix, repeated id and host disconnect were traced too (`fake_host_failures.py`). Traces:
   `scratchpad/trace.txt`, `scratchpad/trace-failures.txt`; an abridged copy is in Appendix A. This also
   demonstrates that the wire protocol can be driven from a non-Rust peer.
2. **Serde acceptance.** `protocol.rs` was copied verbatim into a scratch crate (`scratchpad/serdecheck`, same
   serde 1.0.229 / serde_json 1.0.151 as `Cargo.lock`) and fed edge-case JSON (Appendix B).
3. **Branch note.** The main checkout is on `feat/loader-interop` (open stacked PR for rutis-loader), which adds
   `rows.*` control operations and `Mount::anchor`; they are *not* on main. Summarised in Appendix C because a
   generic runner would eventually need them.

---

## 1. Process launch

### 1.1 What the Rust side does

All launch variants (`Process::launch`, `launch_observed`, `launch_group`, `launch_mount`) funnel into
`Process::mount(node_package, Mount)` (`process.rs:237-295`). The generated mount plugin calls
`Process::mount` directly (`generate.mjs:1003-1013`).

```rust
// process.rs:323-342
let directory = tempfile::Builder::new().prefix("rutis-mount-").tempdir()…;
let socket = directory.path().join("peer.sock");
let listener = tokio::net::UnixListener::bind(&socket)…;
let mut child = tokio::process::Command::new("node")
    .arg("--import").arg("tsx")
    .arg(node_package.join("src/runner.mjs"))
    .arg(&socket)
    .arg(plugin)                       // the FIRST plugin entry of the mount
    .current_dir(node_package)
    .stdin(Stdio::null())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .spawn()…;
```

| Aspect | Behaviour | Where |
| --- | --- | --- |
| Executable | Literal `"node"`, resolved via `PATH` of the host process. Requires Node ≥ 26 per `package.json:17-19` (the trace ran fine on v22.13). | `process.rs:330` |
| Arguments | `--import tsx <runtime>/src/runner.mjs <socket path> <first plugin entry>`. `tsx` lets `.ts` plugin sources load; it is resolved relative to the cwd. | `process.rs:331-335` |
| cwd | The runtime package directory (`node_package`; generated code passes `<npm>/node_modules/@arcships/rutis-interop` or the `runtime` override). | `process.rs:336`, `generate.mjs:1004` |
| Environment | Inherited unchanged; nothing is added. (`RUTIS_INTEROP_ROOT` is read by the generated *Rust* code, not by the runner.) | `process.rs:330-342`, `lib.rs:15-25` |
| Socket | Unix *stream* socket at a filesystem path `$TMPDIR/rutis-mount-XXXX/peer.sock`. **The Rust side binds and listens; the runner connects** to the path given as its first argument. No fd inheritance. Exactly one connection is accepted. The temp dir lives as long as the `Process` (`_directory`). | `process.rs:323-329, 343-350, 234` |
| stdio | stdin = `/dev/null`; stdout/stderr inherited (plugin logs go to the host's terminal; frames never use stdio). | `process.rs:337-339` |
| Process group | Not changed: the runner shares the host's process group/session (terminal SIGINT reaches both). Grandchildren spawned by plugins are never reaped or killed. | `process.rs:330-342`; requirements §7 |
| Kill | `Process` drop ⇒ `Child._kill` (a `oneshot::Sender`) drops ⇒ the watcher task calls `child.start_kill()` (SIGKILL) and reaps. `kill_on_drop(true)` additionally kills if the tokio task itself is dropped (runtime shutdown). | `process.rs:105-148, 495-499` |
| Exit watcher | A dedicated OS thread `rutis-interop-exit` blocks in `waitid(P_PID, pid, WEXITED \| WNOWAIT)` (observes without reaping) and stores a description in a `Mutex+Condvar`; a tokio task reaps via `child.wait()` and publishes the same string on a `watch` channel. Reason: the socket reader thread must be able to put the exit status into errors even when a `current_thread` runtime is blocked in a synchronous call. | `process.rs:116-142, 195-226` |
| Connect race | `select!(listener.accept(), child.wait())`: if the runner exits before connecting ⇒ `Error::Transport("Cordis process exited before connecting: <status>")`. **No timeout**: a runner that neither connects nor exits hangs the mount forever. | `process.rs:343-350` |
| After accept | Stream converted to a blocking `std` stream; `Connection::connect_with` spawns the reader thread `rutis-interop-reader` and **immediately writes `hello`**, then `peer.ready().await` waits for the runner's hello. | `process.rs:351-366`, `rpc.rs:587-631` |
| Platform | Everything except `build` is `#[cfg(unix)]`. | `lib.rs:37-58` |

Exit status strings (`describe`, `process.rs:182-188`): `exited normally`, `exited with exit status: 17`,
`exited with signal: 9 (SIGKILL)`, `cannot be waited for: …`. They surface as
`Error::Transport("Cordis process <status>")` and via `Process::exit_status()` (`process.rs:484-486`).

### 1.2 What the (Node) runner does at startup

1. `const [socketPath, pluginPath] = process.argv.slice(2)` (`runner.mjs:5`). `pluginPath` (first entry) is
   only used to find the Cordis module instance the plugins resolve (`runner.mjs:9-13`) and as a legacy
   fallback when `mount` carries no `plugins` (`runner.mjs:143`).
2. Create the Cordis `Context`, register the slot-change hooks `internal/service` and `internal/set`
   (`runner.mjs:15, 97-102`).
3. `Process.connect(socketPath, dispatch, settled)` (`runner.mjs:233`, `client.mjs:54-58`): a Worker thread
   (`io-worker.mjs:31-33`) connects to the socket and posts `{ready}`; the main thread then sends
   `{"op":"hello","version":1}` (`client.mjs:38`, `peer.mjs:75`) **without waiting for the host's hello**
   (confirmed by the trace), and awaits the host's hello before resolving.
4. Serve until the session closes; then `closing = true; await dispose()` (`runner.mjs:234-236`). There is no
   explicit `process.exit`: the process ends when its event loop is empty, normally with status 0.

Why the Worker: JS cannot block on a socket. During a synchronous outgoing call the main thread sleeps in
`Atomics.wait` on a shared counter that the I/O worker bumps per frame, and drains frames with
`receiveMessageOnPort` (`client.mjs:19-24`, `io-worker.mjs:10-14`). This is purely a Node implementation
device; a runner that can block on a socket (Python, PowerShell) does not need it.

---

## 2. Handshake and mount sequence

### 2.1 Sequence (from the live trace, ids as the Rust side would produce them)

```
R→N {"op":"hello","version":1}                       (both sides send hello unprompted)
N→R {"op":"hello","version":1}
R→N {"op":"invoke","id":"rust:1","path":[],"target":"","method":"mount","args":{"type":"data","value":{…}}}
N→R {"op":"return","id":"rust:1","value":{"type":"reference","value":{"id":1,"kind":"future","home":false,"origin":["rust:1"]}}}
R→N {"op":"await","id":"rust:2","path":["rust:1"],"reference":1}
N→R {"op":"invoke","id":"node:1","path":["rust:1"],"target":"host:clock","method":"watch","args":{"type":"list","value":[]}}   (plugin apply calls a host service)
R→N {"op":"return","id":"node:1","value":{"type":"reference","value":{"id":1,"kind":"function","home":false,"origin":["rust:1","node:1"]}}}
N→R {"op":"return","id":"rust:2","value":{"type":"data","value":{"services":{"counter":["counter",1]}}}}
R→N {"op":"release","reference":1,"count":1}           (Rust dropped its import of the mount future)
```

### 2.2 Version check

* Runtime: each side rejects a `hello` whose `version` differs from its own, or a second `hello`; any other
  frame before the peer's hello is fatal (`rpc.rs:1071-1080`, `peer.mjs:223-227`). The Rust side also refuses
  to *send* requests before its handshake completed (`rpc.rs:748-750`); Node likewise (`peer.mjs:87`).
* Build time: the runtime's `package.json` must declare `"rutisProtocol": 1` equal to `rutis_interop::PROTOCOL`
  (`lib.rs:11-13`, `build.rs:366-388`, `package.json:12`); `peer.mjs:30` reads the same field at run time.
* No negotiation, no capability list; one version only (design §9).
* Observed: a host `hello` with version 2 makes the Node runner print the error and **exit 1** (the
  rejection escapes `Process.connect`'s `ready` await at top level).

### 2.3 The `mount` control call

Sent once, right after the handshake, as `invoke` with `target:""`, `method:"mount"`, and `args` =
`{"type":"data","value":<object>}` built at `process.rs:376-379`:

| Field | Type | Meaning | Built from |
| --- | --- | --- | --- |
| `plugins` | `[{ "entry": <abs path>, "config": <JSON> }]` | Plugins to load **in this order** into one Context. Group member *names* are not sent (they only name `Config` fields on the Rust side). `config` is the serde JSON of the generated `Config` (omitted optional fields are absent). | `Mount.plugins`, `process.rs:319-322` |
| `services` | `{ "<service name>": ["<member>", …] }` | Exported slots and the allow-list of members (methods **and** getter properties) callable / readable on them. | generated manifest, `generate.mjs:481, 1007` |
| `provided` | `{ "<host service>": { "<method>": "sync" \| "async" } }` | rutis services the plugins may depend on. | `Mount.hosts`, `process.rs:308-311`; `generate.mjs:855` |
| `events` | `["<event name>", …]` | Cordis events to forward to rutis. | `Events::names()` |
| `emits` | `["<event name>", …]` | Events rutis may `emit` into the Context. | `Mount.emits` |

Runner algorithm (`runner.mjs:136-185`), in order:

1. Reject a second mount; register each `services` key as a slot (names containing `#` are rejected).
2. **Register host proxies** for `provided` with `ctx.provide(name, proxy)` *before* any plugin loads.
3. Check all plugins resolve the same Cordis module; `import()` each entry; plugin = module if it exports
   `apply`, else `module.default ?? module`; `ctx.plugin(plugin, config)` in order.
4. Create one *exporter fiber* per slot (`inject: [name]`).
5. `await` every plugin fiber and exporter fiber; if a plugin fiber has no `store` (never applied), fail with
   `native plugin dependencies are unresolved: <entry> (<missing services>)`.
6. `refresh()` all slots; `mounted = true` (slot notifications are sent only from now on).
7. Register one Cordis listener per `events` name.
8. Resolve with `{ "services": { "<name>": [<handle or null>, <version>] } }` for every declared slot.

**Success / failure reporting.** The JS `mount` returns a Promise, so the reply is a `future` reference that
Rust awaits (`call_async` → `rpc::settle`, `process.rs:459-468`). A plain-data reply is equally acceptable
(`settle` passes non-futures through, `rpc.rs:390-395`). Failure = the await (or the invoke) gets a `throw`;
`Process::mount` returns `Err(Error::Remote{…})`, the `Arc<Process>` is dropped ⇒ socket closed + SIGKILL.
The `services` key is mandatory in the result: it is decoded as `HashMap<String,(Option<String>,u64)>`
(`process.rs:381-382`); a missing key fails the mount with `Error::Value`.

**What Rust waits for before registering services.** The generated `Plugin::apply` (`generate.mjs:990-1029`):
(1) `Process::mount(...).await?` — i.e. handshake + settled mount result; (2) registers its cleanup effect
*first* (so rutis cleanup withdraws services and runs consumers' disposers before disposing the process);
(3) `events.attach(ctx)`; (4) registers `EmitToCordis` listeners; (5) `projection.attach(ctx, process)`, which
reads the current handle of every slot (`process.service(name)`) and publishes them with
`ctx.provide_mut_as` (`projection.rs:110-131`). Host services appear in the plugin's `injects`
(`generate.mjs:981, 985`), so the **runner process is not even launched until all host services exist in
rutis**.

---

## 3. Frames

### 3.1 Framing

* One bidirectional Unix stream; each frame is one compact JSON object followed by `\n`
  (`rpc.rs:738-745`, `wire.mjs:1-6`). The reader uses `BufRead::lines()` (`rpc.rs:615`): invalid UTF-8 ends
  the session as if disconnected.
* `#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]` (`protocol.rs:88-139`): unknown fields
  are **fatal** (Appendix B). Field order is irrelevant.
* A malformed or rejected frame closes the whole session on the Rust side (`rpc.rs:620, 624-626`) and on the
  Node side (`#fault`, `peer.mjs:82, 264`).

### 3.2 Frame catalogue

| `op` | Exact JSON shape | Sent by | Reply | Notes |
| --- | --- | --- | --- | --- |
| `hello` | `{"op":"hello","version":1}` | both, once, first | none | `version` must be a JSON integer. |
| `invoke` | `{"op":"invoke","id":S,"path":[S…],"target":S,"method":S,"args":V}` | both | `return`/`throw` | `target:""` = control op (§5); `target:"<handle>"` (R→N) = service method; `target:"host:<name>"` (N→R) = host service. |
| `call` | `{"op":"call","id":S,"path":[S…],"reference":N,"args":V}` and, with method, `{…,"method":S,…}` | both (without `method`); **only R→N with `method`** | `return`/`throw` | Calls a *function* exported by the receiver, or (with `method`) a method of an *object* exported by the receiver. Rust exports no objects: `call`+`method` or `get` received by Rust is fatal (`rpc.rs:1142-1146`). `"method":null` is accepted as absent. |
| `get` | `{"op":"get","id":S,"path":[S…],"reference":N,"property":S}` | R→N only | `return`/`throw` | Live property read of an exported object. |
| `await` | `{"op":"await","id":S,"path":[S…],"reference":N}` (no `args`) | both | `return`/`throw` | Await a `future` exported by the receiver. |
| `return` | `{"op":"return","id":S,"value":V}` | both | – | `id` echoes the request id. |
| `throw` | `{"op":"throw","id":S,"error":{"name":S,"message":S,"graph"?:G}}` | both | – | Top-level error object allows only these three keys (a top-level `stack` is fatal; put it in `graph`). |
| `release` | `{"op":"release","reference":N,"count":N}` | both | none | Return `count` grants of a reference the *receiver* exported (§4.3). |
| `cancel` | `{"op":"cancel","id":S}` | R→N in practice (Node never sends it; Rust accepts it) | none | Caller gave up on request `id` (§9). |

`S` = string, `N` = JSON integer (never `1.0`), `V` = wire value (§4), `G` = error graph (§4.5).

### 3.3 Request ids

* Format `"<prefix>:<n>"`, `n` = 1, 2, 3… per sender, a positive integer ≤ 2^53−1. Rust allocates
  `rust:<n>` under the writer lock, so ids appear on the wire in increasing order even with many threads
  (`rpc.rs:747-761, 830`); Node allocates `node:<n>` (`peer.mjs:85-90`).
* **The prefixes are bound to the implementation language, not to the role.** The Rust side accepts incoming
  requests only if `id.starts_with("node:")` (`rpc.rs:1165, 1194`) and even `unwrap`s that prefix when sorting
  queued work (`rpc.rs:822`); Node accepts only `/^rust:[1-9][0-9]*$/` (`peer.mjs:247-249`) and validates
  reference `origin` entries against `/^(node|rust):[1-9][0-9]*$/` (`peer.mjs:146`). In the frozen reverse
  direction (Rust plugin served to a Node host, `server.rs`) the ids are still `rust:`/`node:`. ⇒ **A Python
  runner must today number its requests `node:1, node:2, …` to be accepted.**
* Each receiver requires *strictly increasing* sequence numbers (gaps allowed — Node consumes an id when an
  encode fails and sends nothing, `peer.test.mjs:81-92`) (`rpc.rs:1192-1198`, `peer.mjs:248-250`).
  A repeated/lower id is fatal (trace (e)).
* `path` must not contain the request's own `id` (`rpc.rs:1165`, `peer.mjs:247`).
* Reply ids are not sequence-checked, but a `return`/`throw` for an id that is neither outstanding nor
  cancelled is fatal ("response for unknown call", `rpc.rs:1046-1056`; Node: `peer.mjs:231-232`).

### 3.4 `path`: the call chain

Definition. `path` is the list of request ids (both prefixes) of the *incoming* requests being served by the
logical thread of execution that issues the request, outermost first. On receipt the receiver appends the
request's id and executes the request with that list as its "current chain" (`rpc.rs:1168, 1250`;
`peer.mjs:274-278`). Requests issued while executing it therefore carry it.

* Rust: `current_path()` = thread-local `SYNC_PATH` (set by `PathGuard` while executing an incoming request)
  or the tokio task-local `ASYNC_PATH` (set for spawned awaits) (`rpc.rs:412-430, 762`).
* Node: `#syncPath` during synchronous execution, else `AsyncLocalStorage` so async continuations keep the
  chain (`peer.mjs:61, 83, 276-278`). Trace: the plugin's `await` of a Rust future, issued in a Promise
  continuation, carried `["rust:9","node:5"]`.
* For `await`, the sender adds the awaited future's `origin` ids not already present (`rpc.rs:762-772`;
  `peer.mjs:190` `[...new Set([...this.#path(), ...origin])]`).

What it is used for — **synchronous reentrancy**:

* A sender blocked in a synchronous request keeps reading frames. An incoming request whose `path` contains
  the id it is waiting for (or, for a sync `await`, one of the future's origin ids) is executed **inline on
  the blocked thread**; unrelated requests are queued and run later.
  * Rust (`rpc.rs:1199-1237`): the reader thread scans `path` from the innermost id for a `Waiting::Sync`
    waiter and sends the request over that waiter's channel; `request_sync` executes it on the caller's thread
    (`rpc.rs:841-856`). Otherwise it is queued and spawned on the runtime of the target object (callbacks /
    futures: the runtime they were created on) or, for `invoke`, the runtime that created the connection
    (`rpc.rs:1183-1186, 1225-1232`). A new sync waiter also claims already-queued related work
    (`rpc.rs:811-826`).
  * Node (`peer.mjs:196-210, 262-271`): while `#waiting` is non-empty only jobs whose `path` includes a waiting
    id run; the rest stay queued until the outermost sync wait ends.
* **SyncWaitCycle detection** — a sync waiter needs a result that only the executor it is blocking could
  produce:
  * Node: an `await` for a still-pending Promise arriving while Node is in a sync wait is answered with
    `throw {name:"SyncWaitCycle", message:"await requires the Node thread occupied by its parent synchronous
    call; path: …"}` (`peer.mjs:282`).
  * Rust: an `await` delivered to a sync waiter whose target future lives on the same blocked
    `current_thread` runtime is polled once with a no-op waker; if not ready the future is spawned and the
    await fails with `Error::SyncWaitCycle` (`rpc.rs:324-357, 1252-1258`); a sync `await` also resolves
    related in-flight awaits blocked on its own executor (`rpc.rs:800-810, 836-838`).
  * On the wire a `Failure` named `SyncWaitCycle` *without* graph becomes `Error::SyncWaitCycle`, with a graph
    `Error::Remote{name:"SyncWaitCycle"}` (`protocol.rs:74-86`; test `rpc_callbacks.rs:186-213`).

Consequence for runners: a request issued while serving request X **must** carry `X.path + [X.id]`. If a
runner sends a nested host call with `path: []` while the Rust caller is blocked in a sync call on a
`current_thread` runtime, Rust queues the request onto that same blocked runtime ⇒ **deadlock** (no
timeouts anywhere). On a multi-thread runtime it would run on another worker, losing thread affinity.

### 3.5 Ordering guarantees

* Frames are read and admitted in wire order by a single reader (Rust reader thread / Node I/O worker →
  main thread). Admission resolves reference targets and decodes argument references *before* later frames
  are processed, so `call r` followed by `release r` still runs the call ("admitted call pins its target",
  `rpc/tests.rs:31-106`, `peer.test.mjs:19-35`). A `return` is decoded before routing, so a discarded late
  reply still releases what it granted (`rpc.rs:1082-1090`).
* Execution order of independent incoming requests is **not** guaranteed on the Rust side (separate tokio
  tasks, possibly a multi-thread runtime); hence slot notifications carry a `version` (§6).
* Replies may arrive in any order relative to other requests' replies.
* Node sends the `service` notification caused by a call *before* that call's `return`, with the call's
  path (trace: `node:2` path `["rust:5"]` precedes `return rust:5`); Rust delivers it to the sync waiter and
  applies it before the call returns (design §4.2).

---

## 4. Values

### 4.1 Wire forms

`#[serde(tag="type", content="value", rename_all="snake_case", deny_unknown_fields)]` (`protocol.rs:17-39`):

| Form | JSON | Rust `rpc::Value` (`rpc.rs:27-38`) | Node decode / encode |
| --- | --- | --- | --- |
| undefined | `{"type":"undefined"}` (`"value":null` tolerated) | `Undefined` | `undefined` ↔ `undefined` |
| data | `{"type":"data","value":<any JSON>}` (`value` required) | `Data(Json)` | the JSON as is |
| list | `{"type":"list","value":[V…]}` | `List(Vec<Value>)` | array; **every JS array is encoded as `list`** (`peer.mjs:116`) |
| record | `{"type":"record","value":{"k":V…}}` | `Record(BTreeMap)` | plain object that holds a reference somewhere (`peer.mjs:17-26, 119-121`) |
| signal | `{"type":"signal"}` | `Signal` | a fresh `AbortSignal` for the call being decoded (§9) |
| reference | `{"type":"reference","value":{"id":N,"home":B,"kind":"function"\|"future"\|"object","origin":[S…]}}` | `Reference` | proxy / original (§4.2) |

Encoding details that matter for other languages:

* Node: `undefined` at any value position becomes `{"type":"undefined"}`, but *inside* a `data` payload
  `wire.mjs` turns `undefined` into `null` (`wire.mjs:4`) — e.g. `{a: undefined}` without references crosses
  as `{"a":null}`. Non-finite numbers throw (`wire.mjs:3`). Symbols, bigints, cyclic data, and non-plain
  objects that are not "live" throw `TypeError` (`peer.mjs:32-40`). An `Error` value crosses as data
  `{name, message, stack?}` (`peer.mjs:118`).
* Rust: `Value::json()` maps `Undefined`→`null`, flattens `List`/`Record` (`rpc.rs:47-64`); `Value::list()`
  accepts `List` or `Data(array)` (`rpc.rs:40-46`). `Process::call` sends `data`, generated code sends `list`
  of per-argument values (trace shows both shapes).
* Rust never accepts `signal` in anything it decodes (fatal, `rpc.rs:988`).

### 4.2 References: kinds, `home`, `origin`

| Kind | Exported by Rust | Exported by Node |
| --- | --- | --- |
| `function` | `Value::callback` — Rust closures as callback args, host-returned disposers, generated callback params (`rpc.rs:100-107`) | any JS function (`peer.mjs:98, 103`) |
| `future` | `Value::future` / `independent_future` — async host methods, async callbacks, the `event` ack (`rpc.rs:71-97, 693-711`) | any `Promise` (incl. replies of `mount`/`dispose`/`emit`) |
| `object` | **never** (`rpc.rs:1142`) | "live" values: class instances (prototype ≠ `Object.prototype`/`null`) or plain objects with a function-valued property, excluding built-ins `Date, RegExp, Map, Set, WeakMap, WeakSet, ArrayBuffer, DataView, Error, Promise` (`peer.mjs:9-16`) |

* **`home`** — whose export table `id` indexes. `home:false`: the *sender's* export (receiver imports it,
  counting a grant). `home:true`: the *receiver's own* export travelling back; the receiver resolves it to the
  original object/closure, no grant counted (`rpc.rs:927-939, 995-1006`; `peer.mjs:93-97, 147`). Kind must
  match the export (`rpc.rs:1002-1004`; `peer.mjs:171`).
* **`origin`** — the call chain current when the exported thing was *created* (Rust: `current_path()` in the
  constructor, `rpc.rs:88, 104`) or *first exported* (Node: `[...this.#path()]`, `peer.mjs:104`). Used only to
  extend the `path` of `await` frames and to pick the sync waiter for a future's nested work (§3.4). A repeated
  grant of an existing import must carry the identical `kind` and `origin` or it is fatal
  (`rpc.rs:1019-1021`, `peer.mjs:153`).
* **Identity** — one export id per object while exported (`Exports.identities` by `Arc` pointer,
  `rpc.rs:940-962`; `WeakMap`, `peer.mjs:99-105`); the same remote id maps to one import, so equal ids ⇒ equal
  proxies (`rpc.rs:124-134`). Ids are never reused once an export is fully released (`peer.test.mjs:37-53`).
  Ids are 1…2^53−1 (`rpc.rs:946-950, 1013-1015`; `peer.mjs:101-102, 146`).
* **Laziness** — Rust futures do not start until awaited (`rpc.rs:26`, `81-96`, test
  `rpc_callbacks.rs:151-183`); Node's imported futures are `RemotePromise`s that send `await` only when `then`
  is called (`peer.mjs:42-51`). Consequence: **a runner that never sends `await` for a Rust future never
  causes that Rust work to run** (§8.1).

### 4.3 Reference counting and `release`

* The exporter increments `grants` once **per occurrence** it encodes (`rpc.rs:963-968`; `peer.mjs:112-113`);
  failed encodes roll grants back (`rpc.rs:890-909`; `peer.mjs:125-127, 193`).
* The importer counts received grants per id (`rpc.rs:1016-1041`; `peer.mjs:150-163`). When its proxy dies it
  sends `release{reference:id, count:<all grants it received>}` — Rust on `Import` drop (deterministic,
  `rpc.rs:447-465`); Node only via `FinalizationRegistry` (GC) or explicit `peer.release(proxy)`
  (`peer.mjs:73, 174-186`). In the trace Node released none of the Rust exports during the session.
* The exporter subtracts; the entry disappears at zero. `count == 0`, `count > grants` or an unknown id is
  **fatal** (`rpc.rs:1107-1130`; `peer.mjs:241-245`). Because releases carry counts, an old release crossing a
  new grant cannot delete the export (`rpc/tests.rs:108-184`).
* Session close clears all tables; nothing waits for GC (`rpc.rs:649-686`; `peer.mjs:76-81`).
* Service *handles* (§6) are a separate mechanism with their own `release` control op.

### 4.4 Rust-side decoding of references into generated types (`ObjectRef`)

Generated types use serde. `decode_value` (`objects.rs:162-191`) turns a `Value` into JSON, replacing each
reference by a marker object `{"\u0000rutis:reference": <index>}` (`objects.rs:16, 205-229`; `Undefined` and
`Signal` become `null`), stores the references in a thread-local, and deserializes; `ObjectRef` /
`RemoteFunction` deserialize a marker back to the reference (checking it is an object / function,
`objects.rs:51-59, 89-97`). Afterwards the decoded value is re-serialized in "encoding mode" to collect the
references it actually holds; every occurrence must be held, else `Error::Value("the value holds a live Cordis
object or function that its Rust type cannot represent")`. `arg()` does the reverse for arguments
(`objects.rs:232-273`), producing `Record`/`List`/`Reference` where needed. This is entirely internal to Rust;
runners only see ordinary wire values.

### 4.5 Errors

`Failure` = `{name, message, graph?}` (`protocol.rs:41-48`). Rust-originated errors carry no graph:

| Rust error | Wire `name` | Wire `message` |
| --- | --- | --- |
| `Error::Remote{name,message,graph}` (relayed) | `name` | `message`, `graph` passed through untouched (`error_shape.rs`) |
| `Error::SyncWaitCycle(m)` | `SyncWaitCycle` | `m` |
| `Error::Value(m)` / `Error::Transport(m)` | `BindingError` | `"invalid binding value: m"` / `m` |
| panic or fallible native result (`server::native_error`) | `RustError` | text |

(`protocol.rs:49-73`, `server.rs:14-20`, `rpc.rs:400-411`.)

Node encodes every thrown value with `encodeError` (`errors.mjs:5-27`):

```
{ "name": thrown.name ?? "ThrownValue", "message": String(thrown.message ?? thrown),
  "graph": { "root": GV, "nodes": [NODE…] } }
GV   = {"type":"undefined"} | {"type":"data","value":<JSON primitive>}
     | {"type":"bigint","value":"<decimal>"} | {"type":"number","value":"NaN"|"Infinity"|"-Infinity"}
     | {"type":"reference","value":<node index>}
NODE = {"type":"error","name","message","stack", "cause"?:GV, "errors"?:[GV…]}   // errors only for AggregateError
     | {"type":"array","values":[GV…]} | {"type":"object","values":[["key",GV]…]}
```

Shared and cyclic structure is expressed by node indices. `decodeError` rebuilds only the built-in
constructors `Error, TypeError, RangeError, ReferenceError, SyntaxError, URIError, EvalError` (and
`AggregateError` when `errors` is present), never arbitrary globals (`errors.mjs:3, 29-61`). Rust treats the
graph as opaque JSON. A *value* error (not thrown) crosses as data `{name, message, stack?}` = Rust
`JsError` (`objects.rs:105-121`; generator maps lib `*Error` types to it, `generate.mjs:185-187`).

---

## 5. Control operations

Control operations are `invoke` frames with `target:""`; host services use `target:"host:<name>"`.

### 5.1 Rust → runner (handled by `runner.mjs` `dispatch`, `runner.mjs:187-230`)

| Method | `args` | Result | Semantics | Rust caller |
| --- | --- | --- | --- | --- |
| `mount` | data object (§2.3) | `{services:{name:[handle\|null, version]}}` (usually via a future) | Load plugins, start exporters, report slots. Once only. | `process.rs:374-380` |
| `dispose` | `data null` | `null` (via future) | `closing = true`, then `Promise.all([dispose(), peer.drain()])` — cleanup and drain **concurrently**. Afterwards every `dispatch` throws `plugin is closing` (`runner.mjs:188`). | `process.rs:470-480` |
| `get` | `[handle, property]` | the property value | Live read; `property` must be in the slot's member list, handle must exist. | `process.rs:438-442`; generated getters `generate.mjs:716-719` |
| `release` | `[handle]` | `null` | Rust no longer holds a proxy for `handle`; deleted once also no longer current. Unknown handles are ignored. Sent fire-and-forget on a spawned task. | `process.rs:399-409`, `projection.rs:268-270`, generated `Drop` `generate.mjs:730` |
| `emit` | `[name, [args…]]` (list) | Promise of `ctx.parallel(name, …args)` | Only names declared in `mount.emits`; Rust awaits completion. | `process.rs:430-436`, `events.rs:123-135` |
| *(other)* | – | throw `unknown control method <m>` | | |
| target = handle | method args (array) | method result | Method must be in the slot's member list (`unknown service method <svc>.<m>` otherwise); after the call (or after its Promise settles) slots are re-read. | generated proxies `generate.mjs:720-722` |

### 5.2 Runner → Rust (handled by `Imports`, `process.rs:76-103`)

| Target / method | `args` | Result | Semantics |
| --- | --- | --- | --- |
| `""` / `service` | `[name, handle\|null, version]` (decoded as `(String, Option<String>, u64)`) | `undefined` | Slot change; ignored if `version` ≤ last seen for that slot (`process.rs:61-75`). |
| `""` / `event` | `[name, [args…]]` | a lazy `future` (resolves after rutis `parallel`) or `undefined` (no sink / not attached / unknown route) | Forwarded Cordis event (§8). |
| `host:<name>` / `<method>` | method args | whatever `HostDispatch::invoke` returns (data, `future`, `function`…) | Host service call (§7). Unknown host ⇒ `Error::Value("no host service <name>")`. |
| anything else | – | throw `BindingError` `application has no exported service target` | |

Errors here are per-call `throw` replies (non-fatal), e.g. a float `version` fails tuple decoding and the
notification is lost.

### 5.3 Reverse direction (frozen, for comparison)

In `server.rs` a Rust binary is the *runner* for a Node host: it connects to `argv[1]` (`server.rs:90-97`),
implements `mount {config}` (single plugin, returns `null` via a non-business future) and `dispose`
(`ctx.shutdown()` + `peer.drain()`), and dispatches service calls by service key + Rust method name
(`server.rs:37-81`, `build/rust.rs:331-337`). No handles, versions, hosts or events. It shows the peer layer
(`rpc.rs`) is role-symmetric while the control vocabulary is not.

### 5.4 "Business" calls and drain

A request is *business* unless it is a control `invoke` (`target == ""`) or targets a reference exported from
control context (`rpc.rs:1169-1176`; `peer.mjs:258`; Rust `Value::control_future` vs `future`,
`rpc.rs:71-76`). Business requests (and business futures until settled) are counted; `drain()` waits for the
count to reach 0 (`rpc.rs:719-727`; `peer.mjs:218-219`). The `dispose` reply itself is non-business, otherwise
drain would wait for itself.

---

## 6. Service projection

Runner side (`runner.mjs:23-95`):

* **Slots** are the keys of `mount.services`. Each slot is read through its own *exporter fiber* that
  `inject`s it, so Cordis availability rules (incl. `Service.check()`) gate export, and effects that service
  methods create via the caller's context belong to that fiber (`runner.mjs:39-53`).
* **Handles**: the first object seen in a slot gets the bare service name (`counter`); each later distinct
  object gets `<name>#<generation>` (`counter#2`, …). A handle always addresses the object it was created for
  (`runner.mjs:85-89`); identity is compared on `value[Symbol.for('cordis.original')] ?? value` because Cordis
  wraps services in a fresh tracing proxy on each read (`runner.mjs:30-33`). Tests and the design rely on
  "first handle = service name" (`rpc_callbacks.rs` calls `"callbacks"` directly).
* **Versions**: one global counter across slots, incremented on every slot change (`runner.mjs:20, 90`).
  The mount result carries the version at mount time (0 for a never-available slot).
* **When to re-read slots** (`refresh`): on `internal/service` (provide, withdrawal, provider activation),
  after `internal/set` (property assignment) (`runner.mjs:97-102`); after every service-method call, also when
  it throws, and after its Promise settles (`runner.mjs:221-228`); after every `call`/`get` on an exported
  reference (the `settled` hook, `peer.mjs:67-69, 294-296`, `runner.mjs:233`); when an exporter fiber starts
  or stops. A direct `ctx.set()` emits nothing in Cordis, hence the per-call refresh (boundary rule 1).
* **Notification**: `peer.callAsync('', 'service', [name, handle, version])`, result ignored, only when
  `mounted && !closing` (`runner.mjs:91-93`), sent before the causing call's reply and inside its chain (§3.5).
* **Release**: `release(handle)` marks the handle released; it is deleted when released *and* no longer the
  slot's current object (`runner.mjs:60-65, 209-213`).

Rust side:

* `Imports.update` keeps `(handle, version)` per slot and forwards newer changes to the `ServiceEvents`
  observer (`process.rs:61-75`). `Process::service(name)` returns the current handle (`process.rs:389-397`).
* `Projection` (`projection.rs`) maps slot states to rutis: first handle ⇒ `ctx.provide_mut_as`; new handle
  ⇒ `ServiceWriter::set` (old `Arc` snapshots keep their handle); `None` ⇒ withdraw; re-registration waits
  for the withdrawal; failed publication is retried on the next change; a handle replaced before it got a
  proxy is released immediately (`projection.rs:250-275`).
* Each generated service proxy is `{ process: Arc<Process>, handle: String }` and sends `release(handle)` on
  drop (`generate.mjs:727-730`).
* When the connection ends, every slot becomes unavailable and all services are withdrawn
  (`projection.rs:120-146`).

---

## 7. Host services (rutis → plugin)

* **Declaration**: `provide = ["name"]` ⇒ generated trait `<Iface>Host` (every method defaults to
  `Error::Value("<svc>.<m> is not implemented by the rutis host")`), dispatcher `<Iface>HostDispatch`
  (unknown methods: `"<svc>.<m> is not bound"`), and `provide_<name>(ctx, host)` (`generate.mjs:859-903`).
  The method manifest `{method: "sync"|"async"}` is generated from the TS signature (`generate.mjs:855`) and
  sent as `mount.provided`.
* **Runner**: before loading plugins, `ctx.provide(name, hostProxy(name, methods))` (`runner.mjs:147`). The
  proxy (`runner.mjs:117-131`) has one function per declared method: `"sync"` ⇒ `peer.call('host:'+name, m,
  args)` (blocks the JS thread, pumping the socket), `"async"` ⇒ `peer.callAsync(...)` (Promise). Any other
  string property returns a function that throws `"<name>.<prop> is not provided by the rutis host"` locally
  (no frame); `then`, `toJSON`, `constructor` and symbols pass through (so the proxy is not a thenable).
* **Rust**: `Imports.invoke` strips `host:` and calls `HostDispatch::invoke(method, args)` (`process.rs:78-84`).
  Results may be data, `Value::future` (async methods; lazy until the runner awaits), or `Value::callback`
  (e.g. a disposer the plugin later calls with `call`; trace: dispose ran `call node:9 → reference 1`).
* **Lifecycle**: host services are in the mount plugin's `injects`; withdrawing one disposes the whole mount
  (runner process), providing it again launches a new one; no in-place replacement (design §5).

---

## 8. Events

### 8.1 Cordis → rutis (`events = [...]`)

* Build: each name must have a `Events` interface declaration returning `void`; `internal/*` and names
  selected in both directions are rejected (`generate.mjs:745-757`). A struct implementing `rutis::Event`
  (`Value = ()`) with `from_args(Vec<Value>)` is generated (`generate.mjs:777-792`); the mount registers
  `events.forward::<T>(name, T::from_args)` (`generate.mjs:999-1000`).
* Runner: after the plugins started and `mounted = true`, one listener per name:
  `(...values) => { const done = peer.callAsync('', 'event', [name, values]); done.catch(() => {}); return done }`
  (`runner.mjs:176-182`). Cordis `emit` ignores the returned Promise (fire-and-forget), `parallel`/`serial`
  wait for it. Events emitted while plugins start are not forwarded.
* Rust: `Events::event` decodes the args and returns `Value::future(parallel(...))` (`events.rs:81-97`). The
  future is **lazy**: rutis listeners run only when the runner sends `await` for it. Node always does (Promise
  adoption of the `RemotePromise` calls `then`, even for `emit`; trace `node:8`), and never releases it except
  by GC. **Ack timing**: the await's `return` arrives after all rutis `parallel` listeners finished.

### 8.2 rutis → Cordis (`emits = [...]`)

* Generated `to_args` (keeps `null` vs omitted) and an `EmitToCordis` listener on the rutis bus owned by the
  mount plugin (`generate.mjs:777-781, 1026`; `events.rs:99-135`).
* Wire: `invoke '' 'emit' [name, [args…]]`; runner checks `emits` and returns `ctx.parallel(name, …args)`
  (`runner.mjs:195-200`); Rust awaits it, so rutis `parallel` waits for Cordis listeners while rutis `emit`
  stays fire-and-forget by rutis' own rules.

---

## 9. Cancellation

* **Rust sends `cancel{id}`** when an async request's future is dropped before its reply was consumed
  (`CancelOnDrop`, `rpc.rs:523-538, 857-868`), and also when the reply (a future reference) arrived but was
  not yet awaited (the callee may still be running; `rpc.rs:869-885`; test `cancellation.rs:72-106`). Dropping
  `invoke_async`→`settle` during the await phase cancels the **await id**. The id is remembered; a late reply
  is discarded and counted in `Connection::orphans()` (`rpc.rs:1046-1056`).
* **Signals**: generated methods pass `Value::Signal` for TS `AbortSignal` parameters (`generate.mjs:526-531,
  624-625`). Node creates one `AbortController` per call id while decoding its args (`peer.mjs:133-139`); the
  controller lives until the reply, or until the returned Promise settles (`peer.mjs:297-302, 309-310`). An
  `await` on that Promise maps back to the call (`#awaits`, `peer.mjs:280`). On `cancel{id}` (call id or
  await id) the signal aborts with `DOMException('The operation was cancelled by the caller','AbortError')`
  (`peer.mjs:235-240`); the method decides. In the trace the rejected Promise produced a late
  `throw rust:12 AbortError`, which Rust treats as an orphan.
* **Sync methods are not cancellable**: `request_sync` has no `CancelOnDrop`; their signal never aborts
  (generated doc comment, `generate.mjs:631-633`).
* **Rust receiving `cancel`** (a runner cancelling its own call): drops a pending await and queued not-yet-run
  work **without sending any reply**; a running synchronous call completes and replies normally
  (`rpc.rs:1097-1106, 1299-1304`). A runner that cancels must forget the id itself and discard late replies.
  Node never sends `cancel`; Node treats any unknown reply id as fatal.
* No protocol-level timeouts anywhere; timeouts are caller-side (`tokio::time::timeout` ⇒ drop ⇒ cancel).

---

## 10. Shutdown and failure

### 10.1 Orderly dispose ("cleanup first, then drain")

`Process::dispose` (`process.rs:470-480`):

1. `call_async("", "dispose", null)`. Runner: `closing = true`; starts plugin cleanup — exporter fibers first,
   then plugin fibers in reverse load order (`runner.mjs:106-111`) — **concurrently** with `peer.drain()`;
   replies `null` when both finish (`runner.mjs:192-194`). Concurrency matters: a disposer may be what
   releases an in-flight call (`process_exit.rs:10-54`). During cleanup the runner still calls Rust
   (disposers returned by host services: trace `call node:9`), and Rust still serves them.
2. Rust closes the connection (`peer.close(Transport("plugin has been disposed"))` ⇒ socket shutdown).
3. Runner: socket EOF ⇒ session closed ⇒ `dispose()` (already done) ⇒ event loop empties ⇒ exit 0.
4. Rust waits for the exit **without timeout**; any status other than `exited normally` makes `dispose`
   return `Err(Transport("Cordis process <status>"))`. A plugin that leaves timers/handles alive would hang
   dispose (cf. `projection_lifecycle.rs:197-221`, an interval created by a service method must die with the
   exporter fiber).

Generated cleanup order: the mount plugin's effect closes event forwarding, closes the projection (breaking
the `Process → Projection → ServiceWriter → proxy → Process` cycle) and then disposes the process
(`generate.mjs:1016-1023`); because it was registered before the services, rutis withdraws services and runs
consumers' disposers first (design §6).

### 10.2 Crashes and transport errors

* Socket EOF/error ⇒ reader thread calls `disconnected()`: waits up to 1 s for the exit status, then closes
  the session with `Transport("Cordis process exited with …")` or `Transport("peer disconnected")`
  (`process.rs:161-179`, `rpc.rs:614-627`). `close` fails every pending call with that error, clears
  exports/imports, marks the session ended (`rpc.rs:649-686`); later calls fail immediately. No retries; a
  call that was sent but not answered has an unknown outcome (design §6).
* All projected services are withdrawn; dependants stop by native gating; the mount plugin itself stays
  Active until the application disposes/remounts (`projection.rs:120-146`; experiments doc §1).
* `Process` drop ⇒ `close(Transport("process dropped"))` + SIGKILL (`process.rs:495-499`).
* Fatal protocol violations (session closed) on the Rust side: unparsable JSON / unknown or mistyped fields;
  bad or duplicate `hello`; frames before `hello`; request id without `node:` prefix, non-increasing, or
  contained in its own path; reply for an unknown id; `release` of unknown id / bad count; `call` with method
  or `get`; `call`/`await` on unknown export; `signal` value; `home:true` unknown id or kind mismatch; repeated
  import with different kind/origin; id 0 or > 2^53−1; write errors (`rpc.rs` lines cited in §3–4).
* Runner-side observations (trace (b)–(e)): on a protocol fault Node closes the socket, disposes its plugins
  and exits **0** (so Rust reports `Cordis process exited normally`); on a handshake failure it exits 1. When
  the host disappears without `dispose` the runner disposes plugins and exits 0 (trace (d)). An uncaught
  exception / unhandled rejection in any plugin ends the whole Node process (exit 1), taking the whole mount
  down (README §4, experiments §1).

---

## 11. Build-time contract

### 11.1 Inputs: `[package.metadata.rutis-interop]` (`build.rs:217-364`)

```toml
[package.metadata.rutis-interop]
npm = "cordis"            # npm project, relative to Cargo.toml (default "."); must contain package.json and node_modules
runtime = "…"             # optional; default <npm>/node_modules/@arcships/rutis-interop

[package.metadata.rutis-interop.mounts.<rust_ident>]
plugin = "@scope/pkg"     # npm package under <npm>/node_modules   — or —   path = "src/plugin.ts"
version = "x.y.z"         # optional: must equal the installed package.json version
group = [{ name = "a", plugin = "…" }, { name = "b", path = "…" }]   # instead of plugin/path
provide = ["svc"]         # host services
events = ["ns/evt"]       # Cordis → rutis
emits = ["ns/evt"]        # rutis → Cordis
```

Checks (all fail the build with a fix-it message): section present; `npm/package.json` and `npm/node_modules`
exist; runtime `package.json` exists and `rutisProtocol == PROTOCOL`; runtime has `node_modules` or a hoisted
`../../typescript` (`build.rs:366-388`); each plugin installed and version-matched; mount names are Rust
identifiers; exactly one of `plugin`/`path`.

### 11.2 Generator invocation (`build.rs:124-215`)

```
node <runtime>/src/generate.mjs <runtime> [--provide=S]… [--event=S]… [--emit=S]… [--root=<npm>] <plugin>
node <runtime>/src/generate.mjs <runtime> … <name>=<plugin> <name>=<plugin> …        (group)
```

* `node` from `PATH`, cwd = the build script's cwd (the package dir), env inherited, stdout/stderr captured
  (`build.rs:186-197`). Exit ≠ 0 ⇒ build error `binding generation failed:\n<stderr>`.
* `generate.mjs` uses the TypeScript compiler API (`typescript` 6.0.3, a runtime dependency) to type-check
  the plugin(s) and walk Cordis `Context`/`Events` augmentations (`generate.mjs:45-77, 358-465`).
* **Output: a JSON envelope on stdout, `{"rust": "<complete Rust source>", "inputs": ["<file>", …],
  "diagnostics": ["<file>:<line>:<col>: <message>", …]}`** (`generate.mjs:1033, 1049`; `build.rs:198-204`).
  There is **no language-neutral interface description**: the generator emits Rust source directly; the
  type model it builds (methods, params, getters, hosts, events) is never serialized. The only
  runtime-relevant "interface data" embedded in the Rust source is: the services manifest
  `{name:[members]}`, host manifests `{method:"sync"|"async"}`, event names, entry paths and the runtime path
  (`generate.mjs:994-1013`).
* `build.rs` writes `OUT_DIR/<module>.rs`, emits `cargo:warning=` per diagnostic, and
  `cargo:rerun-if-changed=` for every program source file in `inputs` (incl. `.d.ts` in node_modules),
  `generate.mjs`, `<runtime>/package-lock.json`, each plugin path, `Cargo.toml`, `<npm>/package-lock.json`,
  `<runtime>/package.json` (`build.rs:159, 181-185, 205-207, 241, 280-283, 372`).
* `from_manifest` then writes `OUT_DIR/rutis_interop_mounts.rs` with one
  `#[cfg(unix)] pub mod <name> { include!(…/<name>.rs) }` per mount; `include_mounts!()` includes it
  (`build.rs:357-363`, `lib.rs:29-36`).

### 11.3 Relocation

`from_manifest` always uses `Bindings::relocatable(npm)`: paths stay lexical (symlinked `file:` deps are not
resolved, `build.rs:141-151`), and the generated code computes
`let __rutis_root = ::rutis_interop::npm_root("<abs npm root at build time>")` and joins entry/runtime paths
*relative to it* (`generate.mjs:948-950, 1001-1004`). At run time `RUTIS_INTEROP_ROOT` overrides the root
(`lib.rs:15-25`). Low-level `Bindings` without `relocatable` bake canonical absolute paths.

---

## 12. Classification

**A** = language-neutral, reusable as is · **B** = reusable after generalization (change noted) ·
**C** = Node/JS/Cordis-specific (each runner re-implements its own equivalent).

### 12.1 Components and concepts

| Component / concept | Class | Notes / what to change |
| --- | --- | --- |
| Unix stream socket, path in argv, host listens / runner connects, temp dir | A | Fine for Python/PowerShell 7 (.NET `UnixDomainSocketEndPoint`). Bash needs `socat`/`nc -U` (bash `/dev/tcp` has no Unix sockets); AppleScript needs a helper process. Unix only. |
| NDJSON framing, UTF-8, compact JSON | A | PowerShell must use `ConvertTo-Json -Compress` and enough `-Depth`. |
| `hello{version}` handshake | A | Optionally extend with a peer prefix and capabilities (B, see below). |
| Frame set `invoke/call/get/await/return/throw/release/cancel` | A | Generic RPC-with-references. |
| Request ids `rust:n` / `node:n` | **B** | Rust hard-codes the peer prefix `node:` (`rpc.rs:822, 1165, 1194`); Node hard-codes `rust:` and the `(node\|rust)` origin regex (`peer.mjs:146, 247-249`). Generalize: accept any prefix ≠ own (or declare it in `hello`), parse the number after the last `:`. Until then other runners must emit `node:<n>`. |
| `path` call chain, sync reentrancy routing, `origin` | A | Mechanism is generic; each runner needs a way to carry the chain across its own async continuations (contextvars in Python, runspace-local in PowerShell). |
| `SyncWaitCycle` detection | A (concept) | Implementations are executor-specific (C). Runners without futures never need it. |
| Values `data`, `list`, `record` | A | |
| Value `undefined` | **B** | JS concept on the wire; Rust uses it for "omitted optional argument" (`lib.rs:101-109`) and decodes it like `null`. Document it as "absent"; runners without undefined map it to omission (truncate trailing absent args) or to their null. |
| Value `signal` | **B** | Means "cancellation token for this call"; Python: an `Event`/`asyncio` cancellation, PowerShell: `CancellationToken`; Bash: ignore. |
| References `function`/`future`/`object`, `home`, grants/`release` | A | Kind names are neutral; the JS heuristics deciding what is "live" (`peer.mjs:9-26`) are C. |
| Lazy futures (start on `await`) | A | But undocumented; must be documented for runners (§8.1). |
| `Failure{name,message,graph?}` | A | `graph` optional. |
| Error graph format (`errors.mjs`) | **B** | JS-flavoured (Error/AggregateError/cause/stack, bigint/NaN encodings). Python exceptions map naturally (`__cause__`→`cause`, `ExceptionGroup`→`errors`); others may omit `graph`. Rust treats it as opaque. |
| `JsError{name,message,stack}` | **B** | Rename/alias to a neutral `RemoteError` value type; shape is reusable. |
| Error names `BindingError`, `RustError`, `SyncWaitCycle` | A | |
| `rpc.rs` (Connection) | A, except the `node:` prefix (B) | Role-symmetric; already used as a runner-side peer by `server.rs`. |
| `protocol.rs` | A | Strict serde (`deny_unknown_fields`, integer types) is part of the contract. |
| `objects.rs` (`ObjectRef`, `RemoteFunction`, markers) | A | Rust-internal; only doc strings say "Cordis". |
| `process.rs` launch: `node --import tsx <pkg>/src/runner.mjs <sock> <entry>`, cwd = runtime | **C → B** | Replace the hard-coded command by a runner descriptor (e.g. `Mount.runner = { program, args }` or a manifest file in the runtime package such as `{"protocol":1,"command":["python3","-m","rutis_runner"]}`); keep `<socket>` as first argument. |
| stdio, kill-on-drop/SIGKILL, exit-status thread, Transport errors with status | A | Message text says "Cordis process …" (B: "plugin process"). |
| `Imports` (`service`, `event`, `host:` routing) | A | |
| `mount` control op: envelope + `{services:{…}}` reply | A | |
| `mount` argument semantics (`entry` = JS module, `config` = Cordis config, `services` = Context service names, first entry decides the Cordis instance) | **B** | Each runner defines what an entry is (Python module/file, PS script/module, bash script) and how a plugin registers services; the field set can stay. |
| `dispose` (cleanup ∥ drain, then exit on EOF) | A (contract) | Fiber order is Cordis-specific (C). |
| `get` / `release` handle control ops | A | |
| `emit` control op | A (shape) / C (`ctx.parallel`) | |
| `event` control op + ack future | A | |
| `rows.*` (feat/loader-interop, Appendix C) | **B/C** | Cordis loader semantics (isolate labels, volatile config commit, schemastery → JSON Schema). |
| Projection: handles, versions, `service` notifications, handle release | A | |
| Slot-change *detection* (exporter fibers, `internal/service`, `internal/set`, `cordis.original`, `Service.check`) | C | A simple runner may report static slots (mount result only) and never notify. |
| Host services: `host:<name>` target, `HostDispatch`, `{method:"sync"\|"async"}` manifest | A / B | "async" means "returns a Promise"; for other runners: may return a future or block. |
| Host proxy object (JS `Proxy`, passthrough `then/toJSON/constructor`) | C | |
| Events forwarding in Rust (`Events`, `EmitToCordis`) | A | Type/struct names say Cordis (B, cosmetic). |
| Event listener registration / `emit` vs `parallel` | C | |
| `client.mjs` + `io-worker.mjs` (Worker + `Atomics.wait` pump) | C | Other runners can block on the socket directly or use an I/O thread. |
| `peer.mjs` | B | Reference implementation of the generic peer; JS-specific parts: live-object heuristics, Promise ↔ future, `RemotePromise`, GC-based release, `AsyncLocalStorage`. |
| `runner.mjs` | C | Cordis host logic. |
| `wire.mjs` | A (with quirk) | `undefined`→`null` inside data is a Node artefact. |
| `server.rs` + `build/rust.rs` (reverse direction) | frozen | Rust-as-runner example; not needed for new runners. |
| `from_manifest` TOML schema (`npm`, default runtime `node_modules/@arcships/rutis-interop`, `plugin` = npm package) | **B** | Add a language/runtime selector (e.g. `runtime.kind = "python"`, `python = ".venv"`, `module = …`). |
| Generator: `node <runtime>/src/generate.mjs …` | **C → B** | Make the generator command per runtime; keep the `{rust, inputs, diagnostics}` envelope. Better: have generators emit a language-neutral interface JSON and do Rust codegen once in `build.rs` (today every generator would have to write Rust). |
| `rutisProtocol` in `package.json`; `check_runtime` node_modules/typescript test | **B** / C | Read the protocol from the runtime's own manifest. |
| `RUTIS_INTEROP_ROOT`, `npm_root`, relative paths | A | Naming says npm (B, cosmetic). |
| TS type mapping (`generate.mjs` §§ type model, null vs undefined, live objects, callbacks) | C | Per-language generator (Python type hints, PS parameter attributes, …). |

### 12.2 Implicit assumptions a new runner must satisfy (or the Rust side must relax)

All runners:

1. Request ids `node:<n>`, strictly increasing, ≤ 2^53−1; never include the own id in `path`.
2. Strict JSON: no unknown fields at any level (frame, value, reference payload, error), all protocol
   integers as JSON integers (`1`, not `1.0` — fatal), finite numbers only (Python's `json.dumps` emits `NaN`
   by default ⇒ use `allow_nan=False`; `serde_json` rejects the token and closes the session), one line per
   frame, valid UTF-8.
3. Send `hello` first; do not send requests before the host's `hello`.
4. Answer every request exactly once (any order, any delay); reply to `await` of your own futures possibly
   much later. A reply for an id the host never sent (or forgot after cancel) is fatal on Node, tolerated on
   Rust only after cancel.
5. Propagate `path` correctly, including into asynchronous continuations; otherwise synchronous host calls /
   callbacks deadlock a `current_thread` host (§3.4).
6. Await Rust futures you receive (async host results, `event` acks) — they are lazy — and send `release`
   with the exact number of grants received, at least once you are done (Rust exports otherwise live until
   the session ends).
7. Resolve reference targets at frame-admission time, in wire order (a `release` may immediately follow a
   `call`).
8. Never send `signal` values, `call` with `method`, or `get` to Rust.
9. Send slot notifications before the reply of the call that caused them, with that call's path, and with a
   strictly increasing integer `version`.
10. On `dispose`: start cleanup and drain in-flight business calls concurrently, keep serving calls on
    references during cleanup, reply, then **exit 0 after the socket closes** (the host waits without
    timeout and reports any other status as a dispose error).
11. Never use stdin; stdout/stderr are free for logs.
12. Methods the host binds as *sync* must reply with values, not futures (a sync Rust proxy cannot settle a
    future and fails to decode it). Methods bound as *async* may reply with either.
13. Service names must not contain `#`; handles must be non-empty; the first handle should equal the service
    name (relied on by tests).

Single-threaded / blocking runners (Bash, AppleScript, a plain synchronous Python/PowerShell loop):

* Must keep reading the socket while waiting for their own replies ("pump") and execute incoming requests
  whose `path` contains an id they are waiting for; otherwise any host service or callback that calls back
  deadlocks. Unrelated requests should be queued (executing them inline re-enters plugin code in the middle of
  another call).
* The `service` and `event` invokes are effectively fire-and-forget but still produce replies that must be
  matched later.
* Rust may issue many concurrent requests; a sequential runner serializes them (correct but slow; a long sync
  method stalls everything, including `dispose`).
* A runner-side sync call must not depend on unrelated runner work (undetectable deadlock, the analogue of
  boundary rule 6).

Runners that cannot export references (e.g. Bash):

* Everything they return must be data: `mount`, `dispose`, `emit` and method results can all be plain data
  (`settle` passes non-futures through). Plugins whose interface returns functions/objects/futures cannot be
  bound; the generator should reject them at build time.
* They can still *import*: call Rust function references (`call`) and await Rust futures (`await`); they never
  receive `release` frames.
* No identity preservation is possible for their values.

Runners with no `undefined` vs `null` distinction (Python `None`, PowerShell `$null`, AppleScript
`missing value`, Bash empty string):

* Rust sends `{"type":"undefined"}` for omitted optional arguments and `data null` for explicit nulls; fields
  of generated outbound structs are omitted rather than set to undefined. Map `undefined` to "argument not
  passed" (drop trailing absent args so native defaults apply) and to the language null elsewhere.
* Returning null vs undefined is indistinguishable to most Rust decoders (`Undefined` decodes as `null`),
  except `Option<Option<T>>` fields where *missing* ≠ `null`.

Other implicit assumptions worth noting:

* No GC finalizers ⇒ release explicitly (deterministic is better than Node's GC-driven releases).
* PowerShell `ConvertTo-Json` depth limits and single-element-array unrolling can silently change data
  shapes; integers typed as `[double]` may serialize with a fraction.
* Error graphs are optional; `name`/`message` are mandatory strings.
* Startup cost and no connect/handshake timeout: a runner that hangs before connecting hangs the mount.

### 12.3 Minimal changes on the Rust side to admit a second runner

1. Runner command abstraction in `Process::mount` (and in `Mount`/generated code) instead of
   `node --import tsx …/src/runner.mjs`.
2. Peer id prefix: accept the prefix announced in `hello` (or any non-`rust:` prefix); fix `rpc.rs:822` unwrap.
3. Protocol/manifest discovery not tied to `package.json`; `from_manifest` runtime kinds; generator command
   per runtime (or a neutral interface JSON + shared Rust codegen).
4. Optional: `hello` capabilities (exports objects? futures? signals? notifications?) so unsupported plugins
   fail at mount instead of mid-call; neutral naming (`JsError`, "Cordis process", `EmitToCordis`).

### 12.4 Conformance profiles (suggested)

| Profile | Must implement |
| --- | --- |
| R0 data-only | socket + NDJSON + hello; `mount` (data reply), service `invoke` on handles, `get`, `release`, `dispose`; `node:` ids; path propagation; pump while waiting; import-side `call`/`await`/`release` of Rust references; error `name`/`message`. |
| R1 references | + export functions/futures/objects with grants and `release`; `home`/`origin`; identity; `call`/`get`/`await` on own exports. |
| R2 async + cancel | + futures for async methods, `signal` → cancellation token, `cancel`, SyncWaitCycle when blocked. |
| R3 dynamic services/events | + slot change notifications with versions, `event` forwarding with awaited acks, `emit`, host proxies with missing-member errors. |

---

## Appendix A. Abridged live trace (real `runner.mjs`, Python stand-in host)

Stacks elided; `R→N` = host (Rust role) to runner. Full text in `scratchpad/trace.txt`.

```
# service method, args as data array (Process::call)
R→N {"op":"invoke","id":"rust:3","path":[],"target":"counter","method":"current","args":{"type":"data","value":[]}}
N→R {"op":"return","id":"rust:3","value":{"type":"data","value":1}}
# control get(handle, property)
R→N {"op":"invoke","id":"rust:4","path":[],"target":"","method":"get","args":{"type":"data","value":["counter","label"]}}
N→R {"op":"return","id":"rust:4","value":{"type":"data","value":"counter-1"}}
# slot replacement: notification first, inside the chain
R→N {"op":"invoke","id":"rust:5","path":[],"target":"counter","method":"swap","args":{"type":"data","value":[2]}}
N→R {"op":"invoke","id":"node:2","path":["rust:5"],"target":"","method":"service","args":{"type":"list","value":[{"type":"data","value":"counter"},{"type":"data","value":"counter#2"},{"type":"data","value":2}]}}
R→N {"op":"return","id":"node:2","value":{"type":"undefined"}}
N→R {"op":"return","id":"rust:5","value":{"type":"undefined"}}
R→N {"op":"invoke","id":"rust:6","path":[],"target":"","method":"release","args":{"type":"data","value":["counter"]}}
N→R {"op":"return","id":"rust:6","value":{"type":"data","value":null}}
# Rust callback argument, called back inside the chain
R→N {"op":"invoke","id":"rust:7","path":[],"target":"counter#2","method":"apply","args":{"type":"list","value":[{"type":"reference","value":{"id":2,"kind":"function","home":false,"origin":[]}},{"type":"data","value":20}]}}
N→R {"op":"call","id":"node:3","path":["rust:7"],"reference":2,"args":{"type":"list","value":[{"type":"data","value":20}]}}
R→N {"op":"return","id":"node:3","value":{"type":"data","value":40}}
N→R {"op":"return","id":"rust:7","value":{"type":"data","value":40}}
# async method calling an async host method (Rust future)
R→N {"op":"invoke","id":"rust:9","path":[],"target":"counter#2","method":"later","args":{"type":"list","value":[{"type":"data","value":3}]}}
N→R {"op":"invoke","id":"node:5","path":["rust:9"],"target":"host:clock","method":"later","args":{"type":"list","value":[]}}
R→N {"op":"return","id":"node:5","value":{"type":"reference","value":{"id":3,"kind":"future","home":false,"origin":["rust:9","node:5"]}}}
N→R {"op":"return","id":"rust:9","value":{"type":"reference","value":{"id":2,"kind":"future","home":false,"origin":["rust:9"]}}}
R→N {"op":"await","id":"rust:10","path":["rust:9"],"reference":2}
N→R {"op":"await","id":"node:6","path":["rust:9","node:5"],"reference":3}
R→N {"op":"return","id":"node:6","value":{"type":"data","value":7}}
N→R {"op":"return","id":"rust:10","value":{"type":"data","value":10}}
R→N {"op":"release","reference":2,"count":1}
# cancellation of the await phase
R→N {"op":"invoke","id":"rust:11","path":[],"target":"counter#2","method":"wait","args":{"type":"list","value":[{"type":"signal"}]}}
N→R {"op":"return","id":"rust:11","value":{"type":"reference","value":{"id":3,"kind":"future","home":false,"origin":["rust:11"]}}}
R→N {"op":"await","id":"rust:12","path":["rust:11"],"reference":3}
R→N {"op":"cancel","id":"rust:12"}
R→N {"op":"release","reference":3,"count":1}
N→R {"op":"throw","id":"rust:12","error":{"name":"AbortError","message":"The operation was cancelled by the caller","graph":{…}}}
# Cordis emit → rutis: event invoke answered with a future; Node awaits it (later)
R→N {"op":"invoke","id":"rust:13","path":[],"target":"counter#2","method":"fire","args":{"type":"list","value":[{"type":"data","value":5}]}}
N→R {"op":"invoke","id":"node:7","path":["rust:13"],"target":"","method":"event","args":{"type":"list","value":[{"type":"data","value":"demo/tick"},{"type":"list","value":[{"type":"data","value":5},{"type":"undefined"}]}]}}
R→N {"op":"return","id":"node:7","value":{"type":"reference","value":{"id":4,"kind":"future","home":false,"origin":["rust:13","node:7"]}}}
N→R {"op":"return","id":"rust:13","value":{"type":"undefined"}}
# rutis → Cordis emit
R→N {"op":"invoke","id":"rust:14","path":[],"target":"","method":"emit","args":{"type":"list","value":[{"type":"data","value":"demo/changed"},{"type":"list","value":[{"type":"data","value":"k1"}]}]}}
N→R {"op":"await","id":"node:8","path":["rust:13","node:7"],"reference":4}
R→N {"op":"return","id":"node:8","value":{"type":"undefined"}}
N→R {"op":"return","id":"rust:14","value":{"type":"reference","value":{"id":4,"kind":"future","home":false,"origin":["rust:14"]}}}
R→N {"op":"await","id":"rust:15","path":["rust:14"],"reference":4}
N→R {"op":"return","id":"rust:15","value":{"type":"undefined"}}
R→N {"op":"release","reference":4,"count":1}
# live object
N→R {"op":"return","id":"rust:16","value":{"type":"reference","value":{"id":5,"kind":"object","home":false,"origin":["rust:16"]}}}
R→N {"op":"get","id":"rust:17","path":[],"reference":5,"property":"balance"}
N→R {"op":"return","id":"rust:17","value":{"type":"data","value":5}}
R→N {"op":"call","id":"rust:18","path":[],"reference":5,"method":"deposit","args":{"type":"list","value":[{"type":"data","value":2}]}}
N→R {"op":"return","id":"rust:18","value":{"type":"data","value":7}}
R→N {"op":"release","reference":5,"count":1}
# record/list/undefined: { first: new Account(1), tags: ['a'], n: undefined, list: [1, undefined] }
N→R {"op":"return","id":"rust:19","value":{"type":"record","value":{"first":{"type":"reference","value":{"id":6,"kind":"object","home":false,"origin":["rust:19"]}},"tags":{"type":"list","value":[{"type":"data","value":"a"}]},"n":{"type":"undefined"},"list":{"type":"list","value":[{"type":"data","value":1},{"type":"undefined"}]}}}}
# errors
N→R {"op":"throw","id":"rust:20","error":{"name":"AggregateError","message":"outer","graph":{"root":{"type":"reference","value":0},"nodes":[{"type":"error","name":"AggregateError","message":"outer","stack":"…","cause":{"type":"reference","value":1},"errors":[{"type":"reference","value":2}]},{"type":"error","name":"Error","message":"cause","stack":"…"},{"type":"error","name":"TypeError","message":"first","stack":"…","cause":{"type":"reference","value":1}}]}}}
N→R {"op":"throw","id":"rust:21","error":{"name":"Error","message":"clock.rewind is not provided by the rutis host","graph":{…}}}
N→R {"op":"throw","id":"rust:22","error":{"name":"Error","message":"unknown service method counter.nope","graph":{…}}}
# dispose: the host-returned disposer is called during cleanup
R→N {"op":"invoke","id":"rust:23","path":[],"target":"","method":"dispose","args":{"type":"data","value":null}}
N→R {"op":"return","id":"rust:23","value":{"type":"reference","value":{"id":7,"kind":"future","home":false,"origin":["rust:23"]}}}
R→N {"op":"await","id":"rust:24","path":["rust:23"],"reference":7}
N→R {"op":"call","id":"node:9","path":["rust:23"],"reference":1,"args":{"type":"list","value":[]}}
R→N {"op":"return","id":"node:9","value":{"type":"undefined"}}
N→R {"op":"return","id":"rust:24","value":{"type":"data","value":null}}
R→N {"op":"release","reference":7,"count":1}
# host closes the socket → runner exit code 0
```

Failure traces (`scratchpad/trace-failures.txt`): (a) mount with an unprovided `inject` ⇒ `throw` on the
mount await: `native plugin dependencies are unresolved: <entry> (clock)`; (b) host `hello` version 2 ⇒ runner
closes and exits 1; (c) request id `py:1` ⇒ runner closes, exits 0; (d) host closes after mount without
dispose ⇒ runner disposes plugins, exits 0; (e) repeated id `rust:5` ⇒ runner closes, exits 0.

## Appendix B. Serde acceptance on the Rust side (verbatim `protocol.rs`)

| Input | Result |
| --- | --- |
| `{"op":"hello","version":1.0}` | error: invalid type floating point, expected u32 |
| `{"op":"hello","version":1,"x":0}` | error: unknown field `x` |
| key order `{"version":1,"op":"hello"}` | OK |
| `{"type":"undefined","value":null}` | OK (treated as undefined) |
| `{"type":"data"}` | error: missing field `value` |
| `{"type":"data","value":1,"extra":true}` | error |
| `{"type":"list","value":[1]}` | error: elements must be wire values |
| reference `id: 3.0` / missing `origin` / extra field / `kind:"Function"` | error each |
| error `{"name","message","stack"}` | error: unknown field `stack` |
| error `{"name"}` | error: missing `message` |
| `"graph": null` | OK (absent) |
| `call` with `"method": null` | OK (no method) |
| `await` with `"args"` | error: unknown field |
| `invoke` without `path` / with `path:[1]` | error each |
| Rust serializes `Call{method:None}` | `method` key omitted |
| Rust serializes `Throw{graph:None}` | `graph` key omitted |

## Appendix C. In-flight (branch `feat/loader-interop`, not on main)

`Mount::anchor: Option<&Path>` allows a mount with no plugins (argv's second argument is then the anchor, a
`package.json` from which Cordis resolves), plus control ops used by rutis-loader:

| Method | `args` | Result |
| --- | --- | --- |
| `rows.load` | `[key, entry, config, isolate:[[service,label]…], inject:[service…]]` | `null` once the row's fiber settled |
| `rows.update` | `[key, config]` | `null`; commits Cordis "volatile" values in place (`loader/volatile-update`) or restarts the row |
| `rows.unload` | `[key]` | `null` |
| `rows.schema` | `[entry]` | JSON Schema of the plugin's schemastery `Config`, or `null` |

These are Cordis/loader-specific (B/C); a non-Node runner would need its own notion of rows and config
schemas to support rutis-loader.
