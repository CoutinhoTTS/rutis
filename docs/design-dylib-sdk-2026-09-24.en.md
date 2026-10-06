# Dylib SDK Design for First-Party Plugins (2026-09-24)

> Related to [#45](https://github.com/arcships/rutis/issues/45), tracked by [#52](https://github.com/arcships/rutis/issues/52).
> Status: design with Linux implementation and automated validation. Usage instructions: [dylib-sdk-implementation](dylib-sdk-implementation.en.md). **The default does not change:** first-party plugins are statically linked by default and the host remains a single binary. This design offers an optional path for the limited case of changing first-party plugin code without republishing the host.

## 1. Goals and non-goals

**Goals**

1. A trusted first-party plugin can be built and released independently, loaded by the host at runtime, and updated through rutis's existing lifecycle: unload old generation → evict consumers → assemble new generation → reload consumers.
2. Plugin and host share arbitrary Rust types (services, events, `Ctx`, tokio) without serialization.
3. Reject incompatible plugins before loading, with a readable reason; do not use “`dlopen` succeeded” as a compatibility test.

**Non-goals**

- Third-party plugins, untrusted code, crash isolation, and forced termination use [protocol plugins](design-protocol-plugins-2026-09-25.en.md) (#46–#48).
- Unloading old code (`dlclose`) is not in the first version; see §9.
- State migration (Erlang `code_change`) is out of scope. An update is a clean unload and reinstall, consistent with [research-hot-reload](research-hot-reload-2026-08-17.en.md).
- Compatibility across rustc versions (the abi_stable approach) is out of scope; reassess separately if SDK changes become too frequent.
- Updates are not seamless: there is a gap between unload and load; see #51.

## 2. Relationship to existing decisions

- **Single-binary distribution** ([design-dual-core](design-dual-core-2026-08-20.en.md)) remains the default. Dylib support is an **optional host build variant**; only that variant needs a separate launcher and ships `librutis_sdk.so` and `libstd-*.so` (§10).
- **Lifecycle hot reload and code hot replacement are orthogonal** ([research-hot-reload](research-hot-reload-2026-08-17.en.md) §4.3). This design is the “orthogonal combination point” reserved there: dylib supplies new code; rutis safely swaps it in.
- **Deleted hotplug experiment** (`14f8c53`, cdylib + C ABI + JSON strings) incurred serialization without isolation; `dyn Plugin` could not cross the boundary. This design takes the opposite approach: Rust ABI plus a shared SDK dylib, passing `Box<dyn PluginFactory>` directly across the boundary.
- **TypeId across dylibs** (research-hot-reload §4.4): one SDK dylib gives shared types a single definition, so TypeIds agree (verified in prototype) and registry keys need not become stable IDs. Shared types must be inside the SDK (§4).

## 3. Overall structure

```text
                 ┌──────────────────────── process ─────────────────────────┐
                 │ host (dylib variant)                                     │
                 │   └─ rutis-dylib (host-side loader)                      │
                 │                                                          │
 DT_NEEDED ───▶  │ librutis_sdk.so = rutis + tokio + interface crates + allocator │ ◀── one copy
                 │ libstd-<hash>.so                                         │
                 │                                                          │
 dlopen ──────▶  │ libplugin_a.so (v3)   libplugin_a.so (v4)   libplugin_b.so │
 (RTLD_LOCAL)    │   └─ depends only on SDK; private dependencies linked statically into each .so │
                 └──────────────────────────────────────────────────────────┘
```

| Crate | Form | Contents | Does a change require a new SDK version? |
| --- | --- | --- | --- |
| `rutis-sdk` | `crate-type = ["dylib"]` | Re-exports rutis, tokio, interface crates crossing the boundary; `#[global_allocator]`; SDK identity constants; `export_plugin!` macro | Yes |
| `rutis-dylib` | rlib, host-only dependency | Manifest parsing and validation, content-addressed cache, `dlopen`, `DylibFactory`, module registry | No (host side) |
| Plugin crate | `crate-type = ["dylib"]` | Directly depends only on `rutis-sdk` and its private dependencies | — |

Do not put the loader in the SDK: only the host uses it, and including it would force every plugin to rebuild whenever the loader changes.

## 4. SDK: the compatibility unit

### 4.1 Contents

Include only what **must be shared across the boundary as one copy**:

1. The rustc version (implicit) and `std` (dynamic `libstd-*.so`).
2. `rutis` (global state such as the `InstanceId` counter and registry types).
3. `tokio` (runtime context is thread-local and must be singular, or `tokio::spawn` in a plugin cannot find the host runtime).
4. Interface crates: service and event types exchanged between plugins and between plugins and host.
5. Global allocator (§4.3).
6. Shared plugin configuration carrier, `serde_json::Value` (§7.3).

**Do not include** private plugin dependencies. Link them statically into each plugin `.so`; their types must not cross the boundary (as service keys, events, or values passed between plugins).

### 4.2 Rules for shared dependencies

- If a plugin crate directly depends on a crate already in the SDK (for example `tokio = "1"`) and resolves to the same version, rustc links to the copy in the SDK. Prototype `host2` verified all rutis/tokio symbols came from `librutis_sdk.so`.
- If resolution selects a second semver-incompatible version, a second copy appears and its types have different TypeIds from same-named types in the SDK. CI uses `cargo tree -d` to reject duplicate versions of SDK crates in the plugin dependency graph.
- Service types **defined privately** by a plugin are visible only inside that plugin. Two versions of the same plugin define same-named private types with different TypeIds. Therefore externally provided service keys during replacement must use SDK interface types, or consumers will not find the service after update.

### 4.3 Allocator (required)

Prototype results: a `#[global_allocator]` defined in the host binary **does not apply** to a plugin dylib—the host's counting allocator did not observe a 1 MiB plugin allocation. If the host uses mimalloc and the plugin uses System, plugin allocation followed by host deallocation is undefined behavior.

Rule: **define `#[global_allocator]` only in the SDK**; neither host nor plugin may define one. The prototype confirmed plugin allocations go through it when placed in the SDK. It must be System: with dynamically linked std, internal libstd allocations do not use the SDK allocator (macOS two-level namespaces and Windows); jemalloc with prefer-dynamic also crashes on Linux since rustc 1.71 (unfixed upstream rust-lang/rust#100781 and #114518). Do not switch to mimalloc/jemalloc until upstream fixes this. See [design-dylib-macos-windows](design-dylib-macos-windows-2026-10-03.en.md) §3.5 and §8 R1. `export_plugin!` may add a compile-time check; CI also uses `nm` to check that the plugin `.so` does not export its own `__rust_alloc` implementation.

### 4.4 Panic strategy

SDK, host, and plugins must all use `panic = "unwind"` (`abort` mixed with `unwind` cannot link or has undefined behavior). Panic strategy is part of SDK identity. Prototype confirmed a plugin panic can be caught by host `catch_unwind`; rutis already catches panics at apply/cleanup/listener boundaries (#7), and `export_plugin!` wraps the entry function too.

## 5. Identity and compatibility checks

### 5.1 Why `dlopen` cannot be trusted

Two prototype counterexamples:

| Experiment | Result |
| --- | --- |
| (a) SDK contents change but crate metadata hash does not; plugin is built against new SDK and loaded into a host with old SDK | `dlopen` **succeeds**; only explicit identity comparison catches it |
| (b) SDK tokio features differ (`+net`); plugin built against that version and loaded into host with original SDK | `dlopen` **succeeds**, plugin runs normally by luck; this does not prove compatibility |

Rust symbol-mangling crate hashes catch only some mismatches and cannot detect a changed layout with the same crate name and hash. Two explicit validation layers are therefore required.

### 5.2 L1: declared identity (compile-time constant vs runtime value)

SDK `build.rs` computes `SDK_ID`:

```text
sha256(
  SDK version, full rustc -vV output, target triple,
  SDK transitive dependency tree from release Cargo.lock (name + version + source + checksum),
  enabled features, ABI-affecting profile settings (panic, debug-assertions, overflow-checks; not opt-level or debuginfo),
  normalized RUSTFLAGS (below)
)
```

- `pub const SDK_ID: &str` is **inlined at compile time** into plugin and host artifacts.
- `#[inline(never)] pub fn loaded_sdk_id() -> &'static str` returns the value from the **actually loaded** SDK at runtime.

Reject if these differ (host side §5.4, plugin side §7.1). Prototype (a) used a manually specified identity and was caught by this layer; this does not mean these inputs detect arbitrary source changes. If SDK or path-dependency source changes without version/input changes, L1 may remain the same. L1 helps diagnose configuration; L2 must confirm artifact identity, and plugins must embed L2 rather than trusting only an external manifest. The release script sets the actual resolved lockfile explicitly through `RUTIS_SDK_LOCKFILE`; Cargo does not tell a dependency build script the caller workspace's lockfile path. Without it SDK may still compile (e.g. ordinary downstream build of a published crate), but L1 omits the dependency tree and the artifact cannot be represented as a release SDK.

**Inputs must be machine-independent.** `SDK_ID` is compiled into SDK, so machine-dependent input changes both L1 and L2 and breaks reproducible builds. For example, if two machines use `--remap-path-prefix=/build/a=/target` and `--remap-path-prefix=/build/b=/target`, their raw RUSTFLAGS differ and so would `SDK_ID`; path remapping changes paths in output but cannot change a hash already computed. Therefore normalize RUSTFLAGS by **allowlist**:

- Read `CARGO_ENCODED_RUSTFLAGS`; keep only code-generation or type-layout inputs: `-C target-cpu`, `-C target-feature`, `-C panic`, `-C debug-assertions`, `-C overflow-checks`, `--cfg`, and `-Z` options. Sort them before hashing.
- Explicitly ignore path/link/optimization-only options such as `--remap-path-prefix`, `-L`, `-l`, `-C link-arg(s)`, `-C linker`, `-C debuginfo`, `-C opt-level`, `-C incremental`, and `-C codegen-units`, as well as lint controls such as `-D warnings` and `--cap-lints`.
- If an option is in neither list, `build.rs` fails and requires classification first, so a new option cannot silently enter or escape the identity.
- `build.rs` declares `rerun-if-env-changed` for these inputs.

### 5.3 L2: artifact identity (binary hash)

The evaluation requires “each version number to correspond to one immutable set of validated build artifacts.” L2 implements that:

- Each released SDK version has the SHA-256 of `librutis_sdk.so` (`sdk_artifact`) in its release manifest.
- **Host** embeds the hash of the SDK artifact it links (§5.4).
- **Plugin** also uses a two-stage build: build and hash SDK first, then build plugin with `RUTIS_SDK_ARTIFACT_SHA256`, embedding the hash in its boot blob and runtime `PluginMeta`. This value belongs to the plugin and is not written back into SDK, avoiding a self-referential hash. After plugin build, verify the linked SDK artifact did not change. Packager reads L2 from plugin boot blob and cross-checks SDK release manifest and actual SDK file hash; fail on any mismatch.
- Standalone launcher validates the complete runtime artifact set before executing host (§5.4). After host starts, it uses `dladdr` and related APIs to identify the actual SDK file and compare its hash to the host's embedded value. When loading a plugin, manifest L2, plugin-embedded L2, and host-confirmed L2 must all match.

L2 requires **reproducible builds**: the plugin's Cargo invocation rebuilds SDK from source and must produce byte-identical output. Prototype results:

| Condition | Release SDK hash in two target directories |
| --- | --- |
| No path remapping | Different |
| `--remap-path-prefix=<target>=/target --remap-path-prefix=$HOME=/home` | **Same** |

The prototype used a fixed environment variable for `SDK_ID`, not the actual generation logic in §5.2. Cross-machine reproducibility (different source path, CARGO_HOME, target directory, and username) remains unverified (§11 V1). V1 must use final `build.rs` to generate `SDK_ID` and use different remapping arguments on each machine to verify normalization. Put path remapping in `.cargo/config.toml` and CI, but **exclude it** from L1.

**Fallback if L2 is not reproducible:** release first-party plugins and SDK in the same CI pipeline. Build SDK, then build host and plugins in stages in the same target directory with the same SDK hash embedded, and verify SDK was not rebuilt differently. Publish plugins in batches per SDK version. This is the evaluation's “automatically rebuild all first-party plugins on SDK upgrade” approach; its cost is that “independent releases” become “releases batched by SDK.”

Linux implementation found another Cargo constraint on 2026-09-25: different **transitive dependency feature graphs** for host and standalone plugin change SDK bytes even with identical versions, source, and path remapping. Current `sdk.toml` records host build anchor; plugin packaging includes the host package in the same Cargo build to get the same feature graph, without requiring host binary republishing. A plugin built independently of that graph is rejected if L2 differs. This implements the same-pipeline fallback above; it has not proved that arbitrary independent Cargo graphs produce identical SDKs.

### 5.4 Host binding and startup-check order

Comparing only “runtime SDK” with “SDK used to build the plugin” is insufficient: the host was also compiled against a particular SDK layout. If deployment mistakenly substitutes a same-named SDK whose symbols resolve but whose layout differs, a matching new plugin passes validation while the host still accesses shared types using the old layout, causing undefined behavior. The prototype reproduced this:

| Scenario | Result |
| --- | --- |
| Host built against SDK A; deploy SDK B and plugin built against B; host does no self-check | Startup, load, replacement, and shutdown **all succeed** |
| Same setup, but host compares its inlined `SDK_ID` with `loaded_sdk_id()` at startup | Immediate startup rejection |

The second prototype only showed that an in-process check detects a mismatch; it did not prove SDK code would not execute before the check. SDK is a host `DT_NEEDED` dependency and provides the global allocator; Rust runtime may call it before entering `main`. A Linux/rustc 1.98.1 minimal reproduction reviewed on 2026-09-25 showed the SDK allocator had already been called twice before the first C ABI boot query. Changing the first explicit query to C ABI therefore still does not establish a boundary with no Rust ABI calls before validation.

**The dylib variant must start through a separate launcher.** The launcher does not link `rutis-sdk`; its Rust dependencies (including std) are statically linked. Validation uses only file I/O and hashing and does not load the host or SDK being validated. Bind builds in this order:

1. Build SDK and compute L2 hash.
2. Build host with `RUTIS_SDK_ARTIFACT_SHA256`, embedding the hash through `env!` and also inlining L1 `SDK_ID`. Reuse the SDK artifact from step 1 and verify after the build that its hash did not change.
3. Build the separate launcher last, compiling host, SDK, dynamic libstd filenames and expected hashes into it. Do not read expected values from a misconfigurable external manifest. Launcher changes do not affect SDK identity or plugin compatibility.

**Startup checks:**

1. Launcher validates host, SDK, and libstd in the same release directory. On any mismatch it exits without executing host.
2. Only after validation does it execute host. The release directory must be trusted and immutable from validation until process exit; updates use a new version directory and never overwrite in place. Launcher fixes the absolute host path and controls dynamic-library search environment: temporarily clear overrides such as `LD_LIBRARY_PATH`, `LD_PRELOAD`, and `LD_AUDIT` before executing host. Before starting runtime threads, host restores the caller's original `LD_*` so child processes do not inherit the release-directory search path. Packaging verifies host and SDK dynamic dependencies resolve to the validated SDK/libstd, not the working directory or another installation. Validate platform-specific load-path constraints separately (§11).
3. Before creating root or loading plugins, host checks L1 through C ABI `rutis_sdk_boot_id(buf, cap)` and checks L2 against the SDK file identified from the actually loaded module. This is a post-start cross-check, not a startup ABI-safety boundary.
4. Every plugin manifest, boot blob, and runtime metadata carries matching L1/L2 equal to host-confirmed values. Check blob before `dlopen`, then cross-check runtime metadata after load (§7.1).

Direct execution of the internal host binary lacks guarantee 1 and is not a supported entry point; an in-process check cannot undo incorrect calls made during startup. This boundary handles artifact misconfiguration in trusted deployments, not an attacker who can modify installation files after validation or control the process loader. Linux's separate launcher and load-path checks are implemented and automated; each other platform still needs its own validation.

## 6. Plugin artifacts and manifest

Release each plugin as a directory or archive:

```text
greeter-4.2.0-x86_64-unknown-linux-gnu/
  plugin.toml
  libgreeter.so
```

```toml
[plugin]
id = "greeter"                 # stable ID for version retention and diagnostics
version = "4.2.0"
library = "libgreeter.so"
library_sha256 = "…"

[sdk]
version = "0.3.0"              # human-readable
id = "…"                       # L1, same as the .so's embedded constant
artifact_sha256 = "…"          # L2

[interfaces]                   # required interface versions (interface crate name → semver requirement); diagnostic only
"rutis-iface-llm" = "^1.3"

[build]
target = "x86_64-unknown-linux-gnu"
rustc = "rustc 1.98.1 (48a229cea 2026-09-01)"
lock_sha256 = "…"              # full plugin Cargo.lock
```

`export_plugin!` and the packager keep manifest and embedded `.so` metadata consistent. At load time, check twice: statically parse boot metadata before loading (§7.1 step 2), then cross-check `rutis_plugin_meta()` after loading (§7.1 step 5).

## 7. Loading flow

### 7.1 Three phases: validate → load → construct (all outside `PluginFactory::build`)

`PluginFactory::build` is called once during `update` dry-run and again for actual load; it is expected to be a pure constructor (`crates/rutis/src/plugin.rs:32`), so it must not load libraries. Loading is an explicit host operation: `unsafe fn Loader::load(dir) -> Result<Arc<Module>, LoadError>`.

1. **Read and validate manifest** without touching `.so` code: target triple; `sdk.id` and `sdk.artifact_sha256` equal host values confirmed at startup (§5.4); interface versions; count of retained versions for this plugin ID (§9).
2. **Statically parse `.so` boot metadata** (file I/O only; do not load or execute code): locate the boot blob embedded by `export_plugin!` by ELF section name (§7.2), and check `SDK_ID` and `SDK_ARTIFACT_SHA256` equal manifest and host values and plugin ID equals manifest. Reject if section is missing or checks fail. Manifest is a **declaration** written by the packager; this checks values **embedded in the binary itself**. ELF initialization code (`.init` / `.init_array`) runs before `dlopen` returns; without this step, an artifact whose `library_sha256` matches the file but whose `sdk.id` is falsely labeled as current would run initializer code before the runtime check in step 5, violating goal 3 in §1 (“reject before load”).
3. **Copy to a content-addressed cache:** `<cache>/<library_sha256>/lib<name>.so`, then recalculate and compare hash. Do this on every platform: Windows avoids file locks; Linux avoids SIGBUS from overwriting a mapped `.so` in place. Reuse a valid existing entry with the same hash; replace corrupt entries through a temporary file and atomic rename.
4. **Load the library:** `dlopen(path, RTLD_NOW | RTLD_LOCAL)`. `RTLD_NOW` fails unresolved symbols now rather than crashing on call; `RTLD_LOCAL` allows multiple plugin versions to coexist (prototype loaded same-named crate v1/v2 without cross-binding). The binary's asserted compatible identity was checked at step 2, so running initializer code at this point does not violate “reject before load.”
5. **Check runtime metadata:** call `rutis_plugin_meta()` and compare `sdk_id` (L1), `sdk_artifact_sha256` (L2), plugin ID, and version with manifest and boot-blob fields; SDK L1/L2 must also equal host-confirmed values. This is a post-load cross-check.
6. **Construct factory:** call `rutis_plugin_entry()` to obtain `Box<dyn PluginFactory<ConfigValue>>`, and read `name()` and `injects()` once. Record these with plugin ID in `Module` for invariant checks in §8.2; do not call them again.

Any failed step returns `LoadError` with plugin ID, version, failed step, and reason. A library that failed after step 4 is not unloaded (§9); include it in diagnostics.

### 7.2 Plugin exports and boot interface

```rust
rutis_sdk::export_plugin! {
    id: "greeter",
    factory: GreeterFactory,        // impl PluginFactory<ConfigValue>
}
```

The macro expands to:

- `#[no_mangle] pub fn rutis_plugin_meta() -> rutis_sdk::PluginMeta` (Rust ABI; contains `SDK_ID`, plugin-embedded `SDK_ARTIFACT_SHA256`, ID, and `CARGO_PKG_VERSION`).
- `#[no_mangle] pub fn rutis_plugin_entry() -> Result<Box<dyn PluginFactory<ConfigValue>>, CordisError>`, wrapping construction in `catch_unwind` and converting panic to an error.
- Compile-time assertion for `panic = "unwind"`. At the plugin call site, read `RUTIS_SDK_ARTIFACT_SHA256` using `env!`; fail build if missing or not valid SHA-256. Boot blob and `PluginMeta` use the same plugin constant.
- A **boot blob** (plain data, no initializer code) for static parsing before host calls `dlopen` (§7.1 step 2): `#[used] #[link_section = ".note.rutis.meta"] static RUTIS_BOOT_META: [u8; N]`, containing versioned magic, `SDK_ID`, plugin-embedded `SDK_ARTIFACT_SHA256`, plugin ID, and version, each length-prefixed, with zero padding. Initial Linux loader locates it by ELF section name; Rust artifact `.rustc` metadata also copies magic/blob, so whole-file byte search is ambiguous. macOS/Windows need their own section location and validation; reject as incompatible if not found.

SDK separately exports a **C ABI boot function** for host's post-start L1 cross-check (§5.4 step 3):

`#[no_mangle] pub extern "C" fn rutis_sdk_boot_id(buf: *mut u8, cap: usize) -> usize`

It copies UTF-8 `SDK_ID` bytes into `buf` and returns the length; if `cap` is too small it returns the required length without writing. The body only copies and compares lengths and touches no SDK types.

Other exports (`rutis_plugin_meta()`, `rutis_plugin_entry()`, `loaded_sdk_id()`) use Rust ABI, not `extern "C"`. Host and plugin guarantee the same rustc and SDK, making trait-object passing safe; this is precisely what L1/L2 establish, and these functions must first be called only after relevant checks pass. A C ABI query cannot prevent Rust from calling SDK allocator during startup; the separate launcher protects host startup, and pre-`dlopen` L1/L2 boot-blob validation protects plugins.

### 7.3 Configuration type

`PluginFactory<C>` requires a `C` understood by both host and plugin. Use `ConfigValue = serde_json::Value`:

- Host reads configuration and passes it unchanged to plugin; plugin deserializes and validates in `validate_config`.
- Plugin configuration struct changes do not affect SDK and do not require a new SDK version.

Plugin families that need strongly typed configuration may put the type in an interface crate (and thus the SDK), at the cost that a config change changes the SDK.

## 8. Integration with rutis lifecycle

### 8.1 Code replacement is `update`

rutis `FiberView::update(config)` requires the config type and factory to remain unchanged (`crates/rutis/src/fiber.rs:1279`). Put the module in config to reuse all update semantics (dry-run, pre-cancel, restart, consumer eviction/reload):

```rust
pub struct DylibConfig { module: Arc<Module>, value: ConfigValue }   // private fields; construct with DylibConfig::new

/// Fixed from the first module at spawn and immutable for its lifetime (rutis static dependency declaration, D32f).
struct DylibFactory { id: String, name: String, injects: Vec<TypeKey> }

impl DylibFactory {
    /// Module must match the spawned plugin ID, name, and injects exactly. Pure comparison, no side effects.
    fn check_module(&self, m: &Module) -> Result<(), CordisError> { /* mismatch → CordisError::Validation */ }
}

impl PluginFactory<DylibConfig> for DylibFactory {
    fn validate_config(&self, c: &DylibConfig) -> Result<(), CordisError> {
        self.check_module(&c.module)?;
        c.module.factory.validate_config(&c.value)
    }
    fn build(&self, c: &DylibConfig) -> Result<Box<dyn Plugin>, CordisError> {
        self.check_module(&c.module)?;       // pure construction: library already loaded, check is also a pure comparison
        c.module.factory.build(&c.value)
    }
    // name / injects return the values fixed at spawn
}
```

Host API:

```rust
let view = loader.spawn(&ctx, &module_v3, config)?;     // = ctx.plugin_with(DylibFactory, DylibConfig::new(..))
loader.swap(&view, &module_v4, config).await?;          // convenience: readable early error, then view.update(..)
```

Prototype verified the full path: a consumer depends on plugin-provided `Greeting`; after `update` replaces v1 with v2, the consumer is automatically evicted and reloaded with the new service (output `['hello v1 …', 'hello v2 …']`), and plugin `tokio::spawn` works.

### 8.2 Put invariants in factory, not `swap`

rutis dependency declarations are static (D32f): `injects` is registered once at spawn, and `update` does not change the dependency registry. If a replacement module declares different dependencies, it would run under the old dependency gates.

`FiberView::update` is public, so callers can bypass `loader.swap` and call `view.update(DylibConfig::new(other, ..))` directly. Therefore plugin ID, `name()`, and `injects()` consistency checks belong inside `DylibFactory`, not only in `swap`:

- `update` dry-run calls `validate_config` then `build` (`crates/rutis/src/fiber.rs:1299-1300`); both reject mismatches.
- First load in rutis does not call `validate_config`, but every load calls `build`, so its check covers every path.
- ID, name, and injects needed for the check were recorded in `Module` at load (§7.1 step 6); comparison is pure and meets the `build` pure-construction contract.

On mismatch return `CordisError::Validation` with a message that a dependency declaration or plugin identity change requires dispose then spawn again. `swap` only surfaces the same error earlier.

### 8.3 Old-generation residue

After replacement, old code remains in memory. A background task from the old generation that ignores cancellation continues running and using capabilities. This is the same issue as #12/#41, more visible for dylibs because old code may come from another version. **#41 (reject late registrations by load generation) is a prerequisite for shipping this design.**

## 9. Retain versions; do not unload in first version

- `Module` owns the library handle and **never calls `dlclose`**, even after its last `Arc<Module>` is dropped. External `Arc`s, trait-object vtables, `Drop` implementations, spawned tasks, and TLS destructors may still reference old code; it is not possible to prove all have been released.
- Registry records each loaded version per plugin ID, how many fibers still use each (`Arc::strong_count`), load time, and mapped size for diagnostics.
- **Retention limit:** at most N versions per plugin ID (default 4, configurable). Beyond it, `load` returns `LoadError::RetentionExceeded` and says a process restart is required to reclaim. Count every version ever loaded, even when an old version is no longer used.
- Supporting unload later requires tracking every task from `ctx.spawn`, proving tasks ended, ensuring external references are zero, and validating `dlclose` semantics on every platform. Track separately.

## 10. Build and release

### 10.1 Two host build variants

| Variant | Linking | Artifacts | Can load plugins? |
| --- | --- | --- | --- |
| Default | Fully static | Single binary | No |
| `--features dylib-plugins` | Internal host dynamically links rutis/tokio/std | Separate launcher + internal host binary + `librutis_sdk.so` + `libstd-<hash>.so` | Yes |

Use Bevy `dynamic_linking` style: host's `rutis` dependency remains unchanged and an optional `rutis-sdk` dependency is added; with feature enabled use `use rutis_sdk as _;`. Prototype `host2` confirmed same source, default build has no Rust dynamic library dependencies, and feature build resolves all rutis symbols to `librutis_sdk.so`. Host's `use rutis::…` needs no changes.

Release layout puts all four files in the same immutable version directory; public entry point is the separate launcher. On Linux, configure internal host and SDK with `-C link-args=-Wl,-rpath,$ORIGIN` and verify direct and indirect dynamic dependencies resolve to SDK/libstd in that directory (§5.4). Get `libstd-<hash>.so` from the matching toolchain's `<sysroot>/lib/rustlib/<target>/lib/`.

### 10.2 CI and release

1. Pin `rust-toolchain.toml` in repository. Build scripts and CI set path remapping using their respective source, target, and Cargo home paths.
2. SDK release: build `rutis-sdk` and hash artifact; build host dylib variant with `RUTIS_SDK_ARTIFACT_SHA256` (two-stage build, §5.4); verify SDK file hash is unchanged; then build separate launcher bound to full artifact hashes. Publish SDK manifest (`id`, `artifact_sha256`, included crates and versions).
3. Plugin release: check out SDK's tag and use same lockfile for §5.3 two-stage build; embed SDK artifact hash in plugin. Stage two must reuse stage one's SDK. At packaging, compare actual SDK hash, plugin-blob L2, and SDK release manifest; generate plugin manifest from blob and fail on any mismatch. CI must test that a wrong external manifest cannot override embedded plugin L2.
4. On SDK upgrade, rebuild all first-party plugins against new SDK in CI and release them as a batch. Slow SDK cadence (e.g. quarterly); even additive-only changes yield a new SDK version.
5. Run `cargo-semver-checks` on interface crates (#44).

## 11. Prototype validation record

The prototype lived in a temporary session directory and was not committed. Environment: Linux x86_64, rustc 1.98.1, rutis 0.3.0 (`7d7402d`).

The repository's Linux implementation is separately validated by `tools/test-dylib.sh`, `tools/test-dylib-launcher.sh`, and `tools/test-dylib-repro.sh`: release package startup; replacing two plugin versions and reloading consumers; rejecting L1/L2 errors before ELF initialization; rejecting host/SDK/libstd hash errors before executing host; identical SDK bytes across two source/target paths; and rejecting unclassified RUSTFLAGS. CI also compares SDK hashes on two independent runners; see the relevant PR CI for results.

| Validation | Result |
| --- | --- |
| Host depends on SDK dylib and dynamically links libstd | Yes; ship `libstd-<hash>.so` alongside |
| Shared-type TypeId seen by host, SDK, and plugin | Equal |
| Plugin `tokio::spawn` uses host runtime | Works |
| rutis dependency gating, code replacement with `update`, automatic consumer reload | Works |
| Plugin allocation, host deallocation; host calls plugin type's Drop | Works (Drop once) |
| Host `#[global_allocator]` applies to plugin | **No**; allocator in SDK does |
| Host catches plugin panic with `catch_unwind` | Yes |
| Two versions of same plugin crate loaded simultaneously (`RTLD_LOCAL`) | Works, no cross-binding |
| SDK contents change but metadata hash stays same | `dlopen` succeeds; only L1 catches it |
| SDK dependency features change | `dlopen` succeeds and test runs (not evidence of compatibility) |
| Reproducible SDK build (different target dirs on same machine) | Byte-identical after path remapping |
| Optional dynamic linking for host (Bevy style) | Feasible |
| Old host (SDK A) + new SDK (B) + plugin built against B, no host self-check | Everything succeeds (the §5.4 vulnerability) |
| Same setup, host compares inline `SDK_ID` with `loaded_sdk_id()` at startup | Rejected immediately |

**Unverified; must be completed before implementation:**

- V1 Cross-machine reproducibility (different source paths, CARGO_HOME, target dirs, usernames, and different remapping args); must use real §5.2 `SDK_ID` generation. If it fails, use §5.3 fallback.
- V2 macOS `install_name` / `@rpath`, same-named crate versions under `RTLD_LOCAL`, and two-level namespace effects.
- V3 Windows Rust dylib export-count limit (DLL export table max 65,535; large dylib may exceed), loader lock, and std DLL distribution. If infeasible, Windows stays static-only. **Validated feasible on 2026-10-03**, see “Windows feasibility validation” below; implementation in #118.
- V4 Repeat under release configuration (LTO, `codegen-units`, `opt-level`); SDK must not enable cross-crate LTO.
- V5 Long-running cross-library trait objects: old-version plugin tasks continue while new version runs; destruction executes in old library code.
- V6 Cross-library behavior of thread-local variables and crates with global dispatchers such as `tracing` (if included in SDK).
- V7 Boot interface and blob (not verified in prototype): post-start C ABI query agrees with L1; blob contains plugin-build L1/L2 and agrees with runtime metadata; magic-byte search is reliable on macOS/Windows artifacts (with debuginfo, symbols removed, and stripped variants).
- V8 Separate launcher (new this round, not verified): no SDK/dynamic libstd dependency; mismatched SDK/libstd never executes host or its startup allocator/initializer; load path stays on validated artifacts across working directories, environment overrides, and multiple installed versions. Until this holds on a platform, provide only its static variant.

### Windows feasibility validation (2026-10-03)

For [#103](https://github.com/arcships/rutis/issues/103); plan and infeasibility criteria are in [design-dylib-macos-windows](design-dylib-macos-windows-2026-10-03.en.md) §4 (W1–W8).

**Environment:** GitHub Actions `windows-2025` runner (Windows Server 2025), rustc 1.98.1 (`48a229cea 2026-09-01`, LLVM 22.1.8), target `x86_64-pc-windows-msvc`, Visual Studio 18 Enterprise (MSVC 14.51.36231, `link.exe`), Windows SDK 10.0.26100.0. Experiments are in `docs/probes/windows-dylib/` (README explains rerun and workflow); temporary workflow on `probe/windows-dylib` ran twice: [run 37124981363](https://github.com/arcships/rutis/actions/runs/37124981363) and [run 37125920008](https://github.com/arcships/rutis/actions/runs/37125920008). Round two added opt-level variants, W8 with real SDK, held file handles, and reproducibility; repeated measurements matched.

**Subjects:** W1 used actual `crates/rutis-sdk`. `rutis-cli --features dylib-plugins` failed to compile on Windows (`rutis_dylib::Loader` exists only on Linux), but before failing Cargo linked `rutis_sdk.dll` using its dependency graph. A `w1-anchor` build with the same graph minus `rutis-dylib` produced the same export count. W2, W3, W5–W8 used a minimal model (`runtime/`: SDK is a dylib re-exporting tokio, plugin and host dynamically link it, host loads plugin by full path using `LoadLibraryExW`). W8 also ran with a copy of the real SDK and greeter fixture (`w8-real/`).

| # | Method | Measured result | Conclusion |
| --- | --- | --- | --- |
| W1 | Build `rutis_sdk.dll` as host does; read export directory with `object`, confirm with `dumpbin /exports`; also test a copy of SDK dependency set with common dependencies | Real SDK: release **1,597** exports (2.4% of limit), dev **14,545** (22.2%); no LNK1189 or other link error. Dev exports mostly generic monomorphizations (core 4,945; tokio 3,467; alloc 1,593; std 1,110). Growth (release/dev): copy baseline 1,568/14,259; serde derive 1,585/14,350; regex + tracing + tracing-subscriber + chrono + uuid 2,949/26,773; reqwest (rustls) 4,188/34,882; all combined 5,425/**46,683** (71.2%). Setting opt-level 2 only for `rutis-sdk` did nothing in dev (14,543); setting all dependencies to opt-level 2 (`[profile.dev.package."*"]`) cut to 2,609; copy with all dependencies 6,457 | **Feasible.** Release has ~64K exports of headroom. Dev approaches limit only after major dependency growth; if needed build all SDK dependencies at opt-level ≥2 (SDK package alone is insufficient, contrary to macOS design §8 R7) |
| W2 | Build same plugin crate into two target dirs with `GREETER_VERSION=1/2`; copy to `<cache>/<hashA>/greeter.dll` and `<cache>/<hashB>/greeter.dll`; load sequentially by full path with `LoadLibraryExW` | Different module handles return v1 and v2; private type vtables do not cross-bind. Loading same path again returns same handle. v2 links other target's `sdk.dll.lib` but uses host's loaded `sdk.dll` at runtime and works | **Feasible.** Same name does not conflict; also proves loader matches SDK only by DLL/symbol name, so identity must be checked by L1/L2 |
| W3 | Release dir contains host, `sdk.dll`, `std-*.dll`; load plugin using `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`. Place same-name malicious SDK (same symbols, `mark()` returns evil) and invalid std in working dir, `PATH`, `host.exe.local`, `host.exe.local` empty file + working dir, and plugin cache | In all five placements, host static imports and plugin dependencies resolve to release SDK/std; plugin reuses loaded `sdk.dll`. Control: if release dir lacks `sdk.dll`, startup fails with `STATUS_DLL_NOT_FOUND` absent malicious copy, but finds and runs malicious SDK if it is on `PATH` | **Feasible.** No bypass found while release files exist; if missing, search continues on `PATH`, so launcher must check SDK/std existence and hashes before host and hold them until host exits (W7) |
| W4 | Read import table with `dumpbin /dependents` and `object` | rustup `std-44a584f44bc3dd65.dll` depends only on `KERNEL32`, `ntdll`, `USERENV`, `WS2_32`, `bcryptprimitives`, `api-ms-win-core-synch-l1-2-0`, **not** VC runtime. `rutis_sdk.dll`, plugin, and host depend on `VCRUNTIME140.dll` and UCRT (`api-ms-win-crt-*`). Toolchain has identical std DLL copies in `bin\` and `lib\rustlib\x86_64-pc-windows-msvc\lib\` | No feasibility impact. Per decision, user installs VC++ runtime; dependency comes from rustc default dynamic CRT in host/SDK/plugin, not std (correct macOS design §8 R10). UCRT is a Windows 10+ component |
| W5 | Plugin has `ctor` initializer (`.CRT$XCU` in DllMain under loader lock: allocate, lock, access SDK/plugin `thread_local!`, read environment) and destructing TLS; load in tokio runtime with four workers and continually created/ending blocking threads; also test initializer that starts and waits for another thread | Load takes 0.8–1.1 ms, initializer runs without deadlock; after 64 tasks and 16 short-lived threads, all destructors run on thread exit (after runtime shutdown inits=dtors=29/30). Counterexample waits 5 seconds for new thread inside initializer and times out (new thread waits for DllMain; `join` would hang forever) | **Feasible.** Plugin author guide must forbid waiting for other threads during static initialization (`join`, blocking channel or lock wait). `rutis_plugin_entry` runs after `LoadLibrary` returns and is not affected |
| W6 | Release plugin has unreferenced `#[used] #[link_section = ".rutism"] static [u8; 512]`; locate by section name with `object` | Exactly one `.rutism`, 512 bytes, correct contents. Existing macro's `.note.rutis.meta` is truncated to `.note.ru` in PE (same in real greeter fixture). Same 512 bytes occur twice in file, second in `.rustc` metadata | **Feasible.** Use `.rutism` on PE and locate by section name; never search entire file for magic bytes (V7) |
| W7 | After loading cached DLL, try overwrite, copy-overwrite, delete, rename, delete directory; copy to new path and load; also open first with read-only sharing (`FILE_SHARE_READ`) | Overwrite/copy-overwrite denied (error 32); delete and remove directory denied (error 5); **rename succeeds** and module path changes. New path loads as another module. Open with `FILE_SHARE_READ` before load: load succeeds; rename/delete denied (error 32) | **Feasible.** Content-addressed cache may create but not overwrite. A loaded DLL can be renamed, so hold read-only shared handle from loader hash validation until `LoadLibrary` returns; launcher does the same for release files |
| W8 | Minimal model: TypeId, downcast both directions, plugin `Handle::try_current` and `tokio::spawn`, whether SDK `thread_local!` (const and lazy), static variables are singular across host/plugin, `catch_unwind`, Drop count; run v1/v2. Real SDK: `rutis_plugin_meta`, `rutis_plugin_entry`, fixture on `Ctx::root()` | All passed. TypeId equal; downcast both directions; `try_current` fails outside runtime and succeeds inside; all four tasks run on host `host-worker`; host, plugin (inline access), and SDK function see same TLS addresses/values on two threads; static shared; panic caught with full payload and plugin still callable; one Drop. Real SDK `sdk_id` equals host `SDK_ID`; plugin-provided `String` and `Snapshot` readable from root Ctx; dispose marks one Drop | **Feasible** |

**Additional findings:**

- **SDK byte reproducibility:** without arguments, SDK hashes from two target dirs differ; path remapping alone still differs; adding `-C link-arg=/Brepro` (removes linker timestamp) makes them identical. Release has no debug info by default; `/PDBALTPATH` does not affect result. Only same-machine/different-directory tested; cross-machine V1 remains.
- **`rutis_sdk.dll` size:** release 1.9 MB (import library 0.95 MB), dev 7.5 MB (import library 15 MB). Do not ship import library.

**Conclusion: feasible.** W1–W8 triggered no infeasibility condition. Implementation is in [#118](https://github.com/arcships/rutis/issues/118), and beyond common Linux/macOS behavior must include:

1. **Launcher cannot `exec`:** create and wait for child, forward exit code, ignore its own Ctrl+C, use Job Object so child ends when launcher exits; tools depending on host PID must account for the difference from Linux/macOS.
2. **Launcher holds release files:** after checking host/SDK/std existence and hashes, hold read-only shared handles until child exits (W3 control and W7).
3. **Loader:** load by full path using `LoadLibraryExW` flags `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`; hold read-only shared handle from hash check through load; plugin directory must not contain copies of SDK or std.
4. **Boot blob:** `export_plugin!` uses `.rutism` on PE and locates by section name.
5. **Reproducible build:** add `-C link-arg=/Brepro` and path remapping; CI compares SDK hashes.
6. **Names and layout:** no `lib` prefix (`rutis_sdk.dll`, `std-<hash>.dll`, `<plugin>.dll`); omit `.dll.lib` from release directory.
7. **Dev export headroom:** document it; if needed set opt-level ≥2 for all SDK dependencies and have CI count release exports, alerting above a threshold such as 30,000.
8. **Docs:** user installs VC++ runtime; plugin static initializers must not wait for other threads.

## 12. Implementation steps

1. **Prerequisite:** merge #41 (reject late registrations from old generation).
2. `rutis-sdk` crate: re-exports, allocator, `SDK_ID` (`build.rs`, including RUSTFLAGS allowlist normalization), C ABI boot function, `export_plugin!` (including boot blob), and `ConfigValue`.
3. `rutis-dylib` crate: post-start host cross-check (§5.4; explicit identity query uses C ABI), manifest, static boot metadata parsing (§7.1 step 2), content-addressed cache, three-stage load, `DylibFactory` (module invariant checks), `spawn`/`swap`, module registry and diagnostics, retention limit.
4. Host `dylib-plugins` build variant, separate launcher, and immutable release layout (including dynamic dependency path checks).
5. Packager (`cargo xtask pack-plugin`): two-stage SDK/plugin build, embed plugin L2, generate manifest from blob, verify SDK hash, and `cargo tree -d` check.
6. CI: SDK release pipeline, batch rebuild of plugins, automated V1–V4 and V7–V8 validation.
7. Docs: plugin author guide prohibiting custom allocator, `panic = "abort"`, private types at the boundary, and other versions of SDK crates.

## 13. Acceptance criteria (for #45)

- [x] Host plus one dylib plugin: automated tests cover cross-library TypeId, downcast, trait object, async calls, and destruction.
- [ ] Reject load when either L1 or L2 mismatches, with readable reason; tests cover both §5.1 counterexamples.
- [ ] Host startup boundary (§5.4): separate launcher checks host/SDK/libstd before executing host; “old host + new SDK + new plugin” is rejected before host runtime starts. Use SDK allocator/initializer counters to prove rejection path did not execute SDK code.
- [ ] Load path: in trusted immutable release directory, working directory, environment overrides, and multiple installations cannot make host resolve unvalidated SDK/libstd; after startup, host cross-checks actual SDK identity. Direct execution of internal host does not count.
- [ ] Boot metadata: package whose `library_sha256` matches file but whose `sdk.id` differs from embedded `.so` boot blob is rejected before `dlopen`, without running initializer; mismatch between boot blob and `rutis_plugin_meta()` is rejected at step 5.
- [ ] Plugin L2: SDK A/B have same L1 but different artifact/layout; plugin built against B with manifest marked A (correct plugin file hash) is still rejected before `dlopen` for embedded L2 mismatch, without running initializer; normal two-stage artifact loads.
- [x] `SDK_ID` normalization: two builds differing only in path-remap args produce same `SDK_ID` and SDK artifact hash; unclassified RUSTFLAGS fail build.
- [x] After `swap` loads new version, rutis unloads old instance and consumers switch to the new service.
- [x] Modules with different plugin ID, name, or injects are rejected through both `swap` and direct `view.update(..)`; fiber continues running old module.
- [x] Retention limit is enforced; diagnostics show usage and memory for each version.
- [x] Default build remains a single binary with no Rust dynamic-library dependency.
- [ ] V1–V4 and V7–V8 have conclusions recorded in this document.
