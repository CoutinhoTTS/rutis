# Releasing Transient Child Plugin State (0.2.1)

Date: 2026-09-22. Status: implemented. Type: bug fix, patch release.

## Problem

In rutis 0.2.0, several structures were cleaned up only when the fiber itself unloaded. Under a long-lived root, repeatedly registering and destroying child plugins (the normal D32 factory-loading and keyed multi-instance use cases) caused these records to grow without bound:

| # | Retained state | Location | Growth unit |
| --- | --- | --- | --- |
| 1 | Mount record (child cascade disposal registered as a parent effect) | `FiberInner::effects` | One per plugin, holding a child `FiberView` (`Arc<FiberInner>` shell) |
| 2 | Dependency declaration entry | `Registry::inject_index` | One per plugin per key; keyed declarations use a unique qualifier per instance, so entries are not reused |
| 3 | Empty event-channel entry | `EventBus::hooks/wf_hooks` | Empty list retained after the final listener on a keyed channel is removed |
| 4 | Dispatch-tail handle | `EventBus::dispatch_tail` | Retained task handle for the last emit on a key |
| 5 | Root-level provide eviction cleanup record | `FiberInner::effects` | One per provide; a completed `Disposer::dispose` record remained as `Done` |
| 6 | Root-level provide accounting | `FiberInner::provided` | One per provide; not removed when its `Disposer` was released |

Bindings themselves were already removed by `finalize_binding_if` and are not listed here.

## Fix

Two core mechanisms:

1. **EffectRecord self-removal and error storage** (#1, #5): records hold a weak reference to the host fiber. After draining to `Done`, if the record is still in the host's effects list (the collector is an earlier drain caller, not fiber-level unload), store its error in the host's `drained_errors` and remove it from the list. Records already taken by `drain_effects` do not store errors; the drain's join collects them directly, avoiding duplicates. Stored errors are aggregated on fiber unload/restart. A single error preserves `Arc` identity, matching the old behavior where the record stayed in the list and was joined again (compared with `reentrant.spec.ts:437`).
2. **Release terminal children** (#1, #2): before a non-root fiber driver exits in the `Dispose` terminal state, `release_transient` unregisters `inject_index` entries using the declaration snapshot captured at registration and drains its own mount record. Terminal cleanup is idempotent; `dispose()` immediately returns the cached terminal result. Once the record removes itself, the parent no longer holds the child reference.

Related fixes:

- **#3:** Remove empty channel entries when listeners are removed (also when `take_hooks` takes every `once` listener). A later registration recreates the entry through `or_default`, with unchanged behavior.
- **#4:** Store `(generation, task)` in `dispatch_tail`. When the dispatch completes, remove it only if the generation still matches. If a listener re-enters `emit` on the same key and inserts a new generation, the old task cannot remove it; ordering remains intact.
- **#6:** At the end of `evict_and_finalize`, remove the provider's `provided` accounting for this key and scope only. A newer provide for the same key is preserved.

Semantics retained:

- The mount cleanup closure sends a child error to `ErrorSink` only if `dispose()` has not already delivered it. A child already in `Disposed` is not reported twice because the caller received the same error. Cascading parent unload behavior for Active/Loading/Pending/Failed children is unchanged.
- Dropping a `Disposer` still does not run cleanup. Public signatures for `Ctx::effect()`, `FiberView`, and `EventBus` remain unchanged (semver patch).
- The full parity suite (including shared-promise `Arc` identity, restart-error routing to the sink, and preserving Aggregate structure) passes without changing assertions.

## Validation

- Unit tests (`src/*/transient_tests.rs`): after 25 churn cycles, mount records, `inject_index`, `provided`, and effects return to baseline; keyed channels and dispatch tails can be recreated after removal.
- Black-box test (`tests/transient_release.rs`): churn 50 plugins under one root (keyed service and keyed listener), drop all instances (Drop counter), dispatch again on a removed channel without panic, and reuse the root.
- Existing contract/parity/config_update/event_keys/dispatch_chain_probe flows pass; clippy with `-D warnings` and fmt are clean.

## Known limits

- Non-root terminal release trails the `TaskDone` completion point of disposal. It is bounded but not immediate; use bounded waiting for deterministic assertions.
- The root fiber itself does not exit. In a single-root process the root driver remains alive for the process lifetime (one bounded task per root); this fix does not address that.

## 0.2.2: More readable TypeKey diagnostics

Previously `TypeKey::describe` printed the opaque hexadecimal `TypeId` debug value, making errors and assembly-graph labels unreadable for keyed instances. Since 0.2.2, keys capture `type_name::<T>()` at construction and `describe` / `Debug` print `TypeName#qualifier`. Equality and hashing still use only `TypeId` plus qualifier (the type name is a pure function of `TypeId` and is excluded from matching). Public signatures are unchanged.

## 0.2.3: Preserve rollback errors from failed loads

Previously `fail_load` sent rollback cleanup errors only to `ErrorSink`, leaving the terminal `Failed` error with just the load error. Joiners (`settle` / `dispose` / parent assembly rollback) could not see the cleanup failure. Since 0.2.3, the terminal error aggregates the original load error first, then rollback errors (aggregate only when rollback failed; a single error preserves its original `Arc`). `ErrorSink` remains an additional observer. Three reentrant parity cases (`reentrant.spec.ts:29/519/158`) updated their assertions: the single sink event is unchanged, while the terminal error now includes both execution and cleanup failures.

## 0.2.4: Shrink empty tables

Versions 0.2.1–0.2.3 removed entries, but long-lived roots retained capacity in `inject_index`, `bindings`, bus channel tables, and fiber `provided` / `effects` containers. After creating and destroying N instances sequentially, tables could be logically empty while buckets retained their historical peak capacity (measured at about 7 KB per instance). Since 0.2.4, containers call `shrink_to_fit` when empty; when non-empty, capacity is bounded by the concurrent instance peak. API and behavior are unchanged.

## 0.2.5: Shrink sparse tables instead of only empty ones

Version 0.2.4 shrank only empty tables. Persistent keys (such as Agent-level service bindings and root accounting) kept `bindings` and fiber `provided` non-empty, allowing capacity to retain its historical peak. Version 0.2.5 uses a waste threshold: when capacity exceeds 64 slots and length is below one quarter of capacity, call `shrink_to_fit` (empty tables naturally qualify). Small tables do not oscillate; shrink cost is amortized when crossing the threshold. This covers `inject_index`, `bindings`, three bus channel tables, and fiber `provided` / `effects`.
