# Cordis Plugin Mount: Crash and Performance Experiments

2026-09-29, Linux, 16 cores, Node 26.8.1, Cordis 4.0.4. Harness: [`examples/interop-experiments`](../examples/interop-experiments); plugins: `interop/node/test/fixtures/fragile.ts` and `bench.ts`, mounted normally through generated bindings.

```sh
cargo run -p interop-experiments --bin crash
cargo run --release -p interop-experiments --bin bench
```

## 1. Node process crash or hang

Each scenario mounts a plugin from the rutis side, then mounts a rutis consumer plugin that declares `injects` for its service.

| Scenario | In-flight call | Later call | Mount / service / consumer | Unmount |
| --- | --- | --- | --- | --- |
| SIGKILL while idle | — | Immediate `Transport("peer disconnected")` | Still Active / still registered / still Active | Returns error; consumer stops only then |
| `process.exit(17)` during sync call | Immediately same error | Same | Same | Same |
| SIGKILL while async call waits | Same error after about 6 ms | Same | Same | Same |
| Throw in timer | — | Same (entire Node process exits) | Same | Same |
| Unhandled Promise rejection | — | Same (entire Node process exits) | Same | Same |
| Sync method blocks event loop for 2 s | Async call can be cancelled with `timeout`; sync call waits until event loop resumes | Works after recovery | Normal | Normal |
| Process frozen (`SIGSTOP`) | Async call can be cancelled with `timeout` | — | — | Waits indefinitely until process resumes |
| Exit during plugin `apply` | — | — | Mount becomes Failed after about 137 ms, error `peer disconnected` | — |
| Unmount and remount after crash | — | Normal | New mount Active; consumer restarts | — |

Conclusions:

1. **The rutis side does not detect a crash.** The Node process has exited, but the mount plugin remains Active, its service remains registered, and dependent rutis plugins keep running; every call just fails. This conflicts with rutis dependency gating: when a provider is gone, consumers should stop and wait. Consumers stop only after the application unmounts the mount; after remount they recover.
2. **Calls do not hang after process exit.** In-flight and later calls return a `Transport` error immediately (within milliseconds).
3. **Diagnostics are insufficient.** The error is only `peer disconnected`; it omits exit status or signal, and the unmount error also loses exit details. The only clue is Node output inherited on the rutis process's stderr (for example, an exception stack).
4. **Node's default behavior ends the entire process when any plugin throws an uncaught exception or rejection.** Other plugins in the same mount/group stop too. This matches native Node behavior, with impact limited to that mount.
5. **Only async calls can protect themselves from a hang.** Sync calls have no timeout and block their calling thread until Node recovers. Unmount has no timeout either, and waits indefinitely if the process is frozen; there is no forced-stop fallback.

All these gaps can be fixed in the compatibility layer without changing the rutis core: withdraw projected services when the connection closes (consumers return to waiting by native rules), include exit status/signal in errors, force-stop Node after an unmount timeout, and optionally add timeouts to sync calls.

### After fixes (conclusions 1 and 3)

The compatibility layer now withdraws every projected service after a connection closes; call and unload results include how the process ended. Rerun results:

| Scenario | Call error | Service / consumer |
| --- | --- | --- |
| SIGKILL while idle | `Cordis process exited with signal: 9 (SIGKILL)` | Revoked / stopped and waiting (`Pending`) |
| `process.exit(17)` during sync call | `Cordis process exited with exit status: 17` | Same |
| SIGKILL while async call waits | `… signal: 9 (SIGKILL)`, after about 4 ms | Same |
| Timer exception / unhandled rejection | `… exit status: 1` | Same |
| Exit during `apply` | Mount Failed: `… exit status: 3` | — |

The mount plugin itself remains Active but no longer provides the service. Unmounting and mounting again restores it. Regression tests: `crates/rutis-interop/tests/projection_lifecycle.rs::a_crashed_process_withdraws_its_services` and `process_exit.rs`.

### Not addressed: conclusion 5 (no timeout for hangs)

An event-loop block or hang is a plugin's responsibility: in native Cordis it can hang the entire application. With mounting, the impact is limited to that mount, no worse than native behavior. The compatibility layer does not add timeouts; this is a boundary rule:

- Plugins must not block the event loop for long periods.
- Callers needing a timeout should use an async method wrapped in `tokio::time::timeout`; timeout cancels the call (verified in scenarios 6 and 7). A synchronous method blocks until it returns, matching native synchronous calls.
- The application sets the unload wait deadline through rutis's unload-wait timeout.

## 2. Call overhead

Release build; each operation was warmed up before measurement.

| Operation | Mean | p50 | p99 |
| --- | ---: | ---: | ---: |
| Mount (start Node + apply) | 129 ms | 124 ms | 128 ms |
| Sync method, no args | 46 µs | 43 µs | 86 µs |
| Async method, no args | 71 µs | 71 µs | 125 µs |
| Return 1 KiB string | 55 µs | 53 µs | 169 µs |
| Return 64 KiB string | 265 µs | 241 µs | 680 µs |
| Return 1 MiB string | 3.9 ms | 3.8 ms | 4.6 ms |
| Receive 10 records | 77 µs | 75 µs | 106 µs |
| Receive 1,000 records | 2.7 ms | 2.7 ms | 3.1 ms |
| Receive 10,000 records | 23 ms | 23 ms | 25 ms |
| Send 1,000 records | 1.6 ms | 1.6 ms | 2.0 ms |
| Read live-object property | 32 µs | 30 µs | 57 µs |
| Call live-object method | 51 µs | 47 µs | 121 µs |
| Callback (Node calls Rust closure) | 43 µs / call | | |

Throughput (no-argument calls):

| Mode | Calls/sec |
| --- | ---: |
| Async, serial | 11,200 |
| Async, concurrency 8 | 62,800 |
| Async, concurrency 64 | 65,000 |
| Sync, 8 threads | 87,300 |

Conclusions:

1. **About 30–70 µs per cross-boundary call.** Service-level operations (read/write credentials, files, workspaces) can ignore this overhead. Fine-grained calls in loops accumulate: 10,000 one-by-one calls take about 0.5 s, so batch them or perform them in one call. The same applies to callbacks; each callback is a round trip.
2. **Concurrent throughput is about 65,000 calls/sec**, capped by one Node process's event loop.
3. **Large structured batches are slower:** 10,000 records take about 23 ms (2.3 µs/record), while a single string of similar size takes about 4 ms. A breakdown measured about 6 ms to encode 10,000 records on the Node side; `JSON.stringify` took only 0.7 ms, with the rest spent checking each element for references. This overhead is in the compatibility layer, not Node. It could be optimized by checking the batch in one pass. Current service calls usually carry tens to hundreds of records and cost under a millisecond, so this is a known cost to optimize only if a real case sends tens of thousands of structured records.
4. **Mount takes about 125 ms**, mainly to start Node and load TypeScript. Mount at application startup, not per request.
