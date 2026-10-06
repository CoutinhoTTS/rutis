# Rust Implementation Simplification Review

> 2026-08-18. Subject: the [Rust implementation](../rust/) (about 2,100 core lines, 70 tests all passing, two review rounds of fixes complete) and v5 of [design-rust-port.md](design-rust-port.en.md).
> Criterion: **reduce maintenance and cognitive burden**. Avoid unusual shapes and redundant objects unless needed. Line count is secondary; the primary measure is how many concepts and invariants maintainers must remember.
> Method: read all core source (`fiber.rs` / `registry.rs` / `effect.rs` / `ctx.rs` / `bus.rs` / `event.rs`) and ask of each concept, “does it earn its complexity?” For each deletion candidate, reason through equivalence across all paths and recheck the previous quick conclusions (three were overturned; see §5).

## Conclusion

Simplification is possible, with one structural redundancy: **the reverse dependency index and `last_deps` store the same information, so the reverse map can be removed.** The change would remove about **190 lines** (roughly 2,100 → 1,900 core lines). More importantly, it removes a cross-structure storage system and its synchronization obligation, one special flag, three Adapter structs, one manual Clone, one dead variant, and one ghost type; the public API also shrinks. Risk is concentrated in two changes, both protected by existing regression tests.

## 1. True redundancy: the same information is stored twice

### S1. Remove the reverse dependency index and reuse `last_deps` (structural; highest-value change)

`Registry.reverse: HashMap<(PluginId, u64, TypeKey), Vec<Weak<FiberInner>>>` (`registry.rs:54-57`) and each fiber's `last_deps` contain **the same set**: `add_consumer_edges(this, &deps)` and `last_deps = Some(deps)` in `load()` use the same `deps` (`fiber.rs:342-343`). `drain_effects` clears both during unload. The TypeScript original also has no reverse index; it resolves fibers individually when notifying.

**Fix:** when removing a provider, scan `inject_index[key]` and send eviction join tasks only to fibers whose `last_deps` contains that triple. Delete the `reverse` map, `add_consumer_edges`, `remove_consumer_edges`, and `take_reverse_entry` (about 50 lines), replacing them with about 12 lines of filtering.

**Path-by-path equivalence check:** the load window is identical (`last_deps` is set before apply, at the same point as the reverse edge); failure/reload windows are identical (`fail_load` and unload clear it at the same point); triple matching is equally precise. Each unload also avoids `remove_consumer_edges` scanning all edges (O(all edges)). Cost: provider removal changes from O(direct consumers) to O(fibers declaring that key), negligible at this scale.

**Cognitive benefit:** eliminate an entire maintenance obligation—the invariant that “reverse edges must stay synchronized with binding lifetimes.” Cross-structure synchronization has caused bugs repeatedly.

**Design update:** change D21 from “reverse-edge triple index” to “at removal, identify consumers whose `last_deps` contains the triple.”

### S2. The `resolved` flag is provably redundant

Settle waits for “resolved + zero in flight” (`fiber.rs:33-40, 123, 237-251`), but `ctx.plugin()` always posts an initial `RefreshDeps` before returning (`intents_inflight >= 1`), and the driver decrements the count after handling it. Thus the in-flight count already implies whether initial dependency resolution has completed.

**Path-by-path validation:** root never posts an intent (initially Active, count 0 means stable); no external observer can run between spawn and post because `plugin()` is synchronous; the post-failure count rollback occurs after settle's terminal fast path; the first intent may leave state unchanged (Pending → Pending), but `notify_inflight_drained` publishes an equal snapshot to wake waiters.

**Fix:** remove public `Snapshot.resolved`, `Trans.resolved`, `mark_resolved()`, and snapshot publication (about 25 lines). The complete contract suite protects against the original race where awaiting immediately after registration returned too early.

## 2. Odd shapes: structures that can disappear

### S3. Replace three `PhantomData` Adapters with blanket implementations

`ListenerAdapter<L, E>`, `WaterfallAdapter<L, E>`, and `TerminalAdapter<E, T>` in `event.rs` exist only to bridge typed traits to erased traits—the standard case for a blanket implementation of a local trait over another trait:

```rust
impl<E: Event, L: Listener<E>> ErasedCall for L { ... }
impl<E: Event, L: WaterfallListener<E>> ErasedWaterfallCall for L { ... }
impl<E: Event, T: Terminal<E>> ErasedTerminal for T { ... }  // T: Sized
```

Delete all three structs, their PhantomData fields, and wrapper code (about 40 lines). `event.rs` then contains typed traits, erased traits, and blanket bridges. Coherence is clean because the ErasedCall traits are local and have no other implementors.

### S4. Merge duplicated `Hook` / `WfHook` pairs with generics

The structures and `take_hooks` / `take_wf_hooks` are duplicated, including the subtle once claim (`fired.swap(true)`). **Duplicated subtle logic is a maintenance hazard:** one copy can be fixed while the other is missed. A generic `Hook<C>` plus one claim helper consolidates them (about 25 lines removed, no risk).

### S5. Replace manual `Binding` Clone and snapshot flag with `Arc<Binding>`

`removing: AtomicBool` is copied field-by-field by a manual Clone (`registry.rs:36-49`). A clone does not see later changes; this happens to produce the intended behavior but is a footgun. Store `Arc<Binding>` in Registry so the flag is naturally shared, then delete the entire Clone implementation (about 14 lines; pure improvement).

## 3. Small cleanup items

| # | Item | Notes |
|---|---|---|
| S6 | Remove dead `ServiceNotFound` variant | Confirmed to have no construction site in core; `get` returns Option per D13. Tests can use another variant as a generic error payload. |
| S7 | Remove ghost type `Key<T>` | Used by one test in the whole repository and no external users; it was the result of the tentative §7.2 decision. Reconsider under “do not add concepts unnecessarily”: `TypeKey::keyed::<T>()` is sufficient. |
| S8 | Reduce emit from two spawns to one | Currently spawn a listener task, then spawn an observer to prevent panic loss. Catch unwind around the call inside one task and route to ErrorSink at its tail; preserve D30 semantics and halve task count. |
| S9 | Consolidate duplicated driver cleanup | The root-Restart branch's `continue` bypasses loop-tail cleanup and duplicates a counter decrement; the Dispose arm calls `done_tx.send` directly instead of `complete_task`. Restructure around one exit path. |
| S10 (optional) | `RefreshDepsJoin` → `RefreshDeps(Option<Arc<Task>>)` | Reduce four variants to two and remove the “Join” suffix; simplify `Intent::task()` / `complete_intent` accordingly. |

## 4. Public API changes summary

`Snapshot` loses `resolved`; `CordisError` loses `ServiceNotFound`; `Key<T>` is removed (tentative decision reversed). Everything else is crate-internal.

## 5. Candidates proposed earlier and rejected after review (do not retry)

Three ideas from the previous quick review do not hold up or do not justify their cost:

1. **“Use watch instead of `Mutex<EffectState> + Notify` in EffectRecord” — not viable.** Claiming the transition (“Live → Draining” only once) is essentially a CAS. `watch::send` unconditionally overwrites, so two concurrent drains could both believe they are first. The mutex claim is essential; replacing Notify with watch has no net benefit. `effect.rs` (175 lines) has no odd shape; **leave it alone**.
2. **“Make dispose use snapshot joins and dismantle TransitionTask” — do not.** It saves a few lines but splits one unified join concept into three conventions (dispose waits on snapshot, restart uses oneshot, eviction uses a third), increasing cognitive burden. TransitionTask's unification (every joinable operation has one task, completed once, with Arc-cached result) **is itself the simplified design**; keep it.
3. **“Remove `Ctx::root_with`” — do not.** Zero call sites do not make it dead code: it documents the D8 API promise that an injected Handle takes precedence. Four lines and no maintenance cost.

## 6. Checked and irreducible mechanisms (load-bearing list)

| Mechanism | Why it is needed |
|---|---|
| Two-level `Box<Arc<T>>` in `StoredValue` | `?Sized` services (`Arc<dyn Trait>`) cannot coerce to `Arc<dyn Any>`; Box is the minimal solution (confirmed by two review rounds). |
| Permanent `snapshot_rx` | Tokio watch treats a channel with no receiver as closed and sends fail silently; this has caused a real bug. |
| `alive` + `drain_stale` | Alternatives are keeping drivers alive forever (one leaked task per fiber) or leaving joins waiting forever (deadlock). |
| Separate `intents_inflight` and `alive` atomics | The former tracks settle stability; the latter guards delivery. Their responsibilities differ. |
| FIFO enqueue under lock (`status_queue` + `flush_status`) | Required by D24's commit-order contract. |
| About 20 similar lines in `fail_load` and `unload` | Merging requires parameterized error routing (load error versus aggregated cleanup error), increasing rather than reducing concepts. |
| Two-variant `NextState` enum | Type-checks the target domain for unload more safely than passing a general `FiberState`. |
| `provided` list | Notifies provided keys while Active; scanning all bindings trades O(n) for O(1), with no net gain. |

## 7. Execution plan

Order by increasing risk; run full single-threaded and parallel regression suites after every step:

1. S5, S4 (zero-risk shape cleanup).
2. S3 (blanket bridges, low risk).
3. S6–S9 (small cleanup).
4. S2 (remove `resolved`, medium risk; full contract suite protects it).
5. S1 (remove reverse map, medium risk; `eviction_order`, `isolate_no_cross_evict`, `single_service_evict`, and `evict_after_consumer_disposed_completes` protect eviction precision).

Update the design document accordingly: D21 wording, reverse the §7.2 `Key` decision, and add this simplification batch to §8.
