# Cordis Plugin Mount Roadmap

Based on the [requirements](requirements-protocol-plugins.en.md) and [design](design-protocol-plugin-mount.en.md). Do not modify the rutis core or Cordis. Each capability needs automated cross-process tests compared with native Cordis behavior.

## 1. Ownership

Keep the repository's Cordis integrations separate by direction:

| Direction | Owner | Status |
| --- | --- | --- |
| rutis application mounts Cordis plugins (primary) | `rutis-interop` + `interop/node` (this roadmap) | In development |
| Full dsh interface runs inside a rutis host | `crates/rutis-dsh` (launcher plugin + aimux bundle, built on this compatibility layer) | Complete; replaces old bridge `rutis-cordis` + `host/`, where dsh was host (removed, [#83](https://github.com/arcships/rutis/issues/83)) |
| Reverse direction in `rutis-interop` (Cordis app mounts rutis plugins) | `build/rust.rs`, `server.rs` | **Frozen:** keep code/tests; add no capabilities |

## 2. Completed

| Capability | Tests |
| --- | --- |
| Build-time Rust type generation; sync/async methods, business errors, dependency readiness, cleanup order, startup failure, isolated mounts | `examples/native-mount/src/main.rs` |
| Follow service replacement: new reads get new proxy, old snapshot unchanged; revoke/re-register; handle reclamation | `crates/rutis-interop/tests/service_projection.rs` |
| Handle remains bound to original object; cleanup before drain; calls fail after process exit | `crates/rutis-interop/tests/process_exit.rs` |
| Protocol v1: separate invoke/await, function and async-result references, counter release, reverse call in sync chain, `SyncWaitCycle`, background executor | `crates/rutis-interop/tests/rpc_callbacks.rs`, `src/rpc/tests.rs`, `interop/node/test/peer.test.mjs` |
| Error-object graph round trip | `crates/rutis-interop/tests/error_shape.rs`, `interop/node/test/errors.test.mjs` |
| Published dsh plugins: protocol parity against native; load and call generated typed bindings | `crates/rutis-interop/tests/dsh_baseline.rs`, `examples/dsh-baseline/tests/typed.rs` |
| Combined mount: resolve in-group dependencies and name missing dependencies | `crates/rutis-interop/tests/group_mount.rs` |
| Host provides services to plugins: dependency gating, calls, disposer function, revoke and recovery | `crates/rutis-interop/tests/host_services.rs`, `examples/dsh-baseline/tests/host.rs` |
| Projection and export lifecycle (review regression) | `crates/rutis-interop/tests/projection_lifecycle.rs` |
| Callback arguments and returned functions with real plugins | `examples/dsh-baseline/tests/callbacks.rs` |
| Event forwarding: emit/parallel semantics with real plugins | `crates/rutis-interop/tests/event_forwarding.rs`, `examples/dsh-baseline/tests/events.rs` |
| Cancellation: dropping future aborts AbortSignal; late responses are discarded | `crates/rutis-interop/tests/cancellation.rs` |
| Live objects: property reads, methods, identity, nested in data, passed back as original | `crates/rutis-interop/tests/live_objects.rs`, `interop/node/test/peer.test.mjs`, `examples/dsh-baseline/tests/typed.rs` |

## 3. Follow-up work

### W1: real plugin baseline (first round completed, 2026-09-29)

Harness: `interop/baseline/`. Pin published official dsh npm plugins (`0.2.0-rc.1`, Cordis `4.0.4`). Run the same scenario list under native Cordis and through interop from Rust, comparing results (`crates/rutis-interop/tests/dsh_baseline.rs`, run in CI). `classify.mjs` statically counts the binding capability required by each service member.

```sh
npm --prefix interop/baseline ci
cargo test -p rutis-interop --test dsh_baseline -- --nocapture
node interop/baseline/classify.mjs
```

**Protocol-level results** (bypass generator and call directly as JSON):

| Plugin | Service | Load/availability matches native | Data calls match native |
| --- | --- | --- | --- |
| dsh-invariants | `invariants` | Yes | No pure-data methods |
| dsh-credentials-local | `credentials` | Yes | 7 / 7 |
| dsh-fs-local | `fs` | Yes | 8 / 8 (including `FsError` business error) |
| dsh-jobs-local | `jobs` | Yes | 2 / 2 (including business error) |
| dsh-commands | `commands` | Yes | No pure-data methods |
| dsh-workspace (combined with storage, storage-json, storage-domain, session-persistence-jsonl) | `workspaceRegistry` | Yes | 2 / 2 |

**Binding-level results:** the generator stopped at L0 for all six plugins for the same reason. They register services with `class X extends Service`; service names/types are in `declare module '@deepseek-ai/cordis' { interface Context { ... } }`, while the generator recognized only `ctx.provide('name', value)`.

**Capability counts** (6 plugins, 55 public members): the current generator handled 1; another 28 were supported by the protocol but missing generator work; remaining capabilities required protocol extensions.

| Capability | Members | Generator | Protocol |
| --- | ---: | --- | --- |
| Branded strings / numbers | 42 | No | Yes |
| Data objects (literal unions, nullable included) | 31 | No | Yes |
| Optional parameters | 19 | No | Yes |
| `AbortSignal` parameter | 13 | No | No |
| Live objects (objects with methods / class instances) | 12 | No | No |
| Callback arguments / returned functions | 5 / 5 | No | Yes |
| Public properties | 5 | No | No |
| `Uint8Array` / `AsyncIterable` | 2 / 1 | No | No |

Events: 5 notifications; 3 waterfalls (`fs/write-intent`, `fs/edit-intent`, `workspace/session-activity`); 1 with a return value.

### W1.5: combined mounts (complete)

W3 testing exposed a larger problem than individual members: published plugins are commonly designed to be composed, and a plugin may not load alone (`dsh-workspace` also fails alone in native Cordis). One mount can now group plugins in one Cordis Context and resolve dependencies natively; bindings are generated with `cordis_group`. After mounting `dsh-workspace` with `dsh-storage`, `dsh-storage-json`, `dsh-storage-domain`, and `dsh-session-persistence-jsonl` as one group, protocol behavior matched native and typed bindings could call it. Tests: `crates/rutis-interop/tests/group_mount.rs`, `dsh_baseline.rs`, and `examples/dsh-baseline/tests/typed.rs`.

### W1.6: host provides services to Cordis plugins (complete)

Mounted Cordis plugins may depend on services from the rutis application (design §5). The requirement was accidentally removed when rewriting requirements on 2026-09-29; it was restored and implemented. `Bindings::provide` generates a trait, `provide_*` registration function, and dispatch code. The mount plugin declares these in `injects`; the runner registers proxies before loading plugins.

Acceptance uses `dsh-persona` depending on `systemPrompt`: mount waits until the host provides it; then the plugin registers prompt fragments. Revocation stops and cleans up the plugin; re-provisioning restores it. On unload, Cordis calls the host's returned disposer function (`examples/dsh-baseline/tests/host.rs`, `crates/rutis-interop/tests/host_services.rs`).

Still unsupported: a Cordis plugin depending on another mount's Cordis service (they need to be in the same group); in-place replacement of a host service (revocation restarts the whole mount through dependency rules).

### W2: engineering safeguards (mostly complete)

- Cancellation/timeouts: dropping an async-call future cancels it and aborts the Cordis method's `AbortSignal`; use `tokio::time::timeout` for deadlines. Test: `crates/rutis-interop/tests/cancellation.rs`.
- Late responses: discard and count responses to cancelled calls (`Connection::orphans()`); do not disconnect.
- Deferred: handshake capability negotiation (generator and runner ship together; do this after W4 separates them); log in-flight calls individually on disconnect.

### W3: add capabilities based on gaps found in W1

1. ~~Discover services from type declarations~~ (complete): read declarations from `Context`; support `Service` subclasses and npm package directories.
2. ~~Data types~~ (complete): branded types, data objects, literal unions, nullable, optional parameters, `Record`, dynamic JSON. Optional `AbortSignal` is not exposed yet. All six plugins now generate typed bindings and pass L0; 32 of 55 members are bound (previously 1). `examples/dsh-baseline` calls credentials/fs/jobs through generated types and matches native behavior.
3. ~~Live objects~~ (complete): protocol adds object references, value recording, method calls, and property reads. Generator creates proxies for interfaces with methods and class instances, with live property reads. `dsh-workspace` `create` / `get` / `list` / `resolveByPath` work through typed bindings. Coverage: 39/55 (previously 32).
4. ~~Cancellation~~ (complete): dropping the Rust future aborts the `AbortSignal` parameter; `AbortSignal` fields inside option objects are still not passed. Coverage: 40/55.
5. ~~Callback arguments and returned functions~~ (complete): callback arguments become `impl Fn` closure parameters (sync or returning `BoxFuture`); returned functions become `RemoteFunction`; JS `Error` values become `JsError`. Verified on real plugins with invariants installer, `modifyRecord`, `fs.watch`, `attachController` (`examples/dsh-baseline/tests/callbacks.rs`). Coverage: 47/55.
6. ~~Service properties~~ (complete): service properties generate live getters. Coverage: 52/55; the remaining members are 2 binary and 1 stream.
7. ~~Events (both directions)~~ (complete): `Bindings::event` selects Cordis → rutis notification events and generates rutis event types. Cordis `emit` is fire-and-forget; `parallel` waits for rutis listeners. Verified on two credentials events (`examples/dsh-baseline/tests/events.rs`). `Bindings::emit` selects rutis → Cordis events, used by host services to emit interface-level events (persona example emits `system-prompt/change`). An event can have only one direction. Waterfall / return-value events are not forwarded.
8. `Uint8Array`, `AsyncIterable`: implement as needed.

### W4: package-level integration (complete)

Declare the npm project and mounts in `[package.metadata.rutis-interop]` in `Cargo.toml`; `build.rs` only calls `from_manifest()`, and code uses `include_mounts!()`. Build checks that plugins are installed, versions match, and runtime protocol matches, with a remediation command on error (`manifest_tests` in `crates/rutis-interop/src/build.rs`). Both examples now use manifest-driven mounts. See [`crates/rutis-interop/README.md`](../crates/rutis-interop/README.md).

Release (2026-10-02): publish crate `rutis-interop` (crates.io) and runtime `@arcships/rutis-interop` (npm with provenance) at the same version. Push `interop-vX.Y.Z` for `publish-interop.yml`; before publishing it checks the tag, both package versions, and protocol version. It first runs dry-run publishing for both, then publishes.

Relocatable deployment (2026-10-02, complete): manifest-driven mounts find runtime/plugins relative to the npm project. Use `RUTIS_INTEROP_ROOT` to point to the deployed copy; when unset, use the build-time location (`crates/rutis-dsh/tests/relocate.rs`).

### Experiment: crashes and call overhead (2026-09-29)

See the [experiment report](experiments-native-plugin-mount.en.md) for rutis behavior when Node crashes/hangs and for cross-process call costs. Main findings: after a crash, services stayed registered and consumers kept running, with no exit status in errors (fixed by withdrawing services on disconnect and including exit status); sync calls and unload have no hang timeout (not addressed; plugin boundary rule); each call costs about 30–70 µs, and large structured batches have slow Node-side encoding (known cost; optimize if a large-data use case appears).

## 4. Verification

```sh
npm --prefix interop/node ci
npm --prefix interop/node test
npm --prefix interop/baseline ci
cargo test -p rutis-interop -p native-mount-example -p dsh-baseline
cargo clippy -p rutis-interop -p native-mount-example -p dsh-baseline -p interop-experiments --all-targets -- -D warnings
```

## 5. Performance notes

Synchronous method round trip (2026-09-28; Node 26.8.1, Intel Core Ultra X7 358H; 200 warmups, 3,000 calls):

| Rust build | p50 / p95 / p99 (µs) | Mean (µs) |
| --- | --- | ---: |
| debug | 65.3 / 100.3 / 181.0 | 70.3 |
| release | 32.7 / 59.9 / 101.4 | 36.6 |

Reproduce with `cargo build --release -p native-mount-example --bin rutis-counter && node interop/node/bench/sync-call.mjs target/release/rutis-counter`. Service reads do not use IPC; each method call is one round trip. W1's real plugin load determines whether optimization is needed.
