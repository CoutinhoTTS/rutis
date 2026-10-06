# Mounting AppleScript (and JXA) plugins in rutis: research report

Date: 2026-10-03. Machine: macOS 26.6.2 (25G83), arm64, AppleScript 2.8 (component build 410), JavaScript OSA component 1.1. Xcode toolchain present (`swiftc`).
Experiments live in `scratchpad/experiments/osa/` (full path in Appendix A). Nothing in the rutis repo was modified.

Evidence labels used below: **[measured]** = run on this machine; **[doc]** = Apple documentation or SDK header; **[source]** = third-party source (cited); **[inferred]** = my reasoning, not verified here. Per the safety rule, no Apple Events were sent to any other application. All TCC statements are therefore **[doc]**/**[source]**/**[inferred]**. The only exceptions are two harmless local probes of *attribution* (§5.2), which send no events.

---

## 0. TL;DR and recommendation

1. **Don't build on `osascript`-per-call.** Each call costs ~36–41 ms for AppleScript and ~29–31 ms for JXA, plus ~155 ms more if the script uses AppleScriptObjC **[measured]**. Arguments are strings only. Output is lossy text: reals print with 12 significant digits, the decimal separator follows the locale, and `{}`, `""` and nested lists are ambiguous. Error messages are localized. There is no state. Worse, running a compiled `.scpt` **writes modified properties back into the file** **[measured]**. It is fine only as a debug or one-shot fallback.
2. **Build a persistent native "OSA host" helper process, one per mount**, mirroring the Node runner:
   - **Hosting:** load every AppleScript or JXA plugin through OSAKit, each with **its own `OSALanguageInstance`** and **its own serial executor**.
   - **Data:** convert `NSAppleEventDescriptor` ↔ JSON in the host, using tagged forms for non-JSON values.
   - **Cancellation:** cooperative, through `OSASetActiveProc`.
   - **Gateway:** use `OSASetSendProc` as the single point for every Apple Event the plugin sends. That covers host-service callbacks into rutis, plugin log capture, a target-app allow-list, a "never prompt" consent mode, and effect tracking for "outcome unknown".

   A Swift prototype of this design (`osahost.swift`, about 250 lines) works end to end. Warm spawn→hello takes 5 ms, and spawn→first handler result about 40 ms. Each extra plugin compiles in 13–16 ms. Per-call round-trip is 54 µs median, p99 80 µs. State persists and plugins stay isolated **[measured]**.
3. **Implement the production helper in Rust with `objc2-osa-kit` + `objc2-foundation` + `objc2-core-services`**, plus about six hand-declared OpenScripting C functions, because `objc2-carbon` is empty. Then the toolchain stays Rust-only. Prefer making the **rutis app binary re-exec itself as the helper**, which gives one signed binary, one Info.plist and one TCC identity. A separately built helper binary is the fallback.
4. **Speak a values-only subset of rutis-interop protocol v1** (`hello`, `invoke`, `return`/`throw`, `cancel`, plus runner→rust `invoke` for host services and events) over an inherited socket. The existing `rpc::Connection` can drive it, with a small generalization: today the Rust side requires peer ids to start with `node:`, and `Process::mount` hard-codes `node`.
5. **Interface:** AppleScript has no types, so a declaration is unavoidable. Put it in **plugin-owned in-source annotations** (Raycast-style comment metadata; comments survive compilation). Validate them at build time against the compiled script: `OSAGetHandlerNames` gives exact handler names, and the canonical decompiled source gives positional parameter lists. For JXA, use JSDoc on top-level functions. That can reuse the existing TypeScript-checker-based generator (`allowJs`/`checkJs`).
6. **Capability subset for v1:**
   - Values-only unary handler calls, synchronous in AppleScript and `async fn` on the rutis side.
   - Lifecycle hooks (`rutisApply(config)`, `rutisDispose()`).
   - Values-only calls from the plugin to rutis host services.
   - Notification events in both directions.

   Reject the following at build or mount time:
   - Labeled or prepositional handlers, and piped handler names (these cannot even be invoked).
   - Callbacks, streams, live objects and waterfall/bail events.
   - Run-only scripts without a sidecar declaration.
7. **TCC:** the helper should by default inherit the rutis app's responsibility, as the Node runner already does. So from a terminal, the *terminal app* owns the consent; when launched by launchd or LaunchServices, the app itself does. Document this, and make the runner report permission failures as distinct, typed outcomes (-1743, -1744, -600, -1712, -1719/-25211).
   - **Preflight:** use `AEDeterminePermissionToAutomateTarget(..., askUserIfNeeded=false)` for running targets.
   - **Unattended runs:** OR `kAEDoNotPromptForUserConsent` into sends via the send proc.
   - **Packaged apps** need `NSAppleEventsUsageDescription`, the hardened-runtime entitlement `com.apple.security.automation.apple-events`, and a **stable (non-ad-hoc) signature**.
8. **JXA** costs almost nothing extra in the same host: OSAKit treats it identically **[measured]**. Support it as a secondary language. It is unmaintained: the component is still version 1.1 (copyright 2013), and its release notes end at OS X 10.11. It also has no event loop, so handlers must be synchronous.
9. **Cancellation is honest only as "outcome unknown".**
   - The active proc aborts between statements (~30–60 ms), but a script's own `try` swallows the -128.
   - Blocking commands complete before the cancel lands.
   - Killing the runner leaves `do shell script` children and in-flight app events running **[measured]**/**[doc]**.
   - The send proc can at least tell "no Apple Event was sent before cancellation" from "effects possible".

The old roadmap (docs/roadmap-protocol-plugin-languages-2026-09-26.md §2, AppleScript/JXA) is right about **parameters-not-interpolation**, **opaque descriptors are not JSON**, **test with the packaged runner rather than the dev terminal**, and **cancel = outcome unknown, never kill user apps**. It is **wrong about the order**. It proposed to "start by calling a script-execution tool, later evaluate a resident Cocoa/OSA host". The resident host is small, much faster, and fixes correctness problems the osascript path cannot fix (state, types, precision, locale, `.scpt` write-back, cancellation), so it should be the first version. Details in §11.

---

## 1. How this maps onto the rutis-interop model

rutis-interop today (design doc `docs/design-protocol-plugin-mount.md`):
- A build-time-generated Rust mount plugin spawns a runner process.
- The runner speaks line-delimited JSON protocol v1 over a Unix socket: `hello`, `invoke`, `call`, `get`, `await`, `return`, `throw`, `release`, `cancel`.
- The Rust side projects runner services as rutis services (`Projection`), injects host services (`host:<name>` targets), forwards notification events, and cancels when a future is dropped.
- One mount = one process; a crash withdraws that mount's services.

An AppleScript mount fits this shape with a much smaller surface:

| rutis-interop concept | AppleScript/JXA counterpart |
| --- | --- |
| Cordis plugin (`apply`) | One script file (`.applescript`, `.js`, `.scpt`/`.scptd`); top-level `property`/`global` = plugin state; optional `on rutisApply(config)` / `on rutisDispose()` |
| Service + methods | One service per plugin; methods = declared positional handlers (`on name(a, b)`) / top-level JS functions |
| Service slot replacement | Not applicable (a plugin provides a fixed service; reload = re-mount) |
| Live objects / references / callbacks / futures | Not supported in v1 (descriptors that are not data could become opaque refs later, §3.6) |
| Host services (`provide`) | Script calls rutis via a self-addressed Apple Event intercepted by the host (§7) |
| Events (`events` / `emits`) | Notifications: `«event RUTSemit»` (script→rutis); `on rutisEvent(name, payload)` (rutis→script) |
| `cancel` frame → `AbortSignal` | `cancel` → active proc returns `userCanceledErr` (-128) (§6) |
| Node process crash → `Error::Transport` | Helper crash → same; plus outcome-unknown semantics for in-flight calls |

The Rust `rpc::Connection::connect(UnixStream, Dispatch)` is transport-generic. A runner needs only a subset of frames: `hello`, `invoke`/`return`/`throw`/`cancel` and, for callbacks, its own `invoke`s. Two Node-specific assumptions would need generalizing:
- peer call ids must start with `node:` (`rpc.rs:1165`);
- `Process::mount` spawns `node --import tsx …/runner.mjs` (`process.rs:191`).

Note: `docs/requirements-protocol-plugins.md` §8 lists "other languages" as out of scope today, so this would be a new requirement/design extension, not a change to the Cordis contract.

---

## 2. Execution options and measured costs

### 2.1 `osascript` per call

**Startup** [measured] (`bench.py`; Python `subprocess.run`, n=20 after warm-up; the harness floor for `/usr/bin/true` is 1.6 ms median):

| Command | median | p90 |
| --- | --- | --- |
| `osascript -e 'return 1'` | 41.2 ms (second run: 36.3) | 43.7 ms |
| `osascript -l JavaScript -e '1'` | 30.8 ms (second run: 29.0) | 31.2 ms |
| `osascript hello.applescript a b` (text, compiled each run) | 41.4 ms | 42.9 ms |
| `osascript hello.scpt a b` (precompiled) | 37.9 ms | 39.5 ms |
| AppleScript with `use framework "Foundation"` (AppleScriptObjC) | **197.5 ms** | 203.7 ms |
| JXA with `ObjC.import('Foundation')` | 38.7 ms | 39.1 ms |

**Arguments** [measured][doc `man osascript`]. Anything after the script file (or after the `-e` lines) is passed as a **list of strings** to the `run` handler (`on run argv` / `function run(argv)`). Unicode, quotes, newlines, empty strings and `-e`-looking strings arrive intact. Every argument is `text`, though: structured data must be serialized into strings and parsed inside the script. AppleScript has no JSON parser without AppleScriptObjC, which costs +155 ms per process.

**Output** [measured]:
- **`-s h` (default)** is not machine-parseable:
  - `{}` and `""` both print as an empty line;
  - `{"a", {"b"}}` and `{{"a","b"}}` both print `a, b`;
  - `"missing value"` (text) is indistinguishable from the `missing value` constant;
  - dates print localized (`date Sat Oct 3 10:29:42 2026 on the Chinese-locale host`);
  - `POSIX file "/tmp/x"` prints as an HFS path (`file Macintosh HD:private:tmp:x`);
  - `2147483647` prints `2.147483647E+9`.
- **`-s s`** (recompilable source) is unambiguous, but:
  - it is **not line-safe**: newline, CR and tab are emitted raw inside string literals, and only `"` and `\` are escaped;
  - consuming it requires an AppleScript literal parser.
- **Numbers lose precision either way**: `return 1/3` prints `0.333333333333` (12 significant digits) in both `-s h` and `-s s`.
- **The decimal separator follows the locale**: `3.5 as text` is `"3,5"` under `-AppleLocale de_DE` (tested in a custom host via the argument domain).
- JXA output keeps full JS precision (`0.3333333333333333`), and `JSON.stringify` in the script is the clean path for JXA.
- **`-s o`** only moves error text from stderr to stdout. The exit status is 1 either way.

**Errors** [measured]:
- Format: `<start>:<end>: execution error: <message> (<number>)`, or `syntax error`. Offsets are character offsets into the source, not lines. File scripts are prefixed with `path:`.
- **Messages are localized**. On this zh-Hans machine:
  - `16:23: execution error: cannot coerce "abc" to type integer (-1700)`
  - `LANG=en_US.UTF-8` does **not** change this; only the global `AppleLanguages` matters, and osascript rejects `-AppleLanguages` (getopt).
- Without `number`, `error "x"` yields -2700. User cancel is -128.
- A `do shell script` failure reports stderr as the message and the exit code as the number (`… execution error: oops (3)`).
- JXA: runtime errors have no position (`execution error: Error: Error: boom (-2700)`). Throwing an object with `errorNumber` sets the number (`… (42)`).
- Only the number is stable; parse nothing else.

**State** [measured]:
- Text sources get fresh state every run.
- **Compiled `.scpt` files get their modified top-level properties written back to disk by osascript**: `property n : 0` / `set n to n + 1` returns 1, 2, 3 on successive runs, and the file's mtime changes. That is a hidden, racy persistence channel. It mutates deployed files, which would break a code-signed bundle, and concurrent runs clobber each other.

**Cancellation:** only by killing the process. Killing it does not stop children: after `kill -9` on an osascript running `do shell script "sleep 2; echo written > marker.txt"`, the marker file was still written 2 s later [measured].

**TCC:** osascript is attributed to whoever is responsible for the spawning process (§5).

**Verdict:** only acceptable for stateless one-shot scripts (the Raycast/Keyboard Maestro model). Not for a plugin model.

### 2.2 Persistent JXA runner (`osascript -l JavaScript runner.js`)

**[measured]**:
- A JXA `run()` loop reading `$.NSFileHandle.fileHandleWithStandardInput.availableData` and writing with `writeData` works (`loop.js`).
- Spawn → first reply takes 36.6 ms. A pure-JS handler costs 0.167 ms median per round trip (p99 0.217 ms, n=1000).
- **Hosting AppleScript plugins from JXA through OSAKit** works (`jxarunner.js`, `$.OSAScript.alloc.initWithSourceFromURLLanguageInstanceUsingStorageOptions` + `executeHandlerWithNameArgumentsError`). Load is 67 ms after spawn and calls take 0.344 ms median (p99 0.54 ms). State persists across calls.

Sharp edges found [measured]:
- **`Ref()` + `ref[0]` on an object out-parameter segfaults the whole runner** (exit 139). This applies to OSAKit's `NSDictionary **errorInfo`, `NSAppleScript executeAndReturnError:` and even a plain `NSError **`.
  - The documented idiom for object out-params is a boxed nil, `const e = $(); obj.methodError(…, e); e.objectForKey(…)`, which works. `Ref()` is documented for scalar out-params such as `BOOL *` ([JXA 10.10 release notes](https://developer.apple.com/library/archive/releasenotes/InterapplicationCommunication/RN-JavaScriptForAutomation/Articles/OSX10-10.html)).
  - Still, one wrong idiom kills every plugin in the process.
- **`Library()` / `OSA_LIBRARY_PATH` don't help.** `/usr/bin/osascript` (a platform binary, `Platform identifier=26`) **ignores `OSA_LIBRARY_PATH`** on macOS 26: both `tell script "X"` and JXA `Library("X")` fail with -1728/-43.
  - A custom (non-platform) host **honors it**: `OSA_LIBRARY_PATH=… ./libprobe AppleScript 'tell script "RutisCounter" to …'` returns `[2, 5]`, and JXA `Library("RutisJS")` works too.
  - The variable was added in 10.11 ([AppleScript 10.11 release notes](https://developer.apple.com/library/archive/releasenotes/AppleScript/RN-AppleScript/RN-10_11/RN-10_11.html)). Shane Stanley calls it a tool for script editors, "not for general use" ([Late Night Software forum](https://forum.latenightsw.com/t/applescript-execution-error-cant-find-script-in-non-default-location/128/22)).
  - So with osascript, `Library()` would require installing plugins into `~/Library/Script Libraries`, a global side effect.
- **Single-threaded.** While a handler runs, the runner cannot read a `cancel` frame. JXA also cannot create the C function pointer needed for `OSASetActiveProc`. **Cancellation = kill the whole runner**, which loses every plugin's state.
- No event loop: Promises' `.then` callbacks never run inside a synchronous script, and there is no `setTimeout` [measured].

**Verdict:** a viable no-compiler fallback, about 6× slower per call than a native host. But it has no cooperative cancellation, no send-proc gateway, and one-mistake process crashes.

### 2.3 Native helper (Swift/ObjC or Rust) using OSAKit — **recommended**

Prototype: `osahost.swift` (line-JSON on stdin/stdout, a reader thread, main thread in `RunLoop.main.run()`), driven by `drive.py`, `coldstart.py` and `block.py`.

**Costs [measured]:**

| Step | Cost |
| --- | --- |
| Spawn → `hello` (warm) | 5 ms (median of 10) |
| Spawn → first handler result (load AppleScript component + compile + call) | ~40 ms (median 40.2) |
| First `exec` of a freshly linked, ad-hoc-signed binary | ~560 ms once (system code assessment) |
| Each additional plugin: load + compile | 13–16 ms (first ~32 ms) |
| Handler call round trip (stdin/stdout JSON + descriptor conversion + `executeHandler`) | **54 µs median**, p99 80 µs (n=2000) |
| In-process `executeHandler` alone | 20–50 µs |
| AppleScriptObjC (`use framework`) | +~200 ms on first use **per process**, then ~20 ms compile per script |

**Two call APIs, both verified [measured]:**
- `OSAScript.executeHandler(withName:arguments:error:)`. Arguments are an NSArray of `NSAppleEventDescriptor`s; `NSNumber`s also worked.
- `executeAppleEvent:` / `NSAppleScript.executeAppleEvent(_:error:)` with a constructed subroutine event: class `kASAppleScriptSuite` ('ascr'), id `kASSubroutineEvent` ('psbr'), `keyASSubroutineName` ('snam') = handler name, direct object = list of arguments. This is the [appscript/py-applescript](https://appscript.sourceforge.io/nsapplescript.html) technique.
- The `run` handler is not callable as `executeHandler("run")` (-1708). Use `executeAndReturnError`.

**Gotchas [measured]:**
- `OSAScript(contentsOf:languageInstance:using:)` with a *non-shared* language instance and a text file compiles fine, but **every handler call then fails with -1708**. `OSAScript(source:from:languageInstance:using:)` works. Load the source text yourself.
- A temporary `OSALanguageInstance` must be kept alive. If it is released, its `componentInstance` becomes invalid (`badComponentInstance`, -2147450879).
- Handler names: plain identifiers match **case-insensitively** (`getState` = `getstate` = `GETSTATE`). **Piped names (`|ExactCase|`) cannot be invoked by either API**, with or without pipes (-1708).
- Labeled handlers (`on greet given name:who`) and prepositional handlers (`on fetchItems from source for n`) **cannot be called positionally**: -1701 "The name parameter is missing".
- The error language follows the *host binary's* Info.plist:
  - a binary without an Info.plist gets English messages;
  - one with an embedded `CFBundleAllowMixedLocalizations=true` (as `/usr/bin/osascript` has) gets the user's language (Chinese here);
  - `-AppleLanguages '(en)'` in the helper's argv forces English.

  So the helper can choose stable English messages for logs, while numbers stay the contract.
- AppleScript `log` and JXA `console.log` output in a custom host goes **nowhere** (neither stdout nor stderr). Both arrive as an `ascr/cmnt` event in the send proc, so the host can forward plugin logs to rutis [measured]. The protocol channel is not polluted either way.

### 2.4 In-process in the rutis host via objc2

**Crates [checked on crates.io, 2026-10-03]:**
- `objc2-osa-kit` 0.3.2 (2025-10-04): `OSAScript`, `OSALanguage`, `OSALanguageInstance` (`isThreadSafe`, `executeHandlerWithName_arguments_error`). It does **not** bind `componentInstance`.
- `objc2-foundation` 0.3.2: `NSAppleScript`, `NSAppleEventDescriptor`, including `dateValue`, `fileURLValue` and `aeDesc`.
- `objc2-core-services` 0.3.2: `AEDeterminePermissionToAutomateTarget`, AE functions.
- `objc2-carbon` 0.3.2 is an empty shell (only a link attribute), so `OSASetActiveProc`, `OSASetSendProc`, `OSAGetHandlerNames` and `OSACompile` need hand-written `extern "C"` declarations (Carbon.framework).
- The higher-level [`osakit` crate](https://docs.rs/crate/osakit/latest) (0.3.1) adds serde_json conversion and a `declare_script!` macro with typed function signatures. It **refuses to run unless the current thread is named "main"** and drops error numbers (it keeps only message and range). Not suitable as is.

**Threading [doc][measured]:**
- The [AppleScript 10.6 release notes](https://developer.apple.com/library/archive/releasenotes/AppleScript/RN-AppleScript/RN-10_6/RN-10_6.html) say: "OSA and AppleScript are now thread-safe … This also applies to `NSAppleScript`", but "AppleScript uses locking to ensure that any single connection (a `ComponentInstance`) will only run on one thread at a time". They also say scripting-addition commands are presumed thread-unsafe and run on the main thread.
- `OSALanguage.isThreadSafe` is `true` for both AppleScript and JavaScript [measured].
- Calls from background threads work [measured].
- **However, AppleScript execution is serialized process-wide on macOS 26** [measured, `par.swift`]. 1, 2 and 4 concurrent CPU-bound handlers on *separate* language instances took 239, 487 and 970 ms wall time, with user CPU equal to wall time. An earlier, warm-up-affected run suggested partial overlap.
- The lock **is released while a handler blocks**: during `delay 1` or `do shell script "sleep 1"` in plugin A, plugin B's call completed at 101–104 ms; during a CPU-bound loop in A, B waited until A finished (487 ms) [measured, `block.py`]. Apple Event waits most likely behave like `do shell script` [inferred].
- StandardAdditions marks 17 handlers thread-unsafe in its Info.plist: clipboard, `display dialog/alert/notification`, `choose …`, `set volume`, `time to GMT`, `store script` [measured]. Yet `time to GMT` completed from a background thread even with the main thread parked on a semaphore [measured]. UI commands were not tested (safety rule).
- Third-party experience is less rosy ([Stairways/Peter Lewis](https://www.stairways.com/blog/2014-04-24-nsapplescript-not-thread-safe); [Apple forum](https://developer.apple.com/forums/thread/103443)).

**Crash blast radius [measured]:** a plugin can trivially kill its host:
- JXA `ObjC.import("stdlib"); $.abort()` → host exit 134;
- AppleScript `do shell script "kill -9 $PPID"` → host exit 137;
- AppleScriptObjC can call any Cocoa API. (`tell current application to quit` was harmless: -1708 in a non-app host.)

In-process means one misbehaving script takes down the whole rutis app and every other mount. TCC attribution is the app's anyway (§5).

**Verdict:** use the objc2 bindings *inside the out-of-process helper*, not in the rutis app process.

### 2.5 Comparison

| | osascript per call | persistent JXA runner | native OSA helper (rec.) | in-process (objc2) |
| --- | --- | --- | --- | --- |
| Startup | 30–41 ms *every call* (+155 ms with ASObjC) | ~37 ms once (+67 ms to load an AS plugin) | ~5 ms spawn, ~40 ms to first result | 0 (component load ~30 ms once) |
| Per call | 30–41 ms | 0.17 ms (JS) / 0.34 ms (AS via OSAKit) | 0.054 ms | 0.02–0.05 ms |
| Typed args/results | strings in, lossy text out | descriptors via ObjC bridge (JS code) | descriptors ↔ JSON in native code | same |
| State persistence | none (text) / **file write-back** (`.scpt`) | yes | yes, per plugin | yes |
| Isolation between plugins | process per call | shared JS global (JXA plugins) / per OSAScript (AS) | per `OSALanguageInstance` (TIDs, globals isolated) + serial queue | same, but shares the app's process |
| Cooperative cancel | no (kill) | no (single thread, no C callbacks) | yes (`OSASetActiveProc`) | yes |
| Send-proc gateway (callbacks, logs, policy) | no | no | yes | yes |
| Crash blast radius | one call | all plugins of the runner | the mount | **whole rutis app** |
| TCC attribution | responsible process of the rutis app | same | same (or own identity, if disclaimed, §5) | rutis app |
| Toolchain | none | none | Rust (objc2) or Swift | Rust (objc2) |

---

## 3. State

[measured, `osakit.js`, `probe2/3/4/8.swift`, `osahost`]:
- **Top-level `property` and `global` values persist across handler calls on the same compiled script instance.** This holds for both OSAKit and NSAppleScript: `bump(5)` → 5, then `bump(7)` → 12; `history` = `[5,7]`; `setScratch("kept")` then `getScratch()` → `"kept"`. Reading a never-set `global` gives -2753.
- **Several scripts with separate state in one host:** two `OSAScript`s compiled from the same source keep separate properties, even on the shared language instance (s1: 5, 10; s2: 1).
- **Per-component globals leak across scripts on the shared instance.** After `set AppleScript's text item delimiters to "|A|"` in script a, script b reads `|A|`. With **one `OSALanguageInstance` per plugin** the delimiters are isolated. Rule: one language instance per plugin.
- **JXA:** top-level `var`s persist (`bump` 5 → 12). Separate language instances isolate JXA plugins too.
- **Script objects** returned from a handler come back as a `'scpt'` descriptor. That is a serialized *snapshot*, about 3.2 KB because it embeds the parent context.
  - The host can rehydrate it into an `OSAScript` "handle" (`OSAScript(scriptDataDescriptor:…)`), which then keeps its own state: `inc()` → 101, 102.
  - It is **detached from the parent**: the parent's `made` became 2 while the handle still saw 1.
  - Copy semantics make script-object "live references" confusing; don't offer them in v1.
- **`load script`** requires compiled data (`.scpt`); a text `.applescript` gives -1752. Each `load script` returns a fresh copy.
- **Reload** = a new compile = fresh state, which matches rutis's dependency-driven reload. Persisting state across reloads is the plugin's or app's job (the host must never write back to plugin files).

---

## 4. Data mapping

### 4.1 Descriptor kinds actually produced [measured, `values.js`, `osahost`]

| AppleScript value | Descriptor | Proposed JSON / Rust | Notes |
| --- | --- | --- | --- |
| `text` | `utxt` | string / `String` | Unicode incl. emoji and combining marks round-trips |
| `integer` | `long` | number / `i32`-range checked | **AppleScript integers are ±536 870 911 (29-bit).** 536 870 912 arrives as `real`. An int32 descriptor `2147483647` passed in comes back as `doub 2.147e9`. |
| `real` | `doub` | number / `f64` | Full precision via descriptors. -0.0 is preserved in the descriptor. `1/0` raises -2701 (no inf/NaN). |
| `boolean` | `true`/`fals` (`bool`) | boolean | |
| `list` | `list` | array / `Vec<T>` | |
| record, user labels | `reco { usrf: [k1, v1, k2, v2…] }` | object / struct | Built from JSON the same way; `alpha of r` and `\|Mixed Case\| of r` work |
| record, label that is a defined term | `reco { 'kind': … }` (four-char keyword) | needs a term table, or forbid | `{kind:4, \|kind\|:5}` → `{«kind»:4, kind:5}`. `name`, `id`, `class` and others behave the same way. **`{class:"c"}` fails to pack at all (-1700)**. Pipes force a user label. |
| empty record `{}` | **`list []`** | depends on the declared type | `{}` and `{}`-as-list are indistinguishable; `class of {}` passed in is `list` |
| `missing value` | `type 'msng'` | `null` / `Option::None` | `x is missing value` works when `null` is passed as `msng` |
| `null` | `type 'null'` | `null` | rare |
| `date` | `ldt ` (LongDateTime, **local time, no zone**) | `{"$date": "2026-10-03T10:00:00+08:00"}` | Convert with the host's current time zone. **Date literals are parsed with the user's locale at compile time**: `date "Friday, October 3, 2026 at 10:00:00"` fails to compile on zh-Hans (-30720). `(current date) as text` is localized. |
| `POSIX file "/x"` / `file` | `furl` | `{"$file": "/x"}` / `PathBuf` | |
| `alias` | `alis` | `{"$file": …}` via coercion to `furl` | alias resolution may fail if the file moved |
| class / type constant (`text`) | `type 'ctxt'` | `{"$type": "ctxt"}` | |
| enumerations (`yes`/`ask`) | `enum` | `{"$enum": "…"}` | |
| units (`3 as inches`) | `inch` | reject (or opaque) | |
| raw data `«data rdat00FF»` | `rdat` | reject (or base64) | |
| script object | `scpt` (snapshot) | reject (or opaque handle) | §3 |
| reference / object specifier | `obj ` {form, want, seld, from} | opaque handle | §4.3 |

- **JXA via OSAKit** [measured]: JS objects → `usrf` records; arrays → lists; `null` → `msng`; `undefined` → `null()` (typeNull); thrown `errorNumber` is preserved.
- Inbound, a `usrf` record arrives as a plain JS object and a list as an array. `typeof` a `msng` argument is `"object"` (`null`).
- **For JXA, a JSON-in/JSON-out wrapper (`JSON.parse`/`JSON.stringify`) is an even simpler data path.**

### 4.2 Conversion rules for the host [measured with `osahost.swift`]
- Inbound JSON:
  - `null` → `msng`;
  - integers within ±2^29 → `long`, others → `doub`;
  - objects → `usrf` records;
  - `{"$date"}`, `{"$file"}` and `{"$ref"}` tagged forms → `ldt `, `furl`, or the stored descriptor.
- Outbound: the table above; anything else becomes `{"$ref": "rN", "$type": "<4cc>"}` in a host-side table, released by a `release` op.
- Use the declared types (codegen) to resolve ambiguities: `[]` vs `{}`, int vs real (JSONSerialization collapses `2.0` → `2`; Rust's serde_json `Number` keeps the distinction), and `Option`.
- Keyword-labeled record fields (`«kind»`) should be a build-time error in declared result types, unless the declaration names the four-char code. Tell authors to pipe labels: `|kind|:…`.
- Never coerce numbers or dates to text inside scripts for transport (locale; 12-digit precision).

### 4.3 Object specifiers as "live object references"
- Descriptors are values; a host can retain them indefinitely and pass them back.
  - [measured] `a reference to item 2 of L` comes back as `'obj '{form:'indx', want:'cobj', seld:2, from:[10,20,30]}`. The container is a *copy* of the local list. Passed back into `deref`, it yields 20.
  - Passed back through `vEcho` (`return x`), the reference was **resolved** on return (2), not echoed.
- For application objects, the outermost container is the application (address/bundle id) [doc/inferred]. Retaining such a specifier gives a **re-resolvable path, not an identity**:
  - by-index specifiers drift when the collection changes;
  - by-name and by-id are steadier;
  - after an app relaunch, only stable-id forms survive.

  Each use, even a `return`, may send Apple Events (get/count) to the target, with all the TCC and timeout consequences. That is untested here (no Apple Events allowed).
- **Worth it?** Not for v1. It adds a reference table, `release`, staleness errors (-1728), implicit event traffic, and confusing copy-vs-reference semantics, all for automation scripts that usually return data. If added later, expose it as an opaque `ObjectRef`-like handle with explicit methods (`resolve() -> data`, pass-back as argument). Never fake liveness with string descriptions, as the roadmap already says.

---

## 5. Permissions (TCC) — documented behaviour, plus what the runner should report

### 5.1 Model
- **Automation consent is per (responsible client, target app)** and persisted by TCC (`kTCCServiceAppleEvents`).
  - The *client* is a bundle id or an absolute path (`client_type` 0/1), with a code-requirement blob (`csreq`).
  - The *target* is the "indirect object" (bundle id) ([TCC.db overview, Rainforest QA](https://www.rainforestqa.com/blog/macos-tcc-db-deep-dive), via search summary).
  - Users manage it under System Settings › Privacy & Security › Automation ([Apple Support](https://support.apple.com/guide/mac-help/mchl108e1718/mac)).
- **Responsible process:** TCC attributes a request to the *responsible* process. That is decided by the launch chain, not by which binary calls `AESend`.
  - From a terminal, the terminal app is responsible.
  - Apps launched via Finder/`open`/LaunchServices, and launchd agents, are responsible for themselves.
  - A parent can make a child self-responsible with the private SPI `responsibility_spawnattrs_setdisclaim` (used by LLDB, Chromium, Mozilla and Qt Creator) ([Qt blog](https://www.qt.io/blog/the-curious-case-of-the-responsible-process)).
  - The usage description must be in the *responsible* process's Info.plist (same source).
  - For launchd agents and Terminal, see Quinn's DevForums posts ([example](https://developer.apple.com/forums/thread/694948)).
- **`NSAppleEventsUsageDescription`** is "required if your app uses APIs that send Apple events" ([Apple](https://developer.apple.com/documentation/bundleresources/information-property-list/nsappleeventsusagedescription)). Apps linked against the 10.14+ SDK without it get -1743 **without a prompt** ([mjtsai](https://mjtsai.com/blog/2018/08/23/apple-events-usage-description/), [DevForums](https://developer.apple.com/forums/thread/109561)).
  - A CLI can embed an Info.plist in `__TEXT,__info_plist` (`-Wl,-sectcreate,__TEXT,__info_plist,Info.plist`), which worked here [measured].
  - The linker's ad-hoc signature reports `Info.plist=not bound`; re-sign with `codesign` to bind it.
- **Hardened runtime:** `com.apple.security.automation.apple-events` is "a Boolean value that indicates whether the app may prompt the user for permission to send Apple events to other apps". It is not needed "if it only sends Apple events to itself or to other processes signed with the same team ID" ([Apple](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.automation.apple-events)).
  - Without it, a hardened app's `AEDeterminePermissionToAutomateTarget(…, true)` returns -1743 ([DevForums](https://developer.apple.com/forums/thread/130949)).
  - A 2026 report says a hardened responsible binary without the entitlement blocks Apple Events from its whole child tree ([report](https://claudeissues.com/issue/52712-macos-hardened-runtime-blocks-apple-events-from-child-processes-missing-com-appl)).
  - So the entitlement must be on the responsible process, and also on the helper if the helper is hardened [inferred].
- **Identity stability:** "macOS tracks code identity using the code's designated requirement. Ad hoc signed code does not include a stable DR" (Quinn, [DevForums](https://developer.apple.com/forums/thread/795739)). Every ad-hoc rebuild therefore looks like new code, so grants go stale or users are prompted again. Use Apple Development or Developer ID signing for anything that should keep consent.
- **`tccutil reset AppleEvents [bundle_id]`** (`man tccutil`) resets per *bundle id* only. Path-identified CLI clients can only be reset service-wide, which is another reason to give a self-responsible helper a bundle identifier.
- **Preflight:** `AEDeterminePermissionToAutomateTarget(target, class, id, askUserIfNeeded)` (macOS 10.14+, SDK `AppleEvents.h`) **[doc]**:
  - The target must be a *running* app. It returns `noErr`, `errAEEventNotPermitted` (-1743), `procNotFound` (-600), or `errAEEventWouldRequireUserConsent` (-1744) when `askUserIfNeeded` is false and consent is undetermined.
  - Use `typeWildCard` for class and id to ask about "all events".
  - It is thread-safe since 10.14 and "may take arbitrarily long to return if the user needs to be prompted": never call it on the main thread.
  - Historical complaint: the target must already be running, and early betas mis-reported earlier denials ([mjtsai](https://mjtsai.com/blog/2018/08/31/aedeterminepermissiontoautomatetarget-added-but-aepocalyse-still-looms/)).
- **No-prompt sending:** the `kAEDoNotPromptForUserConsent` (0x00020000) send-mode flag makes `AESend` return -1744 instead of prompting **[doc]**. The OSA host can OR it into every outgoing event in its send proc (§2.3, §7). The send proc receives the mode (observed `0x1063` = wait-reply | can-interact | can-switch-layer | dont-record) [measured].
- **GUI scripting** (System Events UI elements) additionally needs **Accessibility** for the responsible process. Failures look like "… is not allowed assistive access" (-1719) or `kAXErrorAPIDisabled` (-25211, `AXError.h`) ([Keyboard Maestro forum](https://forum.keyboardmaestro.com/t/execute-an-applescript-failed-osascript-is-not-allowed-assistive-access/34490)).

### 5.2 Attribution observed locally (harmless; no events sent)
- `resp.c` uses the private `responsibility_get_pid_responsible_for_pid`. It shows that everything spawned in this session, `zsh` included, is attributed to `…/claude.app/Contents/MacOS/claude`.
- That `claude` binary is itself started by `Claude.app/Contents/Helpers/disclaimer`, i.e. Claude.app uses the disclaim technique to make its tool host self-responsible.
- `disclaim.c` re-spawned `resp` with `responsibility_spawnattrs_setdisclaim(attr, 1)`. The child then reported **itself** as responsible.

Consequence: an osascript, or an OSA helper spawned by a rutis app, inherits whatever launched the rutis app. Dev-time consent given to the terminal says nothing about the packaged app (this confirms the roadmap's warning).

### 5.3 Recommended TCC posture
- **Default: inherit**, exactly like the Node runner ("the process runs with the same user and permissions as the rutis app").
  - Document that consent belongs to the terminal (dev) or to the app (packaged).
  - Packaged apps need `NSAppleEventsUsageDescription`, a stable signature, and the apple-events entitlement if hardened.
- **Optional "own identity" mode:** spawn the helper disclaimed (private SPI; isolate it behind a feature flag), with an embedded Info.plist containing a bundle id derived from the app (e.g. `com.example.app.osa`) and a stable signature.
  - Never use one shared helper binary for many apps: a grant to a shared helper becomes a confused deputy for every app that uses it.
- **Manifest-declared targets** (`targets = ["com.apple.Music"]`):
  - checked at mount: is the app installed? Use a LaunchServices lookup by bundle id, which needs no Apple Event;
  - preflighted without prompting if the target is running;
  - **enforced in the send proc** as an allow-list. Undeclared targets fail fast with a typed error.

  This is a guardrail, not a sandbox: `do shell script "osascript …"` or AppleScriptObjC can bypass the send proc, and both run with the same responsibility.
- **Consent modes:**
  - `interactive`: the default; may prompt; calls block while the dialog is up;
  - `never-prompt`: OR `kAEDoNotPromptForUserConsent`, fail with -1744; for headless/unattended use;
  - `preflight-only`.

### 5.4 Runner error taxonomy (what rutis sees)
Keep the AppleScript number, the English message, `brief`, `range`, `partialResult`, `offendingObject`, `expectedType` and `app` from the OSAKit error dictionary in the error graph [measured: e.g. `fail` → `number 4242, range [496,25], expectedType 'cobj', app "osahost"`]. Then classify:

| Number | Meaning (SDK name) | Report as | Retry? | Outcome |
| --- | --- | --- | --- | --- |
| -1743 | `errAEEventNotPermitted` | `PermissionDenied { target, responsible }`; hint about Automation settings, usage description, entitlement | no, needs user action | not performed (the event was refused) |
| -1744 | `errAEEventWouldRequireUserConsent` | `ConsentRequired { target }` (only in no-prompt mode or preflight) | after interactive consent | not performed |
| -1742 | `errAETargetAddressNotPermitted` | `PermissionDenied` (sandbox) | no | not performed |
| -1719 / -25211 | assistive access / `kAXErrorAPIDisabled` | `AccessibilityDenied` | no | not performed (for that step) |
| -600 | `procNotFound`: app isn't running | `TargetNotRunning { target }` | caller decides | not performed |
| -609 | `connectionInvalid`: app quit or crashed mid-event | `TargetGone` | no auto-retry | **unknown** |
| -1712 | `errAETimeout` (default 2 min) | `Timeout` | no auto-retry | **unknown**: the target keeps working ([AppleScript Language Guide](https://developer.apple.com/library/archive/documentation/AppleScript/Conceptual/AppleScriptLangGuide/reference/ASLR_control_statements.html)) |
| -128 | `userCanceledErr` | `Cancelled` if rutis requested it; else `UserCancelled` (e.g. a dialog's Cancel button) | no | unknown/partial (§6) |
| -10810 / -10814 | LaunchServices launch failure / app not found | `TargetLaunchFailed` / `TargetNotInstalled` | no | not performed |
| -1708 | `errAEEventNotHandled` | for the host's own call: `ContractError` (handler missing; should be caught at mount); inside the script: business error | no | for contract: not performed |
| -1721, -1701, -1700 | wrong arity / missing labeled param / coercion | `ContractError` (binding mismatch) when raised at the call boundary; business error otherwise | no | not performed if raised at the boundary |
| -1728 | `errAENoSuchObject` | business `NotFound` | caller decides | partial possible |
| -2741/-2740…, -30720 | compile errors (syntax, date literal) | mount failure with source range | — | — |
| -2700 / positive | script `error "…" [number n]` | `Remote { name: "AppleScriptError", number }` | caller decides | partial possible |
| helper exit or socket loss | — | `Transport` with exit status (as for Node) | no | **unknown** for all in-flight calls |

---

## 6. Cancellation and timeouts

**[doc]** ([Language Guide, control statements](https://developer.apple.com/library/archive/documentation/AppleScript/Conceptual/AppleScriptLangGuide/reference/ASLR_control_statements.html)):
- `with timeout of N seconds` applies **only to commands sent to application objects**, not to commands handled by the running application.
- The default is two minutes.
- On timeout, "AppleScript does not cancel the operation—it merely stops execution of the script" (-1712).
- `ignoring application responses` makes sends fire-and-forget: no result, no error.

**[measured]**:
- `with timeout of 1 second` around `do shell script "sleep 3"` waits the full 3 s, because a scripting-addition command runs in-process.
- `kill -9` of osascript leaves the `do shell script` child running, and its side effect still happens.

**Cooperative cancel via `OSASetActiveProc`** (`probe5.swift`, `osahost`) [measured]. The proc runs periodically during execution; returning `userCanceledErr` aborts:
- CPU loop: aborted 30–60 ms after the flag was set (the proc ran only 2–3 times in 300 ms).
- `delay 5`: aborted after ~31 ms.
- **`try … on error` inside the script catches the -128** and returns normally (`"caught -128"`). Cancellation is cooperative; authors must re-raise -128. The host keeps the flag set, so any later statement aborts again.
- `do shell script "sleep 2; …"`: the cancel took effect only **after the command completed** (1.7 s later), so the side effect happened. Blocking commands, and most likely Apple Event waits, are not interrupted.
- The script stays usable after a cancel (a later `spin(10)` worked).

**What to report honestly:**
- A cancelled or timed-out call yields `Cancelled { outcome }`. The `outcome` is:
  - `NoEffectsObserved` when the send proc saw **no** outgoing Apple Event (including `syso/exec` for `do shell script`) during the call **and** the plugin does not use AppleScriptObjC;
  - otherwise `Unknown`.
- Never `NotExecuted`, never auto-replay, never roll back, and never kill user apps (they're not in the runner's process tree).
- If a cancel doesn't land, keep rutis-interop's existing contract (requirements §7: no extra timeouts in the compatibility layer).
  - The app can release the mount. The helper is then SIGKILLed; kill its **process group** so `do shell script` children die with it.
  - Every in-flight call reports `Transport` + unknown outcome.
  - Cancellation latency is bounded by the longest single blocking command (≤ that command's AE timeout).

---

## 7. Calling back into rutis; events; capability subset

**Mechanisms tested [measured, `callback.swift`]:**
1. **Self-addressed Apple Event intercepted in the OSA send proc.** The plugin does `tell current application to «event RUTScall» {service, method, args}`. The host's send proc recognizes class `'RUTS'`, creates the reply event (`AECreateAppleEvent(kCoreEventClass, kAEAnswer, …)`; the reply passed in may be a null descriptor) and returns a value.
   - The script receives it (`{got:"host answered via send proc", doubled:42}`).
   - Cost: ~5 µs per call in-process. Add one socket round trip to rutis (~50 µs) in the real design.
   - Events to self need no consent [doc: entitlement page; consistent with the TCC model].
2. **Same event handled with `NSAppleEventManager.setEventHandler`.** It is dispatched **on the script's background thread** (no main-thread hop) and takes ~110 µs per call. It also catches events sent through the ObjC bridge from JXA.
3. **`do shell script "rutis-call …"`** with a socket path in the environment. The script's shell inherits the helper's environment (`RUTIS_SOCKET` was visible) and is a child of the helper. It costs process-spawn time per call and needs `quoted form of` discipline. Fallback only.

**Ergonomics:** ship a tiny `rutis` script library (AppleScript `.scptd` with an optional sdef for natural terminology; a JS module for JXA) on the helper's `OSA_LIBRARY_PATH`, which a custom host honours [measured]. Plugins then write `use rutis : script "rutis"` … `rutis's call("kv", "get", {key:"x"})` and `rutis's emit("my/event", {…})`.

**Events:**
- Script → rutis: notification-only `«event RUTSemit»`, forwarded like Cordis events (rutis `parallel`, fire-and-forget for the script).
- rutis → script: a declared handler (`on rutisEvent(name, payload)` or per-event handlers) queued on the plugin's serial executor.
- Only notification events; waterfall/bail are rejected at build time, as for Cordis.

**v1 capability subset and rejection rules:**
- Supported:
  - declared positional handlers with data-only parameters and results;
  - lifecycle hooks `rutisApply(config)` (after compile, before services register) and `rutisDispose()` (before close; may still call host services, like Cordis's "services usable during unload");
  - host services, values-only, sync from the script's view;
  - notification events in both directions;
  - plugin logs (`log`/`console.log` arrive as `ascr/cmnt` in the send proc [measured]) forwarded to rutis tracing.
- Rejected at **build** time, with a `cargo:warning` for unsupported members, as for the TS generator, or an error when nothing usable remains:
  - labeled or prepositional handlers;
  - piped or case-colliding handler names;
  - parameters or results outside the type grammar (functions, references, script objects, raw data, units);
  - record result types with defined-term labels;
  - callbacks;
  - streams;
  - waterfall/bail events;
  - `requires` on host services not provided by the app;
  - run-only scripts without a sidecar declaration.
- Rejected at **mount** time:
  - compile failure (with range);
  - declared handler missing (`OSAGetHandlerNames`);
  - declared target app not installed;
  - protocol mismatch.
- At **run** time: descriptor kinds outside the declared type → `ContractError`. Never stringify them into fake data.

---

## 8. Interface declaration for codegen

What the OSA API offers [measured, `probe6.swift`, `decompile2.swift`]:
- `OSACompile` + `OSAGetHandlerNames` gives exact handler names with case preserved (`"bump"`, `"greet"`, `"fetchItems"`). Command handlers come back as `evnt` codes (`aevt/oapp` run, `aevt/quit`, `misc/idle`, `aevt/odoc` open).
- `OSAGetPropertyNames` gives `"hits"`, `"label"`, `"Inner"` (script objects are properties).
- **There are no parameter names, counts or types.** `OSAGetSource` on a handler returns only the body.
- **The canonical decompiled source** (compile → `compiledData` → new `OSAScript` → `.source`, or `osadecompile`) normalizes headers to `to AddItem(theName, qty)` / `on ping()` / `end AddItem`. That makes a top-level header scan reliable, and labeled forms (`given`, prepositions) are easy to detect.
- **Comments survive** in compiled, non-run-only scripts. Run-only copies have no source (`.preventGetSource` → empty).

Precedent: [Raycast script commands](https://github.com/raycast/script-commands):
- metadata in comments (`# @raycast.schemaVersion 1`, `# @raycast.title …`, `# @raycast.argument1 { "type": "text", "placeholder": "Arg1" }`) in `#!/usr/bin/osascript` files;
- up to 3 text arguments via `on run argv`;
- errors via non-zero exit status, with the last output line as the message ([manual](https://manual.raycast.com/script-commands)).

Rust precedent: the `osakit` crate's `declare_script!` (typed signatures written by the *consumer*).

**Recommendation:**
- **AppleScript: plugin-owned in-source annotations.** The plugin author declares types next to the code, which is the analogue of TS types shipping with a Cordis plugin. The rutis app developer does not maintain an IDL, which keeps the spirit of requirements §3. Example:
  ```applescript
  -- @rutis.plugin  service=musicControl  targets=com.apple.Music
  -- @rutis.handler currentTrack(): { name: string, artist: string, duration: number } | null
  on currentTrack()
  	…
  end currentTrack
  -- @rutis.handler setVolume(level: integer): boolean
  on setVolume(level)
  	…
  end setVolume
  -- @rutis.requires kv: KvStore            (host service, values-only)
  -- @rutis.emits    music/track-changed { name: string }
  ```
  - Use a restricted TS-like type grammar: `string number integer boolean date file T[] {k: T} T | null any`. It maps onto the existing TS→Rust table (`integer` → `i32` with ±2^29 checks; `date` → a local-time type; `file` → `PathBuf`; `any` → `serde_json::Value` with tagged forms).
  - Validate at build time: compile the plugin with OSAKit in the build script (macOS only), then:
    - check names via `OSAGetHandlerNames`;
    - check arity and parameter names via the canonical header scan;
    - check that `rutisApply`/`rutisDispose` exist if declared;
    - check the target bundle ids (warning only, since the build machine may lack the apps).
  - A sidecar manifest (TOML/JSON next to the script) is the fallback for run-only or third-party scripts. It must not be required for normal plugins.
- **JXA: JSDoc on top-level functions** (`/** @param {string} name @returns {{id:number}} */ function addItem(name) {…}`). Reuse `interop/node/src/generate.mjs` through the TypeScript checker with `allowJs`/`checkJs`, so the same type mapping applies. Alternatively use the same `@rutis.handler` grammar for consistency. JXA handlers must be synchronous (no event loop); reject `async` functions.
- Compile-time terminology: compiling `tell application "X"` blocks needs X's dictionary, so X must be installed where compilation happens [doc/inferred; untested here]. Compile at **mount** time on the target machine, and treat a missing app as a typed mount error.

---

## 9. Platform status and precedents

**Status:**
- **AppleScript** 2.8 still ships in macOS 26.6 (component build 410, rebuilt for 26.6.1) [measured]. The language has had no real evolution in years.
- Apple eliminated its Mac Automation team in 2016 ([appscript status page](https://appscript.sourceforge.io/status.html)).
- **JXA**: the `JavaScript.component` is still version 1.1, `CFBundleVersion 1`, "Copyright © 2013" [measured]. Its release notes end at [OS X 10.11](https://developer.apple.com/library/archive/releasenotes/InterapplicationCommunication/RN-JavaScriptForAutomation/Articles/OSX10-11.html), and hhas calls it "effectively unmaintained and unsupported by Apple" ([appscript status](https://appscript.sourceforge.io/status.html)).
  - Its JS *engine* is the system JavaScriptCore, so modern syntax works [measured: private fields, `?.`, `??`, `flat`, BigInt, WeakRef].
  - It has no `setTimeout` and no microtask turn, so async code doesn't run.
  - Its ObjC bridge has crash-level sharp edges (§2.2).
- **Shortcuts** in macOS 26 gained Mac automations (folder/drive triggers) and Apple Intelligence actions ([Tom's Guide](https://tomsguide.com/computing/software/apples-shortcuts-app-is-getting-a-huge-upgrade-in-ios-26-and-macos-26-heres-how-it-will-help-you)).
  - CLI: `shortcuts run <name-or-id> [-i input] [-o output] [--output-type UTI]` [measured: `shortcuts help run`].
  - A shortcut is a one-shot, stateless, user-authored flow without handlers. It is a complementary *target* that a plugin can invoke (e.g. via `do shell script "shortcuts run …"`), not a plugin model.
- **AppleScriptObjC** is available inside any OSA host (`use framework "Foundation"`). It costs ~200 ms on first use per process [measured]. It gives scripts full Cocoa (including `NSJSONSerialization`, tested), and with that, full ability to crash or bypass the send proc.

**Precedents:**

| Product | How it runs scripts | Notes |
| --- | --- | --- |
| Raycast script commands | `#!/usr/bin/osascript` per run; `# @raycast.*` comment metadata; ≤3 text args; exit code + last line | [repo](https://github.com/raycast/script-commands). Its `runAppleScript` util spawns osascript with `humanReadableOutput`, `timeout` (SIGTERM, default 10 s), and AbortSignal ([docs](https://developers.raycast.com/utilities/functions/runapplescript)). Asks users to grant permissions to Raycast, "not Terminal". |
| Alfred | "Run NSAppleScript" in-process (fast, "can block execution of Alfred"; optional compiled cache) vs "Run Script" via osascript | [docs](https://www.alfredapp.com/help/workflows/actions/run-nsapplescript/) |
| Keyboard Maestro | osascript in the background; no user interaction; variables via `KMVAR_*` env and `tell application "Keyboard Maestro Engine"` | [wiki](https://wiki.keyboardmaestro.com/action/Execute_an_AppleScript). Its author documents NSAppleScript's main-thread hazards ([blog](https://www.stairways.com/blog/2014-04-24-nsapplescript-not-thread-safe)). |
| Hammerspoon `hs.osascript` | In-process OSAKit (`initWithSource:language:` + `executeAndReturnError:`) on the Lua main thread; descriptor → Lua via an `NSAppleEventDescriptor+Parsing` category (handles `usrf`) | [docs](https://www.hammerspoon.org/docs/hs.osascript.html), [source](https://github.com/Hammerspoon/hammerspoon/tree/master/extensions/osascript). New script per call; no state. |
| BetterTouchTool | `runAppleScript(code)` from its JS environment returns a Promise | [docs](https://docs.folivora.ai/docs/1106_java_script.html) (execution model not documented) |
| py-applescript (hhas) | NSAppleScript + 'ascr'/'psbr' subroutine events; persistent script state; type conversion | [PyPI](https://pypi.org/project/py-applescript/), [technique](https://appscript.sourceforge.io/nsapplescript.html) |
| `osakit` (Rust) | OSAKit + serde_json + `declare_script!`; main-thread-only check; error numbers dropped | [docs.rs](https://docs.rs/crate/osakit/latest) |
| macos-automator-mcp | MCP server that shells out to osascript with a knowledge base of scripts | [glama](https://glama.ai/mcp/servers/@steipete/macos-automator-mcp) |

Takeaway: the shipping products split into osascript-per-run (robust, slow, stateless) and in-process NSAppleScript/OSAKit (fast, blocks or crashes the host). None of them ships the persistent, out-of-process, per-plugin-isolated host recommended here. py-applescript's persistent-state model is the closest, but it runs in-process.

---

## 10. Proposed design sketch

```text
rutis app process                                      OSA host process (re-exec of the app, or rutis-osa-host)
+-------------------------------+                      +-----------------------------------------------+
| ctx.plugin(osa_mount::Plugin) |  socketpair (fd 3)   | reader thread -> per-plugin serial executors   |
|  Projection (service per      | <==================> | plugin: OSALanguageInstance + OSAScript        |
|  plugin, generated proxy)     |  protocol v1 subset  |   active proc: cancel flag  (-128)             |
|  host services (`host:` ...)  |  (values only)       |   send proc: RUTS call/emit -> rutis           |
|  event forwarding             |                      |              ascr/cmnt (log) -> rutis           |
+-------------------------------+                      |              other targets: allow-list,         |
                                                       |              no-prompt flag, effect tracking    |
                                                       | main thread: run loop (UI-ish OSAX safety)      |
                                                       +-----------------------------------------------+
```

- **Build** (`build.rs`, macOS):
  - parse `@rutis.*` annotations / JSDoc;
  - compile with OSAKit to validate names, arity and labeled forms;
  - generate Rust types, a service proxy per plugin, host-service traits, and event types (reusing rutis-interop's generator patterns).
  - Non-macOS builds generate stubs that fail at mount.
- **Mount:**
  - spawn the helper (inherit responsibility by default; optional disclaimed mode);
  - `hello` → `mount { plugins: [{path, language, config, handlers, targets}], provided, events, emits, consentMode }`;
  - the host compiles each plugin into its own language instance, checks `OSAGetHandlerNames`, checks installed targets, calls `rutisApply(config)`, and returns services.
- **Calls:** `invoke(target=<plugin>, method=<handler>, args)` → the plugin's serial queue → `executeHandler` → `return`/`throw` with the §5.4 classification and an `outcome` field.
- **Cancel:** `cancel{id}` → set the plugin's flag → the active proc returns -128 at the next statement → reply `throw {name: "Cancelled", outcome}`.
- **Dispose:** `rutisDispose()` per plugin in reverse order → close → the helper exits. On release the helper's process group is SIGKILLed (reaping `do shell script` children).
- **Grouping:** one helper per mount, as for Node. Plugins that must not stall each other during CPU-bound work go into different mounts, because AppleScript execution is serialized process-wide.
- **Implementation language:** Rust + objc2 for the helper (no Swift toolchain needed). FFI to declare by hand:
  - `OSASetActiveProc`, `OSASetSendProc`, `OSACompile`, `OSAGetHandlerNames`, `OSADispose` (Carbon/OpenScripting);
  - `AESend`, `AECreateAppleEvent`, `AEGetAttributePtr`, `AEPutParamDesc` (some exist in `objc2-core-services`);
  - `componentInstance` via `msg_send!`.

**Open items to validate on a disposable machine or VM** (forbidden here by the safety rule):
1. The send proc sees app-targeted events from AppleScript `tell` and from JXA `Application(...)`. Expected, since both components support AESending (feature bit 0x10) and JXA `console.log` already traverses it.
2. Apple Event waits release the process-wide execution lock (as `do shell script` does).
3. The prompt/denial matrix for: terminal-launched vs launchd-launched vs disclaimed helper; ad-hoc vs Developer ID; with and without an embedded Info.plist; hardened with and without the entitlement; SSH/no-GUI sessions.
4. `kAEDoNotPromptForUserConsent` via the send proc returns -1744.
5. The `AEDeterminePermissionToAutomateTarget` result matrix.
6. Stability of retained app object specifiers across app relaunches.
7. Compile-time terminology lookup for non-running and uninstalled targets (does it launch the app? which error?).

---

## 11. Verification of the old roadmap's AppleScript/JXA claims

| Roadmap claim | Verdict |
| --- | --- |
| AppleScript and JXA can share one runner/transport layer | **Confirmed.** OSAKit hosts both identically; same descriptor bridge, active proc and send proc [measured]. |
| Initial version may just call a script-execution tool; evaluate a resident OSA host later | **Refuted as an ordering.** The resident host is ~250 lines (Swift prototype). It is ~700× cheaper per call and fixes problems the osascript path can't: state, typed data, precision, locale, `.scpt` write-back, cooperative cancel, log capture, callbacks. Build it first. |
| Pass business params as handler args or structured data; never interpolate into source | **Confirmed feasible and necessary.** `executeHandler` with descriptors; Unicode, quotes and newlines arrive intact [measured]. |
| Descriptors / app object refs are not general JSON; object capability needs a resident runner keeping verifiable locators; never fake live objects with strings | **Confirmed.** Opaque `$ref` round-trip works in a resident host. App specifiers are re-resolvable paths, not identities. |
| Declare target apps; test not-installed / not-running / denied paths; test with the packaged runner, not the dev terminal | **Confirmed, with a mechanism.** Responsibility follows the launch chain; locally everything spawned is attributed to `claude.app` [measured]. Add send-proc allow-list enforcement and a no-prompt mode. |
| Cancel stops waiting / tries to stop the script; actions sent to the app may continue; report unknown; don't replay; never kill user apps | **Confirmed, and refined.** Cooperative cancel exists (`OSASetActiveProc`), but `try` can swallow it and blocking commands finish first. Killing the runner leaves `do shell script` children running. Report `Unknown`, or `NoEffectsObserved` when the send proc saw no events. |
| GUI scripting needs separately declared and validated permissions | **Confirmed** (Accessibility for the responsible process; -1719/-25211). |

---

## Appendix A — Experiments (commands and key outputs)

Directory: `/private/tmp/claude-501/-Users-eric8810-Code-rutis--claude-worktrees-multilingual-expansion-research-3f78c4/ddf5e072-c6f4-44b6-ac48-2731b08a1b86/scratchpad/experiments/osa`
Swift probes were built with `swiftc -O -o X X.swift -framework OSAKit [-framework Carbon]`.

**A1. osascript startup** — `python3 bench.py 20`
```
baseline /usr/bin/true                                            med=   1.6 ms
osascript -e 'return 1'                                           med=  41.2 ms (rerun 36.3)
osascript -l JavaScript -e '1'                                    med=  30.8 ms (rerun 29.0)
osascript hello.applescript (text source, compile each time)      med=  41.4 ms
osascript hello.scpt (precompiled)                                med=  37.9 ms
osascript AS + use framework Foundation                           med= 197.5 ms
osascript JXA + ObjC.import Foundation                            med=  38.7 ms
```
**A2. argv and output styles** — `osascript -s s args.applescript 'plain' 'with "quotes"' $'multi\nline' '' 'üñî\u4e2d\u6587😀' '42' '-e'`
```
{{text, 5, "plain"}, {text, 13, "with \"quotes\""}, {text, 10, "multi
line"}, {text, 0, ""}, {text, 6, "üñî\\u4e2d\\u6587😀"}, {text, 2, "42"}, {text, 2, "-e"}}
```
`osascript -s s -e 'return "a" & tab & "b\\c" & linefeed & "d" & return & "e"' | od -c` → raw `\t`, `\n`, `\r` bytes inside the literal; only `\\` is escaped.
Ambiguities: `return {}` → h=`` s=`{}`; `return ""` → h=`` s=`""`; `{"a",{"b"}}` and `{{"a","b"}}` → h=`a, b`; `return 1/3` → `0.333333333333` (h and s); `return 2147483647` → `2.147483647E+9`.
**A3. errors** (zh-Hans system):
```
osascript -e 'error "boom: custom" number 1234'   -> 6:20: execution error: boom: custom (1234)        rc=1
osascript -e 'return "abc" as integer'            -> 16:23: execution error: cannot coerce "abc" to type integer (-1700)
LANG=en_US.UTF-8 osascript -e 'return "abc" as integer'  -> same localized message
osascript -e 'return (1 +'                        -> 11:11: syntax error: expected an expression, but found the end of the script (-2741) [translated]
osascript -e 'error "just text"'                  -> 6:17: execution error: just text (-2700)
osascript -e 'do shell script "echo out; echo oops >&2; exit 3"' -> 0:49: execution error: oops (3)
osascript -l JavaScript -e 'throw new Error("boom")'           -> execution error: Error: Error: boom (-2700)
osascript -l JavaScript -e 'throw {errorNumber: 42, message: "m"}' -> execution error: Error: [object Object] (42)
```
Localization source: `./locprobe_plain` (no Info.plist) → `-1700 Can’t make "abc" into type integer.`; `./locprobe_mixed` (embedded Info.plist with `CFBundleAllowMixedLocalizations`) → Chinese; `./locprobe_mixed -AppleLanguages '(en)'` → English. `/usr/bin/osascript` embeds `CFBundleAllowMixedLocalizations = true`, `CFBundleIdentifier = com.apple.osascript`.

**A4. persistent JXA loop** — `python3 pingpong.py osascript -l JavaScript loop.js` → `spawn+first reply: 36.6 ms; per-call n=1000 min=0.142 med=0.167 p99=0.217 max=0.926 ms`. JXA runner hosting AppleScript via OSAKit — `python3 jxadrive.py` → `load … after 66.8 ms`; `med=0.344 p99=0.538 ms`; `bump 5 → 5, bump 7 → 12`, `fail → {number: 4242}`.

**A5. JXA out-param crash** — `osascript -l JavaScript ref_control.js nserror` → exit 139 (Ref()[0] on `NSError**`); `ref_control2.js nserror-box|nsapplescript-box|osakit-box` with `$()` → works: `code=260 domain=NSCocoaErrorDomain`; `num=77 msg=x`; full OSAKit error dict.

**A6. OSAKit state, case, arity, labeled handlers** — `osascript -l JavaScript osakit.js counter.applescript`:
```
["bump 5 (descriptor arg)",{"value":5}]  ["bump 7 (descriptor arg)",{"value":12}]
["getState (mixed case name)",{"value":{"«kind»":"state","hits":12,"history":[5,7]}}]
["getstate (lower case name)",{"value":{"«kind»":"state","hits":12,"history":[5,7]}}]
["bump with NSNumber arg",{"value":13}]  ["bump with text arg \"3\"",{"value":16}]   (text silently coerced)
["fail",{"error":{"number":4242,"message":"plugin failed with 9"}}]
["no such handler",{"error":{"number":-1708}}]  ["wrong arity",{"error":{"number":-1721,…}}]
["getScratch before set",{"error":{"number":-2753,…}}]  ["setScratch",{"value":"kept"}]  ["getScratch after set",{"value":"kept"}]
```
`./probe7 $EXP`: `AS greet (labeled) positional -> ERROR -1701 The name parameter is missing for greet.`; `fetchItems (prepositional) -> -1701`; `run via executeHandler -> -1708`; JXA `bump 5 → 5, 7 → 12`, `getState → {usrf:[hits,12,list,[1,"two",msng],nested,{usrf:[ok,true]}]}`, `fail → 4242`.
`./casetest`: `getState/getstate/GETSTATE → camel` (both APIs); `|ExactCase|` → -1708 by any spelling.
Date-literal compile failure (`osakit_probe`): `-30720 Invalid date and time date Friday, October 3, 2026 at 10:00:00.`

**A7. language instances, TIDs, parallelism** — `./probe3 $EXP`: `OSAScript(contentsOf:languageInstance:using:)` + separate instance → `executeHandler -1708`; `OSAScript(source:from:languageInstance:using:)` → `5, 12, 13`; two scripts on shared instance → `s1: 5, 10; s2: 1`; NSAppleScript + psbr → `n1: 5, 10; n2: 2`.
`./probe4 $EXP`: `shared instance: a set TIDs '|A|'; b reads TIDs: |A|`; `separate instances: … d reads TIDs:  ; c reads: |C|`.
`./par $EXP` (×2): `1 → 239/244 ms; 2 → 487/490 ms; 4 → 970/975 ms wall; user CPU ≈ wall`.
`python3 block.py`: `delay 1 → fast call done at 101 ms`; `do shell script sleep 1 → 104 ms`; `spin (CPU) → fast done at 487 ms (= slow)`.
`./mainthread blocked` → `time to GMT` completed in 22.9 ms with main parked.
`StandardAdditions.osax` Info.plist: 29 thread-safe, 17 not (`JonsgClp JonsiClp JonspClp aevtmvol gtqpchlt sysoGMT_ sysochcl sysochra sysochur sysodisA sysodlog sysonotf sysonwfl sysoppcb sysostdf sysostfl sysostor`).

**A8. descriptor kinds and JSON conversion** — `osascript -l JavaScript values.js values.applescript`:
```
vBig -> doub 3e+09 | vMissing -> type 'msng' | vDate -> 'ldt ' | vPosixFile -> furl | vAlias -> alis | vClass -> type 'ctxt'
vCtx ({name:"n", id:7, class:"c"}) -> ERROR -1700
vUserKeys -> { 'kind':4, 'usrf':[ "alpha",1, "Mixed Case Key",2, "with space",3, "kind",5 ] }
vEmptyRec -> list [ ] | vScriptObject -> 'scpt'(…) | vNull -> type 'null' | vMinusZero -> doub -0
vUnitType -> 'inch' | vRef -> 'obj '{ form:'indx', want:'cobj', seld:2, from:[1,2,3] } | vData -> 'rdat'
```
`osascript -l JavaScript args.js values.applescript`: `classOf 536870911 → integer`, `536870912 → real`, `int32 2147483647 echoed → doub 2.14748e+09`, `classOf {} → list`, `alphaOf record → {a:"A", b:"MC", c:3, d:list}`, unicode echo intact.

**A9. Swift OSA host prototype** — `python3 drive.py` (excerpt):
```
load counterA 37.1 ms … load counterB 14.8 ms … load js 16.6 ms
A.bump(5) 5 · A.bump(7) 12 · B.bump(1) 1
A.fail {'number': 4242, 'message': 'plugin failed with 3', 'range': [496, 25], 'expectedType': 'cobj', 'app': 'osahost', 'offendingObject': {'$ref','$type':'scpt'}}
A.bump() wrong arity {'number': -1721, 'partialResult': {...}}
values.vUserKeys {'alpha': 1, 'with space': 3, 'Mixed Case Key': 2, 'kind': 5, '«kind»': 4}
values.vEcho(date) {'$date': '2026-10-03T10:00:00+08:00'} · vEcho(file) {'$file': '/tmp/x.txt'}
values.vRef -> {'$ref': 'r4', '$type': 'obj '} · objects.deref(ref back) 20
js.getState {'list': [1, 'two', None], 'nested': {'ok': True}, 'hits': 5}
round-trip n=2000: min=0.037 med=0.054 p99=0.080 max=0.234 ms
cancel spin: reply after 357 ms: {'number': -128, 'message': 'User canceled.'}
```
`python3 coldstart.py` → `median hello 5.3 ms, median first result 40.2 ms` (very first run after linking: hello at 562 ms).
`./inproc $EXP` → `in-process executeHandler(bump) n=5000: min=20.4 med=50.8 p99=94.3 us`.

**A10. cancellation, timeouts, kill** — `./probe5 $EXP`:
```
spin cancelled after 300 ms -> ERROR -128 User canceled. ; returned 59.3 ms after cancel
sleepy (delay 5) -> ERROR -128 ; returned 30.7 ms after cancel
caught (try around spin) -> "caught -128"                 (script swallowed the cancel)
shell (do shell script "sleep 2") -> ERROR -128 ; returned 1714.5 ms after cancel   (command completed first)
[sendProc] event syso/exec … mode=0x1063 timeoutTicks=-1 ; event misc/curd …
```
`osascript -e 'with timeout of 1 second' -e 'do shell script "sleep 3; echo finished"' -e 'end timeout'` → `finished`, real 3.08 s.
`osascript -e 'do shell script "sleep 2; echo written > marker.txt"' & … kill -9` → osascript rc=137, `marker.txt` contains `written`.
`.scpt` write-back: `osascript persist.scpt` ×3 → `1 2 3`, mtime changed; `osascript persist.applescript` ×2 → `1 1`.

**A11. handler enumeration and canonical source** — `./probe6 names.applescript`:
```
handlers: [evnt aevt/oapp, "bump", "greet", "fetchItems", evnt aevt/quit, evnt misc/idle, evnt aevt/odoc]
properties: ["hits", "label", "Inner"]
OSAGetSource(handler greet) -> return g & ", " & who
```
`osadecompile messy.scpt` → `to AddItem(theName, qty)` / `on ping()` / `end AddItem`, comments preserved; run-only copy → empty source.

**A12. libraries** — `OSA_LIBRARY_PATH=$EXP/libs osascript -e 'tell script "RutisCounter" to …'` → `-1728`; `OSA_LIBRARY_PATH=$EXP/libs ./libprobe AppleScript 'tell script "RutisCounter" to return {bump(2), bump(3)}'` → `[ 2, 5 ]`; JXA `Library("RutisJS")` in custom host → `7`. `codesign -dv /usr/bin/osascript` → `Platform identifier=26`.

**A13. callbacks and logs** — `./callback $EXP manager` → `useHost(21) -> {got:"host answered kv.get on bg thread", doubled:42}`, `1000 host calls: 0.110 ms each`, `envProbe -> socket=/tmp/example.sock ppid=<helper pid>`; `./callback $EXP sendproc` → `{got:"host answered via send proc", doubled:42}`, `0.005 ms each`. `./logprobe AppleScript 'log "hello…"'` → `[send proc] ascr/cmnt direct=hello from AppleScript`; JXA `console.log` → same event.

**A14. AppleScriptObjC** — `./asocprobe $EXP` → script #1: compile 24.8 ms, first calls 226 ms; #2/#3: ~20 ms; `toJSON({k:7}) → {"k":7}`, `fromJSON(...) → usrf record`. (A variable named `data` is a reserved class name: compile error -2741.)

**A15. crash blast radius** — `./libprobe JavaScript 'ObjC.import("stdlib"); $.abort()'` → exit 134; `./libprobe AppleScript 'do shell script "kill -9 $PPID"'` → exit 137; `tell current application to quit` → `-1708`, host alive.

**A16. responsibility (no Apple Events)** — `./resp` → `pid … (resp) -> responsible pid 37097 (…/claude.app/Contents/MacOS/claude)`; ancestry `zsh → claude → Claude.app/Contents/Helpers/disclaimer → Claude`. `./disclaim 0` → responsible = claude; `./disclaim 1` (with `responsibility_spawnattrs_setdisclaim`) → responsible = the child itself.

**A17. versions** — `osascript -e "AppleScript's version"` → `2.8`; `AppleScript.component` `CFBundleShortVersionString 2.8`, `CFBundleVersion 410`; `JavaScript.component` `1.1`, `CFBundleVersion 1`, `Copyright © 2013 Apple Inc.`; `osalang -l` → `ascr appl cgxervdh AppleScript`, `jscr appl cgxe-v-h JavaScript`; JXA modern-syntax probe → `42 | nullish | 3 | 1 | function | undefined | function | undefined | object | function`; microtasks don't run (`not run`).

## Appendix B — Sources
- `man osascript`, `man osalang`, `man tccutil` (macOS 26.6.2); SDK headers `OSAKit/OSAScript.h`, `OSALanguage.h`, `OSALanguageInstance.h`, `OpenScripting/OSA.h`, `ASDebugging.h`, `AE/AppleEvents.h`, `CarbonCore/MacErrors.h`, `LaunchServices/LSConstants.h`, `HIServices/AXError.h` (MacOSX26.5 SDK).
- AppleScript Release Notes 10.6 (thread safety): https://developer.apple.com/library/archive/releasenotes/AppleScript/RN-AppleScript/RN-10_6/RN-10_6.html
- AppleScript Release Notes 10.11 (OSA_LIBRARY_PATH): https://developer.apple.com/library/archive/releasenotes/AppleScript/RN-AppleScript/RN-10_11/RN-10_11.html
- JXA Release Notes 10.10 / 10.11: https://developer.apple.com/library/archive/releasenotes/InterapplicationCommunication/RN-JavaScriptForAutomation/Articles/OSX10-10.html , https://developer.apple.com/library/archive/releasenotes/InterapplicationCommunication/RN-JavaScriptForAutomation/Articles/OSX10-11.html
- AppleScript Language Guide — control statements (`with timeout`, `ignoring application responses`): https://developer.apple.com/library/archive/documentation/AppleScript/Conceptual/AppleScriptLangGuide/reference/ASLR_control_statements.html ; error numbers: https://developer.apple.com/library/archive/documentation/AppleScript/Conceptual/AppleScriptLangGuide/reference/ASLR_error_codes.html
- Mac Automation Scripting Guide: https://developer.apple.com/library/archive/documentation/LanguagesUtilities/Conceptual/MacAutomationScriptingGuide/
- Apple Events entitlement: https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.automation.apple-events ; NSAppleEventsUsageDescription: https://developer.apple.com/documentation/bundleresources/information-property-list/nsappleeventsusagedescription ; Hardened Runtime: https://developer.apple.com/documentation/security/hardened-runtime
- Apple Support, Automation privacy: https://support.apple.com/guide/mac-help/mchl108e1718/mac
- Responsible process: https://www.qt.io/blog/the-curious-case-of-the-responsible-process ; Quinn on ad-hoc signatures and TCC: https://developer.apple.com/forums/thread/795739 ; responsible code for agents: https://developer.apple.com/forums/thread/694948
- AEDeterminePermissionToAutomateTarget + hardened runtime: https://developer.apple.com/forums/thread/130949 ; https://mjtsai.com/blog/2018/08/31/aedeterminepermissiontoautomatetarget-added-but-aepocalyse-still-looms/ ; usage description: https://mjtsai.com/blog/2018/08/23/apple-events-usage-description/ ; https://developer.apple.com/forums/thread/109561
- CLI + Info.plist + TCC (2025): https://steipete.me/posts/2025/applescript-cli-macos-complete-guide ; hardened parent blocking child Apple Events: https://claudeissues.com/issue/52712-macos-hardened-runtime-blocks-apple-events-from-child-processes-missing-com-appl
- TCC.db columns: https://www.rainforestqa.com/blog/macos-tcc-db-deep-dive (via search summary)
- Assistive access errors: https://forum.keyboardmaestro.com/t/execute-an-applescript-failed-osascript-is-not-allowed-assistive-access/34490
- OSA_LIBRARY_PATH discussion: https://forum.latenightsw.com/t/applescript-execution-error-cant-find-script-in-non-default-location/128/22
- NSAppleScript thread safety: https://www.stairways.com/blog/2014-04-24-nsapplescript-not-thread-safe ; https://developer.apple.com/forums/thread/103443
- Calling handlers from NSAppleScript: https://appscript.sourceforge.io/nsapplescript.html ; status of AppleScript/JXA: https://appscript.sourceforge.io/status.html ; py-applescript: https://pypi.org/project/py-applescript/
- Raycast: https://github.com/raycast/script-commands , https://manual.raycast.com/script-commands , https://developers.raycast.com/utilities/functions/runapplescript
- Alfred: https://www.alfredapp.com/help/workflows/actions/run-nsapplescript/ ; Keyboard Maestro: https://wiki.keyboardmaestro.com/action/Execute_an_AppleScript ; Hammerspoon: https://www.hammerspoon.org/docs/hs.osascript.html , https://github.com/Hammerspoon/hammerspoon/tree/master/extensions/osascript ; BetterTouchTool: https://docs.folivora.ai/docs/1106_java_script.html ; macos-automator-mcp: https://glama.ai/mcp/servers/@steipete/macos-automator-mcp
- Rust crates: https://crates.io/crates/objc2-osa-kit , https://crates.io/crates/objc2-foundation , https://crates.io/crates/objc2-core-services , https://crates.io/crates/objc2-carbon , https://docs.rs/crate/osakit/latest (sources inspected from the crates.io tarballs)
- Shortcuts in macOS 26: https://tomsguide.com/computing/software/apples-shortcuts-app-is-getting-a-huge-upgrade-in-ios-26-and-macos-26-heres-how-it-will-help-you
