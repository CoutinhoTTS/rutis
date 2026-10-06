# feat(core): Add Instance Events and In-Flight Dispatch Ownership

Status: published as [#23](https://github.com/arcships/rutis/issues/23). Covers R2 and depends on [instance keys](01-instance-keys.en.md). Design: [full design §3](../../design-instance-subtrees.en.md#3-instance-events).

## Problem

Existing dynamic event channels can route events but have no instance-subtree registration/sending restrictions or ownership records that let subtree shutdown wait for related in-flight dispatch. Removing a listener and tail-list entry cannot stop a callback whose snapshot was already taken.

## Scope

1. Add `on_instance`, `emit_instance`, `serial_instance`, and `parallel_instance`. Both registrant and emitter must be inside the instance subtree and use that root's bus.
2. `emit_instance` synchronously returns `Result`; `Ok` means the event was admitted, while async callback errors go to `ErrorSink`. `serial` / `parallel` admit on their first poll and return results with existing dispatch semantics.
3. Keep the existing `TypeKey` index and named channels. Instance variants build instance keys without qualifiers. Do not add instance waterfall, `once`, or combined named-plus-instance entry points.
4. Order validation, snapshots, and linking of the emit tail in one commit sequence. Emits for the same type and instance are ordered; different instances are independent. `serial` guarantees listener order only within one call.
5. Record target instance, emitting fiber, and listener registration fiber for admitted dispatches. Conservatively drain cross-generation dispatches by fiber; do not split in-flight counts by generation. Provide admission-stop and drain operations for subtree shutdown. Shutting down a child does not close the whole channel for ancestor instances used by siblings.
6. Define ownership on drop/panic/error: a background task owns `emit`; dropping a borrowed `serial` future releases its admission token; `parallel` releases its token only after all child tasks finish or are cancelled and joined.
7. Removing/reloading an instance listener drains its old-generation in-flight references. Document that a callback may initiate shutdown but must not wait for shutdown containing itself or for its own removal.

Existing callback `Ctx` semantics remain: it comes from the emitter, while listener resources belong to the registrant. Arbitrary tasks spawned by user callbacks are outside bus guarantees.

## Acceptance criteria

- [ ] No-instance events, different instances, and existing named channels do not interfere. Out-of-scope registration and sending fail without side effects.
- [ ] If A blocks, B continues. Concurrent admission order for same-instance emits is deterministic. `serial` short-circuiting and `parallel` error aggregation remain consistent.
- [ ] During races with admission shutdown, a request is either included in the in-flight set and awaited or receives `Closed`; none are missed.
- [ ] Dropping a serial future, dropping a parallel future, or callback panic/failure leaks neither counts nor background callbacks.
- [ ] Before drain completes, listener captures and emit tails have clear owners; after completion they can be released.
- [ ] Existing `event_keys`, `dispatch_chain_probe`, parity, and related flows still pass.

This issue establishes the event-side shutdown protocol. Full integration with public `FiberView::shutdown` is accepted in the third issue.
