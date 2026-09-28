# Cordis: public integration hooks for cross-process native plugins

We are building an external adapter that lets rutis and Cordis applications mount each other's plugins across processes while keeping their native Context, event and lifecycle APIs. [The current PR](https://github.com/arcships/rutis/pull/73) implements a small value-method slice. IPC, serialization, generated bindings, reference accounting and process ownership would stay outside Cordis.

We would like feedback on three integration contracts before expanding the adapter. Our tested baseline is the published @deepseek-ai/cordis 4.0.1 package. We also checked the corresponding service, state and dispatch locations in upstream 4.0.4; this is not a claim of full compatibility between those releases.

## 1. Service resolution and commit boundaries

A remote service getter must preserve the reading Context and distinguish injected property access, strict reflect.get, and non-strict get. Injected property access may retain an implementation during cleanup when a fresh strict get would no longer find it.

Could Cordis expose an adapter resolution entry plus a synchronous, non-reentrant revision sink around the actual publication operations below? The adapter would use it to invalidate a local binding cache; asynchronous internal/service notifications alone leave a stale-read window.

| Location | Publication to cover |
| --- | --- |
| ReflectService.provide | Registry and provider-snapshot insertion; registry removal before dependency cleanup; separate provider-snapshot removal after cleanup |
| ReflectService.set | The implementation's value replacement |
| Fiber._updateState | Final state assignment, especially transitions into or out of ACTIVE |
| Fiber._reload / _unload | Publication and removal of the injected implementation snapshot |
| Context isolation / property definitions | Initial root labels, derived view identity, changes to published inherited mappings and resolver eligibility |

The sink must not invoke plugin code, throw, await or reenter Cordis. It surrounds only publication; ordinary notifications remain outside. In particular, _updateState's callback can start reload/unload and must execute before entering the field-assignment window. Dynamic getters/checks still run normally and are not assumed cacheable. If publicly writable mappings can bypass these boundaries, those paths cannot safely use the cache.

Relevant locations: [reflect.ts](https://github.com/deepseek-ai/deepseek-harness/blob/master/vendor/cordis/src/reflect.ts), [fiber.ts](https://github.com/deepseek-ai/deepseek-harness/blob/master/vendor/cordis/src/fiber.ts), [context.ts](https://github.com/deepseek-ai/deepseek-harness/blob/master/vendor/cordis/src/context.ts). Exact API names and supported mutation boundaries are open for maintainer feedback.

## 2. Event registration and dispatch

For explicitly connected event keys/scopes, the adapter needs routing of individual listener registration/removal and native dispatch entry points, plus an owned listener snapshot that retains filtering, ordering, once eligibility and in-flight lifetime. A direct local backend entry should avoid recursively entering the route.

Could the existing algorithms consume such a snapshot without copying the event implementation into the adapter? This must preserve synchronous emit prefixes, bail returning a Promise object, waterfall's next semantics and native errors. Forwarding an entire remote bus as one listener cannot preserve interleaved local/remote order. No routing is requested for unrelated keys. [events.ts](https://github.com/deepseek-ai/deepseek-harness/blob/master/vendor/cordis/src/events.ts)

## 3. Native mount lifecycle forwarding

The local mount's update/restart must reach the original remote fiber exactly once and associate its completion or failure with the native mount operation. Configuration validation, internal/update interception and child-plugin ownership remain with the original framework. Can a public mount extension support this without a second restart or a new lifecycle state machine?

## Proposed regression cases

- Cached and native reads agree through replacement, ACTIVE → UNLOADING, retained cleanup snapshots, isolate inheritance and dynamic accessors.
- No user callback or await executes inside a commit window; reentrant notifications observe committed state.
- Interleaved local/remote listeners preserve order, filtering, once, Promise bail and next behavior.
- Updates preserve the original validation/interception outcome; child plugins and cleanup retain native ownership.

Would these contracts fit Cordis's extension model, and are there existing supported entry points we have missed? We can develop focused prototypes and native regression cases once the interface direction is agreed. This proposal does not assume that Cordis will accept or schedule an implementation.
