# Instance Keys, Instance Events, and Permanent Subtree Shutdown

Status: in progress; associated GitHub issues [#22](https://github.com/arcships/rutis/issues/22), [#23](https://github.com/arcships/rutis/issues/23), and [#24](https://github.com/arcships/rutis/issues/24).

Comparison baseline: upstream `a6ce10300f3cd75caa7732e7d4e9e18877a073a0` and the rutis vendor snapshot pinned by dim-agent. This document describes proposed contracts; it does not claim current code already satisfies them.

## 1. Scope and baseline

rutis owns fiber identity, service visibility, event channels, subtree shutdown, and internal reclamation. Runtime assembly migration, Session/Branch business types, VENDOR-PATCH source updates, and real Session load tests belong to the consumer repository. rutis keeps its own 1,000-cycle reclamation regression test.

Handle upstream/vendor differences according to the actual code:

| Area | Current upstream | This work |
| --- | --- | --- |
| `TypeKey` | Static/dynamic qualifiers already exist; dynamic path holds an `Arc`; only `Clone` | Preserve and add an instance field |
| Event channels | `hooks`, `wf_hooks`, and `dispatch_tail` are already indexed by `TypeKey` | Extend the existing key; do not revert to a `TypeId` pair |
| Child-plugin reclamation | Mount-effect self-removal, dependency-index removal, empty-channel removal, and completed-tail cleanup already exist | Reuse and strengthen the permanent-shutdown completion barrier |
| Subtree traversal | Only `parent_fiber`; no children table | Add weak child links and detach them at terminal state |
| Diagnostics | No vendor-style `PluginDiagnostics` / `ServiceAccess` | Issue 1 adds a minimal interface in coordination with existing #13 |
| Root shutdown | `Shared.closing` and a cached result already exist; child-error aggregation differs from vendor | Preserve current API and semantics; do not copy vendor implementation directly |

## 2. Identity and service keys

### 2.1 API

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstanceId(NonZeroU64);

impl Ctx {
    pub fn instance(&self) -> InstanceId;
}

impl TypeKey {
    pub fn instance<T: ?Sized + 'static>(id: InstanceId) -> Self;
    pub fn with_instance(self, id: InstanceId) -> Self;
    pub fn instance_id(&self) -> Option<InstanceId>;
}
```

These are draft signatures; imports and implementation are omitted. `TypeKey` remains `Clone`, not `Copy`. Use separate constructor and getter names to avoid Rust method-name collisions. `with_instance` consumes and returns the key, preserving type and qualifier; if an instance is already present, replace it without adding another channel layer.

```rust
let key = TypeKey::keyed_dynamic::<Database>(name).with_instance(ctx.instance());
```

`of`, `keyed`, `keyed_dynamic`, and `Key<T>` conversions still produce `instance=None`. Equality and hashing use `(type_id, qualifier, instance)`; qualifiers are compared by content. `describe` outputs `Type`, `Type#qualifier`, `Type@id`, or `Type#qualifier@id`. This is diagnostic text, not a parseable identifier format.

### 2.2 ID lifetime

Allocate IDs when creating fibers, replacing the original proposal for lazy allocation on first use. Every successfully created fiber, including the root, gets a unique process-local ID. Restart/update/dependency reload of the same fiber preserves it; a recreated child gets a new ID.

Use a process-local monotonic counter. Keep the field private and provide no public constructor that restores an ID from an integer. Use checked allocation; on exhaustion, panic with a clear ID-exhausted reason before registering the fiber or starting its driver. Do not wrap and reuse IDs, and do not change the existing creation API for this unrecoverable boundary. IDs are not persisted, sent across processes, or coupled to a generation.

Shared identity metadata in `Ctx` stores the number, and contexts derived through `isolate` keep the same fiber identity. An externally retained `Ctx` therefore returns the same ID from `instance()` after its fiber ends; it neither allocates a new one nor keeps `FiberInner` alive. Knowing an old ID does not restore registration or access rights. The numeric `Copy` value needs no reclamation; associated tasks and registrations do.

### 2.3 Visibility

Before looking up a registry entry, a key carrying an instance must validate the caller's actual fiber ancestor chain, including itself. The ID is in scope only if its fiber appears in that chain. Walk the existing `parent_fiber` links; do not add a permanent ID-to-service table.

| Operation | Caller outside the instance subtree | Caller inside the subtree |
| --- | --- | --- |
| `provide_as` / `provide_as_with_check` | Return `InstanceOutOfScope`, with no registration side effect | Apply existing type checks, duplicate-registration, and lifecycle rules |
| `get_as` | Return `None` and record the out-of-scope reason | Apply existing provider-active and self-access-during-cleanup rules |
| `resolve_dep` | Treat as missing; do not run that binding's `check` callback | Apply existing gating rules |
| Dependency diagnostics | `OutOfScope`, without exposing an external provider | Return the actual recorded dependency state |

Add the structured error `InstanceOutOfScope { instance: InstanceId }`. A finished/closed context rejects new registrations with the existing lifecycle error. Instance checks do not relax lifecycle checks. Existing `isolate` continues to resolve scope for the complete `TypeKey`; binding identity remains `(provider, generation, key, scope)`.

Instance keys are runtime visibility rules. They do not require every `T` to use an instance key, distinguish business `SessionId` / `BranchId`, or revoke an `Arc` service already obtained legally. Business code must not claim that omitting an ID will fail to compile.

### 2.4 Minimal diagnostics

Add upstream `Ctx::diagnostics()`, using the vendor DTO concept and original `Arc` errors without mechanically copying its old `Copy` assumptions. Include plugin identity, parent, state/generation, declared and resolved dependencies, bindings, and reads during `apply`. `PluginDiagnostics` exposes the fiber's `InstanceId`.

Add an explicit `out_of_scope` field to `ServiceAccess`. Record an out-of-scope access before returning early from lookup; its provider/generation fields are empty. Add `OutOfScope` to `DependencyStatus`. Gating and diagnostics must use the same visibility decision.

Collect and deduplicate read records only during the current generation's `apply`, then clear them for the next generation. Show out-of-scope declarations for `Pending` plugins through dependency diagnostics; do not fabricate a `get` call. `diagnostics` does not call `name` / `injects` / `check` or drive tasks. Normal gating records check state. Reads provide a best-effort consistent snapshot, not a transactional snapshot of the whole tree.

Reuse `declared_injects` captured during registration so the index, gating, and diagnostics use the same declaration. Whole-tree traversal uses reclaimable weak child links; a node stays visible until terminal detachment. #13 can extend this later with `watch_diagnostics` and full observation.

## 3. Instance events

### 3.1 API and dispatch rules

```rust
on_instance<E: Event>(&self, ctx: &Ctx, id: InstanceId,
                     listener: impl Listener<E>) -> Result<Disposer, CordisError>;
emit_instance<E: Event>(&self, ctx: &Ctx, id: InstanceId,
                       event: Arc<E>) -> Result<(), CordisError>;
serial_instance<E: Event>(&self, ctx: &Ctx, id: InstanceId,
                         event: &E) -> impl Future<Output = Result<Option<E::Value>, CordisError>>;
parallel_instance<E: Event>(&self, ctx: &Ctx, id: InstanceId,
                           event: Arc<E>) -> impl Future<Output = Result<(), CordisError>>;
```

`on` / `emit` admit synchronously; `serial` / `parallel` admit on the first poll, so an unpolled future is not admitted. `Ok` from `emit` means only that the event was queued; callback errors still go to ErrorSink. `serial` keeps short-circuit semantics and `parallel` keeps aggregate-error semantics. Existing non-instance API signatures and dispatch modes do not change.

Both the registrant and emitter of an instance event must be inside the target instance subtree and use that root's bus. Cross-subtree business notifications should go through a business service at a shared ancestor. Validation failure returns `InstanceOutOfScope`; a closed subtree returns `Closed`. Validate even when there are no listeners, so sends after closure cannot report false success.

Non-instance events, different instances, and existing named channels never match each other. Instance variants build a `TypeKey` with `qualifier=None`; do not add public instance waterfall or combined named-plus-instance event APIs. Internal indexes keep the complete `TypeKey`, preserving named-channel behavior.

`emit` preserves admission order for the same event type and instance; separate channels are independent. Its linearization point must perform validation, listener snapshot, and tail linking together, preventing concurrent threads from swapping snapshot and queue order. `serial` guarantees listener order only within one call; concurrent serial calls do not share a tail.

The callback's `Ctx` still comes from the emitter. Listener resources belong to the registrant; code that needs the registrant's `Ctx` must capture it rather than confusing it with the callback argument.

### 3.2 Ownership of admitted dispatches

From successful admission until processing ends, instance dispatch holds an internal dispatch permit that shutdown waits on. It identifies the target instance, emitting fiber, and each listener's registration fiber in the snapshot. Count conservatively by fiber, waiting for the full snapshot even across generations; generation is not part of the in-flight count. Implementations may deduplicate counts but cannot count only tasks or retain only the last `JoinHandle`.

Treat the full snapshot as in flight until it is released. Shutting down a child stops registration/sending for that child and its descendants, but does not close the entire channel for ancestor instances used by siblings. Listeners registered by the closing node are excluded from new snapshots; dispatches already in a snapshot finish first.

- `emit` is owned by the bus for execution and completion; caller return/drop does not cancel it.
- `serial` borrows its payload. Dropping the future also drops the borrowed callback future, then releases the permit; do not turn borrowed data into a background task.
- If `parallel` creates child tasks, an owner must cancel/finish and join them. Dropping the caller future cannot release the permit before every started callback ends.
- Completion, errors, panic, and caller cancellation must all release the permit; no permanent in-flight counts.

The permit does not automatically track arbitrary tasks spawned by user callbacks; plugins remain responsible for registering business-task cleanup.

### 3.3 Event behavior during shutdown

Shutdown first stops instance-event admission and cancels subtree tokens, then waits for related admitted dispatches; only afterward may it unload related listeners and services. Do not drop or abort arbitrary callbacks simply to empty tables. A callback that never yields or finishes may block shutdown; a wait timeout does not mean shutdown completed.

After admission closes, an admitted callback that sends another instance event receives `Closed`. A host needing business terminal events should settle business state before permanent framework shutdown.

At stable entry points, public registration and instance dispatch return errors as follows. If multiple conditions apply, evaluate left to right. A fiber after ordinary `dispose()` is inactive; after permanent `shutdown()` it is closed. `effect()` has no instance key. A `Loading` generation already started still follows the existing assembly/rollback protocol.

| Entry point | Root/subtree permanently closed | Fiber unloaded/Disposed | Instance key out of scope |
|---|---|---|---|
| `effect()` | `Closed` | `InactiveEffect` | N/A |
| `provide_as()` | `Closed` | `InactiveEffect` | `InstanceOutOfScope` |
| `on_instance()` | `Closed` | `InactiveEffect` | `InstanceOutOfScope` |
| `emit_instance()` / `serial_instance()` / `parallel_instance()` | `Closed` | `InactiveEffect` | `InstanceOutOfScope` |

A callback may initiate shutdown of its subtree and return, but must not await a shutdown result that includes itself. `apply` and finalizers also must not wait on a shutdown barrier that includes themselves. Examples and API docs must state this. Do not promise automatic detection of arbitrary self-wait cycles created by user tasks.

Removing a listener through its disposer or ordinary reload must also drain its old-generation instance-dispatch references before deregistration completes, so callbacks cannot keep using released plugin resources. A callback must not wait for its own disposer. This constraint applies only to the new instance-event path; existing non-instance event contracts remain unchanged.

## 4. Permanent subtree shutdown and reclamation

### 4.1 API and admission

```rust
impl FiberView {
    pub fn shutdown(&self) -> BoxFuture<'static, Result<(), Arc<CordisError>>>;
}
```

Calling it on the root view delegates to existing `Ctx::shutdown`; calling `Ctx::shutdown` from a child context still shuts down the whole root. Non-root shutdown does not set `Shared.closing` or affect siblings. Ordinary dispose/restart/update keep their current contracts; a permanently closing subtree rejects restart/update and any later reload.

Use a monotonic per-fiber closing state and cached completion task; do not introduce a second business Session registry. A root-shared admission lock serializes shutdown commits with framework registration. Under the lock, touch internal state only: do not call plugin metadata, factories, or user callbacks, and do not await.

Before returning the future, `shutdown()` synchronously claims/reuses the shutdown task, fixes the subtree membership and ownership relations for this operation, closes member admission, cancels current-generation tokens, and submits an independent coordinator. A concurrently created child must either be included in the shutdown set or receive a terminal `Closed` view; it must not become an orphaned driver.

Validation and insertion for services/listeners must share one commit boundary. Publishing a new-generation token or entering `Loading` must also coordinate with the closing check, preventing a new uncancelled generation from appearing after cancellation. All admission paths use one lock order: root admission lock outermost; ancestor/state/children/table work in short critical sections; invoke user callbacks or wait only after unlocking.

### 4.2 Cleanup registration remains possible

Subtree shutdown rejects new plugins, services, listeners, and instance dispatch. However, an in-flight `apply` may still register cleanup for resources it has already acquired until `Unloading`. Registration must either be adopted by the current unload or run immediately and join the shutdown barrier; never return an error while leaving an unowned resource or rollback task.

This does not let cleanup factories bypass service/listener admission. Each framework registration API checks `closing` itself; it cannot rely only on the effect check. Existing public root-shutdown behavior remains based on current tests; this design does not make every old `effect` call succeed during closure.

### 4.3 Shutdown order

1. Synchronously close admission, claim the subtree, and cancel tokens.
2. Wait for admitted `apply` calls and related instance dispatches to exit. During any required cleanup window, services remain readable under existing rules; `closing` does not make every read return `None`.
3. Start dependency eviction and ownership cleanup. Consumers inside the closing subtree terminate directly without entering `Pending`; external consumers use existing dependency recheck/reload. Keep bindings until consumers drain, then remove them by the existing tuple and binding identity.
4. Attempt every cleanup, collect results, end each driver, and drain intent waiters.
5. Remove parent mount/children, declaration indexes, binding accounting, instance listeners and completed tails; release temporary references held by the coordinator.
6. Publish one cached public result only after drivers have actually exited and reclamation is complete.

“Downstream before upstream” means dependent consumers finish unloading before their dependency bindings are removed. It does not promise to infer a global topological order for arbitrary user effects inside plugins.

Members already closing must not reload on `RefreshDeps` / `Restart` / `Update`. A `Pending` member may terminate directly; tests need only ensure shutdown commit causes no new `Pending` transition or `apply`.

### 4.4 Avoid waiting on itself

The current mount-effect cleanup waits for `child.dispose`; the child's `release_transient` then drains the mount. Strict shutdown cannot follow this path and await its own public completion task.

Separate internal “this node's cleanup is done” from public “driver and subtree have fully exited.” Once the driver completes local cleanup, it delivers the internal result and exits. An independent coordinator joins it, detaches parent records, and publishes the public result. A child's self-detachment only completes/removes its mount ownership record; it does not invoke another shutdown callback that waits for itself.

If the parent already took the mount and started cleanup, it waits for the child's internal result; the child's self-detachment does not wait for that mount's drain. One state arbitrates whether the record is claimed or detached, preventing duplicate cleanup. Publish public completion only after real reclamation; do not avoid deadlock by notifying waiters early.

If parent and child shut down concurrently, reuse the existing subtree cleanup task. The parent joins the child's internal result and driver; do not create a second task that waits on the first.

Root shutdown or ordinary parent dispose/restart that encounters a permanently closing subtree must also join the same internal completion; it cannot release related resources early. Keep existing return values, error routing, and root restart rules for old operations. Do not impose the new strong subtree-shutdown barrier retroactively on every old `dispose` call.

### 4.5 Error ownership

Dependency edges order shutdown but do not collect duplicate errors. Ownership edges collect cleanup results from direct children. Multiple failures retain Aggregate structure; one failure preserves its original `Arc`. Successful shutdown does not return an error merely because the state becomes `Closed`.

Cache subtree results in the task held by that subtree's handle. A failed child that shuts down independently and detaches must not leave historical errors retained forever by a long-lived parent. When parent shutdown commits, it claims children not yet detached and includes their results. Claim and detach are mutually exclusive under the shared admission boundary; once claimed, the coordinator owns the result even if the child is later removed from `children`.

Do not change existing aggregation of errors from early effect cleanup as unrelated work. Store results from new permanent-shutdown mounts separately from ordinary `drained_errors`, avoiding duplicate retention through two paths. General historical error consumption remains existing issue #11.

### 4.6 Meaning of reclamation complete

Remove parent mount and weak-child links; unregister by `declared_injects` and delete empty keys; clear binding/provided accounting; remove instance hooks and tails after related dispatches finish; finish drivers and every dispatch/cleanup task owned by the framework. New instances cannot reuse old IDs.

Objects still held externally through `FiberView`, `Ctx`, service `Arc`, diagnostic snapshots, or error results may remain alive. Shutdown guarantees that internal ownership no longer retains the whole subtree; it does not forcibly destroy externally held objects. The framework does not archive diagnostic snapshots.

Use the existing sparse-compaction policy for parent collections. Regression tests check entries/internal references return to baseline and capacity does not grow with historical cycles; they do not require allocator/RSS to return exactly to its initial value.

## 5. Implementation and acceptance

Three issues cover identity/service visibility, instance events/in-flight ownership, and permanent subtree shutdown/reclamation. Deliver R3 and R4 together; do not publish a shutdown that returns early and promise reclamation later.

Issue 1 establishes identity, minimal diagnostics, reclaimable children, and shared admission. Issue 2 adds instance-dispatch permits and shutdown-admission interfaces. Issue 3 connects the full shutdown coordinator. Reuse existing root, dynamic-key, and reclamation logic; do not replace newer upstream capabilities with old vendor code.

Protect these workflows:

- Two sibling instances, nested instances, cross-root use, and stale IDs; ancestor instances are visible, siblings/outsiders are not; type, qualifier, and instance all participate in matching.
- Out-of-scope provides leave no residue; out-of-scope gating does not call `check`; diagnostic reads do not call user code; reloading a legitimate process-wide service affects both instances, while instance service changes affect only legal consumers.
- Instance-event isolation and emit order; B proceeds while A blocks; existing serial/parallel semantics; concurrent shutdown/send/register never admits a late event.
- Shutdown completes only after in-flight emit, dropped borrowed serial/parallel, and failed/panicking callbacks have exited; a callback can start shutdown and return without deadlock.
- Shutdown during Loading adopts late resource cleanup; races with new children, provides, listeners, and reloads leave nothing behind.
- Internal consumers stop first without reloading; external consumers can recover from `Pending`; parent/child shutdown, dispose/shutdown races, and rejoining after dropping a waiter all complete.
- Aggregate errors once by ownership edge and share the error `Arc`; detached historical failures do not accumulate in the parent over repeated subtree creation.
- Run 1,000 cycles of public-API create/shutdown under a long-lived root, including persistent siblings; after external handles are dropped, inspect Weak/Drop, internal entries, task count, and capacity trends.

Reuse `tests/contract.rs`, `parity.rs`, `event_keys.rs`, and `transient_release.rs`, adding instance/subtree workflow tests where needed. Internal counters are test instrumentation only. The vendor's `lifecycle_diagnostics.rs` is not yet upstream; move applicable framework workflow assertions, then run the full original tests when backporting to the consumer repo. Do not report vendor tests as passing upstream if they were not run.

Required implementation commands:

```sh
cargo +1.98.1 test --offline -p rutis
cargo +1.98.1 clippy --offline -p rutis --all-targets -- -D warnings
cargo +1.98.1 fmt --all -- --check
```

dim-agent's migration task owns real Session performance thresholds, runtime migration, and the vendor source commit update.
