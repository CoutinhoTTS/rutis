# #62 / #63 Research: Typed Event Keys, Pattern Subscriptions, and Synchronous Dispatch

Date: 2026-09-27. This document preserves the research conclusions made before implementation began; implementation was carried out the same day at the user's direction. See the [0.5 migration guide](migration-0.3-to-0.5.en.md) for the actual API and semantics. Pattern callbacks receive `EventKey<E>` by value, pattern-hit counting is enabled by default, and exact subscriptions are not counted.

Baseline: rutis main at `603f8220b049c6ad26e82f258fd72f214b8575fe`, identical locally and remotely; local Cordis snapshot `f8ea3cd50f1a5724e8e715995bcde131c9c12b2c`. The latest text of both issues had been read; neither had comments at the time of research.

## Conclusion

Both proposals have practical value and should proceed. #62 should be split into two implementation stages: “unified keys” and “pattern subscriptions.” #63 depends on unified keys, but need not wait for pattern subscriptions to be complete. There is no need to rewrite the business semantics of the four asynchronous dispatch modes.

The issue text is not yet a sufficient implementation contract. #62 does not specify how the matched key is passed, the complete ordering and once semantics, a unified emit error signature, or an actionable migration path. #63 does not specify the listener registration surface for bail, the borrowing constraints of the synchronous terminal, re-entry boundaries, or the complete admission rules for in-flight work. The claim that #63 can reuse #29 also needs correction: a fixed-input waterfall is not equivalent to the current interceptor chain, which passes replacement values along.

| Topic | Recommendation |
| --- | --- |
| Event identity | Preserve `(TypeId, optional name, optional InstanceId)`; same names on different types remain isolated |
| Default type key | Use explicit `EventKey::<E>::of()` and preserve the original unnamed identity; do not replace it with `E::NAME` or a type-name string |
| Patterns | The first version matches string prefixes within an event type, and only matches explicitly named keys without an instance ID |
| Pattern callback | It must receive the actual matched typed key |
| Listener order | Prefer one registration/prepend order shared by exact and pattern listeners; if adopting the issue's exact-first proposal, specify that prepend applies only within each group |
| Synchronous type constraint | Define `SyncEvent` in the first version as a capability declaration for synchronous dispatch; synchronous entry points must require ordinary function listeners |
| Synchronous terminal | Accept a `FnOnce` on the current call stack; do not require `Send + 'static` |
| Lifetime | Reuse admission, effect, and fiber in-flight counters; take the snapshot and increment counts in the same admission critical section |
| #29 | Preserve current service-interceptor semantics; internal lifetime tools may be shared, but do not migrate this work to a fixed-input waterfall |
| Performance | Remove unmeasured ns/μs figures; establish representative benchmarks before deciding on indexes or caches |

## Existing Implementation and Comparison with Cordis

### The kernel already has a unified storage key

[`key.rs`](../crates/rutis/src/key.rs) defines `TypeKey`, which already contains a type, qualifier, and instance ID; static and dynamic names compare by string contents. Services and events share this internal key, but public event entry points do not accept arbitrary `TypeKey` values.

[`bus.rs`](../crates/rutis/src/bus.rs) currently has 21 business registration/dispatch entry points, plus `observe_dispatch`. The main issue is duplicated public signatures. Internal methods such as `add_hook` and `emit_keyed_inner` already share implementation; there are not three completely independent buses.

Support is still incomplete: there is no public instance-waterfall entry point, nor a complete registration surface for instance once/prepend. Acceptance for #62 therefore includes both API consolidation and filling in behavior. The latter must be validated separately for lifecycle correctness.

Instance dispatch cannot simply forward to the existing non-instance implementation. The current `parallel_instance` uses an independent runner to ensure that an admitted dispatch still completes and holds its flight after its future is dropped; `serial_instance` releases its flight when the borrowing future is dropped. Unifying keys must preserve this distinction.

### The correction to the original design assumptions is mostly sound

Cordis binds string names to key parameter/return types through `Events`, so the claim that “including the name in identity necessarily loses type safety” is incorrect. rutis can also bind a key to its payload type with `EventKey<E>`. [Cordis events.ts](https://github.com/cordiverse/cordis/blob/f8ea3cd50f1a5724e8e715995bcde131c9c12b2c/packages/core/src/events.ts#L17-L32)

However, `EventKey::<E>::named("misspelled-name")` still compiles when freely constructed. This is one level weaker than Cordis's limited static set of names from `keyof Events`. It guarantees that “the key's E matches the payload's E”; it does not automatically guarantee that “the name is correct” or that “each name has only one type.” Static business events should export key constants/constructors from the interface crate.

Cordis `bail` does not await at runtime: a listener returning a Promise hands that Promise back immediately; dispatch does not wait for its resolution before deciding whether to continue. The type of `internal/update` is also `Awaitable<void>`, and its terminal can return an asynchronous restart task. It has a synchronous decision phase, but does not prove that the entire update flow is necessarily synchronous. Rust should define its ordinary-function listener contract explicitly rather than mechanically translating `ReturnType`. [Cordis events.ts](https://github.com/cordiverse/cordis/blob/f8ea3cd50f1a5724e8e715995bcde131c9c12b2c/packages/core/src/events.ts#L104-L134), [fiber.ts](https://github.com/cordiverse/cordis/blob/f8ea3cd50f1a5724e8e715995bcde131c9c12b2c/packages/core/src/fiber.ts#L478-L496)

Synchronous decision points do exist. rutis's current delivery observation and service interception already demonstrate that ordinary function callbacks can participate in effect tracking and shutdown waits; making everything asynchronous is not necessary for every use case. [Existing observation and interception design](design-cordis-observation.en.md)

## #62: Contract to Freeze

### Key shape and identity

The following is an interface sketch from before implementation. It has since been implemented along these lines; see the [migration guide](migration-0.3-to-0.5.en.md) for current usage. Public entry points take `&EventKey<E>`:

```rust
const DEFAULT: EventKey<RoomEvent> = EventKey::of();
const ROOM: EventKey<RoomEvent> = EventKey::named("room/main");

let room = EventKey::<RoomEvent>::dynamic(format!("room/{id}"));
let scoped = room.clone().instance(ctx.instance());

bus.on(ctx, &room, listener)?;
bus.emit(ctx, &room, Arc::new(event))?;
bus.serial(ctx, &room, &event).await?;
bus.waterfall(ctx, &scoped, &event, terminal).await?;
```

Key fields are private. When entering internal storage, `TypeId` is generated from E and erased into `TypeKey`. If an API is provided to recover a typed key from `TypeKey`, it must check the type; no unchecked conversion should be exposed.

Preserve `name = None` for the default key. Replacing the original default key with diagnostic-only `Event::NAME` would merge explicitly named channels with the original type channel; using `type_name::<E>()` as identity is also unnecessary. Names compare by their original string contents, are case-sensitive, and are not path-normalized. Static and dynamic keys with the same name are equal; the instance ID remains part of identity.

The repository declares MSRV 1.85, while const stabilization of `TypeId::of` is 1.91. To support `const EventKey::named`, the public key can store only the name representation and a type marker, then generate the internal `TypeId` when used; it cannot put the current `TypeKey` directly inside a const constructor. The type marker should keep E invariant to avoid extra constraints from type erasure and generic variance. [Rust TypeId documentation](https://doc.rust-lang.org/std/any/struct.TypeId.html)

### Pattern listeners must receive the matched key

The current `Listener<E>` receives only `Ctx` and `E`. If multiple rooms share `RoomEvent` and the payload does not repeat the room name, a `room/` subscription cannot tell which room produced the event. The fact that the current `HostEvent` stores a name in its payload is a specific bridge choice, not a requirement to impose implicitly on all events.

Add a typed `PatternListener<E>` and a corresponding waterfall adapter whose callback also receives `&EventKey<E>`. Exact listeners can retain their current payload shape. Registration adapters ultimately enter the same dispatch snapshot. If all listeners are to receive delivery metadata, make that change once as part of the breaking unified-key migration.

Prefixes match only keys of the same type that are explicitly named and have no instance ID. An empty prefix may mean all named keys of that type; it does not match `of()` or instance keys. `room/` is an ordinary string prefix, not a glob/regex or instance-subtree selector.

Patterns do not change the isolate behavior of ordinary events. Instance keys do not enter the global pattern table. “Subscribe to all rooms” applies only when those rooms already use non-instance named keys; it must not be described as covering all instance events as well.

### Ordering, deduplication, and once

Exact and pattern listeners should share one order: ordinary registration appends, and prepend inserts before all matched listeners. This lets prefix middleware clearly wrap an exact waterfall, while prepend retains an intuitive meaning. A registration sequence and prepend flag can merge two ordered snapshots without changing the relative order of exact listeners when no patterns exist.

The issue's “exact first, then patterns” proposal is also implementable, but it means prepended pattern listeners still come after all exact listeners. If that is chosen, document and test this limit; do not also promise cross-group prepend. The global order above applies only to the matching order within one dispatch; it does not introduce a global execution tail across keys.

Deduplication must use registration identity, never closure addresses or `Arc` callback pointers. One `EventPattern::any_prefix([...])` should represent one registration with one HookId; if a dispatch matches several prefixes, it is still selected once. Calling `on_pattern` twice with the same closure creates two independent registrations, each called once.

A pattern once registration is claimed at most once by an entire dispatch snapshot, not once per matching name. If two names are dispatched concurrently, claiming and removal of all matching indexes must happen under the same bus lock.

Existing `claim_once` removes every once entry when selecting the snapshot. A later serial short-circuit or waterfall veto may mean some entries are never actually called. It guarantees at-most-once, not that each entry actually runs once. The first version should preserve this behavior and add a test for short-circuiting by an earlier listener. Claiming at actual invocation time requires separate design; it should not be an implicit behavior change of pattern subscriptions.

### Tail chain and snapshot

After matching patterns, emit still enters `dispatch_tail` using the full dispatched key. A tail must be created even when there are pattern listeners but no exact listeners. Different names under the same prefix may invoke the same pattern listener concurrently; observers are not promised a total order across all names.

Selection, deduplication, and once claiming for exact and pattern entries must form one snapshot. Release the bus lock before calling any listener; the instance path must also retain atomic admission and flight tracking. Registrations/unloads after the snapshot follow the selected snapshot contract; do not rebuild the chain before each callback.

The first version can scan prefixes bucketed by type. Its cost depends on the number of prefixes for that type and total comparison length, not a fixed number of tens of nanoseconds. First measure hits and misses with 0 / 1 / 8 / 64 / 1024 patterns; add a trie or cache only if measurements justify it. When there are many dynamic names, release corresponding entries after tail completion and unload; do not trade a cache of “all historical names” for superficial lookup speed.

Diagnostics should show exact key/pattern, owner, registration identity, pattern count, and selected listeners. Match count, selection count, and actual invocation count are different metrics, especially when serial/waterfall dispatch short-circuits. Pre-dispatch observers know only `DispatchAttempt` and cannot report actual delivery based on it. The bus also cannot enumerate every dynamic name that has not appeared yet.

### Error signature and version migration

The unified emit should return `Result<(), CordisError>`: synchronous rejection due to a closed instance or scope boundary must not be lost; errors from listeners after admission still go to ErrorSink. This means the dispatch was admitted; it does not mean listeners have completed and is not equivalent to the #43 backpressure interface.

Rust does not overload methods by argument count. The old `on(ctx, listener)` and new `on(ctx, key, listener)` cannot coexist under the same inherent method name. Therefore “keep every old API during a deprecation period” is not directly possible.

The recommendation is to unify the default entry points and migrate workspace callers in the next minor release. Non-conflicting names such as `*_keyed` / `*_instance` can remain as deprecated wrappers for one release. If a non-breaking preparation stage is required, first add explicitly keyed methods under different names, then change the final method names in a minor release.

Migration must cover rutis-agent, the HostEvent bridge, documentation, and examples. Preserve the historical context behavior of ordinary asynchronous non-instance dispatch, or explicitly announce a change; do not change it as a side effect of API consolidation. Changes to rutis/interfaces that affect the validated dylib SDK require a new SDK identity and rebuilding the corresponding artifacts.

## #63: Contract to Freeze

### Synchrony and registration surface

`SyncEvent: Event` is only an additional capability constraint. It does not prevent the same E from being registered with the existing async `on`, nor does it make `bus.waterfall` synchronous automatically. This research recommends accepting that capability model in the first version: synchronous entry points require E to implement SyncEvent and accept only synchronous listeners. Do not claim that the event type exclusively determines every dispatch mode.

If the business goal is strict exclusivity, use an associated dispatch kind or separate Sync/Async event traits, and migrate existing Event implementations along with it. That guarantee cannot be implied by convention.

Two distinct synchronous callback shapes are needed:

```rust
trait SyncListener<E: SyncEvent>: Send + Sync + 'static {
    fn call(&self, ctx: &Ctx, event: &E)
        -> Result<Option<E::Value>, CordisError>;
}

trait SyncWaterfallListener<E: SyncEvent>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, event: &'a E, next: SyncNext<'a, E>)
        -> Result<E::Value, CordisError>;
}
```

The corresponding public registration methods are `on_sync` and `on_waterfall_sync`; dispatch methods are `bail_sync` and `waterfall_sync`, all taking `EventKey<E>`. Registering only a synchronous waterfall must not make bail automatically find an `Option<Value>` listener. Once/prepend options and snapshot rules should be explicitly shared; do not multiply equivalent methods by name and instance.

Synchronous callbacks do not box futures and are not spawned by the bus. Two threads may execute callbacks for the same key concurrently; no per-key synchronous execution lock is added. Re-entry restrictions apply to the call stack, not cross-thread serialization. Cross-process protocol plugins do not automatically become synchronous listeners; they need an async interface or explicit business-level changes.

### Continuation and terminal borrowing

`SyncNext::call(self)` consumes the continuation and does not implement Clone/Copy; the next layer still receives a borrow of the same E. This lets the compiler reject both calling it twice and saving it as `'static`.

The synchronous terminal should accept `FnOnce(&Ctx, &E) -> Result<E::Value, CordisError>`, allowing it to borrow from the current call stack without adding the `Send + 'static` constraints of the existing async `Terminal`. The implementation can borrow a local terminal adapter; it does not need to box the closure.

The probe has verified that the terminal can capture a currently held `MutexGuard` and a stack-local mutable counter, and that a listener can wrap or veto. This verifies only the Rust interface shape; it does not mean the production bus supports it.

### Admission, shutdown, and re-entry

Recommended execution order:

1. Validate the context and key; the new synchronous entry point rejects closed, inactive, and stale-generation contexts, with explicit error precedence.
2. Establish a thread-local RAII re-entry guard for `(current bus identity, full event key)`.
3. Run the existing dispatch-attempt observer; then validate again under admission and obtain the business snapshot. Registrations made by an observer can affect the snapshot; shutdown from an observer can cause this dispatch to be rejected.
4. In the same admission critical section, establish flights for the emitter, instance owner, and selected listener owners, then release all framework locks.
5. Call the synchronous chain or terminal; on return, error, or unwind, release flights and the re-entry guard through RAII.

The re-entry guard must cover observers and the terminal, not just business listeners; otherwise an observer can trigger unbounded recursion for the same key before the guard exists. Identity includes the bus so same-type, same-name events in independent roots are not incorrectly blocked. On the same bus and full key, re-entry between bail and waterfall must also be rejected. Different keys or instances may nest; ordinary concurrency on different threads is not rejected by TLS.

When there are no listeners, the waterfall terminal is still user code. If shutdown is expected to wait for this synchronous dispatch, terminal execution must count toward emitter/instance-owner flights instead of bypassing lifecycle tracking. Admission must also exclude listeners whose unload has begun; it is not enough to check whether the owner's Weak reference can be upgraded.

Revocation first removes the registration at the same admission boundary to prevent new snapshots from selecting it, then asynchronously waits for admitted calls to exit. Existing owner-level `wait_events` can be reused, but it may wait for other in-flight calls by that owner too; do not claim that it drains one listener independently. A synchronous callback may initiate its own shutdown and then return; it cannot block waiting for unload that includes its own flight.

Calling outside framework locks is necessary, but does not guarantee that arbitrary business Mutex usage cannot deadlock: a callback can still block if it reacquires the same non-reentrant Mutex already held by its caller. Same-key re-entry protection also cannot resolve business lock cycles between callbacks for different keys. Examples involving locks should exchange required data only through the payload and terminal.

### Errors, panics, and commit point

Return a listener's `CordisError` unchanged. At the synchronous user-call boundary, catch panics, convert them into an explicit error, and report them to ErrorSink; a panic from the sink must not replace the returned error. Define the same behavior for terminal panics and avoid reporting a single panic multiple times as it crosses continuation adapters.

Observers retain their existing contract: report a panic and continue business dispatch. Add two synchronous variants to `DispatchMode`; treat the public enum change as part of version migration. Re-entry protection for the same key must still apply inside the sink.

For a use case such as “compute a final candidate value, then commit,” the terminal should return the candidate, and the actual write should happen only after the entire waterfall returns and validation succeeds. If the terminal commits internally, the bus does not automatically undo that commit when an outer listener later changes the returned value or fails.

The flight protects the current dispatch call stack; business commits after return are not automatically covered. Service writes should retain their existing owner, generation, and binding-identity checks at commit time.

### #29 cannot be consolidated directly

The current [`intercept.rs`](../crates/rutis/src/intercept.rs) `run` passes each replacement to the next interceptor in registration order, performing successive transformations:

```text
original value 1 → first item +10 → second item ×2 → final value 22
```

In a fixed-input waterfall, when two listeners wrap the result of `next()`, values flow back in the opposite direction:

```text
terminal returns 1 → inner ×2 → outer +10 → final value 12
```

The difference is more than order: a downstream interceptor currently sees the replacement from its predecessor, while a downstream listener in a fixed-input waterfall still receives the original E. Changing registration order cannot generally preserve payload visibility, rejection point, and side effects.

#29 also selects hooks by full service key, effective isolate scope, and caller ancestor chain, and rechecks binding identity before commit. Ordinary event listeners do not have all of these selection conditions. General-purpose synchronous dispatch cannot replace these service contracts.

If unification is truly needed in the future, retain an explicit “pass the new value downstream” transform/fold primitive, or share only flight, re-entry, and registration-cleanup tools. A transform helper that merely wraps a fixed-input waterfall does not solve the differences above.

### Limits on performance claims

The first benchmarks after implementation have been completed, covering listener counts, prefix counts, empty instance chains, and old/new exact dispatch. The following preserves the original measurement plan; performance measurements for short-circuit, veto, observer count, and registration/unload contention remain outstanding. See the [performance sample](performance-event-dispatch-2026-09-27.en.md) for actual results.

The claim “without listeners there is only one lookup” conflicts with the full admission/in-flight-wait guarantees: context checks, flight accounting, observers, and re-entry protection may also run. “No future/spawn” is a structural fact; no allocation and lock costs of only a few nanoseconds must be measured.

First measure no listeners; 1/N listeners; bail proceed/short-circuit; waterfall wrap/veto; 0/N observers; instance/non-instance; and registration/unload contention. If the current `ErasedValue` adapter remains, return values are also boxed. To remove this allocation, evaluate typed buckets per E after measurement. The allocation-free chain in a compile probe is not evidence of final bus performance.

## Implementation Stages and Acceptance

First freeze the key identity, pattern ordering, once behavior, error signature, and SyncEvent capability model, then implement in the following scopes:

| Stage | Scope | Main acceptance criteria |
| --- | --- | --- |
| #62-A | Unify EventKey and exact entry points; add instance waterfall/once/prepend; migrate workspace | Identity equality, compile-time rejection of payload mismatch, behavior mapping for 21 old entry points, instance shutdown and cancellation semantics |
| #62-B | Prefix patterns, matched key, merged order, registration-group deduplication, pattern once, diagnostics | Preserve order with patterns only; concurrency across names; overlapping prefixes selected once; two-thread once claim; veto/short-circuit/prepend |
| #63 | Synchronous callbacks and continuation, re-entry, panic, admission/flight for all key forms | Mutex-held example, terminal borrowing, compile failure for double next/escape, shutdown/early unload/reload races |

#63 can proceed after #62-A; #62-B need not be complete. However, public selection and lifecycle tools should be defined once to avoid creating separate keyed/instance synchronous APIs. The complete #43 backpressure project is not a prerequisite; at minimum, record representative benchmarks in this batch and make no unmeasured claims.

New validation should specifically cover: same-key re-entry from an observer; same-key re-entry from the terminal; legal same-key nesting across different buses; mutual re-entry between bail/waterfall; no-listener terminal vs shutdown race; flights returning to zero after panic; rejection of stale-generation contexts; instance sibling isolation; and 1,000 rounds of registration, early unload, and subtree shutdown with no residue. Control important races with Barrier/Notify/oneshot, not sleep.

After migration, rerun existing Cordis parity, event-key, tail-chain, instance-subtree, and observation regressions; check bridge event and agent order. Keep the existing #29 service-interception regression unchanged.

## Verification Performed and Limitations

Existing-code test run:

```sh
cargo test -p rutis --offline --test event_keys --test dispatch_chain_probe --test instance_subtrees --test dispatch_observation --test service_intercepts
```

All 56 tests passed: event keys 11, tail chain 1, instance subtrees 23, dispatch observation 8, and service interception 13. This verifies the current behavior baseline; it does not mean #62/#63 have been implemented. The full workspace suite was not run, and performance was not measured.

The independent probe is at [probes/event-keys-sync-dispatch.rs](probes/event-keys-sync-dispatch.rs); the compiler used was rustc 1.98.1:

```sh
rustc --edition=2021 docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-probe
/tmp/rutis-events-probe

# Each of the following three commands is expected to fail compilation.
rustc --edition=2021 --cfg mismatched_payload docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-mismatch
rustc --edition=2021 --cfg double_next docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-double
rustc --edition=2021 --cfg escape_next docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-escape
```

The normal probe passed: it checked the identity distinction between default and explicitly named keys, equality of static/dynamic names, isolation of different types with the same name, borrowing a MutexGuard and a local variable in the terminal, single terminal invocation and veto, and the 22/12 value-flow difference. The negative cases respectively produced payload type error E0308, use-after-move E0382 for a reused continuation, and a lifetime error for an escaped borrow.

The same probe confirmed two limitations: a freely constructed misspelled name for the same type compiles, and an event implementing SyncEvent can still be passed to an async API that only requires Event. It did not have a real EventBus, InstanceId, admission, panic isolation, or concurrent claiming, so it does not prove that these mechanisms have shipped and is not a performance benchmark. MSRV 1.85 was not installed locally, so that version was not tested.

This research added only local documentation and an independent probe; it did not change runtime code or remote issues.
