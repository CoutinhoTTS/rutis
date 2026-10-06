# First-Party dylib SDK: Usage on Linux, macOS, and Windows

This capability is described in the [design](design-dylib-sdk-2026-09-24.en.md). `rutis-cli` remains statically linked by default; only a release bundle built with `dylib-plugins` loads trusted first-party plugins. Supported targets are Linux (little-endian ELF64), macOS (arm64 Mach-O), and Windows (x64 `x86_64-pc-windows-msvc`, PE). macOS and Windows design details are in [design-dylib-macos-windows](design-dylib-macos-windows-2026-10-03.en.md); Windows feasibility results are in §11 of the design. Plugin and host code run in the same process, so a plugin crash takes down the host.

## Build a host release bundle

Use the pinned Rust 1.98.1 toolchain and run this from the repository root:

```sh
bash tools/build-dylib-bundle.sh
```

The script uses `RUTIS_SDK_LOCKFILE` to identify the `Cargo.lock` used for resolution, builds the SDK with the host's complete Cargo feature graph, computes the SHA-256 of `librutis_sdk.so` (`librutis_sdk.dylib` on macOS), and embeds that hash in the host. It then embeds the filenames and hashes of the host, SDK, and dynamic libstd in a standalone launcher. The script creates a new `target/dylib-bundles/<hash>/` directory containing the public `rutis-cli` entry point, internal `rutis-cli-host`, SDK, libstd, and `sdk.toml`. It refuses to overwrite an existing directory. For deployment, install the whole directory at a trusted versioned path that cannot be modified in place; create a new directory for each update.

Launch only through `rutis-cli` in that directory. The launcher does not link to the SDK or dynamic libstd. It first verifies the hashes of the three runtime files, then runs the internal host with a fixed directory as the dynamic-library search path. After startup, the internal host verifies the SDK actually loaded and restores the caller's original `LD_*` environment variables before starting runtime threads, so child processes inherit the caller's library paths.

## Write and package a plugin

The plugin crate uses `crate-type = ["dylib"]` and depends on the same SDK version. Its configuration type is `rutis_sdk::ConfigValue` (`serde_json::Value`). See the [greeter-v1 factory](../tests/dylib-fixtures/greeter-v1/src/lib.rs):

```rust
rutis_sdk::export_plugin! { id: "greeter", factory: Factory }
```

The macro generates a Rust ABI factory entry point, runtime metadata, and a bootstrap section that can be parsed before loading (ELF `.note.rutis.meta`, Mach-O `__DATA,__rutis_meta`, PE `.rutism`). Plugins must not define their own global allocator and must use `panic = "unwind"`. The SDK allocator must be `System` (upstream rust-lang/rust#100781, #114518).

Plugins may dynamically link native system libraries (for example, `libz.so.1`), but must use only the host's copy of the SDK and libstd. They cannot depend on other Rust dylibs and cannot contain `RUNPATH`/`RPATH`. The host and SDK set `$ORIGIN` through their own `build.rs`; do not put it in `RUSTFLAGS`, or the plugin will get it too. Custom service and event types exchanged across plugins or with the host must come from an interface crate in the SDK; keep plugin-private types inside the plugin. Background tasks should observe `ctx.cancelled()`; registrations through an old-generation `Ctx` are rejected after generation changes.

Pass the `sdk.toml` and `librutis_sdk.so` from the SDK release bundle to the pack command:

```sh
cargo xtask pack-plugin \
  --manifest-path tests/dylib-fixtures/greeter-v1/Cargo.toml \
  --sdk-manifest target/dylib-bundles/<hash>/sdk.toml \
  --sdk-file target/dylib-bundles/<hash>/librutis_sdk.so \
  --features export \
  --output /tmp/greeter-v1
```

The release manifest specifies the build anchor `rutis-cli/dylib-plugins`. The packager uses the plugin workspace's `Cargo.lock` to set `RUTIS_SDK_LOCKFILE`, builds host and plugin in two passes against the same Cargo feature graph, determines the SDK artifact hash, then embeds it in the plugin. The temporary host build result does not need to be released. The packager verifies the SDK file, plugin bootstrap section, plugin dynamic dependencies, duplicate dependency versions, and custom allocator, then generates `plugin.toml` from the actual binary. Its `native_deps` lists the native libraries linked by the plugin; the loader verifies that this matches the binary before `dlopen`. If the plugin is in another Cargo workspace, build it in the SDK release pipeline with the same feature graph and package it with `--prebuilt-library`; a separately built SDK with a different hash is rejected.

The dylib variant of `rutis-cli` loads a plugin with `--plugin <directory> --plugin-config '<JSON>'`. A host that needs code replacement calls `Loader::load`, `Loader::spawn`, and `Loader::swap`; `swap` reuses rutis `FiberView::update`, and consumers reload as the service is removed and provided again. `DylibConfig::new` checks module identity, name, and dependency declarations inside the factory; direct calls to `view.update` cannot bypass these checks. By default the loader keeps at most four mapped versions per plugin ID. Old versions are never `dlclose`d; restart the host to reclaim them after the limit is reached.

## External builds: SDK bundle

For a full plugin-author guide (workspace setup, code examples, troubleshooting), see the [External Plugin Development Guide](external-plugin-guide.en.md); this section is a toolchain-oriented overview. The process above requires host source code because the anchor package must be part of the same Cargo build. External developers without host source use an **sdk-bundle**. After creating the runtime release directory, the release pipeline runs:

```sh
cargo xtask pack-sdk-bundle --bundle-dir target/dylib-bundles/<hash> --output <sdk-bundle>
```

This copies the prebuilt SDK (and on Windows, the `rutis_sdk.dll.lib` import library), collects compile-time artifacts in the dependency closure listed in `sdk.toml`'s `packages` (including proc-macro `.so` files, because rustc recursively loads the complete rmeta dependency chain for closure crates), then uses a probe plugin to test removal of artifacts by variant and shrink the bundle to the minimum set (41 artifacts, about 60 MB, measured on Linux). Finally it writes per-file hashes to `bundle.toml`. It also includes `sdk.toml`, `Cargo.lock`, `rust-toolchain.toml`, a `cargo-config.toml` template, and `GUIDE.md`.

On the plugin side, **do not declare `rutis-sdk` in `Cargo.toml`** (and do not directly depend on `rutis`, `tokio`, `tokio-util`, or `serde_json`; access shared crates only through `rutis_sdk::` re-exports). Shared types are resolved through the injected `--extern`. Package with:

```sh
cargo xtask pack-plugin --bundle <sdk-bundle> \
  --manifest-path <plugin>/Cargo.toml --features export --output <dist>
```

The packager first verifies every file hash in `bundle.toml`, the complete `rustc` version against `sdk.toml` (a mismatch otherwise triggers E0514 during metadata loading, before other checks), and the direct-dependency denylist in the plugin manifest (shared-crate declarations are rejected before build rather than relying on rustc errors—the injected `--extern` in RUSTFLAGS would otherwise be rejected earlier by the SDK build.rs as an “uncategorized RUSTFLAGS argument”). If `RUSTFLAGS` / `CARGO_ENCODED_RUSTFLAGS` is already set in the environment, packaging fails directly: injection is passed through these variables, and silently overwriting caller flags is unsafe. After a successful build, it performs the usual allocator, weak-export, `native_deps`, bootstrap L1/L2, and `sdk.toml` cross-checks, then validates platform link artifacts (Linux `DT_NEEDED`; macOS `@rpath` with no run path; Windows import table).

For development, copy the bundle's `cargo-config.toml` into the plugin workspace as `.cargo/config.toml` and fix the paths; `cargo check` / `cargo build` then work. rust-analyzer completion may not resolve `rutis_sdk::` paths because injected crates are not in Cargo's crate graph, but flycheck still runs `cargo check`. After an SDK upgrade, the host rejects old plugins for L1/L2 mismatch; rebuild with the new bundle. The complete Linux/macOS workflow is covered by `tools/test-sdk-bundle.sh` (E1/E3/E6/E9); Windows uses the corresponding script in the `dylib-windows` workflow.

## Verification and boundaries

Linux verification commands in this repository:

```sh
bash tools/test-dylib.sh
bash tools/test-dylib-launcher.sh
bash tools/test-dylib-repro.sh
bash tools/test-sdk-bundle.sh
cargo test --workspace
```

Tests cover `tokio::spawn` inside plugins; cross-library `Snapshot` types and String service reads/downcasts; consumer reload and old-plugin destruction after v1→v2 replacement; rejecting incorrect L1/L2 before `dlopen` without running ELF initializers; rejecting module-identity changes through both `swap` and direct `update`; version-retention limits; atomic repair of corrupted cache and same-version retry at failed entry; launcher refusal when host/SDK/libstd files change; launching from its own directory despite library-path overrides; identical SDK bytes across different source and target paths; and rejecting late old-generation registrations under both Tokio runtimes and Failed state. CI also builds the SDK on two independent runners and compares artifact hashes.

Direct execution of the internal host has no pre-start verification and is not a valid dylib release entry point. Trusted deployment must control the release directory and plugin cache; do not modify files in place while the program runs.

## macOS differences

- **Library search paths:** SDK install name is `@rpath/librutis_sdk.dylib`; host and SDK run paths are `@loader_path`. Their respective `build.rs` files set these at link time; nothing is modified after build. Plugins have no run path.
- **Launcher:** it does not set any `DYLD_*` variable. Instead, it renames the caller's variables to `RUTIS_ORIG_DYLD_*`, then starts the host; after startup, the host restores the original values so child processes see the same environment as the caller. The launcher cannot prevent code injection into itself through `DYLD_INSERT_LIBRARIES`; that is outside its protection. Publishers that need this protection can sign the launcher with hardened runtime.
- **Quarantine:** before `dlopen`, the loader rejects plugin source/cache files with the `com.apple.quarantine` attribute and reports `xattr -d com.apple.quarantine <file>`. It does not remove the attribute automatically.
- **Signing:** the publisher decides whether the host enables hardened runtime. If enabled, SDK, libstd, and plugins must be signed with the same Team ID as the host, or the host must have `com.apple.security.cs.disable-library-validation`. The host may call `Loader::require_team_ids` to accept only specified Team ID signatures; this rejects only ad-hoc-signed plugins.
- **Native plugin dependencies** must use absolute paths (for example, `/usr/lib/libSystem.B.dylib`); `@rpath`, `@loader_path`, `@executable_path`, and relative paths are forbidden. Plugins must also use the two-level namespace, must not use `-undefined dynamic_lookup`, and exported symbols must not contain weak Rust-symbol definitions.
- **Allocator:** SDK allocator must be `System`. Allocations internal to macOS libstd do not go through the SDK allocator.
- **Build:** the script fixes `MACOSX_DEPLOYMENT_TARGET` to 13.0. Different Xcode versions produce different SDK bytes, so pin Xcode in the release pipeline.

## Windows differences

- **Filenames:** no `lib` prefix. The release directory contains `rutis-cli.exe` (launcher), `rutis-cli-host.exe`, `rutis_sdk.dll`, and `std-<hash>.dll` (from the toolchain's `bin\`); plugin is `<id>.dll`. `rutis_sdk.dll.lib` import library is not included in the release directory.
- **VC++ runtime:** SDK, host, and plugins depend on `VCRUNTIME140.dll` and UCRT (`api-ms-win-crt-*`, system components since Windows 10). Users install the VC++ runtime themselves; rutis does not distribute or check it. The std DLL itself does not depend on it.
- **Launcher:** Windows has no `exec`. The launcher verifies the existence and hashes of the three runtime files, starts the host as a child, waits, and forwards its exit code. It opens these files with read-only sharing and keeps them open until the host exits, preventing writes, deletion, or renaming meanwhile. It places itself in a Job Object with `KILL_ON_JOB_CLOSE`; if the launcher exits (including forced termination), the host and its children exit too. A host that needs a child to survive can start it with `CREATE_BREAKAWAY_FROM_JOB`. The launcher ignores Ctrl+C; the host in the same console handles it. Host and launcher have different process IDs.
- **Library search paths:** host imports for SDK and std resolve first from the host directory, where the launcher has verified both files exist, so same-named files in the working directory or `PATH` are not used. The launcher does not modify environment variables. Do not launch the internal host from a directory missing these files: Windows will continue searching `PATH` when a file is absent.
- **Plugin loading:** the loader calls `LoadLibraryExW` with a full path and flags `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`. Plugins reuse the SDK and std already loaded by the host; the cache directory is not searched. From cache-hash verification until `LoadLibraryExW` returns, the loader holds the file read-only with sharing. A loaded DLL may be renamed but not overwritten. The loader never calls `FreeLibrary`.
- **Native plugin dependencies** must be DLL filenames only, without paths; both import tables and delay-load import tables are checked. DLL names are case-insensitive; list them in lowercase in `plugin.toml`'s `native_deps`.
- **Static initialization:** plugin static initializers (`.CRT$XCU`, for example from the `ctor` crate) run before `LoadLibraryExW` returns, while the loader lock is held. They must not wait for another thread (`join`, or blocking on a channel or lock): new threads cannot run until the loader lock is released, so waiting deadlocks. Allocation, locking, and accessing `thread_local!` are fine. `rutis_plugin_entry` runs after `LoadLibraryExW` returns and is not subject to this restriction.
- **Export count:** one DLL can export at most 65,535 symbols. A release `rutis_sdk.dll` has about 1,600; `build-dylib-bundle.sh` fails above 30,000. Dev builds (opt-level 0) export generic instances and currently have about 14,500; many additional dependencies could approach the limit. If necessary, set opt-level ≥ 2 for all SDK dependencies, for example `[profile.dev.package."*"] opt-level = 2`, plus separate settings for workspace members such as `rutis`. Setting only the `rutis-sdk` package has no effect.
- **Build:** each SDK/host `build.rs` adds `/Brepro` to remove linker timestamps; combined with path remapping, this makes SDK bytes identical across build directories.
- **Tests:** the three scripts above run in Git Bash. Additional Windows tests verify that same-named DLLs in cache, working directory, and `PATH` are ignored; the three release files cannot be modified, deleted, or renamed while the host runs; forcibly terminating the launcher terminates the host; and the launcher refuses to start if any runtime file is missing.
- **CI:** Windows dylib tests take about 30 minutes and run in a separate `dylib-windows` workflow only for PRs changing dylib-related code (SDK, `rutis-dylib*`, xtask, test fixtures, and scripts), pushes of release tags (`v*`, `rutis-v*`, `loader-v*`), or manual dispatch. Kernel or dependency upgrades can also affect Windows plugins, so consider running this workflow manually before merging those changes.
