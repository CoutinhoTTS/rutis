# feat(core): Permanently Shut Down and Reclaim Subtrees

Status: published as [#24](https://github.com/arcships/rutis/issues/24). Combines R4 and R3; depends on [instance keys](01-instance-keys.en.md) and [instance events](02-instance-events.en.md). Design: [full design §4](../../design-instance-subtrees.en.md#4-permanent-subtree-shutdown-and-reclamation).

## Problem

The framework has no independent permanent subtree shutdown entry point that guarantees synchronous admission stop, related event drain, driver exit, error ownership, and parent-record reclamation. Existing ordinary disposal notifications occur before some reclamation and cannot serve as the completion barrier for shutdown.

## Scope

1. Add `FiberView::shutdown() -> BoxFuture<'static, Result<(), Arc<CordisError>>>`. A non-root view shuts down only its subtree; a root view delegates to existing root shutdown. `Ctx::shutdown` retains its root scope.
2. The call site synchronously closes subtree admission and pre-cancels tokens, coordinating with registration and publication of new-generation tokens. The coordinator owns cleanup independently, so dropping a waiter does not stop it. Members can no longer restart/update/reload.
3. Keep cleanup registration for resources acquired by in-flight `apply`. Wait for `apply` and related instance dispatches to finish, then unload by dependency/ownership order. Include late rollback tasks in the completion barrier.
4. Terminate consumers inside the subtree directly without creating new `Pending` / load states; external consumers recheck and reload. Remove services only after dependent consumers clean up.
5. Separate internal cleanup completion from public shutdown completion. A child detaches itself from its parent without waiting for itself through mount cleanup. Parent and child concurrent shutdown calls claim the same task and cannot wait on each other.
6. Dependency edges determine order; ownership edges aggregate each error once. Parent claims and child detachment are mutually exclusive. Failures from independently shut down and detached children are not retained forever as parent history.
7. Reuse existing mount detachment, `inject_index` removal, empty event-table deletion, and sparse compaction. Reclaim child and instance in-flight records. Publish the cached result only after the driver has actually exited and reclamation is done.

## Acceptance criteria

- [ ] Immediately after calling `shutdown`, new plugins, services, listeners, and instance events are rejected without polling the returned future.
- [ ] `Loading`, `Pending`, `Active`, `Failed`, and already-disposed nodes all converge; shutdown creates no new loads.
- [ ] Consumers inside a subtree stop before providers. External consumers can reload if a provider later returns. Sibling subtrees keep working.
- [ ] Shutdown does not finish while an in-flight callback is blocked; it completes after release. A callback can initiate shutdown and return without deadlocking.
- [ ] All waiters finish when shutdown races with dispose/restart/update, parent shutdown, or independent child shutdown.
- [ ] Late resource registrations are cleaned up; one cleanup failure does not skip other cleanups. Concurrent waiters receive the same cached error `Arc`, and dependency edges are not aggregated twice.
- [ ] After 1,000 create/shutdown cycles and release of external handles, mount/children/inject_index/binding/instance-event entries and drivers return to baseline; capacity does not grow continuously with cycle count.
- [ ] Reclamation also works with persistent siblings, shutdown failures, repeated shutdown, and dropped waiting futures.
- [ ] Existing root dispose/restart/shutdown, no-instance events, parity, and lifecycle flows do not regress.

## Validation and boundaries

Run the cargo +1.98.1 test/clippy/fmt commands from the full design. Internal counters may help workflow tests; do not require RSS to return exactly to its prior value. For `lifecycle_diagnostics.rs`, which exists only in the vendor, state which applicable assertions moved upstream; do not claim consumer-repository tests ran if they did not.

This does not promise to interrupt non-cooperative synchronous code and does not cover business terminal events, Session assembly migration, or real product load testing. A timeout ends the wait; it must not falsely report successful shutdown.
