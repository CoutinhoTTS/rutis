# Requirements: Mounting Cordis Plugins in rutis

## 1. Goal

**Center the design on rutis: rutis applications must be able to mount and use existing Cordis (Node) plugins across processes. All compatibility work belongs in the compatibility layer; the rutis core must not change for compatibility.**

- Cordis plugins continue to run in real Cordis without source changes. The rutis side uses automatically generated Rust types and treats them according to native rutis rules.
- One mount may contain a group of interdependent Cordis plugins (published plugins are often designed to be composed); dependencies within the group use native Cordis resolution.
- **Dependencies work in both directions:** rutis plugins can depend on services provided by mounted plugins, and mounted Cordis plugins can use services provided by the rutis application through native `inject` / `ctx.xxx` patterns, such as host-provided LLM or storage capabilities. Each framework keeps its own dependency gating and cleanup order.
- The compatibility layer is an external library (`crates/rutis-interop`, `interop/node`), not another host or plugin framework.
- rutis is the host. The full dsh web interface also runs through this compatibility layer (`crates/rutis-dsh`), with model calls provided by rutis application services. The old design where dsh was the host and used Rust through `rutis-cordis` + `host/` has been removed ([#83](https://github.com/arcships/rutis/issues/83)). The implementation in `rutis-interop` for Cordis applications mounting rutis plugins is frozen and will gain no further capabilities. This does not freeze the primary direction in this requirement: rutis applications providing services to mounted Cordis plugins.

## 2. Usage model

```text
rutis application
    |
ctx.plugin(generated mount plugin)       <- generated from the original TS plugin at build time
    |
compatibility layer (Rust) <==== Unix socket ====> compatibility layer (Node)
                                                      |
                                               real Cordis + original TS plugin
```

| Participant | Responsible for | Not responsible for |
| --- | --- | --- |
| rutis application developer | Choose and configure plugins, mount generated plugins, read services and subscribe to events using rutis | Write the communication protocol, hand-code bridge logic, or maintain generated files |
| Cordis plugin author | Write plugins using native Cordis conventions | Rewrite business logic for cross-process use; only the boundaries in §5 need to be followed |

## 3. Constraints

| Area | Requirement |
| --- | --- |
| rutis core | No compatibility-specific changes; use only existing public rutis APIs |
| Cordis | Use public APIs (including `internal/*` hooks); do not modify or fork source, monkey-patch it, or propose upstream changes |
| Plugin source | Do not rewrite original TS plugin or rutis consumer business source for cross-process use |
| Type bindings | Generate automatically during ordinary Cargo builds; no hand-written contract or maintained IDL |
| Method shapes | Synchronous methods stay synchronous; async methods still return Futures. This is part of the interface contract. |
| Implementation tradeoffs | Record capabilities that public APIs cannot support as “not possible,” with scenario and reason. Do not expand scope just to achieve superficial parity. |

## 4. Framework contracts and language-stack differences

The compatibility layer preserves **framework contracts** strictly. Differences caused by the JS / Rust language stacks may be accepted and expressed as boundary rules in §5.

| Category | Behavior | Handling |
| --- | --- | --- |
| Framework contract (required) | Sync / async service method shapes, arguments and return values, business errors (including `AggregateError.errors` / `cause`) | Preserve |
| Framework contract (required) | Plugin assembly, dependency readiness, unload cleanup; services remain callable during unload | Preserve |
| Framework contract (required) | Object identity: an acquired service object, callback, or async result is not replaced by a different object or snapshot | Preserve |
| Framework contract (required) | Meaning of event return values (for example, rutis `Some(false)` and Cordis `false` differ) | Convert explicitly; report incompatibility if conversion is impossible |
| Language-stack difference (accepted) | Synchronous listeners have run when Cordis `emit` returns | Across the boundary, consistently use rutis's “emit and forget” behavior |
| Language-stack difference (accepted) | The synchronous prefix of a JS async function runs immediately when called | Not guaranteed across the boundary |
| Language-stack difference (accepted) | Listeners on both Cordis sides interleave one by one | Forward each side's listeners as a group |
| Language-stack difference (accepted) | `bail` treats the returned Promise itself as a short-circuit value | Await the result across the boundary before deciding |
| Language-stack difference (accepted) | A waterfall `next` callback can be called multiple times | Allow only one call across the boundary; report an error if called more than once |
| Language-stack difference (accepted) | Immediate visibility between service replacement and reads | After replacement, the compatibility layer swaps the proxy on the rutis side; a stale object may be read briefly |

## 5. Boundaries for Cordis plugins

Cordis plugins connected to rutis must follow these rules. Detectable violations must produce explicit errors at mount time or runtime, never silent downgrades.

1. **Services:** Publish and replace services through `ctx.provide` or property assignment. Direct replacement with `ctx.set` emits no notification; the rutis side switches to the new object only after the next call to that service.
2. **Cross-boundary events:** `emit` is a notification and does not guarantee that rutis has run its listeners when `emit` returns. Use `parallel`, `serial`, or `bail` when waiting or a result is required.
3. **Event ordering:** Native ordering is guaranteed only on each side. Listeners on the other side run as a group in a fixed position. Do not depend on interleaving across sides.
4. **Middleware chains:** Waterfall / bail chains do not interleave across sides; the other side's listeners participate as one stage.
5. **Framework-internal events:** `internal/*` events do not cross the boundary.
6. **Synchronous waits:** A synchronous method cannot wait for a result that requires the Node event loop to advance (such as a timer or Promise). This returns `SyncWaitCycle`.
7. **Host-provided services:** Services provided by rutis appear on the Cordis side as interface-generated proxy objects and work through the declared interface, but are not instances of the plugin's declared class. `instanceof` therefore fails.
8. **Event loop:** Plugins must not block or hang the event loop for long periods. The compatibility layer adds no call timeout. For timeouts, the rutis side should use an async method and set its own timeout (which cancels the call); the application decides how long unload may wait. An uncaught exception or rejection ends the entire Node process, revoking all services from that mount.

## 6. Acceptance

- Drive the work with real plugins: establish a baseline using published official dsh npm plugins, grade coverage L0–L4 (same definitions as §9 of the dsh bridge design), and fill actual gaps incrementally. Each connected plugin counts as a delivery.
- Every capability has an automated cross-process test compared with native Cordis behavior. Accepted differences from §4 are checked against the §5 boundary rules.
- A mounted plugin that depends on services from the rutis application obtains them through its original dependency and service-read patterns. It waits while a service is not ready and stops according to native rules when the service is revoked.
- Remote exits, communication interruptions, and invalid references fail explicitly; they do not report false success or automatically retry side effects.

## 7. Initial decisions: stopping, process sharing, and permissions

Decision record: [#71](https://github.com/arcships/rutis/issues/71). These are product boundaries for the first release and match current implementation.

| Area | Initial decision | Behavior visible to the application |
| --- | --- | --- |
| Force-stop a hung plugin | No separate force-stop operation | Normal unload waits for plugin cleanup; the compatibility layer adds no timeout (rule 8 in §5). When the mount is fully released, the Node process ends with SIGKILL. Only the Node process is stopped; child processes started by plugins are not reaped by the compatibility layer. Applications needing a deadline add an unload timeout and release the mount afterward. |
| Plugins sharing a process | Plugins in one mount (including a combined mount) share one Node process; each separate mount has its own process | If the process crashes or exits, every service from that mount is revoked and rutis consumers stop by native rules. Other mounts and unrelated features are unaffected. Put plugins that need isolation in separate mounts. |
| Difference between unload, termination, and timeout | Report them separately | Successful normal unload reports success. Abnormal process exit makes unload and calls return `Error::Transport` with the cause (for example, signal termination). The application cancels a timed-out call by dropping its future; Cordis receives `AbortSignal`. If the outcome is unknown, do not report that the operation did not execute. |
| System permissions | Running out of process is not a sandbox | The Node process runs as the rutis application user with the same permissions and environment variables, and can access the same files and network. Mount only trusted plugins. |

## 8. Out of scope

- Changes to the rutis core or Cordis.
- Mechanisms built solely for perfect native parity: shared-memory version pages, a unified cross-framework event queue, or distributed garbage collection.
- Deployment platforms, plugin marketplaces, automatic restart, reconnect recovery, or operating-system sandboxes.
- Other languages. After research on 2026-10-03, the decision was **not to add them inside rutis**: rutis supports mounting with same-paradigm Cordis (rutis is the host; the reverse direction remains frozen). Languages without the Cordis paradigm, such as PowerShell, Bash, and AppleScript, and min_cordis (Python), should be ordinary plugins outside rutis if there is a real need, using only the public rutis / rutis-interop APIs. See the [decision record](decision-multilang-2026-10-03.en.md).

Historical requirements and proposals: [#66](https://github.com/arcships/rutis/issues/66)–[#70](https://github.com/arcships/rutis/issues/70), the [old design](design-protocol-plugins-2026-09-25.en.md), and [#74](https://github.com/arcships/rutis/pull/74) (core event queue, no longer pursued). They are not normative for this requirement.
