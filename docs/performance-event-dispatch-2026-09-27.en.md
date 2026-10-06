# Event Dispatch Performance Samples: #62 / #63

Date: 2026-09-27. Implementation branch: `feat/event-keys-and-sync-dispatch`, rutis 0.4.0 (this development version was ultimately released as 0.5.0); old baseline: main `603f8220b049c6ad26e82f258fd72f214b8575fe` (0.3.0).

Environment: Linux x86_64, Intel Core Ultra X7 358H, rustc 1.98.1, default release optimizations, Tokio with two worker threads. Samples below pin the process to CPU 0; CPU frequency was not fixed and no confidence interval was computed, so differences of a few nanoseconds must not be treated as cross-machine conclusions.

## Run

```sh
# Branch interface: ordinary synchronous listeners, synchronous waterfall, and varying prefix counts.
taskset -c 0 cargo bench -p rutis --offline --bench events

# Link old and new implementations into one binary and use the same keyed interface and dependencies.
RUTIS_BENCH_CPU=0 bash tools/compare-event-dispatch.sh
```

`taskset` / `RUTIS_BENCH_CPU` are optional Linux CPU pinning; the benchmark can run without them. Dependencies must already be in Cargo's cache. `RUTIS_BENCH_ITERATIONS` changes the first benchmark's iteration count (default 100,000); it warms up for 1,000 iterations first.

The old/new comparison uses [the probe](probes/compare-exact-dispatch.rs): seven rounds per listener count, each with 500,000 dispatches, reporting the median. Execution order alternates. Deprecated `serial_keyed` / `on_keyed` wrappers keep both versions' test code identical; new business code should not use those deprecated entry points.

## Synchronous dispatch

Listeners are minimal empty callbacks: `bail` returns None, `waterfall` calls `next`, and the terminal returns `u64`. Values below are average ns/op from one benchmark run.

| Exact listeners | `bail_sync` | `waterfall_sync` |
| --- | ---: | ---: |
| 0 | 412.8 | 420.5 |
| 1 | 475.0 | 543.8 |
| 8 | 971.8 | 1085.2 |
| 64 | 4345.7 | 5248.2 |

An instance-key `waterfall_sync` with no listeners was 458.6 ns/op. The empty path creates no future or task and does not box the terminal result, but still checks shutdown/generation, prevents reentry, and records the in-flight call.

## Prefix subscriptions

Static key `room/hit` has one exact async listener. When prefixes exist, the first matches `room/`; all others miss. Every callback returns None. No emit queueing or business I/O is involved.

| Prefix registrations | `serial` ns/op |
| ---: | ---: |
| 0 | 94.1 |
| 1 | 568.7 |
| 8 | 563.2 |
| 64 | 702.6 |
| 1024 | 2309.9 |

Prefix dispatch scans, merges snapshots, counts pattern matches, and protects unload waits; owner liveness is checked only for matched subscriptions. With few prefixes, lifecycle protection costs more than string comparison. The first version uses a linear scan; #43 can consider indexes or caches if data shows a need for more throughput.

## Exact dispatch without patterns: old vs new

The table compares `serial_keyed` for the same dynamic name in one binary, with no patterns, observers, or business callback work. Values are medians of seven rounds (ns/op):

| Exact listeners | 0.3.0 | 0.4.0 | Difference |
| ---: | ---: | ---: | ---: |
| 0 | 55.2 | 58.5 | +5.9% (+3.3 ns) |
| 1 | 112.6 | 97.8 | -13.1% |
| 8 | 273.3 | 262.7 | -3.9% |
| 64 | 1589.7 | 1605.5 | +1.0% |

The implementation avoids atomic counters for exact subscriptions and moves diagnostic metadata out of common callback fields; pattern selection and invocation are counted. Async adapters call the user listener inside the future and therefore preserve the existing one-poll panic capture instead of paying for duplicate capture. The empty path still adds bus-identity and pattern checks. Since the table has both positive and negative differences, it does not prove “no performance regression on every path.” These are representative local samples, not hard CI thresholds.

This batch did not complete #43 benchmarks for service reads, load/eviction, concurrent throughput, or emit backlog, and did not add backpressure. Regression tests verify existing same-key emit order, cross-key concurrency, and cancellation while waiting.

## Emit backlog, service reads, and lifecycle (#43, 2026-10-02)

Environment: macOS, Apple M4 Max, rustc 1.98.1, release build, Tokio with two worker threads; not CPU-pinned; one sample.

```sh
cargo bench -p rutis --bench events    # includes emit admission and drain
cargo bench -p rutis --bench runtime   # service reads, plugin load / unload
```

| Operation | ns/op |
| --- | ---: |
| Admit emit, no listeners | 15.5 |
| Admit emit, one same-key listener | 560.0 |
| Drain emit, one same-key listener (per event, admission through completion) | 974.8 |
| `ctx.get` | 82.2 |
| `ctx.require` (declared dependency) | 108.7 |
| Plugin load + unload | 27162.6 |
| Plugin load + unload with dependency | 24768.7 |

- **Emit admission:** the caller pays only registration and enqueue/spawn cost; no work is queued when there are no listeners. With one empty same-key listener, one chain handles about one million events/sec; slow listeners accumulate serially according to their own duration.
- **Backlog is observable:** `Ctx::diagnostics().event_backlogs` reports, per event key, received but incomplete emit count (including the one currently executing) and the wait time of the oldest. Entries are sorted by wait time. This does not change dispatch. For emit admission with one same-key listener (seven rounds each), medians were 1113 ns old and 947 ns new, within noise.
- **Service reads and lifecycle:** reads take under 100 ns; loading through unloading a plugin takes about 25 µs. With dependencies already ready, plugin load time is similar with and without dependencies.

### serial / parallel / waterfall

Measured inside a runtime task (as plugins normally call them), with empty callbacks: serial returns None for every listener (runs all); waterfall calls `next` through to the terminal. Two runs agreed; one is shown:

| Exact listeners | serial | parallel | waterfall |
| ---: | ---: | ---: | ---: |
| 0 | 25.1 | 25.1 | 97.7 |
| 1 | 122.3 | 609.6 | 212.3 |
| 8 | 546.4 | 3555.5 | 677.9 |
| 64 | 3941.9 | 24129.6 | 5032.8 |

- **serial / waterfall:** await sequentially in the caller's task, no spawn; roughly 60–80 ns per listener. Waterfall adds chained `next` and terminal calls.
- **parallel:** spawn one Tokio task per listener. Listeners can truly run concurrently on multiple workers and one panic does not affect others; overhead is about 0.4 µs per listener. Serial costs less for tiny callbacks. For I/O or substantial computation, concurrency gains can outweigh this overhead.
- **Outside a runtime task** (`block_on`), parallel also wakes the caller across threads on every call: about 8 µs for one listener. The benchmark therefore runs inside a task.
- Parallel's fixed overhead could be reduced by polling concurrently inside the caller's task without spawning, at the cost of losing multi-thread parallelism. This version does not change it.

### Backpressure: not added in this version

Decision: keep same-key emit semantics—fire and forget, serialized in emission order, with an unbounded queue. Do not add queue caps, dropping, or blocking.

- **Preserve semantics:** any cap must choose between dropping events, failing `emit`, or making `emit` wait. All three change existing caller behavior, and none is correct for every event.
- **Existing alternatives:** callers that need completion use `parallel` / `serial`, naturally bounded by processing speed. Slow work inside a listener should be spawned into its own task or bounded queue (see “Choose a dispatch mode” in the development handbook).
- **Observe first:** diagnostics now expose backlog for alerting. If a real backlog case appears, add an opt-in option (such as a bounded emit variant) without changing the default.
- **Prefix indexes:** linear scan costs about 1.5 µs per dispatch at 1,024 prefixes; no index or cache is added yet.
