# Review: Simplifying Rust Design and Implementation for Maintainability

> Date: 2026-08-18
> Subject: [Rust design](design-rust-port.en.md) and current [`rust/`](../rust/) implementation
> Goal: reduce long-term maintenance cost and cognitive burden without changing existing functionality or concurrency semantics.
> Principle: avoid special forms, duplicate state, redundant objects, or a second lifecycle rule unless necessary. Line count is not the goal.

## 1. Conclusion

The implementation already has a reasonably complete feature and test foundation; the core architecture is not fundamentally over-designed. Fiber mailbox, state snapshots, generation cancellation, exactly-once Effect cleanup, and typed events all have clear responsibilities and should not be deleted just to shorten code.

The real simplification target is not “the number of objects,” but a few places where **the same fact is represented by multiple mechanisms**:

| Fact to represent | Mechanisms currently involved | Recommended consolidated model |
|---|---|---|
| Has prior Fiber work completed? | `resolved`, `intents_inflight`, Notify, double watch read, stale drain | Answer directly with a mailbox `Settle` barrier |
| Can an Effect still be registered? | Transition state lock, effects lock, state check before registration | Put state and Effect ownership in one lifecycle critical section |
| Is a Binding still visible? | Provider state, `removing`, copied Binding | One shared `Arc<Binding>` identity |
| Has a once listener been consumed? | EventBus mutex, `AtomicBool` | EventBus mutex only; remove it when selected |

These four are worth implementing. They reduce the invariants maintainers must hold simultaneously, not merely the line count.

The implementation also has two concurrency windows that should be fixed first, plus several error-handling issues. Prefer to fix them using the consolidated models above; do not add reservations, Preparing objects, or other side-channel state.

## 2. Review criteria

This review does not judge whether “a struct can be deleted.” It asks whether a change is genuinely simpler:

1. Does it reduce state that must stay consistent across objects, locks, or tasks?
2. Does it give each lifecycle one clear owner?
3. Can existing core project models express it without exceptional paths?
4. Does the new concept replace several old rules, rather than merely moving code into a new abstraction?
5. On failure or contention, can a maintainer determine behavior from the local code?

If a change only removes a field, allocation, or a few wrapper lines without reducing those burdens, it should not be a standalone simplification task.

## 3. Core models to retain

These mechanisms add implementation volume, but have clear responsibilities tied to real feature or concurrency requirements:

- **Fiber mailbox:** serializes control operations such as load, unload, restart, refresh, and dispose.
- **watch snapshot:** provides low-cost, subscribable state observation to external callers.
- **Generation CancellationToken:** isolates background tasks from other load generations.
- **EffectRecord's Live / Draining / Done states:** guarantee exactly-once cleanup and let multiple callers share the result.
- **TransitionTask:** gives awaitable state transitions a unified completion semantic and error identity.
- **Typed public events + internal type erasure:** retain public API type safety while supporting heterogeneous internal storage.
- **Separate regular and waterfall events:** execution and error propagation semantics genuinely differ.
- **`Arc<CordisError>` error identity:** ensures concurrent waiters see the same aggregate error.

The boundaries between these mechanisms are generally sound. Combining them into a “universal state machine,” generic Completion, or one event enum would make the system harder to understand.

## 4. Implementation issues to fix first

### P1: `Binding::clone()` copies mutable concurrent state

Location: `Binding`, its manual `Clone`, `lookup()`, and `mark_binding_removing()` in `rust/crates/min-cordis/src/registry.rs`.

`Binding` contains `removing: AtomicBool`, but manual Clone creates a new AtomicBool. After `lookup()` returns a copy, marking the original Binding as removing may not be visible to that copy.

Thus, if service removal races with `get()`, a service already being removed may remain briefly visible.

Recommended fix:

- Store `Arc<Binding>` in Registry.
- Return an Arc to that same Binding from lookup.
- Remove `Binding`'s manual `Clone`.

The goal is not merely to remove an implementation; it establishes a direct invariant:

> Registry, lookup, dependency resolution, and disposer all observe the same Binding identity.

### P1: `Ctx::effect()` has a race between check and registration

Location: `Ctx::effect()` in `rust/crates/min-cordis/src/ctx.rs`, and effect draining in `fiber.rs`.

The current flow checks Fiber state, releases the transition lock, runs the effect factory, and only then inserts the EffectRecord into a separate effects collection.

If the Fiber enters Unloading after the check and has already taken its effects, an EffectRecord inserted afterward is not cleaned up by that unload. It may leave behind a listener, service, or background task.

Do not add these concepts to solve it:

- effect registration reservation;
- `PreparingEffect`;
- registration ticket;
- a second “registration in progress” counter.

Those approaches expand lifecycle state space further.

Recommended fix: put Fiber lifecycle state and Effect ownership under the same critical section:

1. Run the user effect factory outside the lock, never while holding a lock across user code.
2. Create the EffectRecord.
3. Under one lifecycle critical section, check state and add the record to `effects` atomically.
4. If lifecycle has already changed, clean up the new EffectRecord immediately and return registration failure.

The corresponding invariant is:

> Fiber state transitions and Effect-set changes are serialized by one lifecycle domain.

### P2: Nonterminal unload silently discards cleanup errors

Location: `unload()` in `rust/crates/min-cordis/src/fiber.rs`.

Cleanup errors are already aggregated, but the branch entering Pending neither returns the error nor sends it to ErrorSink. Cleanup failures during restart or dependency refresh are therefore invisible.

To preserve existing restart behavior, send this aggregate error to ErrorSink rather than letting it change the next generation's state.

### P2: `JoinError::into_panic()` does not distinguish panic from cancellation

Locations include:

- `rust/crates/min-cordis/src/bus.rs`;
- `rust/crates/min-cordis/src/effect.rs`;
- `rust/crates/min-cordis-agent/src/lib.rs`.

A Tokio task can panic or be cancelled. Calling `into_panic()` directly panics again on cancellation and breaks the error-convergence path.

Handle them uniformly but distinctly:

- panic → convert to the existing panic error;
- cancellation → convert to an explicit task-cancelled error;
- for either outcome, ensure EffectRecord eventually reaches Done.

### P2: Invalid Ctx returns a non-cancelled token

Location: `cancellation_token()` in `rust/crates/min-cordis/src/ctx.rs`.

When Fiber no longer exists, current code returns a new, non-cancelled token. Calling `cancelled().await` through a retained old Ctx can wait forever.

Return an already-cancelled token so that “Fiber no longer exists” is equivalent to “its generation has ended.”

## 5. Cognitive simplifications to implement

### S1: Replace stability inference with a mailbox `Settle` barrier

#### Current problem

Fiber already serializes control operations through a mailbox, but `FiberView::into_future()` infers “the Fiber is truly stable” externally by combining several signals:

- `Snapshot.resolved`;
- `intents_inflight`;
- `notify_inflight_drained`;
- double watch read;
- stale-intent drain and yield when the driver exits.

Maintainers must understand update order and all race windows among these signals to know why an await cannot return early or wait forever.

#### Recommended model

Add one explicit mailbox intent:

```rust
Intent::Settle(TransitionTask)
```

When the driver handles Settle, every control operation already in the mailbox ahead of it has completed. Settle completes the TransitionTask using the state at that point.

On Dispose, close the receiver and drain all pending intents with completion signals. If a send occurs before close, it is drained; if it occurs after close, send fails and the sender immediately completes its task.

With this model, evaluate removing:

- `Snapshot.resolved`;
- `Trans.resolved`;
- `intents_inflight`;
- `notify_inflight_drained`;
- the double-check and yield protocol used to infer stability.

#### Why it is simpler

Settle adds one Intent variant but replaces several scattered invariants. It uses the project's existing actor/mailbox model rather than introducing a second synchronization system.

The contract needs only one clear sentence:

> Settle guarantees that operations enqueued before it in the same mailbox have completed; operations delivered concurrently with Settle are linearized by their actual mailbox order.

If product semantics require waiting for “concurrent messages that may arrive in the future,” no finite instant can strictly establish stability. Mailbox ordering itself should therefore define settle.

### S2: Unify the atomic ownership domain for Fiber lifecycle and effects

This also fixes the `Ctx::effect()` race.

The goal is not to put all Fiber data behind one lock, but to bring together only the two facts that must be atomically consistent:

- whether the current lifecycle permits effect registration;
- the EffectRecord set owned by the current lifecycle.

State observation, event publication, and CancellationToken need not be merged just to reduce fields.

Recommended rules:

- Always run the effect factory outside the lock.
- Complete state check + EffectRecord insertion in one critical section.
- During unload, change lifecycle state and take the effect set in that same critical section.
- If registration fails, use existing cleanup semantics immediately; do not add a new partially initialized state.

### S3: Give Binding one shared identity

Change Registry to store `Arc<Binding>` and remove snapshot-style copying of internal mutable state.

Keep this change local. There is no need to rewrite the entire registry or change StoredValue's erasure form just to save an allocation.

### S4: Let EventBus mutex alone manage once listeners

The listener collection is already serialized by the EventBus mutex, so a separate AtomicBool for once expresses the same state through two synchronization mechanisms.

Remove the once listener from the collection at the moment it is selected under the lock:

- concurrent emit still lets only one caller obtain it;
- a later disposer naturally becomes a no-op;
- a fired closure is no longer retained;
- remove `fired` and its atomic-ordering reasoning.

Regular and waterfall hooks may share a small internal helper for taking and consuming once, but do not wrap their distinct call signatures in a complex generic enum just for reuse.

## 6. Changes not recommended as simplification projects

### 1. Do not rewrite StoredValue just to remove one Box layer

This is a storage detail. Unless profiling or an API constraint demonstrates a problem, it does not affect the main mental model and is not worth changing by itself.

### 2. Do not rewrite Disposer just to remove an Option

Whether Disposer internally stores `Option<FnOnce>` has little effect on caller or lifecycle understanding. Avoid churn without benefit.

### 3. Do not force CancellationToken into the transition mutex

State transitions and cancellation are two understandable responsibilities. Coupling them to save one Mutex would make lock scope and call order harder to reason about.

### 4. Do not split status queue into scattered return-value publication

The current status queue clearly means “determine order under lock, publish outside.” If each call site returned and published events independently, a new omission rule would emerge. Preserve the queue without evidence of a problem.

### 5. Do not introduce a generic Completion abstraction

EffectRecord and TransitionTask both have completion/waiting behavior, but their ownership and state transitions differ. Forcing an abstraction would likely create infrastructure with many parameters understood only by its implementer, without reducing domain concepts.

### 6. Do not proactively merge regular, parallel, serial, and waterfall

Their call forms, execution order, and error propagation genuinely differ. Small internal helpers are fine; do not collapse public semantics into one executor with many branches.

### 7. Do not add an empty Effect so every side effect “looks the same”

Background tasks need clear ownership: either a generation token owns their lifecycle, or an Effect disposer is responsible for join/abort. Do not use `Effect::Done` to create the appearance of registration.

Choose one ownership path based on whether task termination must be awaited, rather than keeping both mechanisms for structural uniformity.

### 8. Do not add a Weak-index maintenance protocol without evidence

If Registry weak references are later shown to accumulate, add a local `retain` at an existing traversal point. Do not preemptively add explicit registration, unregistration, or generation cleanup protocols.

## 7. Implementation order

Proceed in this order, using a separate commit and full test suite for each step:

1. **Fix Binding identity:** make Registry use `Arc<Binding>`.
2. **Unify the lifecycle/effect atomic boundary:** fix effect registration vs unload.
3. **Fix error convergence:** cleanup errors to ErrorSink, JoinError cancellation, invalid-Ctx token.
4. **Simplify once listener:** consume/remove under lock and delete AtomicBool.
5. **Add Settle barrier:** add contract tests first, then remove old stability-inference state.

Do not combine these into one large refactor. Implement Settle separately after the preceding correctness fixes stabilize so behavior changes remain attributable.

## 8. Acceptance requirements

In addition to existing full-suite tests, add at least:

1. Concurrent `get()` and service disposer: a Binding marked removing is no longer visible.
2. Unload during effect-factory execution: the new effect is immediately cleaned up, exactly once.
3. Settle queued after initial RefreshDeps does not complete early.
4. Settle racing a provider notification follows mailbox order.
5. Concurrent Dispose and Settle/Restart delivery completes every TransitionTask.
6. Concurrent emit against a once listener invokes it only once and does not retain the Hook afterward.
7. If a cleanup task is cancelled, EffectRecord still reaches Done and all waiters get a consistent result.

Verification commands:

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

## 9. Final boundary judgment

This implementation can be simplified further, but “how many structs were deleted” is the wrong measure.

The most valuable reductions remove three kinds of maintenance burden:

1. inferring mailbox stability from several atomics and snapshots;
2. scattering one lifecycle fact across several locks and collections;
3. maintaining atomic flags or copied snapshots for state already protected by a mutex.

After S1–S4, the core model can be summarized as:

- mailbox owns control-operation order and settle;
- the lifecycle critical section keeps state and effects consistent;
- every Registry Binding has one shared identity;
- EventBus mutex owns the listener set and once consumption.

These four sentences explain the main concurrency ownership relationships. Reaching that clarity is a better measure of simplification than the number of lines removed.
