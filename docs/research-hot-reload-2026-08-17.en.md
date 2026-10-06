# Survey of Hot-Reloading Precedents (2026-08-17)

> For [`design-rust-port.md`](design-rust-port.md) v4. Question: are there precedents for dependency-gated hot reload?
> **Conclusion:** code-level hot replacement (dylibs / hot patches) and lifecycle-level reload (unload/reassemble instances) are orthogonal problems. rutis (then min-cordis) v4 addresses the latter, with precedents in the Erlang / OSGi family. The former (`dyxlib` / subsecond) can complement it.

## 1. Two kinds of hot update

| | Code-level (replace machine code) | Lifecycle-level (replace running instance) |
|---|---|---|
| What changes | Load a new dylib / patch a running process | Code is unchanged; clean up the old instance and rerun assembly, possibly with new config |
| Examples | hot-lib-reloader, subsecond | Erlang hot upgrade, OSGi, Cordis update/restart |
| rutis v4 | **Out of scope** (orthogonal and composable) | **Core M2** (fiber eviction/reload/restart) |

## 2. Code-level precedents (Rust ecosystem)

### hot-lib-reloader (practical Bevy community tool)

- Mechanism: put mutable code in a dylib crate, watch files, rebuild, `dlopen` the new library, then use a macro-generated wrapper to swap function pointers. State lives in the host; the library exposes only `#[no_mangle]` pure functions.
- Lessons: cross-boundary types cannot use generics or inline functions; stability requires the same compiler/version; on Windows, DLLs stay locked, so copy under a new name before loading.
- Sources: <https://robert.kra.hn/posts/hot-reloading-rust/> · <https://github.com/rksm/hot-lib-reloader-rs> · Bevy's official ECS hot-reload issue points to it (#15613).

### Subsecond (Dioxus 0.7 hotpatch, 2025)

- Mechanism: intercept rustc's link stage, drive compilation manually, and patch the running process directly. No dylib crate split is needed; supports macOS/Windows/Linux/iOS/Android; was in alpha.
- Position: sub-second feedback for frontend/live iteration, not a plugin lifecycle tool.
- Sources: <https://docs.rs/subsecond> · HN 44369642 · Bevy #19296 discussion of hotpatching systems.

### abi_stable / nullderef family (abandoned)

- Goal: safely interoperate between dylibs built with different rustc versions.
- Conclusion: high cost and many pitfalls (stable ABI layer, type restrictions); most projects abandoned it in favor of data-driven designs.
- Sources: <https://nullderef.com/blog/plugin-abi-stable/> · <https://docs.rs/abi_stable/>.

### Bevy itself (corroborating evidence)

- Officially supports hot reload of **assets** (data) only. Runtime hot replacement of ECS systems/plugins remains in issues (#15613, #19296) that point to external tools. There is **no precedent for framework-level plugin unload/reload**, corroborating the v4 survey.
- Source: <https://bevy-cheatbook.github.io/assets/hot-reload.html>.

## 3. Lifecycle-level precedents (rutis's model)

### Erlang/OTP (the canonical precedent)

- Hot-code upgrade protocol: **suspend process → load new version → migrate state with `code_change/3` → resume**; supervisor trees restart by dependency order. “Upgrade is not an exception; it is routine.”
- Mapping: fiber unload → re-apply is roughly suspend/load/code_change/resume; Cordis `update()` / `restart()` are the JS descendant of this idea.
- v4 difference: **no state migration** (the `code_change` step). Reload means clean unload + reassembly; evaluate state migration after M4.
- Sources: <https://learnyousomeerlang.com/relups> · <https://stackoverflow.com/questions/37368376/>.

### OSGi (Eclipse plugin runtime, Java)

- Bundle lifecycle (RESOLVED → STARTED → STOPPED), service registry, and declarative dependencies: **activate only when dependencies are satisfied; unbind consumers automatically when a provider disappears**. This closely matches Cordis inject gating/eviction and is an established example of registry plus dependency-driven activation.

### .NET `AssemblyLoadContext` (counterexample: why unloading is hard)

- A collectible ALC can theoretically unload an assembly, but **one leaked reference keeps the old assembly in memory**; `TypeLoadException` and failed unloads are common in practice.
- Lesson: Rust ownership addresses this directly through effect disposer lists, terminal state via watch, and `Arc` reaching zero without GC-held references.
- Source: <https://jordansrowles.medium.com/real-plugin-systems-in-net-assemblyloadcontext-unloadability-and-reflection-free-discovery-81f920c83644>.

### Frontend HMR (React Fast Refresh, etc.)

- Replace modules, preserve what state is safe, and refresh the full page otherwise; error boundaries handle the replacement window.
- Related to rutis's “self-access remains valid during cleanup” (three-stage provide unload): **what happens to code depending on the old component at the moment of replacement?**

## 4. Implications for v4

1. **Position:** M2 lifecycle hot reload (provider unload → evict consumers → reload once ready) follows Erlang/OSGi. Lack of Rust precedent is not itself a risk; precedents exist in other language models, and Rust ownership is a stronger foundation.
2. **No state migration:** reload is clean reassembly (omit Erlang's `code_change` step), as defined in §1.
3. **Orthogonal composition point (candidate for M4):** subsecond / hot-lib-reloader obtains new code; rutis safely swaps instances: dispose old plugin → evict dependents → assemble new plugin → reload dependents. Code-level tools stay outside the core and are not coupled to it.
4. **Record the risk:** `TypeId` is unstable across dylibs (Bevy StableTypeId issue). If a dylib design is combined in future, registry keys need stable IDs; dynamic dispatch inside one binary is unaffected.
