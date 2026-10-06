# Rust Implementation Simplification: Combined Conclusion from Three Reviews

> 2026-08-18. Combined findings from Dim's independent analysis, [simplification-rust-impl-2026-08-18.md](simplification-rust-impl-2026-08-18.en.md) (“Review A,” focused on removing redundancy), and [review-rust-maintainability-simplification-2026-08-18.md](review-rust-maintainability-simplification-2026-08-18.en.md) (“Review B,” focused on correctness and consolidation).
> Subject: the [Rust implementation](../rust/) (about 2,100 core lines, 70 tests all passing, two review rounds of fixes complete).
> Criterion: reduce maintenance and cognitive burden. Do not count structs; ask whether each mechanism earns its complexity.

## Summary

The code is not over-designed. Three cases of “the same fact stored twice” should be removed, and four correctness bugs must be fixed first. Most complexity is honest complexity introduced to fix real bugs. The right simplification is not to delete seemingly unnecessary objects, but to express duplicated facts once.

## First, correct two false positives: apparent duplication that is not duplication

Two simplifications suggested in Dim's first two rounds were disproven by cross-checking all three reviews. Record these conclusions to prevent regressions:

- **Keep complete, ordered state events; narrow the claim about `status_queue` (Codex item 6).** The `state_transitions` test ([contract.rs:250-264](../rust/crates/min-cordis/tests/contract.rs#L250)) proves that `FiberStatusChanged` cannot be replaced by watch: watch exposes only the latest value, so slow subscribers can miss intermediate states and cannot observe the full transition path. It does not prove that a queue is the only implementation. Accurate wording: complete ordered state events are required; the current status queue is clear and has no demonstrated problem, so there is no reason to rewrite it.
- **Do not merge the two watch channels.** `TransitionTask.done_tx` (completion and error identity for one transition) and `snapshot_tx` (a continuous state stream where intermediate states may be skipped) serve different purposes. Merging them would stuff transition fields into `Snapshot` and increase cognitive burden.

Lesson: **something that looks duplicated is not necessarily duplicated.** Judge whether a mechanism earns its complexity, not how many structs exist.

## Three real redundancies (independently identified by all reviews)

### 1. `Binding` identity is copied (do first; no behavioral risk)

`lookup()` returns a copy whose `removing: AtomicBool` reflects only the value at copy time. After a service is marked for removal, an old copy still considers it visible. This is not just redundant; it is a latent correctness bug that briefly exposes a service while removing it.

**Fix:** store `Arc<Binding>` in Registry and return an Arc to the same identity from lookup; remove the manual Clone ([registry.rs:36-49](../rust/crates/min-cordis/src/registry.rs#L36)). Establish the invariant that Registry, lookup, dependency resolution, and disposers all observe the same Binding identity.

### 2. The once-listener `fired` atomic is a second lock

The EventBus mutex already serializes the listener set, but once listeners also have a separate `fired: AtomicBool` ([bus.rs](../rust/crates/min-cordis/src/bus.rs)). This means one state has two synchronization schemes. The subtle `fired.swap(true)` claim logic is also duplicated in `take_hooks` and `take_wf_hooks`, making it easy to fix one and miss the other.

**Fix:** while holding the lock, remove a once listener as soon as it is selected. Concurrent emits still allow only one caller to take it; a later disposer naturally becomes a no-op. Delete `fired` and its atomic-ordering reasoning.

### 3. “Is the plugin stable?” is inferred from five signals (the largest cognitive burden)

To determine why an await will neither return early nor wait forever, maintainers must remember five update mechanisms:

- `Snapshot.resolved`
- the `intents_inflight` atomic counter
- `notify_inflight_drained`
- settle's double watch read
- the yield loop in `drain_stale`

**Fix (Review B's Settle proposal, endorsed by Dim):** add an `Intent::Settle(TransitionTask)` barrier. Stability means “all work queued before me in the mailbox has completed.” FIFO mailbox order can answer that directly; there is no need to infer it from five side-channel signals. When the driver handles Settle, earlier control operations are complete, so it completes the `TransitionTask` using the state at that point.

One new Intent variant replaces five scattered mechanisms. This is the one worthwhile trade of one concept for several fewer concepts. Remove `resolved`, `intents_inflight`, `notify_inflight_drained`, the double-check, and the `drain_stale` yield protocol.

## A storage redundancy and its relationship to Settle (found independently by Review A)

**The reverse dependency index and `last_deps` store the same dependency set.** In `load()`, `add_consumer_edges(this, &deps)` and `last_deps = Some(deps)` use the same `deps` ([fiber.rs:342-343](../rust/crates/min-cordis/src/fiber.rs#L342)). Removing the index could eliminate the cross-structure invariant that reverse edges must stay synchronized with binding lifetimes, a recurring source of bugs.

**This is a candidate, not a confirmed conclusion (Codex correction).** Deletion would require scanning `inject_index` when removing a provider, making Registry eviction logic read `Fiber.last_deps`, and proving that `last_deps` matches the active consumer relation across every load, failure, and unload path. Verify independently before deciding; if the proof holds, removal is preferred.

**There is no required coupling to Settle (Codex correction; Dim withdrew the earlier coupling claim).** Settle changes how we wait for earlier mailbox work; removing the reverse map changes how we find consumers to evict. `RefreshDepsJoin` can remain as-is. These changes are semantically independent. **Use two separate commits** so the regression surface stays small and failures remain attributable.

## Correctness issues (independently found by Review B; fix before simplification)

There are five issues total: the Binding identity issue above, plus these four:

| # | Location | Problem | Fix |
|---|---|---|---|
| P1 | [`ctx.rs` `effect()`](../rust/crates/min-cordis/src/ctx.rs) | Registration has a race: check state, release lock, run factory, then register. If the fiber enters Unloading meanwhile, the new effect is missed by this unload and leaks a listener/service. | Atomically check state and add to `effects` under one critical section. Run the factory outside the lock. If lifecycle state changed, clean up the new record immediately and return failure. |
| P2 | [`fiber.rs` `unload()`](../rust/crates/min-cordis/src/fiber.rs) | A nonterminal unload (restart/dependency refresh) silently drops cleanup errors. | Aggregate and route errors to ErrorSink without changing the next generation's state. |
| P2 | bus/effect/agent code | `JoinError::into_panic()` does not distinguish panic from cancellation; cancellation panics again and breaks error convergence. | Distinguish panic (panic error) from cancellation (explicit task-cancelled error). In either case, ensure `EffectRecord` reaches Done. |
| P2 | [`ctx.rs` `cancellation_token()`](../rust/crates/min-cordis/src/ctx.rs) | If the fiber no longer exists, it returns a non-cancelled token and `cancelled().await` waits forever. | Return an already-cancelled token so “fiber absent” means “generation ended.” |

These are correctness bugs. No amount of elegant simplification compensates for them; fix them first.

## Small cleanups (low risk; can be done at any time)

- Reduce emit's double spawn to one: catch unwind inside the task, route to ErrorSink at the end, and preserve D30 semantics.
- Merge duplicated `Hook` / `WfHook` logic with generics (alongside once simplification #2). Share the helper that removes and consumes a once registration, but do not wrap the two call signatures in a generic enum.

**Do not do these (Codex correction; Dim withdrew two aggressive suggestions from Review A):**

- **Do not remove `ServiceNotFound` or `Key<T>`.** They are public API: user plugins can construct `ServiceNotFound`; `Key<T>` provides a typed qualifier that can be declared `const`, which `TypeKey::keyed::<T>()` does not fully replace. Removing them affects functionality and compatibility while barely reducing the core mental model.
- **Do not use blanket Adapter implementations (Codex item 1; Dim retracts adoption of Review A's S3).** `impl<E, L: Listener<E>> ErasedCall for L` has an unconstrained type parameter: one `L` can implement `Listener<E>` for multiple E types, so E is not uniquely determined. `ListenerAdapter<L, E>`'s `PhantomData<fn() -> E>` fixes E in the self type and is required by Rust's type system. Keep it as a load-bearing design.

## Load-bearing mechanisms confirmed by all three reviews

| Mechanism | Why it should stay |
|---|---|
| Two-level `Box<Arc<T>>` in `StoredValue` | The reason to leave it is that it is a local storage detail and changing it would not reduce cognitive burden—not that it is technically impossible to change. (Codex correction: an outer `Arc<dyn Any>` could store `Arc<T>` directly; Box is not the only solution.) |
| Permanent `snapshot_rx` | Tokio watch treats a channel with no receiver as closed and sends fail silently; this pitfall has occurred in practice. |
| `status_queue` + `FiberStatusChanged` | See the false-positive section: complete ordered events are required, and the current queue is clear and has no demonstrated problem. (Argument narrowed per Codex correction.) |
| Two separate watch channels | They serve different concerns, as described above. |
| Standalone CancellationToken | State transitions and cancellation are distinct responsibilities; combining them would make locking harder to understand. |
| Four dispatch modes | Calling convention, execution order, and error propagation genuinely differ. Share small helpers if useful, but do not compress them into one executor. |
| `EffectRecord`'s `Mutex<EffectState> + Notify` | Claiming Live → Draining must happen once (a CAS-like transition); unconditional watch overwrite cannot express it. |
| Three Adapters with `PhantomData<E>` | Required by Rust's type system to fix E in the self type; otherwise E is unconstrained. See above. |
| No generic Completion abstraction | `EffectRecord` and `TransitionTask` have different ownership and state transitions. A forced abstraction would become infrastructure with many parameters that only its implementer understands. |

## Execution order (risk-based; separate commit and full tests at each step)

1. **Fix correctness:** all five issues (`Arc<Binding>` shared identity, atomic effect lifecycle boundary, cleanup errors to ErrorSink, distinguish cancellation from panic, return an already-cancelled token from stale Ctx). Make the code correct first; this is the foundation for simplification.
2. **Remove once's `fired`:** low risk and immediately removes a synchronization scheme.
3. **Implement and verify Settle independently:** add one Intent variant to replace five stability signals. Keep it as a separate step for attribution.
4. **Evaluate whether `last_deps` can replace the reverse map independently:** prove consistency across load/fail/unload before deciding; if proved, deletion is preferred. **Keep separate from step 3; do not bundle.**
5. Optional: reduce emit to a single spawn.

After step 3, fiber concurrency ownership can be summarized in three sentences: the mailbox owns ordering; one lock owns state and effects; each Binding in Registry has one identity.

## Companion updates

- In design decision D21, change “reverse dependency triple index” to “determine consumers at removal by checking whether `last_deps` contains the triple” only if step 4 validates and implements the replacement.
- Add this simplification round to §8 of the design.
- (Codex correction) Keep `Key<T>`; do not revise the §7.2 decision.

## Acceptance

Run after each step:

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

Step 3 (Settle barrier) needs tests proving:

- Settle queued after initial `RefreshDeps` does not complete early.
- Settle and provider notification racing follow mailbox order.
- Concurrent Dispose and Settle/Restart delivery completes every `TransitionTask`.

If step 4 removes the reverse map independently, verify that `last_deps` matches the active consumer relation on load/fail/unload paths and retain eviction regressions for `eviction_order`, `isolate_no_cross_evict`, and `single_service_evict`.

## Final assessment

Only after step 3 (Settle) will the number of concepts maintainers must remember truly drop a level. Steps 1, 2, and 5 are worthwhile cleanup; decide on step 4 after independent verification. Everything else moves code without removing concepts and is not worth doing. The complexity is honest: each mechanism corresponds to a real failure mode. Simplification therefore means **finding the few places where the same fact is stated twice and making it stated once**. Keep Codex's boundaries: do not remove public API, do not alter Adapters required by the type system, and do not bundle independent changes. Otherwise “reduce cognitive burden” turns back into “delete things for the sake of deletion.”
