# Mounting PowerShell "plugins" in rutis — design research

Date: 2026-10-03. Scope: how a rutis app could mount PowerShell modules as Cordis-style plugins through a
`rutis-interop`-like compatibility layer (system automation is the motivating use case). Nothing in the
rutis repo was modified. All experiments ran in Docker; scripts and raw outputs are under
`scratchpad/experiments/powershell/` (see Appendix A).

Baseline read for the model: `docs/requirements-protocol-plugins.md`, `docs/design-protocol-plugin-mount.md`,
`crates/rutis-interop/README.md`, `crates/rutis-interop/src/{protocol,process}.rs`,
`interop/node/src/{runner,peer,client,io-worker}.mjs`, and the abandoned
`docs/roadmap-protocol-plugin-languages-2026-09-26.md` (its PowerShell section is checked claim by claim in §12).

---

## 0. TL;DR

- **Execution model.** Run one long-lived **installed `pwsh` (7.4+)** process per mount, executing a rutis
  runtime module (`runner.ps1` + plugin-facing helper functions). Give every plugin its **own runspace with
  its own pipeline thread**. Put the parts that need threads and fast serialization (socket I/O, frame codec,
  value walker, per-plugin dispatch queues, re-entrancy) in a **small C# core loaded into that pwsh**. Ship
  it either as an IL-only DLL or as source that `Add-Type` compiles once and caches. This is the PowerShell
  Editor Services model: a module loaded into the user's `pwsh`.
  - Do not build a self-contained SDK host (141–193 MB per RID, measured) unless a "no pwsh installed" deployment is required.
  - Do not start one `pwsh` per call: 166–330 ms per call, no state, no live objects.
- **Measured costs** (arm64 Linux VM, PowerShell 7.6.6):
  - `pwsh -NoProfile -c 1`: 166 ms wall.
  - Idle `pwsh` RSS: 113–122 MB.
  - Extra runspace with a small module imported: about 2 ms to open, about 5 ms to import and make a first call, about 1.1 MB.
  - Warm call through the PowerShell API: 0.06 ms, or 0.2 ms with JSON.
  - Self-contained SDK host built ReadyToRun: 126 ms wall, but 193 MB on disk.
- **Runspaces isolate:** variables, functions, modules (and their `$script:` state), and `$PWD`.
- **Runspaces do not isolate:** environment variables, process cwd (all .NET path APIs), static .NET state,
  loaded assemblies, `Add-Type` types, or the console. Plugins whose modules conflict must go in separate mounts (processes).
- **Thread affinity is the biggest hazard (verified).** Plugin-owned scriptblocks and class methods must only
  run on the plugin's own runspace thread:
  - `& $sb` from another runspace runs with the plugin's session state on the wrong thread.
  - `$sb.Invoke()` and PowerShell class methods wait about 250 ms for a busy owner, then run concurrently anyway.
  - A scriptblock used as a .NET delegate on a thread-pool thread throws "There is no Runspace available…".
    Unhandled, that crashes the whole runner.
  - Nested pipelines on the owner thread (`RunspaceMode.CurrentRunspace`) work. That is the primitive for
    re-entrant host-service calls, prototyped successfully.
- **Cancellation is cooperative and leaky (verified).**
  - `PowerShell.Stop()` kills only the direct native child.
  - Grandchildren survive. If they hold the redirected stdout they **block `Stop()`/`Close()` until they
    exit** (102 s in one test).
  - A blocking .NET call delays `Stop()` until it returns.
  - `clean{}` and `finally` do run.
  - `Runspace.Close()` fires `PowerShell.Exiting` but **not** module `OnRemove`; only `Remove-Module` fires that.
  - Thread jobs and `Start-Process` children survive closing the runspace.
- **Serialization: never use `ConvertTo-Json` defaults (verified).**
  - Depth 2 truncates to `"System.Collections.Hashtable"`. 7.1+ only warns; 5.1 is silent.
  - `@(1)|ConvertTo-Json` gives `1`, and `@()|ConvertTo-Json` gives *no output*.
  - Enums become numbers, `TimeSpan`/`Version` become property bags, and NaN becomes the string `"NaN"`.
  - `ConvertFrom-Json` turns ISO strings into `DateTime` and enumerates arrays.
  - System.Text.Json cannot serialize `PSObject`.
  - The runner should return the **pipeline output collection** and serialize it with its own walker,
    projected by the function's declared `[OutputType()]`.
- **Interface extraction at build time.** Use `pwsh` with `Parser.ParseFile` plus `Import-PowerShellDataFile`.
  This executes no plugin code, and the data it yields is exactly what codegen needs. tree-sitter-powershell
  (a Rust crate) parsed the sample in 0.7 ms but **mis-parsed valid code**, so it can only be a fallback.
  Authors must type their parameters and use `[OutputType()]` and PowerShell classes for DTOs. Untyped code
  degrades to `serde_json::Value`.
- **Proposed plugin shape** (§11):
  - **Plugin:** a script module.
  - **Services:** its exported advanced functions form one service.
  - **apply:** an explicit `Apply` function. Its named parameters are the Config.
  - **Plugin state:** `$script:` scope.
  - **Cleanup:** `Register-RutisEffect` runs LIFO, then `OnRemove`, then `Exiting`.
  - **Host services:** `Inject` (in the manifest) names a contract class in the module; the plugin reaches
    the service with `Get-RutisService`.
  - **Events:** `Send-RutisEvent`.
  - **Method shape:** every method is `async` on the Rust side.
- **v1 scope:**
  - **In:** values only, host services (with re-entrancy), notification events, cancellation, PowerShell 7.4+
    on Linux/macOS (rutis-interop is Unix-only today).
  - **Deferred:** live object or scriptblock references, Rust callbacks passed into PowerShell, Windows,
    Windows PowerShell 5.1.

---

## 1. What a PowerShell runner has to provide (mapping from rutis-interop)

The Node runner (`interop/node/src/runner.mjs`, `peer.mjs`) shows the contract the Rust side expects from a
"language process":

| rutis-interop concept | Node runner today | PowerShell equivalent |
| --- | --- | --- |
| Process per mount, Unix socket, line-delimited JSON frames v1 (`hello/invoke/call/get/await/return/throw/release/cancel`) | `node runner.mjs <socket> <plugin>`; frames on a socket, stdout/stderr inherited | `pwsh -NoLogo -NoProfile -NonInteractive -File runner.ps1 <socket>`; same frames; stdout/stderr inherited (verified safe: plugin `[Console]::WriteLine` goes to process stdout, §4) |
| `mount` control call: load plugins in order, report service handles | Cordis Context + exporter fibers | one runspace per plugin; `Import-Module` and an `Apply` function; handles per service |
| Service method `invoke {target: handle, method, args}` | `Reflect.apply` | `PowerShell.Create(rs).AddCommand(fn).AddParameter(..)` (never `AddScript` with data) |
| Sync vs async method shape | JS sync vs Promise | PowerShell has no async: **all methods async on the Rust side** (cancellable) |
| Host services (`target: host:<name>`) | `hostProxy` → `peer.call/callAsync` | proxy object created *inside* the plugin runspace; blocks the plugin thread; pumps nested calls (§8) |
| Re-entrancy by call `path` | `#waiting` + `path` check in `Peer.#run` | per-plugin thread: a nested invoke whose `path` contains the call the plugin is waiting on runs on that thread (nested pipeline); everything else queues |
| `cancel` | aborts the `AbortSignal` | `PowerShell.StopAsync()`/`BeginStop()` (§6) |
| Events | forwarding listener / `parallel` | `Send-RutisEvent` → `invoke {target:'', method:'event'}` |
| Values: `undefined/data/list/record/signal/reference` | `#encode/#decode` | a custom walker over PowerShell values (§5) |

Two protocol conventions would differ, with no frame change needed:
- **Arguments.** Send PowerShell arguments as a **record of named parameters**, not a positional list.
  PowerShell binds by name; positional binding is fragile, as shown for `-ArgumentList` in §11.
- **Results.** An async method's `invoke` gets its final value in the reply. No Future reference or `await`
  frame is involved, because there is no Promise to keep.

---

## 2. Execution models (Q1)

### 2.1 Options and measured costs

All numbers come from `mcr.microsoft.com/dotnet/sdk:10.0`: Ubuntu 24.04.5, aarch64, 16 vCPU, Docker Desktop
on Apple Silicon. PowerShell 7.6.6 is installed there as a .NET global tool, i.e. framework-dependent.
Absolute values vary a lot by machine: a 2021 PowerShell team post reported a **1176 ms** baseline for
`pwsh -noprofile -command 1` on the author's machine ([devblogs][optimizing-profile]). Treat the numbers
below as relative.

| | (a) pure-PowerShell runner in `pwsh` | (a′) `pwsh` + runner module with C# core (**recommended**) | (b) self-contained C# host embedding `Microsoft.PowerShell.SDK` | (c) one `pwsh` per call |
| --- | --- | --- | --- | --- |
| Start of a mount | ~166 ms `pwsh -c 1`; ~277 ms with first `ConvertTo-Json` (Utility autoload); ~330 ms with a module import + call (exp 01) | same + C#: `Add-Type` compile **616 ms** first time, 13 ms after; prebuilt DLL: `pwsh` + `Add-Type -Path` **333 ms** wall (exp 16) | IL-only publish: **307 ms** wall; **ReadyToRun: 126 ms** wall (open runspace 54 ms, first call done 100 ms) (exp 08b) | — |
| Per call | 0.06–0.07 ms warm (0.21 ms incl. JSON) (exp 13) | same | same | **166–330 ms** + module import each time; no state |
| Memory | idle `pwsh` RSS **113–122 MB**; +~1.1 MB per plugin runspace (10 → 147 MB, 50 → 191 MB) (exp 01/02) | same (+DLL) | 92–116 MB (exp 08) | 113+ MB per concurrent call |
| Disk / packaging | runner is text; needs `pwsh` installed (release archives: linux-arm64 71.9 MB, linux-x64 75.9 MB, osx-arm64 70.6 MB, win-x64 zip 106.3 MB ([v7.6.6 release][ps-release])); dotnet-tool install 60 MB without the runtime (exp 01) | text + optional ~KB DLL (IL-only, one artifact for all OSes) | **141 MB** (530 files, 50 MB .tgz) self-contained IL; **193 MB** (68 MB .tgz) ReadyToRun; framework-dependent 57 MB + .NET 10 runtime — **per RID** (exp 08/08b) | none beyond `pwsh` |
| PowerShell version | whatever the user installed (7.x; 5.1 possible with a different transport and JSON code) | same; a `netstandard2.0` build can also load into Windows PowerShell 5.1 ([NuGet guide][nuget-guide]) | exactly the bundled SDK: 7.6.x needs .NET 10 ([NuGet][sdk-nuget]); "a single host application can't multi-target PowerShell versions" ([NuGet guide][nuget-guide]) | any |
| Threading control | limited (BeginInvoke, BlockingCollection, no safe delegates) — but a re-entrant prototype works (exp 12) | full (dedicated pipeline threads like PSES) | full, plus custom `PSHost` | n/a |
| Ecosystem quirks | uses the user's `$PSHOME/Modules`, profile skipped with `-NoProfile` | same | `CreateDefault2()` **cannot find `ConvertTo-Json`/`Get-ChildItem`** in a plain SDK publish (no `Modules` folder) — must use `CreateDefault()` or ship module manifests (exp 08); **`Start-Job` unsupported** ("not supported by design … ThreadJob … recommended", exp 08; [NuGet guide][nuget-guide]) | `Start-Job` works |
| Precedent | AWS Lambda PowerShell custom runtime (§9) | PowerShell Editor Services (§9) | Azure Functions worker, .NET Interactive (§9) | — |

Notes:
- **The R2R gap.** The ReadyToRun host starts faster than `pwsh` here (126 ms vs 300 ms for a comparable
  command). `pwsh` autoloads `Microsoft.PowerShell.Utility`/`Management` on first use, while the host's
  `CreateDefault()` loads all built-in cmdlets up front. For a long-lived mount the difference doesn't matter.
- **Windows startup hazards are environmental, not structural.** Examples are certificate-revocation
  lookups when loading signed modules offline, and the update check ([PowerShell#18090][ps-18090]).
- **(c) as an add-on.** (c) is still useful as an optional "command mode" for stateless one-shot scripts,
  with stdin JSON in and a result file out. It is not a plugin model: no apply, no effects, no live state.

### 2.2 Version and platform support

| | Windows PowerShell 5.1 | PowerShell 7.4 (LTS) | 7.5 | 7.6 (LTS) |
| --- | --- | --- | --- | --- |
| Runtime / OS | .NET Framework 4.x, Windows only ([differences][diff-winps]) | .NET 8, Win/Linux/macOS | .NET 9 | .NET 10 |
| Support ends | with Windows ([lifecycle][lifecycle]) | **2026-11-10** | 2026-11-10 | 2028-11-14 |
| `UnixDomainSocketEndPoint` | no (.NET Core 2.1+/netstandard2.1 only, [API][uds-api]) | yes | yes | yes |
| `ConvertFrom-Json -AsHashtable/-Depth/-NoEnumerate` | none ([5.1 doc][cfj-51]) | yes (ordered hashtable since 7.3) | + `-DateKind` (7.5) | yes |
| `ConvertTo-Json` | JavaScriptSerializer, `\/Date(ms)\/` dates, only `-Depth/-Compress` ([5.1 doc][ctj-51]); truncates past depth — the 7.x doc dates the truncation *warning* to 7.1, although the 5.1 parameter text also mentions a warning (docs inconsistent; not testable here) | Newtonsoft; warning on truncation since 7.1 ([7.6 doc][ctj-76]) | + BigInteger as number | |
| `clean {}` block | no | yes (7.3+, [doc][advanced-methods]) | yes | yes |
| `$PSNativeCommandUseErrorActionPreference` | no | yes (7.4, [doc][pref-vars]) | yes | yes |
| `[NoRunspaceAffinity()]` for classes | no | 7.4 ([about_Classes][about-classes]) | yes | yes |
| `ConvertTo-/ConvertFrom-CliXml` | no | no | 7.5 | yes (exp: present in 7.6) |

**Recommendation.** Target **PowerShell 7.4+** on Linux and macOS first, matching rutis-interop, which is
Unix-only (`process.rs` uses `UnixListener` and `waitid`). Windows `pwsh` comes next; it needs a named-pipe
transport (§10).

Treat Windows PowerShell 5.1 as unsupported in v1. 5.1-only modules can still be used from `pwsh` via
`Import-Module -UseWindowsPowerShell` (implicit remoting into `powershell.exe`, [conflicts doc][dep-conflicts]).
A later 5.1 runner is possible with a `netstandard2.0` core, named pipes, and its own JSON code. It would
lack `clean{}`, `NoRunspaceAffinity` and native-exit-code errors.

### 2.3 Packaging/deployment implications

- (a/a′) mirrors the Node design.
  - `pwsh` must be installed, as `node` must today.
  - The runtime is a versioned **PowerShell module** (say `RutisInterop`), vendored into a project `Modules/`
    directory or installed from the PowerShell Gallery. Its manifest `PrivateData` carries `RutisProtocol = 1`,
    which the build checks against the crate's `PROTOCOL`, exactly like `rutisProtocol` in `package.json`.
  - Plugin modules and their `RequiredModules` are saved next to it with `Save-PSResource` and found through a
    prepended `PSModulePath`. Azure Functions does the same with its `Modules` folder.
  - Deployment = binary + module directory, with an `RUTIS_INTEROP_ROOT`-style override.
- (b) removes the `pwsh` prerequisite but costs:
  - 141–193 MB per platform;
  - a PowerShell version pinned to the rutis release, so security servicing is our job;
  - having to ship the built-in modules plus PowerShellGet/ThreadJob, as .NET Interactive does (§9);
  - no `Start-Job`.

---

## 3. Runspace semantics (Q2)

### 3.1 What is per runspace vs per process (measured, exp 02)

Runspace A set: `$x`, a function `f`, `Set-Location /tmp`, `$env:SHARED_ENV`, `[Environment]::CurrentDirectory`,
an `Add-Type` class with a static field = 42, and imported a module. Runspace B (same process) then saw:

| State | Scope | Observation |
| --- | --- | --- |
| Variables, functions, aliases | **runspace** | B: `$x` empty, `f` absent |
| Imported modules and their `$script:` state | **runspace** | B didn't see `Microsoft.PowerShell.Security`; the plugin's own call counter: A=3, B=1 |
| `$PWD` / `Set-Location` | **runspace** | B: `/`, A: `/tmp` |
| Preferences, language mode, visible commands (InitialSessionState) | **runspace** | by construction |
| Process cwd (`[Environment]::CurrentDirectory`) | **process** | B saw `/etc`. Cmdlets resolve relative paths against `$PWD`, .NET APIs against process cwd: `Set-Content rel.txt` wrote `/tmp/rel.txt` but `[IO.File]::Exists('rel.txt')` = False |
| Environment variables | **process** | B saw `set by A` |
| Static .NET state, `Add-Type` types, loaded assemblies | **process** | B saw `[SharedStatic]::Counter = 42`. "PowerShell always loads assemblies into the same context" ([dependency conflicts][dep-conflicts]); `Remove-Module` doesn't unload assemblies ([Remove-Module][remove-module]) |
| PowerShell classes | types are process-level, cannot be unloaded or reloaded; instances keep runspace affinity | [about_Classes][about-classes] |
| Console | **process** | `[Console]::Out.WriteLine` from a plugin goes straight to process stdout (exp 07) |
| Memory | **process** | after disposing 50 runspaces, WS *rose* to 258 MB. Memory isn't returned promptly, so frequent in-process reloads should recycle the process |

**Consequence:** a runspace is a *namespace*, not an isolation boundary. This confirms the roadmap's statement.
Plugins that set environment variables or the cwd, or that load conflicting assemblies (Az.*, AWS.Tools,
Newtonsoft versions), belong in separate mounts. The runner should also give `cwd` a defined value: it should
not change the process cwd per plugin, and the docs should tell authors to use `$PSScriptRoot` or absolute paths.

### 3.2 Threads and thread affinity (measured, exp 02/02b/14)

- **Thread options.**
  - With the default thread option, a synchronous `Invoke()` from another thread ran on a **new thread every
    time** (ids 33, 37, 42).
  - `ThreadOptions = ReuseThread` gave one stable thread (43, 43, 43); `UseNewThread` gave a new one per call.
  - PSES uses its own thread with `UseCurrentThread` (§9).
  - One runspace runs one pipeline at a time: `RunspaceAvailability = Busy`.
- **A scriptblock created in plugin runspace A, called from the runner runspace:**
  - `& $sb` ran on the **runner thread, with A's session state** (it saw `$x = 'A'`) and no marshalling at
    all. While A was busy it still returned in 1 ms, so A's session state was being used **concurrently**
    from two threads.
  - `$sb.Invoke()` / `InvokeWithContext()`: when A was idle, the call was marshalled to A (runspace 2, a new
    thread). When A was busy, it **waited ~250–259 ms and then ran on the caller's thread** with A as
    DefaultRunspace, again concurrently.
  - PowerShell **class instance methods** behave the same way (marshalled when idle; ~256 ms then the caller
    thread when busy). This is the documented "can corrupt the state of the Runspace or cause a deadlock"
    hazard. `[NoRunspaceAffinity()]` (7.4+) opts out ([about_Classes][about-classes]).
- **Delegates on other threads.**
  - A scriptblock converted to a .NET delegate and run on a thread-pool thread (`Task.Run`) failed with
    `PSInvalidOperationException: There is no Runspace available to run scripts in this thread`.
  - When that happens in a `System.Threading.Timer` callback, the exception is **unhandled**. PowerShell
    printed "An error has occurred that was not properly handled … The PowerShell process will exit". The
    process then spun at ~99 % CPU until it was killed (exp 14).
  - So one plugin can take the whole mount down. `Register-ObjectEvent` is the safe pattern: event actions
    are queued to the owning runspace and do run in an *idle* hosted runspace (10 timer ticks in 1 s idle, exp 10).
- **Nested pipeline on the owner thread works.** `[powershell]::Create([RunspaceMode]::CurrentRunspace)` from
  plugin code ran on the same thread and saw the outer call's state (exp 02b #5). This is the re-entrancy
  primitive (§8).

**Rule for the runner:**
- Never execute plugin-owned script (scriptblocks, functions, class methods) from the runner thread or from
  .NET callbacks.
- Queue the work to the plugin's pipeline thread: a new pipeline when the plugin is idle, or a nested pipeline
  when it is blocked inside a host-service call in the same call chain.

### 3.3 Jobs

- **`Start-ThreadJob`** runs scriptblocks on in-process threads with their own runspaces (throttle default 5,
  session-global, [doc][threadjob]). In exp 04 #8 a thread job **kept running after its creating runspace was
  closed and disposed**, and finished its work.
- **`Start-Job`** starts a child `pwsh` that speaks PSRP over stdio (`pwsh -s`). In an SDK host it fails by
  design (exp 08, [NuGet guide][nuget-guide]). In the dotnet-tool `pwsh` image it reported `Running` but no
  child process appeared (exp 04c); this was not investigated further.
- **What the runner must do.** Track jobs per plugin runspace and `Stop-Job`/`Remove-Job -Force` them on
  dispose. Azure Functions does the same after every invocation (`ResetRunspace` → `Remove-Job -Force`, §9).

---

## 4. Safe invocation, binding, streams, errors (Q3) — measured (exp 07, 09)

### 4.1 Invocation and binding

- **`AddCommand` plus `AddParameter` passes data as data.**
  - The value `x"; Remove-Item -Recurse / ; $(whoami) \`n` reached the function verbatim.
  - The same idea through `AddScript("Write-Output '$evil'")` printed `INJECTED`.
  - Rule: **the runner only ever uses `AddCommand(<validated exported function name>)` plus `AddParameter`.**
- **The parameter binder does the type work.** `'7'` became `Int32`, `'2024-01-02T03:04:05Z'` became
  `[datetime]` (UTC), a hashtable bound to `[hashtable]`, and `$true` to a `[switch]`.
- **Binding failures throw before the function body runs**, with precise exception types:
  - `ParameterBindingArgumentTransformationException`: "Cannot convert value "abc" to type "System.Int32"".
  - `ParameterBindingValidationException`: ValidateSet.
  - `ParameterBindingException`: missing mandatory parameter. There is no prompt in a hosted runspace.
- **Typed output.** `Invoke()` returns a `Collection<PSObject>`: one item per object written to the Success
  stream. The runner works on `PSObject.BaseObject` plus the ETS view.

### 4.2 Streams and where a host should route them

What a hosted runspace captured for one call (exp 07 #1, `-Verbose` passed by the host):

| Source in plugin | Captured as | Route in rutis |
| --- | --- | --- |
| `Write-Output`, bare expressions | Output collection | **result** (projected by `[OutputType]`) |
| native command **stdout** (`sh -c 'echo native-stdout'`) | **Output** — strings mixed into the result! | result → strict `[OutputType]` validation turns accidental output into a clear error (§5.3) |
| `Write-Error` | Error stream, `WriteErrorException` | log (warn/error) with call id; see §4.3 |
| native command **stderr** | Error stream, `RemoteException`, FQID `NativeCommandError` | log |
| `Write-Warning` | Warning | `tracing::warn` |
| `Write-Verbose` | Verbose (only if `-Verbose` or `$VerbosePreference`) | `tracing::debug` |
| `Write-Debug` | Debug (not recorded at default `SilentlyContinue`) | `tracing::trace` |
| `Write-Information` | Information (recorded even at default `SilentlyContinue`) | `tracing::info` |
| `Write-Host` | **Information** with tag `PSHOST` (default host) | `tracing::info` |
| `Write-Progress` | Progress record | drop or trace (also set `$ProgressPreference='SilentlyContinue'`) |
| `[Console]::WriteLine` | not captured; process stdout | inherited stdout (harmless because frames use a socket) |

- **Live forwarding.** `PSDataCollection.DataAdded`/`DataAdding` events fire while the call runs (a warning
  was visible 150 ms into a 600 ms call). Azure Functions forwards logs exactly this way (§9).
- **Never give plugin runspaces the console host (exp 09).**
  - With the console host, `Write-Host` printed to process stdout.
  - `exit 9` in such a runspace set the **runner process's exit code to 9**.
  - With the default host, `exit 7` just ended that pipeline and the runner lived on.
  - The runner should create runspaces without `$Host` (or with a custom `PSHost` that rejects prompts), so
    `Read-Host` and `ShouldContinue` fail fast.

### 4.3 Errors and exit codes

- **Terminating errors** (`throw`, `-ErrorAction Stop`, `$ErrorActionPreference='Stop'`) make
  `Invoke()`/`EndInvoke()` throw.
  - Examples: `RuntimeException` carrying an `ErrorRecord` (FQID, category, `ScriptStackTrace` such as
    `at Fail-Terminating, <No file>: line 17`), or `ActionPreferenceStopException`.
  - Map this to a `throw` frame. Include `name` (exception type), `message`, and a `graph` with FQID,
    category, target, script stack, invocation position, and the inner exception chain.
- **Non-terminating errors** let the function continue; `HadErrors` becomes true.
- **`-ErrorAction Stop` escalates the first error of any kind.** In test 8 it was a null-method call, raised
  as `CmdletInvocationException`.
- **Native exit codes.**
  - By default a non-zero exit code is only `$LASTEXITCODE`/`$?` (exp 07 #7: "continued, LASTEXITCODE=5",
    `HadErrors=True`).
  - With `$PSNativeCommandUseErrorActionPreference = $true` (7.4) and `Stop`, it raises
    `ActionPreferenceStopException: Program "sh" ended with non-zero exit code: 4` ([pref vars][pref-vars]).
  - The docs note that tools like robocopy use non-zero codes for success, so authors may need to switch the
    preference off locally.
- **Recommended runner defaults for plugin runspaces** (set via InitialSessionState; overridable per module
  in the manifest):
  - `$ErrorActionPreference = 'Stop'`
  - `$PSNativeCommandUseErrorActionPreference = $true`
  - `$ProgressPreference = 'SilentlyContinue'`
  - `$ConfirmPreference = 'None'`
  - `-NonInteractive` semantics

  Silent partial failure is the worst outcome for system automation, and a Rust caller cannot see
  non-terminating errors in a `Result<T>`. Any non-terminating errors that still occur (an explicit
  `-ErrorAction Continue`) are forwarded as log records that carry the call id.

---

## 5. Serialization (Q4)

### 5.1 Measured pitfalls (exp 03/03b; PowerShell 7.6.6)

| Case | Result | Implication |
| --- | --- | --- |
| nested hashtable, default `-Depth 2` | `WARNING: Resulting JSON is truncated…` + `{"l1":{"l2":{"l3":"System.Collections.Hashtable"}}}` | silent data loss on 5.1 (no warning before 7.1, [doc][ctj-76]); max depth 100 (`-Depth 101` is a validation error) |
| `@(1) \| ConvertTo-Json` / `-InputObject @(1)` / `-AsArray` | `1` / `[1]` / `[1]` | pipeline enumeration unrolls single-element arrays |
| `@() \| ConvertTo-Json` / `-InputObject @()` | **no output** / `[]` | |
| `$null \| ConvertTo-Json` | `null` (7.x); 5.1 doc: "does not generate any output" ([5.1][ctj-51]) | version difference |
| function returning `@(1)` / `@()` / `,@(1)` | `Int32 1` / `$null` / `Object[]` (count 1) | `return @(...)` is not an array to the caller |
| via `PowerShell.Invoke()` | `One`→ Count 1, `@()` → **Count 0**, `$null` → Count 1 (null item), `return ,@(1,2)` → Count 1 (an `Object[]`) | the **output collection** is unambiguous; the runner should use it |
| enums | `1`; `-EnumsAsStrings` → `"Monday"`; flags → `"ReadOnly, Hidden"` | |
| DateTime UTC / Unspecified / DateTimeOffset | `"2024-01-02T03:04:05Z"` / `"2024-01-02T03:04:05"` (no zone!) / `"…+08:00"`; 5.1: `"\/Date(ms)\/"` | |
| TimeSpan, Version | objects with all properties (`Ticks`, `TotalDays`…) | needs explicit mapping |
| Guid, Uri, char, `byte[]` | strings; `byte[]` → array of numbers | |
| `[long]::MaxValue`, `[decimal]0.1`, `[bigint]2^70` | raw numbers (bigint since 7.5) | JS consumers lose precision; Rust/serde fine with i64/u64; bigint needs string |
| NaN / Infinity | `"NaN"`, `"Infinity"` strings | not valid JSON numbers — type confusion |
| hashtable key order | unspecified; `[ordered]` preserved | |
| non-string dictionary keys | error ("Keys must be strings") | |
| PSCustomObject with `$null` and `@()` | `{"b":1,"a":null,"c":[]}` | fine when not via pipeline |
| class instance | includes **`hidden`** properties (`"Secret":"s3cret"`) — also documented ([about_Classes][about-classes]) | leaks |
| ETS NoteProperty on an object | included | |
| ETS props on strings (`Get-Content` lines' `PSPath`…) | dropped since 7.2 (5.1 emitted `{"value":…,"PSPath":…}`) | |
| arbitrary .NET objects | `Get-Item` (depth 1) 3.5 KB; `Get-Process` (depth 1) 18 KB, 32 ms | accidental huge payloads |
| self-referencing hashtable | depth-capped repetition, warning — no cycle error | |
| `ConvertFrom-Json '[1]'` / `-NoEnumerate` | `Int64 1` / `Object[]` count 1 | arrays enumerate since 7.0; before 7 they did not, and `-NoEnumerate` restores the old behaviour ([Petri][petri-json]) |
| `ConvertFrom-Json '[]'` / `-NoEnumerate` | `$null` / empty `Object[]` | |
| ISO date strings | auto-converted to `DateTime` (`Kind=Utc`); `-DateKind String` (7.5+) keeps the string | strings that look like dates change type |
| numbers | `Int64`, `Int64`, `BigInteger`, `Double`, `Double` | |
| case-colliding keys / empty key | error without `-AsHashtable`; `-AsHashtable` → `OrderedHashtable` keeps both | |
| System.Text.Json on hashtable / class instance / enum | works (`{"a":1,"b":[1,2]}`, `{"Name":"x","N":1}`, `1`) | |
| System.Text.Json on `PSCustomObject` or pipeline output (`PSObject`) | **throws** "possible object cycle… `$.Members.Value.Value…`" | must unwrap/walk `PSObject` manually |

### 5.2 What the runner should use

**Outbound: a custom walker, implemented in C# in the runner core.** It replaces `ConvertTo-Json`. It never
truncates silently, emits the rutis wire value directly (`data/list/record/reference`), and applies these rules:

1. Unwrap `PSObject` to `BaseObject`. If the `PSObject` is a `PSCustomObject`, enumerate its *adapted/ETS*
   properties in order.
2. Primitives:
   - `string` and `char` stay strings.
   - `bool` stays a boolean.
   - Integers map to JSON numbers. Values beyond ±2^53 still go as numbers, because serde reads i64/u64;
     document this.
   - `float`/`double` map to numbers; **NaN/±Inf are an error**.
   - `decimal` and `BigInteger` map to numbers when they fit, otherwise strings.
3. `DateTime` maps to ISO 8601 with offset. `Unspecified` kind is treated as an error, or as local with a
   warning; document the choice. `DateTimeOffset` maps to ISO 8601.
4. `TimeSpan` maps to a duration string (`[c]` format `d.hh:mm:ss.fffffff`) or to seconds. Pick one and
   generate `std::time::Duration`.
5. `Guid`, `Uri` and `Version` map to strings.
6. Enums map to their **name** (`[Flags]` to a list of names).
7. `byte[]` maps to base64 (or a future binary frame).
8. `IDictionary` maps to a `record`/object. Keys must be strings, otherwise error.
9. Any `IEnumerable` except string maps to a `list`.
10. PowerShell class instances map to their public non-hidden properties, in declaration order.
11. Any other .NET object is an **error** ("not a data value"), unless the declared return type is a live
    object type (v2, §8). Never dump `Process`/`FileInfo` property bags by accident.
12. Cycles are an error, with no depth cap needed because cycles are detected. Use a configurable maximum
    size instead.

**Inbound** — in C#, read frames with `System.Text.Json` (`JsonDocument`). Then build .NET values:
- JSON objects become `Hashtable`/`OrderedHashtable`; arrays become `object[]`; strings stay **strings**
  (no date sniffing); integers become `long`.
- Let the **PowerShell parameter binder** do the target conversion through `AddParameter`. It turns
  string→int, hashtable→PowerShell class (verified), and string→`[datetime]`/`[timespan]`.

In pure PowerShell the closest equivalent is `ConvertFrom-Json -AsHashtable -NoEnumerate -DateKind String`
(7.5+; on 7.4 date sniffing cannot be turned off).

### 5.3 Results: return the output collection, project by `[OutputType]`

Rule (fixes the unrolling ambiguity):

| Declared | Rust type | Projection of `Invoke()`'s output collection |
| --- | --- | --- |
| `[OutputType([T])]` | `T` | exactly one item of type T; 0 items → error (or `Option<T>` if declared `[OutputType([T], [void])]`/nullable convention); >1 → error "declared one T, emitted n" |
| `[OutputType([T[]])]` | `Vec<T>` | N items of T, or a single item that is an `IList` of T (from `,@()` / `-NoEnumerate`) — normalised to a list |
| `[OutputType([void])]` | `()` | any output → error (catches leaked native stdout) |
| none | `Vec<serde_json::Value>` | all items, walked as data |

**Strict type checking** of each item against the declared T turns the classic PowerShell bug (stray
`native-stdout` strings or `$list.Add()` return values in the output) into an actionable error. The error
names the function and the offending item type.

---

## 6. Cancellation and cleanup (Q5) — measured (exp 04, 04b, 15)

| Scenario | Measured behaviour |
| --- | --- |
| `Stop()` with native `sleep 101` in the pipeline | returned in **14 ms**; process killed; `EndInvoke` → `PipelineStoppedException` ([Stop() doc][ps-stop]: Invoke returns partial results, async throws) |
| native child with children `sh -c 'sleep 102 & sleep 103; wait'` | `Stop()` **blocked 102 006 ms**: `sh` was killed, grandchildren were re-parented to PID 1 and kept the redirected stdout open; `Stop()` waited for EOF |
| same with `StopAsync()` | after 500 ms: `sh` gone, `sleep 8/9` alive (PPID 1), state `Stopping`; task completed after 8017 ms |
| same piped to `Out-Null` | still 8205 ms |
| grandchild redirects its own output (`sleep 9 >/dev/null 2>&1 & sleep 8`) | `Stop()` 7211 ms; `sleep 9` **survived** |
| blocking .NET call `[Threading.Thread]::Sleep(4000)` | `Stop()` took **3504 ms** (waits for the call) |
| `Start-Sleep 30` | `Stop()` 1 ms |
| `begin`/`end{try/finally}`/`clean{}` function, stopped | markers `begin,finally,clean` — both `finally` and `clean{}` ran ([clean doc][advanced-methods]) |
| `Runspace.Close()` during `Start-Sleep 30` | 4 ms, pipeline `Stopped` |
| `Close()` during a blocking .NET call / a grandchild holding stdout | **blocked 2356 ms / 2522 ms** (until the call/grandchild ended) |
| module `OnRemove` + `Register-EngineEvent PowerShell.Exiting`, then `Close()+Dispose()` | only **`Exiting`** fired; `OnRemove` fired only on explicit `Remove-Module` (docs pair them for this reason: [Remove-Module example 5][remove-module]; `Exiting` "only fired when the session is exited under the control of PowerShell", [Register-EngineEvent][register-engineevent]) |
| `Start-Process sleep 104`, `Start-ThreadJob {…}` then `Close()+Dispose()` | `sleep 104` survived; the thread job **finished after** the runspace was gone |

**Why.** `NativeCommandProcessor.StopProcessing()` calls `KillProcess(_nativeProcess)`, which is
`processToKill.Kill()`, i.e. **direct child only**. The exception is when `NativeCommandProcessor.IsServerSide`
(the remoting server mode) is set: then it kills the tree, children first ([source][ncp-source], lines
~1249–1400). It also only kills when the output is redirected, which is always the case in a hosted runspace.

**Mapping onto Cordis/rutis**

- **Call cancellation** (Rust future dropped → `cancel {id}`):
  - The runner calls `StopAsync()`/`BeginStop()` and never blocks its I/O loop.
  - Per-call cleanup belongs in the author's `clean{}` (7.3+) or `try/finally`.
  - If the pipeline hasn't stopped within a deadline, log `StopUnconfirmed`. Keep the plugin's queue blocked:
    the runspace is still busy and cannot be reused.
  - Report the late result as an orphan; the Rust side already discards and counts late replies.
  - Optional v1.1: an `Invoke-RutisNative` helper that starts native commands in their own process group,
    so a cancel can kill the whole group. Without a helper, grandchildren cannot be attributed to a call.
- **Plugin dispose** (the fiber's effects running LIFO). The runner receives `dispose` and works on the plugin's thread:
  1. Let queued/in-flight calls finish. Cordis allows calls during unload; the rutis side revokes the projected
     service first (design §6).
  2. Run `Register-RutisEffect` blocks **LIFO**, each in its own try/catch, with errors logged.
  3. Run `Get-Job` in the runspace, then `Stop-Job`/`Remove-Job -Force`.
  4. `Remove-Module` the plugin, which fires `OnRemove`.
  5. `Runspace.Close()` on a background task with a deadline; this fires `PowerShell.Exiting`.
  6. `Dispose()`.
- **Whole-mount dispose:** reverse load order, then the process exits. The existing Rust behaviour stays the
  backstop: dropping the mount SIGKILLs the process, and children of `pwsh` are not reaped, as requirements §7
  already states for Node.
- **No promise of forced termination.** `Close()` can block exactly like `Stop()`. Apps that need a deadline
  put a timeout on unload; this matches requirements §7.

---

## 7. Interface extraction for build-time codegen (Q6)

### 7.1 AST extraction with pwsh (measured, exp 06)

Build-time inputs, none of which run module code:
- `Import-PowerShellDataFile` on the `.psd1`. It evaluates restricted data only.
- `[System.Management.Automation.Language.Parser]::ParseFile` on the `RootModule`.

The extractor read:

- the manifest: `ModuleVersion`, `PowerShellVersion`, `CompatiblePSEditions`, `FunctionsToExport`,
  `PrivateData.Rutis`;
- exports: `Export-ModuleMember -Function …` call sites, intersected with `FunctionsToExport`. The non-exported
  `helper` was flagged `exported: false`;
- per function:
  - `[CmdletBinding()]` (`advanced`);
  - `[OutputType([DiskUsage])]` → `"DiskUsage"`;
  - comment-based help: `.SYNOPSIS` and the `.PARAMETER` texts via `FunctionDefinitionAst.GetHelpContent()`;
  - parameters with the declared type text (`string`, `Nullable[int]`, `System.IO.FileInfo`), the resolved
    `StaticType` (`System.Nullable\`1[System.Int32]`, untyped → `System.Object`), `Mandatory`,
    `ValueFromPipeline`, `ValidateSet` values (`Local`, `Network`), the default expression text (`'Local'`),
    and other attributes (`ValidateNotNullOrEmpty`);
  - whether a `clean{}` block exists;
- classes and enums:
  - `DiskUsage` with typed properties (`long`, `DiskKind`) and `hidden` flags;
  - `Counter` with typed methods (`[int] Increment([int] $by)`, `static [Counter] Create()`);
  - enum `DiskKind` with explicit values (`Removable = 7`).

The whole run took 269 ms including `pwsh` module loading. Sample output for `Get-DiskUsage`:
`{"name":"Get-DiskUsage","exported":true,"advanced":true,"outputType":["DiskUsage"],"synopsis":"Returns disk usage for a mount point.","parameters":[{"name":"Mount","declared":"string","staticType":"System.String","mandatory":true,...,"otherAttrs":["ValidateNotNullOrEmpty"]},{"name":"Kind",...,"validateSet":["Local","Network"],"default":"'Local'"},{"name":"Detailed","declared":"switch",...}]}`

The runtime alternative is `Import-Module` and then `Get-Command`. It gives resolved `ParameterType`/`OutputType`,
but **executes module code** and needs configuration, so it is not suitable for `build.rs`.

**Build-time requirement.** `build.rs` runs `pwsh -NoProfile -NonInteractive -File <runtime>/extract.ps1 <module-dir>`
and turns the JSON into Rust, exactly as `generate.mjs` does for TypeScript. This requires `pwsh` on the build
machine. That is consistent with requiring `node` at build time today, and `pwsh` is needed at runtime anyway.
Missing `pwsh` or a wrong runtime-module version must fail the build with an actionable message, as in README §5.
`cargo:rerun-if-changed` should list the module's files.

### 7.2 Non-pwsh parser?

- **tree-sitter-powershell** (Airbus CERT, MIT, [repo][ts-powershell]) has a Rust crate,
  `tree-sitter-powershell`; the latest is 0.26.4 and 0.25.10 was tested. It built and ran in the local Rust image. It parsed the sample in **0.64–0.74 ms** and
  exposed `function_statement`, `param_block`, `script_parameter`, `attribute`, `type_literal`,
  `class_statement`, `class_method_definition` and `enum_statement`.
- **But the tree had errors on valid code.** `function Get-Calls { [OutputType([int])] param() $script:State.Calls }`
  (param block and a statement on one line) came out as `ERROR`, and error recovery then swallowed the
  following `Export-ModuleMember` line (exp `ts-parse.out`).
- It also has no semantic layer: no type resolution, no `Import-PowerShellDataFile` semantics, and no help parsing.
- **Use it only** as an optional no-`pwsh` fallback, e.g. for a lint step; never as the source of truth.

### 7.3 Reliability of types and what authors must annotate

Many real scripts are untyped. `Restart-ServiceSafe`'s `$Name` gave `staticType = System.Object`, and
`[OutputType]` was missing, which yields `serde_json::Value` / `Vec<Value>` with a `cargo:warning`.
`[OutputType]` is advisory in PowerShell, so the runner must enforce it at run time (§5.3).

Type mapping proposal:

| PowerShell | Rust |
| --- | --- |
| `[string]`, `[char]` | `String` / `&str` in args |
| `[int]`/`[long]`/`[double]`/`[decimal]`/`[bool]` | `i32`/`i64`/`f64`/`f64 or String`/`bool` |
| `[switch]` | `bool` (false → parameter omitted) |
| `[Nullable[T]]` or non-mandatory param | `Option<T>`; **`None` → parameter omitted, so the PowerShell default applies** (omitted ≠ `$null`) |
| `[ValidateSet('a','b')][string]` | generated enum, serialized as the string |
| PowerShell `enum` | Rust enum, by name |
| PowerShell class with properties only | struct (`hidden` excluded) |
| PowerShell class with methods | v2: live object proxy (`ObjectRef`); v1: build warning, not bound |
| `[T[]]`, `[List[T]]` | `Vec<T>` |
| `[hashtable]`, `[ordered]`, `[pscustomobject]`, `[object]`, untyped | `serde_json::Value` (or `BTreeMap<String, Value>`) |
| `[datetime]`/`[datetimeoffset]`/`[timespan]` | `chrono` types or RFC 3339 `String` / `Duration` |
| `[guid]`, `[uri]`, `[version]`, `[System.IO.FileInfo]` | `String` (path for FileInfo — binder converts) |
| `[pscredential]`, `[securestring]` | v1: unsupported/warn (secret handling open question, §13) |
| `SupportsShouldProcess` | extra `what_if: bool` parameter (dry-run for automation) |

Not supported in v1 (build warning, member skipped, like TypeScript overloads):
- multiple parameter sets;
- `ValueFromRemainingArguments`;
- `[ref]`;
- `DynamicParam`;
- pipeline-only input. A `ValueFromPipeline` parameter is still passed by name, which works.

**What authors must annotate:**
- `[CmdletBinding()]`;
- a type on every parameter, plus `[Parameter(Mandatory)]`;
- `[OutputType([T])]` or `[OutputType([T[]])]`;
- PowerShell classes for structured inputs and outputs (DTOs), and enums or `ValidateSet` for closed sets;
- comment-based help, which becomes rustdoc.

These are standard PowerShell best practices (PSScriptAnalyzer rules), not rutis-specific ceremony.

---

## 8. Callbacks, events, live objects (Q7)

- **Calling Rust from PowerShell (host services and Rust callbacks)** — feasible and **prototyped** (exp 12):
  1. Plugin code calls a proxy (`Invoke-HostService`). The proxy enqueues a request for the runner and blocks
     the plugin's own thread on its inbox.
  2. The "Rust host" calls back into the same plugin before answering.
  3. The runner hands the nested `invoke` to the plugin's inbox. The waiting proxy runs it on the same thread
     (thread 17, runspace 2), seeing the outer call's state (`state=set-by-outer-call`), and returns the
     nested result.
  4. Only then does the outer result arrive: `hello world + suffix(from runspace 2, thread 17, state=set-by-outer-call)`.

  This is the PowerShell equivalent of the Node peer's `path`/`#waiting` logic.

  A Rust function reference passed *into* a PowerShell function can be represented the same way, as a proxy
  object with `.Invoke(...)` created inside the plugin runspace.

  Deadlock analogue of `SyncWaitCycle`: the Rust host awaits something that needs the busy plugin thread but
  is not in the call chain. Detect this from the path rule and fail with a clear error.
- **Handing PowerShell scriptblocks or objects to Rust (references)** — feasible but must obey §3.2:
  - The object stays in a per-plugin table.
  - A Rust `call`/`get` on it is **queued to the owning runspace's thread**, as a new pipeline when idle or a
    nested one in the active chain. The runner never calls it directly: `&` corrupts state, `.Invoke()` races
    after ~250 ms, and delegates crash on pool threads.
  - On plugin dispose, released references go first.
  - PowerShell classes would need `[NoRunspaceAffinity()]` (7.4+) or strict routing.
- **Events.**
  - *PowerShell → rutis* (notifications): `Send-RutisEvent name payload`, sent as an `invoke {target:'', method:'event'}`
    like Node's forwarder. Sources inside plugins should use `Register-ObjectEvent -Action`, whose actions run
    in the plugin runspace even when it is idle (exp 10), never raw .NET delegates (exp 14).
  - *rutis → PowerShell*: `Register-RutisEventHandler name { param($e) … }`; the runner queues the handler on
    the plugin thread.
- **Worth it in v1?** Host services with re-entrancy: **yes**, because `inject` is half of the Cordis model and
  the mechanism is simple. Notification events PowerShell → rutis: **yes**.

  Defer to v2:
  - live objects or scriptblocks as references;
  - Rust callbacks as function arguments;
  - rutis → PowerShell events.

  They need object tables, release counting and affinity routing, while typical automation is value-oriented.
  Unsupported members are reported at build time, as rutis-interop already does.

---

## 9. Precedents (Q8)

**Azure Functions PowerShell worker** ([repo][af-repo], [dev reference][af-docs]):
- **Architecture.** A C# executable on .NET 10 that references `Microsoft.PowerShell.SDK 7.6.6`
  (`Microsoft.Azure.Functions.PowerShellWorker.csproj`) and talks gRPC to the Functions host.
- **Concurrency.**
  - A `PowerShellManagerPool` of runspaces (`BlockingCollection`), grown lazily up to
    `PSWorkerInProcConcurrencyUpperBound`. The code default is 1 when the setting is unset; the docs say 1000
    in Functions 4.x ([issue #939][af-939]). Requests queue when the pool is exhausted.
  - `FUNCTIONS_WORKER_PROCESS_COUNT` gives process-level parallelism.
  - The docs warn that Azure PowerShell keeps *process-level* context, so in-proc concurrency can race.
- **Runspace setup.**
  - Runspaces come from `InitialSessionState.CreateDefault()` with `PSModulePath` set to the app's `Modules`
    folder and, on Windows, `ExecutionPolicy = Unrestricted`.
  - `profile.ps1` runs **once per runspace** when the runspace is created.
  - Function scripts are deployed as constant functions.
  - The worker module adds `Push-OutputBinding`/`Get-OutputBinding` cmdlets and type accelerators for the
    HTTP context types.
- **Invocation.** `AddCommand(entryPoint)` plus `AddParameter(name, value)` for each input binding. Pipeline
  output is logged through `Trace-PipelineObject` and returned as `$return` only for activity functions;
  outputs are explicit bindings.
- **Streams → log levels.** `DataAdding` handlers on Debug, Error, Information (including Write-Host), Progress,
  Verbose and Warning (`StreamHandler.cs`), mapped as documented: Error/Warning/Information/Debug/Trace.
- **After each invocation** `ResetRunspace` runs `Remove-Job -Force` for jobs started by the call and deletes
  new global variables. Module state survives.
- **Managed dependencies:** `requirements.psd1` resolved from the PowerShell Gallery with background upgrades;
  vendored `Modules/` is recommended for control.
- **Durable functions:** implemented as cmdlets (`Invoke-DurableActivity`, `Wait-DurableTask`, …) inside the
  worker module, with an orchestration replay model.
- **Lessons for rutis:**
  - pooling is incompatible with stateful plugins, which is why rutis should use one runspace per plugin;
  - forward streams live;
  - clean up jobs;
  - give plugin-facing APIs as cmdlets from a worker module;
  - pin modules locally.

**PowerShell Editor Services** ([repo][pses-repo]):
- **Loading.** A module that a stock `pwsh` or `powershell.exe` loads with `Start-EditorServices.ps1`, which
  then "takes over that process to rehost PowerShell within itself" ([NuGet guide][nuget-guide]).
- **Builds.** It ships `net461` and `netcoreapp`/`net` hosting assemblies plus a `netstandard2.0` core built
  against PowerShell Standard. It isolates its own dependencies with a custom `AssemblyLoadContext`
  (`PsesLoadContext.cs`), which also illustrates the shared-ALC problem.
- **Transports.** JSON-RPC (LSP/DAP) over stdio or named pipes. On Unix .NET named pipes are UDS files at
  `$TMPDIR/CoreFxPipe_<name>` (`NamedPipeUtils.cs`). On .NET Framework the pipe is ACL'd manually.
- **Threading.**
  - A dedicated "PSES Pipeline Execution Thread" (STA on Windows) consumes a `BlockingConcurrentDeque` of tasks.
  - The runspace uses `ThreadOptions = UseCurrentThread`.
  - Nested work goes through `PowerShell.Create(RunspaceMode.CurrentRunspace)`.
  - Cancellation contexts scope stops.
  - It runs artificial pipelines to process engine events (`PsesInternalHost.cs`).
- **Lesson:** this is the threading model rutis should copy for each plugin runspace.

**AWS Lambda PowerShell custom runtime** ([repo][aws-repo], [blog][aws-blog]):
- **Structure.**
  - A `bootstrap` file that is itself a PowerShell script (`#!/opt/powershell/pwsh -noprofile`).
  - It imports a runtime module and `Add-Type`s a small C# context class.
  - It loops over HTTP long-polling of the Lambda Runtime API in **one runspace**.
- **Handlers.** `script.ps1`, `script.ps1::Function` (dot-sourced at cold start) and `Module::Name::Function`
  (imported once).
- **Results.** The handler's pipeline output becomes the response via `ConvertTo-Json -Compress`, which uses
  default **depth 2** (`Invoke-FunctionHandler.ps1`): exactly the truncation pitfall of §5.
- **Isolation.** An `exit` in user code kills the runtime ([issue #1][aws-issue1]).
- **History.** The older .NET-based approach (AWSLambdaPSCore) compiled scripts into a C# Lambda that embeds
  the SDK; the blog cites the extra compile step as the reason for moving to native `pwsh`.
- **Lessons:**
  - a pure-PowerShell loop is viable for one-at-a-time invocation;
  - never use `ConvertTo-Json` defaults;
  - guard against `exit`, which a hosted runspace with the default host already does (exp 04 #9).

**.NET Interactive / Jupyter PowerShell kernel** ([PowerShellKernel.cs][dni-kernel]):
- A C# kernel referencing `Microsoft.PowerShell.SDK` (net10.0).
- Its runspace uses a custom `PSKernelHost` and `CreateDefault2()`.
- It **bundles PowerShellGet, ThreadJob, SecretManagement, Archive** into its own `Modules` folder.
- It shares variables through `SessionStateProxy`.
- Lesson: an SDK host must ship the module ecosystem itself, consistent with exp 08's `CreateDefault2` finding.

**PSRP** (MS-PSRP, protocol revision 21.0, 2024 — [spec][ms-psrp]):
- **Layers.** Fragmented messages (`SESSION_CAPABILITY` 0x00010002, `INIT_RUNSPACEPOOL` 0x00010004,
  `CREATE_PIPELINE` 0x00021006, pipeline output/error/state, host calls, …) carrying CLIXML objects.
- **Transports.** WinRM/WSMan, or the simpler **OutOfProc** XML packets (`<Data Stream=… PSGuid=…>base64</Data>`,
  `Command`/`Close`/`Signal` plus acks) over SSH (`pwsh -sshs`), process stdio (`pwsh -s`, used by `Start-Job`),
  and named pipes or UDS ([psrpcore transport][psrpcore-transport]). Python has full implementations
  (psrpcore, pypsrp); Rust has a young WinRM-based client (`psrp-rs` 2.0.2, Sept 2026).
- **Why not for rutis:**
  - it gives remote *execution* of commands, not a plugin lifecycle (no apply, effects, injects, events or
    service slots);
  - objects come back as **deserialized property bags** without methods (no live references);
  - host calls are UI-oriented;
  - implementing runspace-pool and pipeline state machines, fragmentation and CLIXML in Rust is a large
    project that adds nothing over our own runner speaking protocol v1.
- **Where it might matter later:** driving *remote* Windows machines from a rutis host. Even then the runner
  should use PowerShell remoting internally (`Invoke-Command`) rather than Rust speaking PSRP.

**CliXml / `-OutputFormat XML`:**
- `pwsh -o XML -c …` writes `#< CLIXML` followed by `<Objs><Obj S="Output">…` on stdout (verified).
- `ConvertTo-/ConvertFrom-CliXml` exist since 7.5 ([doc][cliXml-doc]); a round trip yields `Deserialized.*`
  type names (verified).
- It has type fidelity for primitives and collections but is depth-limited and has no live objects.
- Usable for option (c) (command mode), not as the wire format: rutis already has a JSON value model.

---

## 10. Transport (Q9)

- **pwsh as a UDS client works (exp 05).** Two variants both reached a .NET UDS listener standing in for
  Rust's `UnixListener`:
  - `Socket` with `UnixDomainSocketEndPoint` (.NET Core 2.1+ / netstandard2.1; not .NET Framework, [API][uds-api]);
  - `System.IO.Pipes.NamedPipeClientStream('.', '/abs/path/peer.sock')`. On Unix, .NET named pipes are UDS,
    and an absolute path is used as-is.
- **Script-level performance:** a JSON line request/response round trip, including `ConvertFrom-Json` of each
  request, cost **0.036 ms**.
- **Portable choice for the runner: `NamedPipeClientStream`.**
  - On Linux/macOS it connects to the Rust-created UDS path (keep `sun_path` ≤ 104 bytes on macOS, 108 on Linux).
  - On Windows the same code connects to a named pipe `\\.\pipe\rutis-<id>`, which the Rust side can create
    with `tokio::net::windows::named_pipe` ([tokio][tokio-pipes]).
  - It works on .NET Framework too, which keeps 5.1 possible.
  - AF_UNIX also exists on Windows 10 1803+ / Server 2019 ([AF_UNIX blog][afunix-blog], [Microsoft gRPC UDS doc][grpc-uds]).
    But Rust std has no Windows UDS (the PR adding it, [rust#147335][rust-147335], was closed) and tokio does
    not support it either, so named pipes are the pragmatic Windows route.
- **rutis-interop is Unix-only today.** `UnixListener`, `libc::waitid` and SIGKILL in `process.rs`. Windows
  needs a transport abstraction and process supervision (Job Objects for tree kill), which is independent of
  PowerShell.
- **Stdio framing is possible but discouraged.** `[Console]::WriteLine`, native commands that inherit stdout,
  and console-host `Write-Host` (exp 07/09) all write to the process stdout and would corrupt frames. PSES
  supports stdio only because it controls its host. Keep the separate socket, as rutis-interop already does
  for Node.

---

## 11. Proposal: what a PowerShell plugin is (Q10)

### 11.1 Cordis concepts → PowerShell

| Cordis / rutis-interop | PowerShell plugin |
| --- | --- |
| plugin (assembly unit, one `apply`) | a **script module** directory (`Name.psd1` + `Name.psm1`) |
| `apply(ctx, config)` | **Apply function** named in the manifest (`PrivateData.Rutis.Apply`), called once after `Import-Module`, config passed as **named parameters** via `AddParameter` |
| `Config` type | the Apply function's `param()` block → Rust `Config` (Mandatory → required; others `Option`, `None` = omitted → PowerShell default) |
| service provided (`ctx.provide` / Service class) | the module's exported advanced functions → one rutis service named `PrivateData.Rutis.Service` (methods: `Get-DiskUsage` → `get_disk_usage`, all `async`) |
| service state / plugin state | module `$script:` scope in the plugin's **own runspace** |
| `inject: [...]` (host services) | `PrivateData.Rutis.Inject = @{ notifier = 'Notifier' }` naming a **contract class** declared in the module (typed method signatures; bodies throw) → generated `NotifierHost` trait; `Get-RutisService notifier` returns the proxy; mount waits until rutis provides it (as today) |
| `ctx.effect` / disposal | `Register-RutisEffect { … }` (LIFO) → then `Remove-Module` (`OnRemove`) → `Runspace.Close` (`PowerShell.Exiting`) |
| events (notifications) | `Send-RutisEvent 'sysinfo/disk-low' $payload`; payload type from `PrivateData.Rutis.Events = @{ 'sysinfo/disk-low' = 'DiskLow' }` (a DTO class) |
| `AbortSignal` / cancel | `Stop()` → `PipelineStoppedException`; author cleanup in `clean{}`/`finally` |
| fiber | runspace + dedicated pipeline thread + FIFO call queue |
| group mount | one `pwsh` process; one runspace per member; cross-member `Inject` resolved by the runner (routed to the provider's thread) |

**Why not the module's own `param()` block for config?** Verified in exp 11:
- `param([CfgModConfig] $Config)` fails with "Unable to find type [CfgModConfig]": a class from the same file
  is not yet defined when the module's param block binds.
- `Import-Module -ArgumentList` is positional and can't skip optional parameters. Passing `$null` for
  `[int]$B = 5` bound **0**, not the default.

**Why functions rather than class methods for service methods?**
- Functions support `[Parameter(Mandatory)]`, validation attributes, defaults, `ShouldProcess`, comment help
  and `clean{}`.
- Class methods allow no parameter attributes or defaults (every parameter is mandatory), carry runspace
  affinity, can't be reloaded, and aren't exported by `Import-Module` ([about_Classes][about-classes]).
- Classes are the right tool for **DTOs** and **contracts**, and later for live objects.

### 11.2 Example

`pwsh/SysInfo/SysInfo.psd1`
```powershell
@{
    RootModule           = 'SysInfo.psm1'
    ModuleVersion        = '0.1.0'
    GUID                 = '6d1c5b0e-3c38-4a8a-9a43-1f2f3c9d2a10'
    PowerShellVersion    = '7.4'
    CompatiblePSEditions = @('Core')
    FunctionsToExport    = @('Initialize-SysInfo', 'Get-DiskUsage', 'Clear-TempFiles')
    RequiredModules      = @(@{ ModuleName = 'RutisInterop'; ModuleVersion = '0.1.0' })
    PrivateData          = @{
        Rutis = @{
            Service = 'sysinfo'
            Apply   = 'Initialize-SysInfo'
            Inject  = @{ notifier = 'Notifier' }
            Events  = @{ 'sysinfo/disk-low' = 'DiskLow' }
        }
    }
}
```

`pwsh/SysInfo/SysInfo.psm1`
```powershell
# Contract of a service the rutis host provides (bodies never run; the runner injects a proxy).
class Notifier {
    [void] Notify([string] $Title, [string] $Body) { throw 'provided by the rutis host' }
}

class DiskUsage { [string] $Mount; [long] $UsedBytes; [long] $FreeBytes }   # DTO -> Rust struct
class DiskLow   { [string] $Mount; [double] $FreeRatio }                     # event payload

function Initialize-SysInfo {            # Cordis apply; its parameters are the Config
    [CmdletBinding()]
    param(
        [ValidateRange(0.0, 1.0)][double] $Threshold = 0.1,
        [ValidateRange(5, 3600)][int] $PollSeconds = 60
    )
    $script:Threshold = $Threshold
    $script:Timer = [System.Timers.Timer]::new($PollSeconds * 1000)
    # Event actions run in this plugin's runspace (never use raw .NET delegates for callbacks).
    $null = Register-ObjectEvent $script:Timer Elapsed -SourceIdentifier sysinfo.poll -Action { Test-DiskLow }
    $script:Timer.Start()
    Register-RutisEffect { Unregister-Event sysinfo.poll; $script:Timer.Dispose() }   # LIFO on dispose
}

<#
.SYNOPSIS
Disk usage of a mount point.
#>
function Get-DiskUsage {
    [CmdletBinding()]
    [OutputType([DiskUsage])]
    param([Parameter(Mandatory)][ValidateNotNullOrEmpty()][string] $Mount)
    $d = [System.IO.DriveInfo]::new($Mount)
    [DiskUsage]@{ Mount = $Mount; UsedBytes = $d.TotalSize - $d.AvailableFreeSpace; FreeBytes = $d.AvailableFreeSpace }
}

function Clear-TempFiles {
    [CmdletBinding(SupportsShouldProcess)]
    [OutputType([int])]
    param([Parameter(Mandatory)][string] $Path, [timespan] $OlderThan = '7.00:00:00')
    $n = 0
    foreach ($f in Get-ChildItem -LiteralPath $Path -File | Where-Object LastWriteTime -lt ((Get-Date) - $OlderThan)) {
        if ($PSCmdlet.ShouldProcess($f.FullName, 'Remove')) { Remove-Item -LiteralPath $f.FullName; $n++ }
    }
    (Get-RutisService notifier).Notify('Temp cleanup', "$n files removed from $Path")   # host service call
    $n
}

function Test-DiskLow {                  # internal (not exported)
    $u = Get-DiskUsage -Mount '/'
    $ratio = $u.FreeBytes / ($u.FreeBytes + $u.UsedBytes)
    if ($ratio -lt $script:Threshold) { Send-RutisEvent 'sysinfo/disk-low' ([DiskLow]@{ Mount = '/'; FreeRatio = $ratio }) }
}
```

`Cargo.toml` (same metadata style as today):
```toml
[package.metadata.rutis-interop.mounts.sysinfo]
powershell = "pwsh/SysInfo"     # module directory; or { name, version } resolved from a Save-PSResource'd Modules dir
version = "0.1.0"               # must equal ModuleVersion
provide = ["notifier"]          # rutis services the plugin injects
events  = ["sysinfo/disk-low"]  # PowerShell -> rutis notifications
```

Generated Rust (sketch):
```rust
pub mod sysinfo {
    #[derive(Default)] pub struct Config { pub threshold: Option<f64>, pub poll_seconds: Option<i32> }
    pub struct DiskUsage { pub mount: String, pub used_bytes: i64, pub free_bytes: i64 }
    pub struct SysInfo { /* proxy over the PowerShell service handle */ }
    impl SysInfo {
        pub async fn get_disk_usage(&self, mount: &str) -> Result<DiskUsage, rutis_interop::Error>;
        pub async fn clear_temp_files(&self, path: &str, older_than: Option<std::time::Duration>,
                                      what_if: bool) -> Result<i32, rutis_interop::Error>;
    }
    pub trait NotifierHost: Send + Sync {
        fn notify(&self, title: String, body: String) -> BoxFuture<'static, Result<(), rutis_interop::Error>>;
    }
    pub fn provide_notifier(ctx: &rutis::Ctx, host: impl NotifierHost + 'static) /* -> registration */;
    pub struct SysinfoDiskLow { pub mount: String, pub free_ratio: f64 }   // impl rutis::Event
    pub struct Plugin { /* mount plugin; injects dyn NotifierHost */ }
}
```

### 11.3 What the runner must do

1. **Start.** Rust spawns
   `pwsh -NoLogo -NoProfile -NonInteractive -File <runtime>/runner.ps1 <socket>`; on Windows add
   `-ExecutionPolicy Bypass`.
   - Environment: `PSModulePath` gets the mount's `Modules` dirs prepended.
   - The runner connects (`NamedPipeClientStream` or UDS), sends `hello`, and loads its C# core
     (cached `Add-Type` or prebuilt DLL).
2. **mount.** For each plugin, in order:
   1. Build an `InitialSessionState`: `CreateDefault2()` in `pwsh`, plus the preferences from §4.3 and the
      plugin-facing `RutisInterop` functions.
   2. Create a runspace **without the console host**, with its own pipeline thread (`ReuseThread`, or a
      dedicated thread with `UseCurrentThread` as in PSES). Bind the plugin context.
   3. `Import-Module` the plugin, checking `PowerShellVersion` and editions.
   4. Run `Apply` through `AddCommand`/`AddParameter`. A failure here fails the mount.
   5. Register services and report handles, as Node's `mount` does.
3. **Calls.**
   - Per-plugin FIFO queue.
   - Build `AddCommand(<exported name>)` plus one `AddParameter` per named argument; switches only when true.
   - Run on the plugin thread and collect the output collection.
   - Project and validate by `[OutputType]` (§5.3), walk values (§5.2), and map errors (§4.3).
   - Forward streams live as log records tagged with the call id.
4. **Re-entrancy.** Track the call-chain `path` per plugin thread. A nested `invoke` whose `path` contains the
   call that thread is waiting on runs there, through the host-proxy pump (exp 12). Unrelated calls queue.
5. **Host services.** Inject proxies built **inside** the plugin runspace. Proxy calls send `invoke {target: 'host:<name>'}`
   with the current path and block the plugin thread while pumping.
6. **Events.** `Send-RutisEvent` sends `invoke {method:'event'}`, fire-and-forget like Cordis `emit` across
   the boundary (boundary rule 2).
7. **Cancellation:** §6. Never block the I/O loop; use `StopAsync`, a deadline, `StopUnconfirmed`, and orphans.
8. **Dispose:** §6, in order: effects LIFO → jobs → `Remove-Module` → `Close` (with deadline) → `Dispose`.
9. **Never run plugin script on the runner thread or on .NET callback threads** (§3.2).
10. **Logging.** Runner diagnostics go to stderr or log frames, never to frames by accident.

**Implementation choice inside (a′).** A *pure* `runner.ps1` is feasible:
- `BlockingCollection` inboxes plus BeginInvoke on per-plugin runspaces gave a working re-entrant host-call
  round trip (exp 12);
- UDS works from script;
- per-call overhead is tiny.

The production core should still be C#. Concurrency, cancellation deadlines, reference tables and the value
walker are much easier to make correct and test there, which is what PSES and Azure Functions concluded.
An IL-only DLL (or source compiled by `Add-Type`, 616 ms once and then cacheable) keeps one artifact for all
OSes. Plugin-facing commands (`Register-RutisEffect`, `Get-RutisService`, `Send-RutisEvent`) remain PowerShell
functions in the runtime module, as Azure Functions does with `Push-OutputBinding`.

---

## 12. The 2026-09-26 roadmap's PowerShell section, checked

| Roadmap claim | Verdict |
| --- | --- |
| One dedicated runspace per plugin activation in one runner process; never route stateful calls to an arbitrary pool runspace | **Confirmed and refined.** Cheap (~2 ms open, ~5 ms import+call, ~1.1 MB each). Calls to one plugin are **serialized** (one pipeline per runspace). Pools only fit stateless code (Azure Functions resets globals because of this). |
| Runspaces separate variables/modules, but assemblies, process environment and some static state are shared; not process isolation | **Confirmed** (exp 02): env, process cwd, statics, `Add-Type` types shared; plus `$PWD` vs process cwd divergence for .NET APIs |
| Split incompatible modules / runtime versions into separate runtime groups | **Confirmed** ("PowerShell always loads assemblies into the same context", [doc][dep-conflicts]) |
| Use parameter binding for explicit commands, never script text carrying user data | **Confirmed** (exp 07 #1 vs #9 injection) |
| Runner handles output, error, warning, information and native output separately; protocol connection independent | **Confirmed, with a correction:** native **stdout lands in the Output stream** (i.e. in the result) and stderr in the Error stream; `Write-Host` is Information (PSHOST). Strict `[OutputType]` validation is needed. |
| Value returns are projected to DTOs per contract; declared .NET objects stay in their runspace and calls return to the right runspace | **Confirmed, and it is mandatory**: cross-runspace `&`, `.Invoke()`, class methods and delegates are all unsafe (exp 02b, 14) |
| Test null, empty and single-item arrays, deep objects; ConvertTo-Json's depth is no serialization guarantee | **Confirmed** (exp 03); recommend not using ConvertTo-Json at all |
| After stopping a pipeline, confirm associated jobs and child processes finished; otherwise StopUnconfirmed; Runspace.Dispose is no kill guarantee | **Confirmed and sharpened:** Stop kills only the direct child; grandchildren survive and can **block Stop/Close** for their whole lifetime; thread jobs and `Start-Process` survive Close/Dispose |
| PowerShell 7 first; publish per platform as each passes | **Confirmed** (7.4+ on Linux/macOS first; Windows needs a transport) |
| E05: two runspaces, module conflicts, continuous object calls and release | **Partly confirmed.** "Module conflict" can't be solved by runspaces (shared ALC), only by separate mounts |
| **Missing from the roadmap:** | thread affinity of scriptblocks and classes; process crash via thread-pool delegates; Stop/Close blocking; `OnRemove` not firing on Close; console host `exit`; `CreateDefault2` vs SDK hosts; config can't use the module param block; `[OutputType]` is advisory; build-time AST extraction; async-only method shape |

---

## 13. Risks and open questions

1. **`pwsh` as a runtime dependency.** It must be installed and supported (7.4 support ends 2026-11-10; 7.6
   LTS runs to 2028-11-14). Behaviour differs between 7.4, 7.5 and 7.6 (e.g. `-DateKind` 7.5+, BigInteger 7.5+).
   Pin a minimum and test a matrix. Build machines need `pwsh` too.
2. **Shared process state** across plugins in one mount: env, cwd, statics, assemblies, culture, the console.
   Assembly conflicts are order-dependent and lazy, appearing only when a code path runs
   ([doc][dep-conflicts]). Guidance: one mount per independent plugin unless they must share.
3. **A plugin can kill or hang the mount.** Causes include `[Environment]::Exit`, unhandled exceptions on
   thread-pool callbacks (exp 14: crash then 99 % CPU hang), stack overflow, or a console-host `exit` if
   misconfigured. Requirements §5 rule 8's wording ("uncaught exceptions end the whole process") applies
   unchanged. The Rust side already reports `Transport` errors.
4. **Cancellation can't be guaranteed.** Stuck .NET calls and native grandchildren block `Stop()`/`Close()`
   (exp 04/04b/15). This needs deadlines, `StopUnconfirmed`, and process kill as the backstop. Is a
   process-group helper for native commands worth adding in v1?
5. **Serialized calls per plugin.** A long call blocks later calls to the same plugin. Should a manifest flag
   allow N activations (a pool) for stateless plugins?
6. **Weak typing.** Untyped scripts give `Value`. Strict `[OutputType]` enforcement may surprise authors; it
   needs very good error messages and a lint mode (PSScriptAnalyzer rules).
7. **Reload semantics.** PowerShell classes and binary-module assemblies can't be unloaded; re-import in a new
   runspace leaks types, and memory isn't returned after runspace disposal (WS 191 → 258 MB). For rutis's
   dependency-driven reload: reload the plugin in a **new runspace** when it has no classes or binary modules,
   otherwise **restart the mount process**. Decide whether the loader can express "restart process".
8. **Windows.**
   - Named-pipe transport and process supervision on the Rust side.
   - Execution policy (`-ExecutionPolicy Bypass`; Azure Functions sets `Unrestricted` in the ISS).
   - STA requirements for COM/GUI automation (runspace `ApartmentState`).
   - 5.1-only modules (WinCompat).
   - Signed-module revocation-check latency offline.
9. **Secrets.** `[pscredential]`/`[securestring]` parameters and `Get-Credential`. Values cross a same-user
   socket in plaintext, and verbose/debug streams may log them. A redaction policy is needed.
10. **Streaming and large outputs.** v1 collects the entire output collection. Long-running pipelines that
    emit progressively need protocol streams, already on rutis-interop's "unsupported" list.
11. **Culture.** The PowerShell binder converts using the invariant culture, but string formatting inside
    plugins uses the current culture. Should the runner pin `CurrentCulture`? It is per thread, so the runner
    can set it on each plugin thread.
12. **Parameter sets, pipeline input, dynamic parameters** are unsupported in v1. Is "one Rust method per
    parameter set" worth generating later?
13. **Security model** is unchanged from Node: same user and same permissions, not a sandbox.
    ConstrainedLanguage/JEA-style restrictions in the InitialSessionState are not a security boundary
    in-process; that needs WDAC/AppLocker.
14. **The SDK host (b) is a fallback** for "no pwsh on target". It would need its own module bundle
    (Utility/Management manifests, ThreadJob) and a Start-Job alternative. Not recommended unless required.

---

## Appendix A — Experiments (commands and key outputs)

Environment:
- Docker Desktop 28.1.1 (Apple Silicon, aarch64 VM, 16 vCPU).
- `mcr.microsoft.com/powershell:latest` has **no linux/arm64 manifest** (only linux/amd64, linux/arm, windows/amd64),
  and its pull hung, so it was aborted.
- Used instead: **`mcr.microsoft.com/dotnet/sdk:10.0`**. It is Ubuntu 24.04.5 with PowerShell **7.6.6**
  installed as a .NET tool at `/usr/share/powershell`, and .NET SDK 10.0.401. Pull time was 4 min 14 s.
  The PowerShell lifecycle page points to the .NET SDK images as containing the latest PowerShell ([lifecycle][lifecycle]).
- tree-sitter test: local `rust:1.98.1-bookworm` image.

Working directory: `scratchpad/experiments/powershell` (mounted at `/x`). Each `*.ps1`/`*.sh` has a matching `*.out`.

| # | Command | Key output |
| --- | --- | --- |
| 01 | `docker run --rm -v "$PWD":/x mcr.microsoft.com/dotnet/sdk:10.0 bash /x/01-startup.sh` | `pwsh -NoProfile -NonInteractive -c 1: avg 166 ms`; `…ConvertTo-Json: avg 277 ms`; `Import-Module SysInfo + call: avg 330 ms`; idle `WorkingSet=113.1 MB … GCHeap=2.4 MB Assemblies=70`; `60M /usr/share/powershell` |
| 02 | `… pwsh -NoProfile -NonInteractive -File /x/02-runspaces.ps1` | `baseline WS: 122 MB`; `runspace 1: open 3.8 ms, import module + first call 77.9 ms`; `runspace 2..10: open 1.6–2.1 ms, import+call 4.5–5.3 ms`; `after 50 runspaces …: WS=190.8 MB`; `B: $x=[] f=[<none>] PWD=[/] env=[set by A] [Environment]::CurrentDirectory=[/etc] static=[42] Security module loaded=[False]`; `cmdlet path exists: True at /tmp/rel.txt; .NET File.Exists(rel.txt): False`; `A Get-Calls=3 B Get-Calls=1`; thread ids sync/default `33,37,42`, ReuseThread `43,43,43`, UseNewThread `42,44,45`; `& $sb in runner: runspace=1 thread=14 x=A`; `$sb.Invoke(): runspace=2 thread=47`; `delegate on thread pool: FAILED: There is no Runspace available…`; `after disposing 50 runspaces: WS=258.4 MB` |
| 02b | `… -File /x/02b-affinity.ps1` | `3) $sb.Invoke() while A busy: 'runspace=2 thread=14 x=A' after 259 ms`; `4) & $sb while A busy: 'runspace=1 thread=14 x=A' after 1 ms`; `5) outer thread 19; nested: inner sees x=A on thread 19`; `6) class method … idle: runspace 2 thread 21`; `6b) … busy: runspace 2 thread 14 after 256 ms` |
| 03 | `… -File /x/03-json.ps1` (+ `03b-json-stj.ps1`) | see table §5.1 (verbatim lines in `03-json.out`, e.g. `WARNING: Resulting JSON is truncated as serialization has exceeded the set depth of 2. ‖ {"l1":{"l2":{"l3":"System.Collections.Hashtable"}}}`, `@() \| ConvertTo-Json => <no output>`, `double NaN / Infinity => ["NaN","Infinity"]`, `STJ on PSCustomObject => throws JsonException: A possible object cycle was detected…`) |
| 04 | `… -File /x/04-stop.ps1` | `1) … Stop() took 14 ms … survivors: []`; `2) … Stop() took 102006 ms`; `3) … Stop() took 3504 ms`; `4) … 1 ms`; `5) markers: begin,finally,clean`; `6) Close() took 4 ms`; `7) markers after Close/Dispose: [Exiting]`, `after explicit Remove-Module: [OnRemove]`; `8) 3 s after close: [59:/usr/bin/sleep 104] markers=[threadjob-finished]`; `9) output=[before] … state=Completed runner still alive` |
| 04b | `… -File /x/04b-stop-tree.ps1` | `500 ms after StopAsync: … invocation=Stopping procs=[29(ppid 1):sleep 8; 30(ppid 1):sleep 9]`; `StopAsync completed after 8017 ms`; `B) … Stop() took 8205 ms`; `C) … Stop() took 7211 ms; procs after: [41(ppid 1):sleep 9]` |
| 04c | `… -File /x/04c-startjob.ps1` | `job state: Running` with no child process (dotnet-tool image; not investigated) |
| 05 | `… -File /x/05-uds.ps1` | `1) Socket+UnixDomainSocketEndPoint: server received {"op":"hello","version":1}`; `2) NamedPipeClientStream('.', '/tmp/rutis-mount-test/peer.sock'): server received …`; `3) 2000 request/response pairs … 0.036 ms per round trip` |
| 06 | `… -File /x/06-ast.ps1` | JSON of manifest, module params, functions (types, Mandatory, ValidateSet, defaults, OutputType, help), classes, enums; `elapsedMs: 269`; runtime view `Set-Thing: OutputType=[System.Void] params: Spec:Hashtable, Ids:Int32[], MaybeInt:Nullable\`1, …` |
| 07 | `… -File /x/07-streams.ps1` | `Output: [String] Name=<x"; Remove-Item -Recurse / ; $(whoami) \`n> Count=7(Int32) … When=2024-01-02T03:04:05.0000000+00:00 … \| [String] native-stdout \| [String] LASTEXITCODE=3 $?=False \| [PSCustomObject] @{answer=42}`; `Error: WriteErrorException … \| RemoteException: native-stderr (FQID=NativeCommandError)`; `Information: an information message {tags: } \| Write-Host text {tags: PSHOST}`; binding exceptions; `ActionPreferenceStopException: … Program "sh" ended with non-zero exit code: 4`; `9) output: x \| INJECTED`; `warnings visible 150 ms into the call: 1 (live)` |
| 08 | `… bash /x/08-sdkhost.sh` (C# console app, `Microsoft.PowerShell.SDK` 7.6.6, net10.0) | `141M /tmp/out-sc` (530 files, `50M` tgz), `57M /tmp/out-fd`; `open=135ms first-call-done=250ms … total=~280ms ws=96–110MB`; with `CreateDefault2`: `The term 'ConvertTo-Json' is not recognized…`; with `CreateDefault`: works; `Start-Job failed: The pwsh executable cannot be found … not supported by design … ThreadJob … recommended` |
| 08b | `… bash /x/08b-sdkhost-r2r.sh` | `193M /tmp/out-r2r`, `68M` tgz; `SDK host self-contained (IL) … avg wall 307 ms`; `ReadyToRun … avg wall 126 ms`; `pwsh -NoProfile -c '…ConvertTo-Json; (gci /).Count' avg wall 300 ms`; `open=54ms first-call-done=100ms total=113ms ws=92MB` |
| 09 | `… -File /x/09-host-exit.ps1; echo exit code` | `plain runspace: host name=Default Host`; console-host runspace printed Write-Host to stdout; after `exit 9`: `runner still alive …`, **process exit code: 9** |
| 10 | `… -File /x/10-events.ps1` | `after 1 s idle: action ran 10 times`; `after Wait-Event -Timeout 1: 31` |
| 11 | `… -File /x/11-config.ps1` | `class-typed module config FAILED: … Unable to find type [CfgModConfig].`; `positional args with $null for B: A=a B=0 C=c` |
| 12 | `… -File /x/12-reentrant.ps1` | `result: hello world + suffix(from runspace 2, thread 17, state=set-by-outer-call) \| outer ran on runspace 2, thread 17`; `runner thread: 14` |
| 13 | `… -File /x/13-call-overhead.ps1` | `sync Invoke … 0.063 ms/call`; `BeginInvoke/EndInvoke … 0.067 ms/call`; `+ ConvertTo-Json … 0.211 ms/call` |
| 14 | `… -File /x/14-threadpool-crash.ps1` | `An error has occurred that was not properly handled… The PowerShell process will exit. Unhandled exception. System.Management.Automation.PSInvalidOperationException: There is no Runspace available…`; process then spun at ~99 % CPU; killed (`exit code: 137`) |
| 15 | `… -File /x/15-close-blocking.ps1` | `blocked in [Threading.Thread]::Sleep(3000): Close() 2356 ms`; `native grandchild …: Close() 2522 ms`; `Start-Sleep 30: Close() 1 ms` |
| 16 | `… bash -c 'pwsh … -File /x/16-addtype.ps1; …'` | `Add-Type compile (+write DLL): 616 ms`; `second Add-Type in the same process: 13 ms`; `pwsh start + Add-Type -Path prebuilt DLL: 333 ms` |
| ts | `docker run --rm -v "$PWD":/x -w /x/ts-parse rust:1.98.1-bookworm bash -c 'cargo run --release -q -- /x/SysInfo/SysInfo.psm1'` (tree-sitter 0.25 + tree-sitter-powershell 0.25.10) | `root=program has_error=true parse_time=741.583µs` (second run 635 µs); `!! ERROR at line 68: "[OutputType([int])] param()"`; `!! ERROR at line 76: "Export-ModuleMember …"` |
| cli | `docker run --rm mcr.microsoft.com/dotnet/sdk:10.0 pwsh -NoProfile -c '…ConvertTo-CliXml…'` and `pwsh -NoProfile -OutputFormat XML -c "[pscustomobject]@{a=1; b=@(1,2)}"` | `ConvertTo-CliXml in Microsoft.PowerShell.Utility`; `roundtrip type names: Deserialized.System.Management.Automation.PSCustomObject, …`; `#< CLIXML <Objs Version="1.1.0.1" …><Obj S="Output" …>` |

Sample module used throughout: `SysInfo/SysInfo.psd1`, `SysInfo/SysInfo.psm1`. The latter has a module
param block, an enum, a DTO class, a class with methods, typed, untyped and attribute-rich functions, comment
help, `OnRemove`, and `Export-ModuleMember`.

---

## Sources

- [optimizing-profile]: https://devblogs.microsoft.com/powershell/optimizing-your-profile/
- [ps-release]: https://github.com/PowerShell/PowerShell/releases/tag/v7.6.6 (asset sizes via GitHub API)
- [ps-18090]: https://github.com/PowerShell/PowerShell/issues/18090
- [nuget-guide]: https://learn.microsoft.com/en-us/powershell/scripting/dev-cross-plat/choosing-the-right-nuget-package
- [sdk-nuget]: https://www.nuget.org/packages/Microsoft.PowerShell.SDK/
- [lifecycle]: https://learn.microsoft.com/en-us/powershell/scripting/install/powershell-support-lifecycle
- [diff-winps]: https://learn.microsoft.com/en-us/powershell/scripting/whats-new/differences-from-windows-powershell
- [dep-conflicts]: https://learn.microsoft.com/en-us/powershell/scripting/dev-cross-plat/resolving-dependency-conflicts
- [about-classes]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_classes
- [threadjob]: https://learn.microsoft.com/en-us/powershell/module/threadjob/start-threadjob
- Creating multiple runspaces: https://learn.microsoft.com/en-us/powershell/scripting/developer/hosting/creating-multiple-runspaces
- [pref-vars]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_preference_variables
- [advanced-methods]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_functions_advanced_methods
- [ps-stop]: https://learn.microsoft.com/en-us/dotnet/api/system.management.automation.powershell.stop
- [remove-module]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/remove-module
- [register-engineevent]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/register-engineevent
- about_Pwsh (CLI, `-OutputFormat XML`, exit codes): https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_pwsh
- [ctj-76]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertto-json?view=powershell-7.6
- [ctj-51]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertto-json?view=powershell-5.1
- ConvertFrom-Json 7.6: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertfrom-json?view=powershell-7.6
- [cfj-51]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertfrom-json?view=powershell-5.1
- [cliXml-doc]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertfrom-clixml?view=powershell-7.5
- [ncp-source]: https://github.com/PowerShell/PowerShell/blob/master/src/System.Management.Automation/engine/NativeCommandProcessor.cs
- [af-repo]: https://github.com/Azure/azure-functions-powershell-worker (files: `src/PowerShell/PowerShellManager.cs`, `PowerShellManagerPool.cs`, `StreamHandler.cs`, `src/Utility/Utils.cs`, `src/worker.config.json`, `src/Microsoft.Azure.Functions.PowerShellWorker.csproj`)
- [af-docs]: https://learn.microsoft.com/en-us/azure/azure-functions/functions-reference-powershell
- [af-939]: https://github.com/Azure/azure-functions-powershell-worker/issues/939
- [pses-repo]: https://github.com/PowerShell/PowerShellEditorServices (files: `src/PowerShellEditorServices/Services/PowerShell/Host/PsesInternalHost.cs`, `src/PowerShellEditorServices.Hosting/Internal/PsesLoadContext.cs`, `…/Internal/NamedPipeUtils.cs`, `…/Configuration/TransportConfig.cs`)
- [aws-repo]: https://github.com/awslabs/aws-lambda-powershell-runtime (files: `powershell-runtime/source/bootstrap`, `…/modules/Private/Invoke-FunctionHandler.ps1`)
- [aws-blog]: https://aws.amazon.com/blogs/compute/introducing-the-powershell-custom-runtime-for-aws-lambda
- [aws-issue1]: https://github.com/awslabs/aws-lambda-powershell-runtime/issues/1
- [dni-kernel]: https://github.com/dotnet/interactive/blob/main/src/Microsoft.DotNet.Interactive.PowerShell/PowerShellKernel.cs
- [ms-psrp]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-psrp/602ee78e-9a19-45ad-90fa-bb132b7cecec ; message types e.g. SESSION_CAPABILITY https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-psrp/2f41abfb-7e30-4fb1-b286-527e9d67ad30
- [psrpcore-transport]: https://psrpcore.readthedocs.io/en/latest/transport.html ; psrpcore https://psrpcore.readthedocs.io ; psrp-rs https://docs.rs/psrp-rs
- [uds-api]: https://learn.microsoft.com/en-us/dotnet/api/system.net.sockets.unixdomainsocketendpoint
- [afunix-blog]: https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/
- [grpc-uds]: https://learn.microsoft.com/en-us/aspnet/core/grpc/interprocess-uds
- [tokio-pipes]: https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/index.html
- [rust-147335]: https://github.com/rust-lang/rust/pull/147335
- [ts-powershell]: https://github.com/airbus-cert/tree-sitter-powershell ; crate https://crates.io/crates/tree-sitter-powershell
- [petri-json]: https://petri.com/how-to-use-powershell-7-to-work-with-json-files/

[optimizing-profile]: https://devblogs.microsoft.com/powershell/optimizing-your-profile/
[ps-release]: https://github.com/PowerShell/PowerShell/releases/tag/v7.6.6
[ps-18090]: https://github.com/PowerShell/PowerShell/issues/18090
[nuget-guide]: https://learn.microsoft.com/en-us/powershell/scripting/dev-cross-plat/choosing-the-right-nuget-package
[sdk-nuget]: https://www.nuget.org/packages/Microsoft.PowerShell.SDK/
[lifecycle]: https://learn.microsoft.com/en-us/powershell/scripting/install/powershell-support-lifecycle
[diff-winps]: https://learn.microsoft.com/en-us/powershell/scripting/whats-new/differences-from-windows-powershell
[dep-conflicts]: https://learn.microsoft.com/en-us/powershell/scripting/dev-cross-plat/resolving-dependency-conflicts
[about-classes]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_classes
[threadjob]: https://learn.microsoft.com/en-us/powershell/module/threadjob/start-threadjob
[pref-vars]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_preference_variables
[advanced-methods]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_functions_advanced_methods
[ps-stop]: https://learn.microsoft.com/en-us/dotnet/api/system.management.automation.powershell.stop
[remove-module]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/remove-module
[register-engineevent]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/register-engineevent
[ctj-76]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertto-json?view=powershell-7.6
[ctj-51]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertto-json?view=powershell-5.1
[cfj-51]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertfrom-json?view=powershell-5.1
[cliXml-doc]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/convertfrom-clixml?view=powershell-7.5
[ncp-source]: https://github.com/PowerShell/PowerShell/blob/master/src/System.Management.Automation/engine/NativeCommandProcessor.cs
[af-repo]: https://github.com/Azure/azure-functions-powershell-worker
[af-docs]: https://learn.microsoft.com/en-us/azure/azure-functions/functions-reference-powershell
[af-939]: https://github.com/Azure/azure-functions-powershell-worker/issues/939
[pses-repo]: https://github.com/PowerShell/PowerShellEditorServices
[aws-repo]: https://github.com/awslabs/aws-lambda-powershell-runtime
[aws-blog]: https://aws.amazon.com/blogs/compute/introducing-the-powershell-custom-runtime-for-aws-lambda
[aws-issue1]: https://github.com/awslabs/aws-lambda-powershell-runtime/issues/1
[dni-kernel]: https://github.com/dotnet/interactive/blob/main/src/Microsoft.DotNet.Interactive.PowerShell/PowerShellKernel.cs
[ms-psrp]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-psrp/602ee78e-9a19-45ad-90fa-bb132b7cecec
[psrpcore-transport]: https://psrpcore.readthedocs.io/en/latest/transport.html
[uds-api]: https://learn.microsoft.com/en-us/dotnet/api/system.net.sockets.unixdomainsocketendpoint
[afunix-blog]: https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/
[grpc-uds]: https://learn.microsoft.com/en-us/aspnet/core/grpc/interprocess-uds
[tokio-pipes]: https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/index.html
[rust-147335]: https://github.com/rust-lang/rust/pull/147335
[ts-powershell]: https://github.com/airbus-cert/tree-sitter-powershell
[petri-json]: https://petri.com/how-to-use-powershell-7-to-work-with-json-files/
