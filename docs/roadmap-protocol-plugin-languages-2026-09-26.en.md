# Protocol Plugin Language Roadmap (2026-09-26)

> Historical reference only; this is not a requirement or implementation guide for the current branch. See [protocol plugin requirements](requirements-protocol-plugins.en.md) and the new [cross-process mount design](design-protocol-plugin-mount.en.md). The mechanisms, stages, and acceptance gates below are no longer in force.

> Status at the time: prioritize Rust/rutis and TS/Cordis. Other languages were future validation directions, not yet implemented or required for the base release.
> The core protocol, M0–M5 stages, and T01–T24 are in the [protocol plugin design](design-protocol-plugins-2026-09-25.en.md).
> Execution models were candidates. Review each runner against core prototype results before implementation; inclusion here does not freeze the API.

## 1. Language roadmap and release order

First validate object, context, event, and cleanup semantics with real rutis and Cordis, then extend to other languages.
New languages mainly add local context/service and cleanup adapters, object-proxy SDKs, runners, and interoperability tests; they should not add language branches to the rutis core.
The goal is neither JSON RPC wrappers for every language nor a full copy of Cordis internals. This table shows validation dependencies and investment order, not schedule commitments.
Later experiments could start after the M2 object vertical works. Official support requires the frozen M5 protocol and applicable lifecycle, object, and platform acceptance.

| Batch | Language / runtime | Execution model | Main goal and entry condition |
| --- | --- | --- | --- |
| L0 first-release priority | Rust / rutis, TS / Cordis | Native framework hosts plugins; shared or single-member runner; static composition for initial Rust | M0 go/no-go; M1–M5 validate scoped objects, borrowed callbacks, parallel/serial dispatch, and cleanup. Both Rust and TS are required. |
| L1 general-purpose | Python, Go | Native language context and object proxies; Go uses single-member or static-composition runners | Integrate after Rust/TS object model works; use the same object/authorization/lifecycle corpus and measure each language. |
| L1 system automation | Shell (Bash/Linux first) | Shared protocol runner managing a child shell/command per plugin or call | Prioritize existing scripts and file/process/system tools; plugin authors should not write protocol loops. |
| L1 system automation | PowerShell (PowerShell 7 first) | One dedicated Runspace per activation inside each runner process | Reuse the PowerShell engine and assemblies while retaining plugin sessions; release only on tested OSes. |
| L2 macOS automation | AppleScript, JXA | macOS automation runner; initial version manages script child processes | Finish macOS transport/reclamation validation and verify automation permissions and target-app behavior. |
| L3 as needed | Lua / LuaJIT | Lua runner managing plugin environments in a shared process; validate VM layout separately | Start when lightweight embedding or scripting use cases exist. |
| L3 as needed | Ruby, PHP, Perl | Persistent language runner for each; group compatible dependency environments | Implement when concrete ecosystem demand exists; write PHP for persistent CLI lifetime. |
| L3 alternate runtimes | Bun, Deno | Separate JS/TS runners grouped with Node | Support after passing the same corpus and lifecycle tests; JS execution alone does not imply Node equivalence. |

Language runners share the contract and management protocol but can use different execution models. The smallest shared unit is the protocol host. Whether a language reuses an interpreter, Runspace, or only script-process management depends on its capabilities; a separate process does not automatically provide a full permission sandbox. The base release does not require all L1–L3 runtimes. Each later runner records its experiments/readiness, execution environment, and passing tests separately.

**Python:** Provide idiomatic async APIs, local service objects, context, and cleanup scopes; authors should not maintain remote object IDs. Use a locked dependency environment per group and split groups when dependencies conflict; changing `sys.path` is not isolation. Native-object and proxy identity, callback, and release rules follow the core protocol.

**Go:** Standalone executable plugins use one-member groups. Deployments willing to use a unified build may compile multiple packages with the runner into one shared executable, reusing the Go runtime. Bind factory/contract manifests to the runner hash and reject missing members. Code updates rebuild and restart the group; configuration updates reinstall only the instance. This shares capabilities independently of the main process release but does not preserve independent deployment per member. The deployer's pipeline builds; the host does not compile plugin source on demand. Use generated interfaces, `context.Context`, explicit scopes, and cleanup. Cancelling a context does not prove a task exited; registered goroutines must confirm completion. Do not use Go's dynamic `plugin` loading by default; see [Go plugin documentation](https://pkg.go.dev/plugin) for toolchain/shared-dependency limits and inability to unload. Measure default group sizes rather than reusing Node conclusions.

## 2. System automation runner boundaries

**Consistent author experience:** A manifest declares service interfaces, configuration, and implemented capabilities. Authors implement script functions/entry points; the runner handles protocol I/O, arguments, value validation, object tables/proxies, error mapping, and task tracking. Simple commands may support values only. If objects are needed, a persistent runner keeps the real objects alive and manages references; never expose an object from an already-exited one-shot process. The manifest names the interpreter/language and explicit entry points; never build executable source from user-provided method names. Protocol ports and result channels must not consume normal plugin logs. Preserve command exit codes, script errors, and protocol errors separately.

**Shell / Bash:**

- A shared runner may run several plugins, but must not source unrelated scripts into one shell. Keep variables, working directory, traps, and exit ownership separate. See the [Bash manual](https://www.gnu.org/software/bash/manual/html_node/Command-Execution-Environment.html) for child execution environments.
- The first version uses command-style entry points that start a script per call. Persistent state requires an explicit persistent-per-activation mode with separately tested load/unload and cleanup. Finishing a command does not destroy the proxy fiber; later requests can still call the same plugin service.
- Pass arguments with argv or agreed JSON input. Never insert business strings into `sh -c` source text. Normal stdout/stderr are logs; a helper returns structured results through a dedicated pipe/fd. The helper encodes JSON; do not parse human-readable logs as results.
- Child shells, pipelines, and background tasks belong to their activation's execution record. Cancellation first requests stop and confirms managed processes exited; do not acknowledge completion if they remain. Runner crashes are runtime-group failures.
- The first release promises only tested Bash/Linux combinations. POSIX sh, other shells, and other OSes need separate declarations and acceptance; do not call this general cross-platform Shell support.

**PowerShell:**

- Each plugin activation uses its own Runspace while sharing the engine in one runner process. Do not send stateful plugin requests to arbitrary pooled Runspaces. See Microsoft's [multiple Runspaces hosting guide](https://learn.microsoft.com/en-us/powershell/scripting/developer/hosting/creating-multiple-runspaces).
- Runspaces separate session variables and modules, but still share assemblies, process environment, and some static state; this is not process isolation. Split runtime groups for incompatible modules/versions or separate failure boundaries.
- Invoke named commands/functions through parameter binding; do not place user data in concatenated script text. The runner handles output, errors, warnings, information, and native command output separately from protocol traffic.
- Project returned values to contract DTOs. Keep declared .NET/system objects in their owning Runspace and export references through the runner so calls return to the correct Runspace. Do not promise arbitrary objects can be losslessly converted to JSON or expose every reflected member by default. Release pinned objects before destroying the Runspace. Test null, empty/single-item arrays, nested objects, and errors explicitly; the depth behavior of [`ConvertTo-Json`](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertto-json) is not a full object-serialization guarantee.
- After stopping a pipeline, confirm related tasks and child processes have finished. If that cannot be confirmed, report `StopUnconfirmed`; do not treat `Runspace.Dispose` as a guarantee that arbitrary background work was killed.

**AppleScript / JXA:**

- Both use the management/transport layer of one macOS automation runner. The first version may invoke script execution tools from the runner; validate a persistent Cocoa/OSA host later before adopting it. Do not promise shared script objects or execution context in the first version. Apple's [Mac Automation Scripting Guide](https://developer.apple.com/library/archive/documentation/LanguagesUtilities/Conceptual/MacAutomationScriptingGuide/) describes the two languages.
- Pass business arguments as handler/run arguments or structured data from a host wrapper; do not interpolate them into AppleScript/JXA source. The wrapper explicitly converts returned values. Application object references / Apple Event descriptors are not general JSON. Later object support would require a persistent automation runner to retain and validate application-object handles; these are not the user's application memory. The initial command mode must explicitly reject unsupported objects rather than pretending a string description is a live proxy.
- Declare application identifiers and automation permissions. Test paths where the app is absent, not running, or permission is denied. Test authorization with the packaged runner and execution chain on macOS; prior authorization in a developer terminal does not prove release behavior. See [Apple permission guidance](https://support.apple.com/en-mz/guide/mac-help/mchl108e1718/mac).
- Cancellation stops waiting and attempts to terminate script execution, but an action already sent to the target app may continue or have completed. Report an unknown result and do not replay automatically. A user's target application is outside the runtime process tree; never kill Finder, a browser, or another user's app to stop a script.
- Initial samples should use application-provided scripting APIs. GUI automation scripts must separately declare and validate required system permissions, interactive sessions, and app versions.

## 3. Capability extensions and acceptance

The runner declares implemented capabilities such as object references, callbacks, event modes, streams, and reverse calls in runtime/hello. The plugin manifest and reachable interface jointly determine requirements.
Rust/TS first releases must meet core baseline acceptance T01–T24. Delegates, persistent callbacks, waterfall, and streams are separate core X01–X04 acceptance items; a reduced capability subset cannot bypass the basic object goal. Later script adapters may initially support value-only unary calls, but still need lifecycle, contract validation, cancellation/exit confirmation, and error routing.
Reject interfaces containing objects when object references are unavailable; likewise reject callbacks/events/streams individually when unsupported, and reject `requires` when reverse calls are unavailable. Never downgrade unsupported objects to snapshots, callbacks to function names, or events to notifications. Languages without static types must provide validation wrappers and object helpers.

Each extension runner must pass applicable T01–T24 tests and the following checks before release. A failed L1–L3 capability must be explicitly marked unsupported:

| ID | Scenario | Required assertion |
| --- | --- | --- |
| E01 | Two script plugins, configuration update, and stopping one plugin | Proxies, arguments, and state do not leak across plugins; stopping a member declared to execute independently does not stop the rest of its group. |
| E02 | Arguments with spaces, quotes, newlines, Unicode, or text resembling shell/script code | Arrive unchanged as data and are not executed; results and logs never mix with protocol frames. |
| E03 | Child-command failure, script exception, wrong result type, nested/empty/single-item values, and object return | Values and references are distinguished; formatted output is not treated as data; objects inside an exited script are not presented as live. |
| E04 | Pipelines, background tasks, cancellation, and exit races | Completion acknowledgement follows managed-task exit; non-cooperation is reported honestly and forced stopping respects group boundaries. |
| E05 | Two PowerShell Runspaces, module conflict, repeated object calls and release | Session and object ownership stay stable; objects are released before destruction; conflicting members can be split into groups. |
| E06 | Packaged macOS runner with authorization allowed/denied, target app absent, cancellation after sending an app action | Causes are readable; no automatic replay; user apps are not closed; no claim that an action was rolled back. |
| E07 | A value-only unary runner loads plugins requiring objects/callbacks/events/streams/reverse calls, or runs under a different OS/interpreter | Unsupported capabilities are rejected before load with no downgrade; availability is claimed only after tests in that environment. |

The roadmap does not introduce preset resource quotas. Performance and memory comparisons remain measurement work; discuss flow-control optimization, pool size, and default timeouts separately after data is available.
