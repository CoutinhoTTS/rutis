# Simplification Review of the Rust Implementation (Final)

> 2026-08-17. Goal: reduce maintenance and cognitive burden without changing functionality, especially where mechanism outweighs semantics.
> Method: read all 10 modules (`lib/bus/ctx/effect/error/event/fiber/key/plugin/registry`) and the test patterns in `contract.rs`; assess each proposal for lost functionality or broken tests.

## Methodological constraint

Most complexity in this code was **added through fixes** (review → bug fix → add mechanism), and nearly every mechanism corresponds to a real race that was fixed. Simplification is not “delete unnecessary code”; it is **identify which races are artifacts of a structural choice**. Change the root structure and an entire class of patches and their races may disappear together.

## Core conclusion: only the fiber driver task model is worth changing

### Current state: six patches to support “state machine runs in a separate task”

The fiber state machine currently runs in a persistent mpsc driver task ([`fiber.rs:453`](../rust/crates/min-cordis/src/fiber.rs#L453), `drive`). Reading all of `drive` / `post` / `drain_stale` / `settle_inner` / `evict_and_finalize` found six mechanisms that are **structural only, with no business semantics**:

| Mechanism | Location | Why it exists |
|---|---|---|
| Four-state `Intent` enum + `mpsc::UnboundedSender` | [`fiber.rs:142`](../rust/crates/min-cordis/src/fiber.rs#L142) | Send intents to the driver |
| `intents_inflight` atomic counter | [`fiber.rs:145`](../rust/crates/min-cordis/src/fiber.rs#L145) | Let settle distinguish “resolved but there are still unprocessed intents” |
| `alive` flag | [`fiber.rs:148`](../rust/crates/min-cordis/src/fiber.rs#L148) | Reject sends after driver exit to prevent an infinite join wait |
| `drain_stale` drain protocol | [`fiber.rs:529-543`](../rust/crates/min-cordis/src/fiber.rs#L529) | Drain leftovers before driver exit, with a yield loop to wait for late sends |
| `resolved` flag + double confirmation in settle | [`fiber.rs:714-719`](../rust/crates/min-cordis/src/fiber.rs#L714) | Narrow the TOCTOU between resolved Pending and an unprocessed reload notification |
| Out-of-band pre-cancellation (cancel before enqueueing dispose/restart) | [`fiber.rs:652`](../rust/crates/min-cordis/src/fiber.rs#L652), `:678` | Driver is serial; without pre-cancel, `apply` never receives cancellation |

**None of these six is business semantics; they are all patches to make the structural choice “state machine in a separate task” safe.**

### Why the driver task creates these races

The state machine has one essential requirement: **only one transition per fiber runs at a time.**

The driver expresses that through “one task serially consumes an intent queue,” but adds indirection: “caller → queue → driver.” Every patch fills a gap caused by that indirection:

- Queue may drop intents after driver exit → `alive` + `drain_stale` + `post` returning false (fixes for bugs 2/3).
- Intent completion and state updates can be out of sync → `intents_inflight` + `resolved` + double confirmation.
- Both apply and dispose use the same driver → out-of-band pre-cancellation bypasses serialization.

### Alternative: one `tokio::Mutex` per fiber

A `tokio::Mutex` directly expresses “only one transition at a time,” without a task, queue, counter, flag, or drain protocol. After the change:

- `dispose` / `restart` / `refresh` each `lock().await`, perform the full transition, and release.
- **Bugs 2/3 disappear at the root:** without a queue, there can be no queued intent that nobody processes.
- Remove `intents_inflight` / `alive` / `drain_stale` / `resolved` / out-of-band pre-cancellation.
- Settle no longer needs to wait for `inflight == 0`; **holding the lock directly proves there is no transition in flight.**

### Why Tokio Mutex is sufficient (two key arguments)

**Argument 1: dispose during apply is semantically equivalent.** The concern that “apply holds the lock, so dispose cannot get it” is unnecessary: in the driver model, dispose is also queued behind apply and waits for apply to exit (both are cooperative cancellation, neither preempts). Both models are equivalent in waiting for apply to finish. In the existing dispose-during-loading test, a cancellation token lets apply return cooperatively; with a lock, dispose can call `cancel_current()` outside the lock, then wait with `lock().await`.

**Argument 2: concurrent notifications do not depend on a driver task.** When a provider is evicted, `notify_key_changed` sends a notification to each of N consumers. The current fire-and-forget `post(RefreshDeps)` makes their driver tasks concurrent. With locks, the notifier can remain fire-and-forget and spawn one task per consumer to acquire its lock and refresh. All N consumers remain concurrent. **Concurrency comes from the notifier not waiting, not from driver tasks.**

### The one invariant that needs a new anchor: exactly once

Today, dispose is exactly once through a single `Arc<TransitionTask>` in `terminal_task` under lock plus a join watch ([`fiber.rs:647-660`](../rust/crates/min-cordis/src/fiber.rs#L647)). With the lock model, callers hold the lock and perform unload themselves. Exactly once should be represented as:

- Under the lock, check whether terminal state already exists (`Disposed` plus cached `Arc<CordisError>`).
- The first caller performs unload and stores terminal state. Later/concurrent callers join the same `Arc<CordisError>`.

This is simpler: `tokio::Mutex` plus one `watch<Option<Arc<CordisError>>>` terminal signal replaces `TransitionTask` + `TaskDone` + the dedicated `join_task` loop + two completion paths (`complete_task` / `complete_intent`).

## Do not change these (confirmed as load-bearing after reading all code)

Two judgments from the previous round were wrong and corrected after reading the tests:

- **Keep the FIFO `status_queue` and `FiberStatusChanged` event.** The `state_transitions` test ([`contract.rs:250-264`](../rust/crates/min-cordis/tests/contract.rs#L250)) asserts the full path `[Pending→Loading→Active→Unloading→Disposed]`, sorted by `e.seq`. Watch has last-value semantics; slow subscribers skip intermediate states and cannot produce the full path. `FiberStatusChanged` is public API (exported at `lib.rs:27`) and carries the history watch cannot provide.
- **Keep the two watch channels (`TransitionTask.done_tx` and `snapshot_tx`) separate.** The first carries completion of one transition plus error identity (exactly-once + `Arc<CordisError>`); the second carries a continuous status stream where updates may be skipped. They answer different questions. Combining them would overload `Snapshot` with transition fields and increase cognitive burden.
- Other confirmed load-bearing mechanisms: double `StoredValue` wrapping (supports `T: ?Sized`); `CatchUnwind` (user callback panic boundary); reverse-index triples (required granularity); `removing` flag (self-access during cleanup); independent spawned task for `EffectRecord` (cancellation safety, fix for bug 4).

## Effort and risks (honest limits)

This is a small refactor, not free work:

1. Re-anchor exactly-once behavior (above).
2. Change internals of the public `dispose`, `restart`, and `settle_inner` entry points.
3. Change `RefreshDepsJoin` in `evict_and_finalize` ([`ctx.rs:333-362`](../rust/crates/min-cordis/src/ctx.rs#L333)) to acquire each consumer's lock, refresh, and join completion.
4. Rerun 69 tests plus adversarial cases for bugs 2/3.

**Benefit:** remove one entire abstraction layer (intent queue) and six patch mechanisms, lowering the cognitive burden of fiber lifecycle from “understand an actor model and its patches” to “understand one lock.” This is the crate's only opportunity to remove a whole abstraction rather than move code.

**Risk:** a bad exactly-once re-anchor could reintroduce bugs 2/3/8. Existing concurrency tests (`concurrent_dispose_join`, `dispose_during_dispose`, `cancel_wakes_awaiters`) and adversarial bug 2/3 cases must cover it.

## Conclusion

Only proposal #1 (fiber driver task → `tokio::Mutex`) is worth doing. It removes an abstraction layer and eliminates the whole class of concurrency bugs 2/3 at the root. Other candidates (`status_queue`, merging watches, simplifying `Disposer`) are load-bearing or only move code, so leave them unchanged.

Recommendation: implement #1 in a separate commit, then run the full suite plus adversarial tests; do not bundle unrelated changes.
