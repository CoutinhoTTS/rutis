# feat(core): Add Fiber Instance Keys and Subtree Visibility

Status: published as [#22](https://github.com/arcships/rutis/issues/22). Covers R1. Design: [full design §2](../../design-instance-subtrees.en.md#2-identity-and-service-keys).

## Problem

Existing `TypeKey` can express a type and qualifier but cannot express that a service may be provided and read only within a fiber subtree. Callers must configure isolation themselves, and the framework cannot check access boundaries from an instance key.

## Scope

1. Add a private-field `InstanceId(NonZeroU64)` with `Copy`, `Eq`, `Hash`, and `Debug`. Allocate it when creating a fiber, never reuse it within the process, and keep it stable across reloads. `Ctx::instance` returns the current fiber identity; isolated derived contexts share that identity.
2. Add an instance field to `TypeKey` while keeping `Clone`; add `instance::<T>(id)`, `with_instance(id)`, and `instance_id()`. Keep all existing qualifier APIs. Include the instance in `Eq` / `Hash` and show it in `describe`.
3. Use one actual-ancestor-chain check for `get_as`, dependency gating, and providing. An out-of-scope provide returns `InstanceOutOfScope`; an out-of-scope read or gate is unresolved. A different root or stale ID cannot bypass the check.
4. Add a root-shared admission facility and reclaimable weak child references. Service commits and lifecycle checks share the same synchronization boundary. Do not execute user code such as metadata callbacks under the admission lock.
5. Add minimal diagnostic DTOs and record out-of-scope `ServiceAccess` during `apply`. Reuse fixed declarations, actual bindings, and observed check state; reading a snapshot must not call user code. Share this foundation with #13; full watch/observation work is out of scope.

Do not introduce `SessionId` / `BranchId`, forbid `TypeKey::of::<T>()`, or claim that missing IDs fail at compile time. Keep existing provider/generation/key/scope eviction identity.

## Acceptance criteria

- [ ] Two instances can each read a service of the same type in their own subtree; outsiders and sibling subtrees cannot. A nested instance can read a valid ancestor instance.
- [ ] Out-of-scope `provide_as` / `provide_as_with_check` inserts no binding and wakes no consumer; an out-of-scope gate does not call an external check.
- [ ] Out-of-scope declared dependencies and actual reads during `apply` are diagnosed without exposing external provider details.
- [ ] Equality, hashing, and descriptions work for combinations of static/dynamic qualifiers and instance fields; old key behavior is unchanged.
- [ ] IDs survive reloads and are not reused across new roots/fibers. Calling `instance()` on an old `Ctx` after shutdown does not panic or revive the fiber.
- [ ] Reloading a process-wide service reloads consumers in both instances; reloading an instance-scoped service does not affect sibling instances.
- [ ] Weak child references do not accumulate from diagnostics reads, failed creation, or ordinary disposal.
- [ ] Existing contract/parity/config_update flows pass; run and record test/clippy/fmt as specified in the design.

## Related work and delivery

Related: https://github.com/arcships/rutis/issues/13. Deliver upstream code, workflow tests, and rustdoc. The consumer repository handles vendor backport and source tracking in its migration task.
