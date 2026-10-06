# Decision Record: Keep Other Languages Out of rutis; Make Cordis Runtime a Plugin (2026-10-03)

This records a discussion that started with expanding multilingual support and narrowed down to changing only P6. It preserves the reasoning for future reference.

## 1. Starting point

The original roadmap ([protocol plugin languages](roadmap-protocol-plugin-languages-2026-09-26.en.md), now marked historical) planned to add Python, Go, Shell, PowerShell, and AppleScript/JXA after Cordis. After Cordis mounting was nearly complete, we revisited how other languages should work, focusing on system languages such as PowerShell, Bash, and AppleScript. Python was informed by the min-cordis Python implementation.

## 2. Research

Six parallel investigations produced reports in [plan/analysis/multilang](plan/analysis/multilang/README.en.md).

| Area | Main conclusions |
| --- | --- |
| Existing protocol contract | Most of protocol v1 is language-independent, but several details assume Node: request IDs accept only `node:` prefixes, the launch command is fixed to `node`, and the generator emits Rust source instead of a neutral interface description. Other implicit assumptions: Rust futures are lazy; an incorrect `path` can deadlock; the runner must exit with code 0 after disposal. |
| Python / min_cordis | Technically feasible; the runner can be ported almost line by line. But min_cordis is not published and has no plugins. It needs new conventions (seam classes, typed Context, an `EVENTS` table), and virtual environments cannot be moved for deployment. |
| PowerShell | A feasible shape is one persistent `pwsh` per mount and one runspace per plugin. It has many pitfalls: thread-affinity errors can crash the process; `Stop()` can block on child processes (measured at 102 seconds); `ConvertTo-Json` cannot be used. Its value is mainly on Windows, while interop supports only Unix. |
| Bash | A feasible shape is a process per call, results over fd 3, cancellation by process group, and interface declarations through argc annotations. Only data can be passed; processes that escape the group with `setsid` cannot be reaped. |
| AppleScript / JXA | A feasible shape is a persistent native OSA host, with `send` proc for host calls and log capture. TCC authorization belongs to the responsible process and cannot be tested in CI. The platform is in maintenance mode and JXA is mostly unmaintained. |
| Precedents | Neovim is removing its “one shared host per language” model (neovim#27949); Pulumi's “extract per language → neutral description → generate typed SDK” is closest to rutis. Lessons: do not silently downgrade, use a separately refreshed manifest, or tie protocol versions to host releases. |

A full draft design was written around framework languages (Node, Python) and script languages (PowerShell, Bash, AppleScript), with a neutral interface description and a shared Rust code generator, staged as P0–P7. That draft was withdrawn.

## 3. Counterarguments

**Value:**

1. Generating typed bindings for scripts serves the wrong users. Rust users can use `Command`; scripts are mainly for agents, which need runtime discovery and dynamic invocation.
2. A complete Python solution would support an ecosystem that does not exist: there are no min_cordis plugins, and min_cordis has not been released.
3. Applying the Cordis model to scripts is overengineering. Scripts do not need dependency reloads, service replacement, cleanup trees, or child plugins; what remains is typed RPC.
4. The maintenance cost is disproportionate to the benefit. One person maintaining five runtimes would likely leave all of them at demo quality.
5. PowerShell has few users outside Windows; AppleScript has no concrete use case.

**Technical limits:**

1. Scripts can pass only data, limiting cross-language composition to a command-call layer.
2. Cancellation is best-effort and can report only “unknown” outcomes.
3. The types are weak in practice: Bash has only strings, and PowerShell `[OutputType]` is a hint.
4. Builds would depend on pwsh, virtual environments, or macOS, complicating CI and cross-compilation.
5. Python would be pinned to one event-loop thread, suitable only for control-plane work.
6. Letting an agent run arbitrary system scripts without a sandbox is a real security risk.

## 4. Reasoning

1. **These should all be plugins.** In rutis's own model, language support should be split into a runtime plugin that provides execution, each script as a plugin or loader row that injects it, and agent tools as plugins. Multilingual support then becomes an optional plugin rather than a framework feature.
2. **If they are plugins, they do not belong in the rutis library.** They are outside the core's responsibility and can use the public API out of tree. Build-time code generation does not fit the plugin model, another sign that scripts should not use typed bindings.
3. **The only cross-language work rutis should do is mount Cordis in both directions.** Services, dependency gating, cleanup, and events map directly between the two frameworks. Only one direction should be implemented: on 2026-10-02 rutis was chosen as the host, and the reverse direction remains frozen.
4. **Check whether Cordis mounting itself is fully plugin-based.** Static mounting is: generated mounts are ordinary rutis plugins. The P6 dynamic path is only partly so: JS rows are plugins, but their shared Node process and Cordis Context live in an `OnceCell` on `InteropResolver`, outside lifecycle management. This means a process crash leaves rows holding a dead process without any plugin entering a failure state; host services do not participate in dependency gating; and diagnostics do not show the runtime.

## 5. Decision

| Area | Decision |
| --- | --- |
| PowerShell, Bash, AppleScript/JXA | Keep out of rutis. Build plugins outside the library if needed. |
| Python (min_cordis) | Same paradigm, but there are no users now; defer as “out of tree, revisit later.” |
| Reverse direction (Cordis hosting rutis) | Keep frozen. |
| Neutral interface description, abstract launch command, relaxing the `node:` prefix | Do not do these; they only pave the way for other languages. |
| P6 | **Change it:** make the Cordis runtime a plugin (#109); see [Cordis runtime plugin design](design-cordis-runtime-plugin-2026-10-03.en.md). |
| Requirements document | Add the conclusion to §8 (done). |

## 6. When to reconsider

- If a real min_cordis plugin needs mounting, build a Python runtime plugin using the Cordis mounting model; the runner port described in the research report can be reused.
- If an agent needs system scripts as tools, build a command-style runtime plugin out of tree, with runtime discovery and JSON Schema, but no code generation. Reuse the Bash and PowerShell reports' findings on processes and cancellation.
- Start PowerShell and Windows support together if Windows automation becomes necessary.

## 7. Later revision (2026-10-03)

In a discussion later the same day, Python, Swift, and potentially Go were brought back in, but not as “one complete framework per language” (the withdrawn design from §2). Instead, each language gets one runtime plugin and its plugins are leaves: the paradigm is implemented once in rutis, with one process per language and no IPC for same-process calls. See [Multilingual Plugins: One Runtime Plugin per Language](design-multilang-runtimes-2026-10-03.en.md), §10, for which conclusions in §5 remain and which were revised. Shell-style languages remain outside the plugin protocol.
