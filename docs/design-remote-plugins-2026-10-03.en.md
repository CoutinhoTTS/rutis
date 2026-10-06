# Remote Plugins: Runtime Integration and Node Interconnection

> Package structure updated (2026-10-06): `rutis-channel`, `rutis-interop`, `rutis-transport-*`, and `rutis-runtime-local` were merged into modules of `rutis-bridge` (`channel`, `session`, `runtime`, `transport::*`, `cordis`); see [Packages, Tools, and Workflow for Developers](design-developer-packages-2026-10-06.en.md). The crate names below are retained from the original design.

Status: remote-capability design; not implemented. Updated 2026-10-05.

Related specifications: [network stack](design-protocol-channel-decoupling-2026-10-03.en.md), [compatibility layer](design-protocol-plugin-mount.en.md), and [loader](design-rutis-loader-2026-10-02.en.md). Multilingual baseline: [overall design #121](https://github.com/arcships/rutis/pull/121), [M1 design #127](https://github.com/arcships/rutis/pull/127), [M1a #129](https://github.com/arcships/rutis/pull/129), [M1b #128](https://github.com/arcships/rutis/pull/128), [M1c #130](https://github.com/arcships/rutis/pull/130), and [M2 #131](https://github.com/arcships/rutis/pull/131). The local runtime implementation in those PRs does not mean network deployment is supported.

## 1. Endpoint model

Execution location and management ownership are independent: the transport determines whether execution is local or remote, while the integration above it determines who manages the plugin.

| Endpoint | Management owner | Execution-side requirement |
| --- | --- | --- |
| Language runtime | The controlling rutis and loader manage rows, dependencies, configuration, and lifecycle | Load, execute, invoke, and clean up; a leaf runtime needs neither rutis nor a local dependency graph |
| Full framework node | Each node manages its own plugins; a peer may install plugins through `host` | Full rutis, Cordis, or a compatible framework, with bridge plugins installed as needed |

```text
Local rutis + loader
  ├─ Runtime integration ─ link / Session ─ transport ─ Python / Node runtime
  └─ Node bridge         ─ link / Session ─ transport ─ rutis / Cordis node
                                                        └─ local language runtime
```

- Node retains full Cordis for npm plugins and their internal dependencies; Python uses the leaf SDK. Node may be a controlled runtime or an independent node; assembly determines the role, not the language.
- This repository maintains rutis, Cordis integration, and the Python leaf runtime; it does not need to implement a full framework for Python.
- The framework core and loader core do not change. JS runs in real Node.js and Cordis; no JS engine is embedded.
- Out of scope: code distribution, plugin marketplaces, deployment platforms, automatic process restart, session recovery, disconnect grace periods, OS sandboxes, and independent forced termination.

## 2. Modules and plugins

| Module | Responsibility | Location |
| --- | --- | --- |
| Channel / connector | One connection, framing, backpressure, transport authentication, liveness, close reason | Contract in `rutis-channel`; implementations in `rutis-transport-*` crates; implementations for each language |
| Session / codec | Handshake, calls, references, cancellation, call paths, errors, encoding | Rust `rutis-interop` and corresponding implementations in each language |
| Transport plugin | Configure and own connectors, listeners, and local child processes | `rutis-transport-local` / `rutis-transport-websocket` / `rutis-transport-memory` / … |
| Identity plugin | Credentials, validation rules, endpoint identity mapping | `rutis-bridge`: identity |
| Link plugin | Connection and session lifecycle, identity checks, reconnect, old-session replacement, session readiness | `rutis-bridge`: link |
| Runtime integration plugin | Runtime contract, service leases and projection | `rutis-interop`: RuntimePlugin; `rutis-bridge`: adapter from Peer to runtime |
| Node feature plugins | Import, export, remote hosting, events | `rutis-bridge`: import / export / host / events |
| Row integration and resolver | Description lookup, dependency resolution, refresh, row readiness, row load/unload | `rutis-loader` |
| Composite plugin | Assemble existing plugins and companion implementations | In `rutis-bridge` if no row integration; in `rutis-loader` if it includes row integration |

- Transport, identity, link, and node bridge names use the `rutis-bridge/` prefix; runtime names follow the multilingual design.
- Channel, Session, and codec are mechanism libraries, not standalone plugins.
- WebSocket is one transport protocol, not a fixed architectural layer. A new transport adds an adapter and transport plugin; it does not change sessions, the runtime contract, or node feature plugins.
- `rutis-interop` and `rutis-bridge` do not depend on `rutis-loader`. Link does not query schemas, refresh rows, or provide RuntimeRows or PeerRows.
- A leaf execution endpoint implements the required transport, session, and runtime handlers; it need not implement a full plugin framework or a node bridge plugin group. The leaf side has no link, so the existing leaf SDK provides the corresponding responsibilities:
  - Existing (`interop/python/rutis_runtime`): local session, `rows.*` / `hosts.*` runtime handlers, and cleanup of every row followed by exit on disconnect. The parent starts the process with `<socket> <project>`; it calls back to the parent over a Unix socket. One process serves one session.
  - New for network access: listener and transport authentication (configure the controller identity and validation rules; accept only authenticated controllers). The process remains running and serves multiple controller sessions in sequence, with only one active session at a time; takeover order is in §4.4. On disconnect it clears leases and continues listening.
  - Over a network transport, a leaf only listens and does not own reconnect backoff; the controller's link reconnects. A local child process continues to call back or use an inherited fd as before.

### Crate ownership and dependencies

Develop, integrate, and test everything in the rutis repository, enabling independent crates as needed. Core `rutis` gains no network, runtime, or reconnect concepts and does not depend on these extensions.

- `rutis-channel`: Channel, connector contract, connection metadata, and structured errors. No dependency on rutis, bridge, or interop, and no concrete transport implementation.
- `rutis-interop`: Session, codec, runtime contract, and RuntimePlugin. Integrates through a generic session entry point; it does not depend on bridge Peer types or a concrete transport.
- `rutis-bridge`: Transport service and registration API, identity, link, node feature plugins, and adapter from Peer to runtime. No dependency on a concrete transport crate.
- `rutis-transport-local`: `LocalPlugin` (Unix-socket dialing; after registering spawn configuration with `Spawn`, `spawn:<name>` starts and connects a process using an inherited fd or callback socket. The channel owns the process, and the channel close reason describes how the process ends; after channel closure, allow two seconds before ending the process). Depends only on rutis, rutis-channel, and rutis-bridge; it does not know what runs in the process. Do not split crates further by internal mechanism.
- `rutis-runtime-local`: Local language runtime composite `LocalRuntime` (transport, compatibility-mode link, runtime integration, and `RuntimePlugin::session`). Interop's `Launcher` (`Launcher::node` / `Launcher::python`) defines how a language starts (program, arguments, and channel handoff); the composite crate converts that to transport `Spawn`. It cannot live in interop (interop → transport → bridge → interop would form a cycle) or in loader. The `Process` compatibility facade keeps its own launch code, which is removed along with the facade.
- `rutis-transport-websocket`: `WebSocketPlugin`, internally implementing dialing, listening, TLS, heartbeat, and close handling.
- `rutis-transport-memory`: `MemoryPlugin`, internally implementing bounded in-memory channels for tests and in-process interconnection.
- `rutis-loader`: Both kinds of row integration, resolvers, and their composites. Application assembly chooses the transport; the loader does not bind to a fixed transport list.

Transport crates depend on `rutis-channel`, `rutis-bridge`, and the core plugin API. Native rutis plugins are the primary delivery mechanism; do not make plugin packaging an optional `plugin` feature. A plugin validates configuration, provides Transport, owns resources, and withdraws its service and cleans up its resources on unload; native gating stops dependent links.

Composites use a shared Transport service to reference a concrete transport. Convenience composites that create a transport belong in application assembly; they must not make bridge depend on transport. Interop defines the generic session entry point for runtimes, and the bridge adapter converts Peer to that entry point to avoid a crate dependency cycle.

Using core alone must not pull in extensions; using only a local runtime must not pull in WebSocket; connecting only to a remote execution endpoint must not require a local Node/Python installation. Memory is selected only by tests or explicitly configured applications. A new external transport crate implements the same contract and requires no bridge changes.

## 3. Shared connections and sessions

### 3.1 Services and dependencies

- A transport plugin provides `Transport#<kind>`, configured by the `transport` key.
- A network link depends on Transport and Identity. For a local execution endpoint, the parent process supplies its identity.
- After identity and protocol handshakes, link provides `Peer#<id>`. This means the session is ready; it does not mean any management feature is authorized.
- The bridge runtime adapter and node feature plugins depend on Peer and register their respective control operations. RuntimePlugin uses the generic session entry point defined by interop and does not directly depend on Peer types. Handlers are unregistered when their owning plugin unloads.
- Peer exposes endpoint ID, Session, operation registration, and observation of peer feature declarations. The Session belongs solely to link.
- A link explicitly selects the runtime or node integration contract. Do not enable both management ownership models on one plugin instance.
- Local runtime session source: RuntimePlugin's local source is `local` transport + link (identity supplied by the launcher), using the same entry point as remote sources (`RuntimeSession#<name>`). Link's local compatibility mode (`LinkConfig::local_runtime`) speaks protocol 2, does not validate the contract, does not send `link.offers`, and does not reconnect after session end. When the process exits, link stops and runtime state becomes `Down(reason)`; `LocalRuntime::restart` launches a new process. `LocalRuntime::apply` waits for runtime readiness or link shutdown; it can be disposed or restarted during startup, and startup failure is fatal. Keep the self-launching `RuntimePlugin::node` / `python` / `launcher` paths as a deprecated compatibility layer. Keep the `Process` facade only for generated code and existing callers, sharing the same Session implementation.

### Session sharing and resource ownership

**A Session is created per runtime instance or node link, not per plugin. Loading or unloading plugins does not create or close a shared Session.**

| Integration mode | Active session boundary | Objects sharing the session |
| --- | --- | --- |
| Language runtime | One active controller Session per Runtime instance; its link owns remote access | All controlled plugin rows, host-service proxies, and cross-endpoint calls in that instance |
| Full node | One active Session per configured node link | That link's host/import/export/events and multiple business plugins installed through host |

- Runtime integration and node bridge features use the session they belong to; they do not create another Session per plugin, service, or event.
- Plugin instances are distinguished by instance keys. Calls, services, and object references route by their own IDs; resource ownership is tracked against the plugin instance or bridge feature. Instance keys and references are interpreted only in their session namespace.
- Unloading a business plugin cleans only its resources; unloading a bridge feature unregisters its operations and cleans its owned resources. Neither closes the shared Session or cleans unrelated plugins.
- When link ends, it closes the Session. Runtime integration cleans all rows under that controller session's lease; node interconnection cleans resources installed through that link and its imports/exports, without affecting plugins managed independently by the remote node.
- Services within one runtime can still pass native objects; a shared control Session does not require internal runtime calls to use the network.
- Sharing a Session among plugins is the management model for runtime and node integration. Sharing one physical connection across multiple logical Channels is a transport-adapter policy; the two are independent.

### 3.2 Connection registration and retry

- Each Session uses one logical Channel. Physical connection pooling, reuse, multiplexing, and maintenance belong to the Transport adapter; link does not require a session-exclusive physical connection.
- Each connector call returns one logical Channel establishment result. The dialing-side link is the sole owner of session-recreation backoff state. An adapter must not run another session retry loop, revive a terminated Channel, or replay a call whose result is unknown.
- Structured results such as `Retryable`, `AuthRejected`, and `Incompatible` determine retry policy; diagnostic text does not. Parameters are in the network-stack specification.
- A shared listener's validation rules and accept routing are handed to link through a revocable registration handle. Reject connections when no valid registration exists.
- At the end of the transport handshake, recheck identity and registration generation before handing off the connection; an old registration must not receive a late connection.
- When link stops or Identity is revoked, cancel retries, revoke registration, and close its sessions and handshaking logical Channels. Do not close shared physical connections used by other links. If revoking a lower-level credential invalidates the entire physical connection, the adapter notifies all affected Channels.
- Identity and authorization bind to the logical Channel; reuse must not mix permissions. A physical connection failure terminates affected Channels and each link handles its own session. Closing one Channel does not affect other Channels on the same connection.
- Unloading a transport closes its connections; dependent links stop through native gating.

### 3.3 Composition and updates

- A composite entry point only assembles existing implementations; it adds no protocol, authorization, or reconnect logic.
- Instances it creates belong to the composite subtree; referenced shared transport or identity instances do not.
- Update configuration by diffing child plugins. If only export, import, or event lists change, retain link and the existing session rather than rebuilding the entire composite subtree.
- Starting a local process or connecting remotely changes only the source of the session, not the management semantics of runtime rows.

## 4. Language runtime integration

### 4.1 Management and execution

- `RuntimePlugin` connects a named runtime whose session source may be a local transport or an already connected remote transport.
- One runtime instance hosts multiple plugins; configure multiple instances when isolation is needed. Manage the runtime instance name separately from the session endpoint ID.
- A runtime plugin depends only on execution-endpoint readiness, not on services provided by business rows.
- Leaf plugins declare apply, inject, provides, configuration, and cleanup; they do not provide subplugin trees, an event system, or an independent dependency graph.
- Services within one runtime pass native objects directly; cross-runtime or Rust calls route through the controlling rutis.
- A remote runtime must not decide dependency satisfaction, startup order, or restart policy itself.

### 4.2 Runtime contract

Reuse multilingual runtime operations; do not rename them as node operations.

| Operation / information | Meaning |
| --- | --- |
| mount / features | Validate runtime features such as `rows.v2`, `hosts`, and `leaf` |
| `rows.schema` | Return configuration schema, inject, provides, and method shapes |
| `rows.load / update / unload` | Load, update, and unload a row |
| `hosts.provide / withdraw` | Register and withdraw a host-service proxy in the runtime |
| Row service notification and invocation | Project row-provided services into rutis and access them through the runtime contract |

- Row services use `host_key(name)` / `dyn HostDispatch`; `methods()` supplies method shapes and `origin()` identifies the source runtime. `origin()` currently returns `Option<&Process>`; N2 changes it to a runtime-independent identity (runtime instance name + current session generation), and same-runtime checks use that identity rather than a local process object.
- Runtime features (`rows.v2`, `hosts`, `leaf`, etc.) are exposed by the Runtime service after contract validation. Loader-side row integration reads them from Runtime rather than accessing `Process` directly.
- Host services are leased by consuming rows: the first lease registers the proxy; the final release withdraws it. Operations are serialized and registration finishes before the lease is returned.
- Do not register a duplicate proxy for a native service in the same runtime; duplicate service exports fail with a conflict.
- rutis gates every inject for a `leaf` runtime. Cordis gives only `register_shared` names to rutis and manages the other dependencies itself.
- On row unload, withdraw the service projection and wait for consumers to stop before unloading the provider plugin.
- Updates follow runtime capabilities. A Python leaf plugin has no volatile fields, so an in-place update request is handled as unload followed by load; continuity of the instance is not promised.

### 4.3 Two-stage readiness

1. Link provides Peer; RuntimePlugin validates the runtime contract and then provides `Runtime::key(instance_name)`.
2. Loader-side `RuntimeRowsPlugin` depends on Runtime and Loader, asynchronously refreshes rows resolved offline or with changed declarations, and then provides `RuntimeRows::key(instance_name)`.
3. Rows depend on RuntimeRows and their resolved service dependencies; they must not start before declarations are complete.

Run resolution refresh in a separate task; do not synchronously wait for `reload` inside an active reconcile. If the runtime disconnects, Runtime is withdrawn, gating withdraws RuntimeRows, and the affected rows and service projections stop.

### 4.4 Remote leases

- A remote runtime accepts management only from configured and authenticated controllers; each runtime instance has one active controller session.
- Loaded rows, injected proxies, and exported references all belong to that session lease. When the controller session disconnects, the execution endpoint cleans every row and proxy in the lease; a remote rutis deployment is not required.
- A runtime daemon may continue listening. A child process started by a local parent cleans up and exits after disconnect. Neither process restarts automatically.
- End the old lease before a new controller session takes over; late instructions from the old session cannot affect the new lease. The execution endpoint handles a new connection in this order: transport authentication → close the old session channel and stop reading its subsequent frames → clean all rows and proxies in the old lease → reply to the new session's `hello`. Do not reply to the new session until cleanup finishes, so old and new leases never overlap.
- Over network transports, a remote leaf runtime only listens; the controller's link dials and owns reconnect. Use a reverse proxy or tunnel to traverse NAT; this design does not add leaf-initiated dialing.
- After reconnect, query declarations again and load the desired state. Do not restore old references or replay calls with unknown results.

## 5. Full node interconnection

### 5.1 Node feature plugins

| Plugin | Responsibility | Dependencies |
| --- | --- | --- |
| export | Announce selected local services and track replacement and withdrawal | Peer and exported services |
| import | Register selected remote services as native local services | Peer |
| host | Accept remote installation requests; installed plugins belong to its subtree | Peer |
| events | Forward notifications by event name and direction | Peer |

- Each node owns its configuration, services, and dependency graph. Feature plugins depend only on session readiness; they do not wait for remote rows.
- Feature plugins are not installed by default. The features installed are the authorization granted to that link.
- Feature registration and removal update `link.offers`; link relays it without interpreting row-management semantics.
- Importing a duplicate service name is a configuration error: the first registration remains and the later import fails, identifying both links. Registration order during cold start is unspecified, so no winner is guaranteed; configure distinct local names for same-named services from different links.

### 5.2 Node control operations

Node operations are available in both directions. An unregistered operation returns an error without ending the session.

| Operation | Meaning / handler |
| --- | --- |
| `services.announce { name, service, shape, version }` | Announce a service object reference. The version comes from one session-wide monotonic sequence (`Peer::next_version`); it continues increasing if the export plugin restarts after its list changes, so the announcement is not mistaken for an old message / import |
| `services.withdraw { name, version }` | Withdraw a service. The importer retains the withdrawn version, so a late older announcement cannot restore the service / import |
| `plugins.describe(plugin)` | Return `{ schema, version, integrity }` / host |
| `plugins.load(key, plugin, config, isolate, inject)` | Remote install: isolate by row `isolate` (`[service name, tag]`, with tags scoped by peer), gate by `inject`; map service names to keys using the host mapping (loader composite uses its service catalog; default is `host_key`); reject if mapping fails / host |
| `plugins.update(key, config)`, `plugins.unload(key)` | Update and unload / host |
| `events.forward { name, args }` | Send a parallel notification and wait for listeners to finish / events |
| `link.offers { families, version, since }` | Announce registered feature families with a monotonic version and ignore old announcements. `since` is the version at which each family was registered; if withdrawn and registered again, it is a new offer. Followers use this version (not just whether the family exists) to re-provide dependents. If omitted, every family on that side is treated as version 0 / link |

- Access services by calling/getting Session objects. On replacement, announce a new reference, order by version, and release the old reference according to its count.
- Cordis services are read through the export plugin's export fiber; effects caused by calls belong to that fiber.
- Forward notifications only. For the same link and event name, allow only one forwarding direction.
- Nodes can form a tree; importing and re-exporting across links requires reference forwarding and call-path routing.

### 5.3 Loader-side remote installation

Use the same two-stage readiness as §4.3:

1. Link provides Peer, and the peer announces its `plugins` feature family.
2. Loader-side `PeerRowsPlugin` depends on Peer and Loader, refreshes resolution asynchronously, then provides `PeerRows#<id>`.
3. `PeerRow` depends on PeerRows and must not start until declarations are complete.

- `PeerResolver` resolves `peer:<id>/<plugin>`; the target node interprets the plugin name.
- `PeerRowsPlugin` lives in rutis-loader, depends on Peer and Loader, observes `plugins` announcements, queries schemas, refreshes resolutions, and provides `PeerRows#<id>`.
- If the peer has no `plugins` feature or is offline, mark the resolution offline with an empty schema. The row waits for PeerRows; do not mark it Unresolved.
- Run refresh in a separate task using `entries()` + `reload(id)`. Old resolutions cannot start before gating opens; each row starts only once.
- `PeerRow` depends on PeerRows; `apply` sends load and its disposer sends unload. Treat unload after disconnect as successful.
- When the peer withdraws `plugins`, PeerRowsPlugin withdraws PeerRows; Peer and other features keep running.
- A session registry lets resolvers without `ctx` look up sessions. Row integration and resolvers are wired by the composite entry point, not link.
- The target framework manages dependencies within a full node; do not copy its internal dependency graph into the controller's rutis.

## 6. Session and compatibility

### 6.1 Contract isolation

- Sharing a session mechanism does not mean sharing control operations or authorization. Runtimes use rows/hosts; nodes use plugins/services/events.
- Configure the endpoint contract explicitly. Validate version and capabilities at startup; do not guess the endpoint type from a failed call or silently switch modes.
- The existing multilingual implementation's `PROTOCOL = 2` is not the proposed node-session format in this design. A new endpoint-identity handshake and call-ID format are breaking changes and require a different major version; choose the exact release version before implementation.
- The WebSocket subprotocol follows the actual session major version. Do not label the new format as the existing `rutis.2`.
- Preserve the existing runtime operations and wire format on the local compatibility path. New network sessions use an explicitly negotiated format and fail as incompatible if the old endpoint does not support it.
- During transition, both formats coexist: Rust interop and the Node/Python SDKs support `PROTOCOL = 2` and the new major version. The session source selects the format (2 for local compatibility, new version for network); handshake determines it, and it never changes within a session. Whether the local path later migrates is a separate decision outside this design.
- Identity on a local compatibility session: `PROTOCOL = 2` `hello` contains only the version, not an endpoint ID. When link connects to a local child process through the `local` transport, take endpoint identity from launcher-supplied `ChannelInfo.peer` without checking a `hello` endpoint ID. This exception applies only to an inherited fd or private socket owned by the launcher; network sessions always validate identity.

### 6.2 Target session capabilities

- The handshake declares session version, endpoint ID, implementation name/version, and capabilities on both sides. Node and runtime implementations can use it; framework objects are not required.
- A network endpoint ID must match the identity validated in ChannelInfo. A local ID is supplied by the parent process. Endpoint IDs must be unique within a deployment and contain only lowercase letters, digits, and `-`.
- New-format call IDs are `<endpoint id>:<n>`; validate the prefix and increasing sequence. Reference origin and call-path values must agree.
- The compatibility runtime path retains existing session tags and call-path `rebase`; changing call IDs alone must not lose cross-session routing.
- Conversion of references and call paths between formats happens only in the rutis relay (`rpc/relay.rs`): during forwarding it rewrites a compatibility session's tagged path to endpoint IDs for the new format, and vice versa. Execution endpoints need not know the other format. Network multi-hop (N4) is defined only between new-format sessions.
- Declare functions, async results, and object references by capability. Grant an object's shape on first access. Node/Python must not declare object-receive support until implemented.
- Map cancellation to AbortSignal, Rust cancellation tokens, or language equivalents; dropping a future sends cancellation.
- Preserve identity by `(session, reference ID)` during forwarding and propagate release along the relay chain. Reuse M1a's relay mechanism and add network multi-hop contracts and tests.
- Session capabilities are `objects`, `signals`, `reentrant-sync`, and `forwarding`, declared per implementation (Rust: all four; Node: `signals`; Python: `signals` and `reentrant-sync`). Endpoint contracts declare `runtime` or `node`; link checks them after handshake using `require` and stops as incompatible if one is missing. Feature families such as plugins, events, and volatile are declared through `link.offers`, not as session capabilities. Capabilities do not grant permissions; reject use when a required capability is missing.
- If synchronous reentrancy is unsupported, a same-chain reverse call during synchronous wait returns `SyncWaitCycle` immediately; it must not be queued.

### 6.3 Service shape and sync behavior

- Shapes describe sync/async methods, properties, and object shape. Do not expose undeclared members across endpoints or guess async behavior at runtime.
- Runtime services retain the existing provides method shape. Node services may generate shapes from Rust trait derives, TS declaration manifests, or Python inspection. Dynamic interoperation does not require typed bindings.
- Placement does not change sync/async method types. A remote sync call blocks for at least one RTT; expose call count and duration, but do not add automatic timeouts.
- Use async methods for operations with deadlines and let the caller set a timeout.
- Python's runtime can process incoming calls while synchronously waiting. Plugins must not hold a lock that blocks reentrancy while calling an external service.
- Independent call chains that cross-wait synchronously between two Node runtimes can still deadlock; a shared transport does not remove this limitation. Use async methods for such calls.

## 7. Lifecycle and security

| Event | Runtime integration | Node interconnection |
| --- | --- | --- |
| Session ready | Validate contract, provide Runtime, then loader provides RuntimeRows | Start feature plugins; loader refreshes and provides PeerRows |
| Disconnect | Link withdraws Peer; Runtime/RuntimeRows cascade withdrawal; controller projections are withdrawn and execution endpoint cleans leases | Link withdraws Peer; loader withdraws PeerRows; imported services are withdrawn and host subtree unloads |
| Reconnect | New lease, declarations, and row instances | New session, announcements, resolution, remote installation |
| Local process exit | Report exit status; do not restart automatically | Same |
| Replace old session | Clean old lease before accepting the new session | Link ends the old session and its feature subtree before accepting the new session |

- In-flight calls fail with a Transport error on disconnect; their result is unknown and they are not retried automatically. Old references return session closed.
- A full node cleans only resources owned by the disconnected link; it does not unload plugins managed independently by the node. A minimal local child node may clean up and exit under its process contract.
- Trust model: plugins are trusted; networks and remote machines are not fully trusted. Out-of-process execution is not a sandbox.
- Non-loopback networks must use TLS; a fronting reverse proxy may terminate TLS. The transport must authenticate identity; permissions do not depend on dialing direction.
- Node `host` can install only plugins already installed locally. Enabling `host` grants plugin-management authority to the peer, so expose it only to trusted peers.
- Remote runtime control belongs only to configured controllers. They may load plugins already deployed in the execution environment; code upload is not provided.
- By default, remote loading rejects `file:` and absolute-path plugin names. An explicitly local runtime may retain the profile's `file://` compatibility path.
- Evaluated configuration is sent to the execution machine and may contain secrets; diagnostics and traces must not record secrets.
- Each transport configures message-size limits and closes a channel when exceeded. This design adds no call-count or reference-count quotas.

## 8. Placement and configuration

| Scenario | Configuration |
| --- | --- |
| Local language plugin | Keep `py:<module>`, npm names, and named Runtime bindings; runtime session comes from local transport |
| Remote language plugin | Same language-row resolution rules, bound to a remote Runtime instance; runtime session comes from a network link |
| Remote node installation | `peer:<id>/<plugin>`, using the node host contract |
| Language plugin inside remote rutis | Outer node interconnection; remote rutis assembles its own Runtime and language rows |

- A language prefix selects the resolver; runtime-instance configuration determines execution location. Do not add a remote-language prefix.
- An unprefixed npm name resolves as a language row against the Node Runtime instance bound to the resolver. The old “default node” is replaced by this named Runtime; there is no implicit local node.
- Existing npm resolution uses the local Runtime anchor to find entry files. Remote Runtime plugin files live on the execution endpoint, so the controller must not check the local filesystem. The remote instance resolves the entry in `rows.schema`; if not found, return a structured result and mark the row Unresolved.
- Remote session support for Runtime is a proposed new configuration capability; this does not claim existing launchers support network connections.
- Evaluate configuration expressions on the controller and interpret them in the plugin on the execution endpoint; expression and plugin read/write environments belong to their respective sides.
- Isolate tags are shared only within the same execution framework instance; leaf runtimes follow their own contract. Recreate rows when changing execution instances.
- Register imported services in the native service system and loader catalog. Runtime projection continues to use `host_key`; do not add a parallel shared-service key.

## 9. Implementation phases

| Phase | Delivery |
| --- | --- |
| D1 | Decouple channels while preserving local multilingual contract and wire format |
| N1 | Shared link / identity integration, endpoint contracts and version checks; new session identity and capability format |
| N2 | Optional session source for Runtime integration (local through local + link); make `origin()` and runtime features independent of `Process`; both loader row-readiness paths and remote entry resolution; independent node feature plugins and composite entry point |
| N3 | Network stack D2–D3; remote Python / Node runtimes and rutis / Cordis nodes; lease and reconnect cleanup |
| N4 | Network multi-hop service forwarding, reference forwarding, and call-path routing |
| N5 | Shared consistency suites for sessions, runtimes, and nodes; add other endpoints by contract |
| Later | Binary encoding, more type bindings, Windows transport |

Keep the old reverse-direction `server.rs` and `build/rust.rs` frozen; removal needs a separate decision. Reuse existing multilingual M1/M2 capability; node interconnection does not force migration to a full framework.

## 10. Acceptance criteria

### Shared mechanisms

- In both runtime and node modes, load at least two plugins and verify they share one active Session. Loading creates no session; unloading one plugin does not close the session, and the other plugin remains callable.
- Enable multiple bridge features on one node link. Revoking one cleans only its resources; closing the link cleans all resources belonging to that session.
- Exercise the same session over memory, local IPC, and network transports; adding a transport must not add management operations.
- Test local/websocket/memory through native plugin loading, configuration validation, service provision, and unload. Unloading a transport stops dependent links and cleans resources.
- Core, interop, bridge, loader, and transport crates have no dependency cycles; bridge does not depend on concrete transports, and a local-only composite does not compile a WebSocket dependency.
- Reject identity, version, or endpoint-contract mismatches. Handle reconnect by structured errors, not text parsing.
- Link has no loader dependency and can establish a session while rows remain unresolved.
- Revoking link / Identity prevents old registrations and late handshakes from attaching, without affecting other links on a shared listener.
- Half-open connections are cleaned up within the heartbeat deadline; old references fail; a local process crash does not automatically restart.
- Rust, Node, and Python pass session tests under both `PROTOCOL = 2` and the new major version. Reference round trips through the rutis relay preserve identity and call paths across formats.
- Synchronous callbacks can progress on a blocked `current_thread`; supported references, cancellation, and error object graphs pass cross-language tests, and unsupported capabilities are rejected explicitly.

### Remote runtimes

- A remote machine running only the Python leaf endpoint, with no rutis, can still have rows, dependencies, configuration, and cleanup managed by the local loader.
- Run the same row tests on local and remote runtimes; same-runtime calls receive native objects and cross-runtime calls route through rutis.
- After cold-start declaration refresh, each row starts once. Withdraw provider projections before stopping consumers; a runtime crash affects only related rows.
- Disconnect cleans the execution-side lease; a new controller session cannot reuse old rows or references or receive old commands.
- If the old session is still connected when a new controller arrives, the endpoint closes it and cleans its lease before replying to the new `hello`; reject unauthenticated controllers.
- An npm name for a remote Node runtime resolves and loads even when absent on the controller; a plugin absent on the execution endpoint makes the row Unresolved.
- Python configuration updates restart the instance; Node uses volatile only when supported by its capabilities.

### Node interconnection

- Remote rutis / Cordis supports service import/export, remote installation, events, and native local dependency gating.
- Without `host`, a row waits for PeerRows; enabling it starts the row, withdrawing it stops the row, and other features remain unaffected.
- Simultaneous cold start and reconnect during mutual remote installation do not deadlock; host does not wait for rows to become ready.
- Disconnect cleans only plugins and services owned by that link; independently managed node plugins keep running.
- Composite and separate assembly have the same permissions and behavior; changing export configuration retains the session and unrelated features.
