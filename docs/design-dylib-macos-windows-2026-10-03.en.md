# Dylib Plugin Loading: macOS Implementation and Windows Feasibility (2026-10-03)

> Related to [#102](https://github.com/arcships/rutis/issues/102) (macOS) and [#103](https://github.com/arcships/rutis/issues/103) (Windows), and to V1, V2, V3, V7, and V8 in the upstream [dylib SDK design](design-dylib-sdk-2026-09-24.en.md) §11.
>
> Status: design, macOS preliminary experiments, one review (§7), and external research (§8). The default remains a statically linked single binary on every platform.

## 1. Scope and assumptions

| Platform | This document provides | Condition for conclusion |
| --- | --- | --- |
| macOS arm64 | Implementation plan and acceptance criteria like Linux | Preliminary experiments (§2) ruled out the main infeasible options |
| macOS x86_64 | Out of scope for this round | Open a separate CI task after arm64 passes |
| Windows x64 (msvc) | Validation plan and stop conditions | Record results in SDK design §11 before deciding whether to implement |
| windows-gnu, other Unix | Unsupported | Continue to provide static linking only |

**Prerequisite 1: P3–P6 must reach main first.** #98 (P3 `DylibResolver`) through #101 were merged onto stacked base branches, not `main`: `origin/main` was still at #97, `DylibResolver` existed only on `feat/loader-catalog-expr`, and complete P6 code was on `feat/loader-volatile`. The #102 acceptance criteria include the `loader_host` example. [#104](https://github.com/arcships/rutis/pull/104) merged the same tree into main on 2026-10-03, satisfying this prerequisite.

**Prerequisite 2: fix the existing SDK reproducibility test.** `tools/test-dylib-repro.sh` and CI's `sdk-repro` build with `cargo build -p rutis-sdk`. When SDK is the primary package, Cargo does not pass `-C prefer-dynamic`, so the artifact statically links std. The released SDK is built as a dependency of `rutis-cli --features dylib-plugins` and dynamically links libstd; the bytes differ (review reproduced this on macOS: `otool -L` showed no libstd for the former, and the hashes differed). The test therefore proves reproducibility of a different artifact. Build against the host anchor, as `build-dylib-bundle.sh` does, before comparing SDK hashes. This is already a Linux problem and should be fixed separately.

## 2. macOS preliminary experiments (2026-10-03)

Environment: macOS 26 arm64, rustc 1.98.1, ld-1267, MacOSX SDK 26.5. The prototype had three minimal components: `sdk` (dylib with a global counter), `greeter` (dylib depending on sdk, with `#[used] #[link_section]` boot data), and `host` (depends on sdk and loads plugins with `dlopen`). v1 and v2 were the same crate name built into separate target directories. The prototype lived in a temporary session directory and was not committed. Review added E13–E16.

| # | Experiment | Result | Impact on design |
| --- | --- | --- | --- |
| E1 | `#[used] #[link_section = "__DATA,__rutis_meta"]` | Preserved in release dylib; `otool -s __DATA __rutis_meta` read the full contents | Mach-O section is usable (§3.1) |
| E2 | Default install name written by rustc | Absolute build-directory path (`…/target/release/deps/libsdk.dylib`), not `@rpath/…`; libstd uses `@rpath/libstd-<hash>.dylib` | Host and plugin record an absolute SDK dependency |
| E3 | Load v1 and v2 plugins simultaneously using E2 defaults | Both load, but v2 loads a second SDK from its own target directory; after each plugin increments the counter, host reads 1 rather than 2 | SDK splits: TypeId and tokio runtime are no longer singletons. `dlopen` and L1/L2 do not prevent it (§3.3) |
| E4 | SDK `build.rs` emits `cargo::rustc-link-arg=-Wl,-install_name,@rpath/libsdk.dylib` | Applies only to SDK and works for `crate-type = ["dylib"]`; host and plugin dependencies become `@rpath/libsdk.dylib` | Per-crate linker argument injection works (§3.3) |
| E5 | After E4, delete both target directories, keep only release directories, load v1 and v2 | Each reports its own version; private types remain separate; counter is 2 and SDK is singular. Review also tested both plugins with install name `@rpath/libgreeter.dylib`; both loaded separately | The central V2 claim holds: two namespace levels plus `RTLD_LOCAL` allow same-named crate versions to coexist |
| E6 | Build SDK as a dependency in different target directories and compare path-remapped SHA-256 | Byte-identical, including install name and linker's ad-hoc signature; review repeated with five target directories | Reproducible on one machine; cross-machine CI remains (§3.6) |
| E7 | Set host `DYLD_LIBRARY_PATH` to a malicious directory containing a valid SDK with a `__mod_init_func` initializer | Initializer runs before `main`; `DYLD_INSERT_LIBRARIES` also works | Caller DYLD_* reaches a process without hardened runtime (§3.4) |
| E8 | Put an invalid file in the malicious directory | dyld skips it and loads the correct SDK by `@rpath` | Only a valid Mach-O is a threat |
| E9 | Host uses hardened runtime (`codesign -o runtime`) | DYLD_* is ignored and removed from environment, but library validation rejects ad-hoc SDK because Team ID differs | Decide hardened runtime together with signing design (§3.4) |
| E10 | Hardened runtime plus `com.apple.security.cs.disable-library-validation` | DYLD_* remains ignored; SDK and plugin load normally | Defense-in-depth option on the host |
| E11 | `dlopen` a plugin with `com.apple.quarantine` | Call never returns while Gatekeeper evaluates it; without UI it hangs. `cp` copies the attribute to the target | Check the actual file before `dlopen` (§3.2) |
| E12 | Weak definitions in plugin export table | Prototype has no weak Rust symbols; toolchain libstd has compiler-rt weak definitions such as `___isOSVersionAtLeast` | Check only Rust-mangled names for weak definitions (§3.5) |
| E13 | `DYLD_INSERT_LIBRARIES` in the environment of a launcher without hardened runtime | Inserted initializer runs before launcher `main`; after re-signing launcher with `-o runtime`, it no longer runs and `main` sees no DYLD_* | Injecting the launcher itself is “controlling the process loader,” outside SDK design §5.4 protection; launcher only has to keep DYLD_* from reaching host (§3.4) |
| E14 | Add a counting `#[global_allocator]` to SDK | Host and plugin allocations use it; libstd internal allocations do not (`current_dir()` and `read_to_string()` leave count unchanged). `nm -m` shows libstd exports its own `___rust_alloc`, while host and plugin bind `(from libsdk)` | Two namespace levels split allocators; SDK allocator must be `System` (§3.5, §8 R1) |
| E15 | Run `install_name_tool -id` and `strip -x` on linker-signed plugin | `codesign -v` still passes and host loads; Apple tools automatically re-sign linker-signed ad-hoc signatures | Post-link edits do not break signature but change bytes; set values at link time (§3.4) |
| E16 | Test `DYLD_X=… /usr/bin/env prog`, `/usr/bin/env DYLD_X=… prog`, and `/bin/sh -c` | First and third are stripped; second takes effect | SIP strips variables only when executing protected system binaries; E7 is real |

## 3. macOS design

Expand the five items in #102; deviations from the issue are called out.

### 3.1 Read identity before loading

Add `rutis-dylib-meta`, depending only on `object` (features `read_core`, `elf`, `macho`, `pe`, `std`) and not on `rutis` or `rutis-sdk`. It provides `read_boot(bytes, target) -> Result<BootMeta, String>` and `check_deps(bytes, policy) -> Result<(), String>` (§3.3), shared by `rutis-dylib` and `xtask pack-plugin`.

The separate crate lets xtask inspect artifacts for any target without linking the SDK dylib and dynamic libstd as it would through `rutis-dylib`. The SDK must not depend on this crate because that would change the SDK dependency tree and identity. Therefore `BOOT_MAGIC` and `BOOT_SIZE` are duplicated on both sides, with a test in `rutis-dylib` asserting that the values match.

Select the section name by target format in `export_plugin!` with `cfg_attr`:

| Format | Section | Notes |
| --- | --- | --- |
| ELF | `.note.rutis.meta` | Unchanged |
| Mach-O | `__DATA,__rutis_meta` | Section name is 12 bytes, below the 16-byte limit; verified by E1 |
| PE | `.rutism` | Image section names are at most 8 bytes; `.note.rutis.meta` would be truncated (§4 W6) |

Read strictly (stricter than current ELF parsing):

1. Format must match target. ELF: ELF64 little-endian, `ET_DYN`, and `e_machine` matches. Mach-O: 64-bit, `MH_DYLIB`, CPU matches, and `LC_BUILD_VERSION.platform` is macOS (an arm64 iOS simulator artifact has the same CPU type). PE: PE32+ DLL with matching `Machine`. Report a readable architecture error here instead of dyld's “incompatible architecture.”
2. **Reject fat (universal) Mach-O.** Build for one target and publish one architecture.
3. Exactly one section with the expected name; its size equals `BOOT_SIZE` and it starts with `BOOT_MAGIC`.
4. Read section bytes only; do not apply relocations. The boot blob is pure data and its format is unchanged.

Remove the hand-written Linux `elf_section` parser and use the same implementation. Existing Linux tests, including bad-boot, must still pass.

### 3.2 Loader

Rename `rutis-dylib`'s `linux` module to `unix`. Gate it with `cfg(any(target_os = "linux", target_os = "macos"))`; do not use `cfg(unix)` because FreeBSD and others have not been validated. Do not emit a custom cfg from build.rs because it would not reach downstream crates such as `rutis-cli` and examples. Update `DylibResolver`, the `dylib-plugins` feature in `rutis-cli`, and examples to use this condition. The `compile_error!` at the top of current `linux.rs` never fires because `lib.rs` excludes the module; remove it during the rename.

`dlopen` / `dlsym` / `dladdr` usage remains unchanged. macOS differences:

- **Library filenames are not hard-coded.** Packaging derives prefixes and suffixes (`lib*.so`, `lib*.dylib`, `*.dll`) from the target triple. The loader already reads `library` from the manifest.
- **Quarantine (E11):** reject if either the source file or cache file has `com.apple.quarantine`. Explain the reason and the user-controlled remedy (`xattr -d com.apple.quarantine`); never clear it automatically, which would bypass Gatekeeper on the user's behalf.
  1. **Source file:** on every `load`, check before reading the source or writing/reusing cache, whether or not the cache will hit. Cache writes use “read bytes → write temporary file → rename” and do not preserve extended attributes. If only the cache were checked, the quarantine bit would be lost on the first load of a downloaded plugin and the check would be meaningless. Copying into cache is not user consent.
  2. **Cache file:** check the actual cache file before `dlopen`. `ensure_cached` reuses an existing hash-matched entry (`linux.rs:555`); a same-hash file placed with `cp` or Finder may carry the attribute.
- **Replacing mapped files:** rewriting a loaded dylib in place fails code-signature page checks and kills the process. Existing cache behavior (temporary file plus atomic rename, never overwrite in place) already prevents this; document it in a comment.
- **Host self-check:** `loaded_sdk_path()` continues to use `dladdr`; review confirmed that macOS returns the expanded absolute path.

### 3.3 Install names, library search paths, and dependency checks

This is the key point missing from the issue. E2/E3 show that **rustc's default install name is an absolute build-directory path, so a plugin can load a second SDK even when L1/L2 both pass.** L1/L2 check which SDK compiled the plugin, not where dyld actually loads the SDK. If the SDK copies have identical bytes, both checks pass. Linux does not have this issue: rustc emits a filename in `DT_NEEDED`, and glibc reuses the SDK already loaded by name.

**Inject linker arguments per artifact; stop putting them in `RUSTFLAGS`.** Today `-rpath,$ORIGIN` in RUSTFLAGS applies to SDK, host, and plugin (`tools/pack-dylib-plugin.py:155`, `tools/test-dylib.sh:10`, `tools/build-dylib-bundle.sh:13`, and CI `sdk-repro`). Plugin builds must reproduce identical SDK bytes, so RUSTFLAGS cannot vary only for plugins; similarly, setting plugin install name in RUSTFLAGS would affect SDK. Instead use each crate's build.rs:

| Artifact | Injection point | Linux | macOS |
| --- | --- | --- | --- |
| SDK | `rutis-sdk/build.rs`, `rustc-link-arg` | `-rpath,$ORIGIN` | `-install_name,@rpath/librutis_sdk.dylib`, `-rpath,@loader_path` |
| Host | `rutis-cli/build.rs`, `rustc-link-arg-bins` under `CARGO_FEATURE_DYLIB_PLUGINS` | `-rpath,$ORIGIN` | `-rpath,@loader_path` |
| Example host | `rutis-dylib/build.rs`, `rustc-link-arg-examples` | Same | Same (test scripts no longer use `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH`) |
| Plugin | None | No RUNPATH | No LC_RPATH; install name is not forced because `dlopen` uses its path |

This changes Linux SDK bytes and should be combined with the SDK upgrade (§3.6).

**Plugins have no rpath.** When a plugin is `dlopen`ed, SDK and libstd are already loaded by the host, so dyld reuses them by install name (review saw `already-loaded-by-rpath` with `DYLD_PRINT_SEARCHING`). Review also confirmed that a missing `@rpath` dependency in a plugin without LC_RPATH is searched only at the host's `@loader_path` (release directory), not in the plugin cache directory.

**Check dependencies before loading** (new, part of step 2 in SDK design §7.1). Every plugin dependency must fit one of these three categories; otherwise reject it.

| Category | Allowed | Reason |
| --- | --- | --- |
| Shared Rust components | `librutis_sdk`; exact match to the launcher's bound `libstd-<hash>` | Must reuse the copy already loaded by host |
| Native libraries (for example `libssl`, `libz`, system frameworks) | Allowed | Plugins may dynamically link system or user-installed native libraries |
| Other Rust dylibs | Not allowed | Would bring in another std or SDK |

Native-library dependency paths must be written safely so they cannot resolve from the plugin cache or another uncontrolled location:

- **Mach-O:** inspect all dylib load commands (`LC_LOAD_DYLIB`, `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`, `LC_LOAD_UPWARD_DYLIB`, `LC_LAZY_LOAD_DYLIB`). SDK and libstd must use `@rpath/librutis_sdk.dylib` and `@rpath/libstd-<hash>.dylib`. Reject other `@rpath/…` dependencies (they resolve in the release directory, where those libraries are absent). Native libraries must use absolute paths, e.g. `/usr/lib/libz.1.dylib`, `/System/Library/Frameworks/…`, or `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`. Reject dependency paths containing `@executable_path`, `@loader_path`, relative paths, or `..`. Plugins must not have `LC_RPATH`; reject unknown commands with `LC_REQ_DYLD`. Require `MH_TWOLEVEL`; reject `MH_FORCE_FLAT` and flat-lookup bindings, since a plugin C dependency built with `-undefined dynamic_lookup` could bind v2 symbols to v1. For host and SDK (checked at package time), also reject `LC_DYLD_ENVIRONMENT`.
- **ELF:** `DT_NEEDED` must not contain `/`; SDK must be `librutis_sdk.so`, and libstd must exactly match the bound filename. Treat other names as native libraries for the system dynamic linker to search normally. Plugins must not contain `DT_RUNPATH` / `DT_RPATH`; reject `DT_AUXILIARY`, `DT_FILTER`, `DT_AUDIT`, and `DT_DEPAUDIT`.
- Dependencies named like `libstd-*` or `librutis_sdk*` but not matching the bound value are rejected as “other Rust dylibs.” Rust dylibs in transitive dependencies cannot all be identified before loading; document this for plugin authors.

Native libraries are **outside the launcher's validation scope**; the party deploying the plugin is responsible for them, and they are trusted just like the plugin. For diagnosis and auditability:

- Packaging writes native dependencies to manifest `[plugin] native_deps`; the loader compares that list with binary dependencies and rejects mismatches.
- If a native library is missing, `dlopen` (`RTLD_NOW`) fails directly and names the missing library in the error.
- On Linux the dynamic linker reuses an already-loaded library by SONAME. If two plugin versions require different implementations with the same SONAME, the later plugin uses the first one. Incompatible native-library versions must have different SONAMEs (as system libraries usually do); document this for plugin authors.
- If host uses hardened runtime (§3.4), native libraries must also satisfy its signing requirements.

Use the same checker on host and SDK at package time, replacing `ldd` in `build-dylib-bundle.sh`. CI additionally prints `otool -L` / `otool -l` output for manual comparison.

### 3.4 Launcher and code signing

**Correct the issue text.** It says “SIP clears DYLD_* environment variables, so they cannot be relied on.” The latter conclusion is right: the release directory uses `@rpath` / `@loader_path`, not `DYLD_LIBRARY_PATH`. The former claim is wrong: SIP strips variables only when executing protected system binaries (E16). Caller DYLD_* can reach the host; in E7, an impostor SDK ran before host `main`.

**Protection scope matches SDK design §5.4: protect against deployment and environment mistakes, not an attacker who controls the process loader.** The typical threat is a developer machine with `DYLD_LIBRARY_PATH` or `DYLD_INSERT_LIBRARIES` pointing at another SDK build. These variables do not affect the launcher (it does not link the SDK); they affect the host it starts. The launcher therefore handles them like Linux handles `LD_*`:

1. Verify host, SDK, and libstd hashes, as on Linux.
2. Rename all `DYLD_*` variables to `RUTIS_ORIG_DYLD_*` and remove the originals from the host environment; set no `DYLD_*` variables.
3. `exec` the host. Before starting runtime threads, host restores `RUTIS_ORIG_*` (extend `rutis-cli`'s existing `LD_*` restoration to `DYLD_`). Child processes started by host see the caller's original environment. Review confirmed that setting `DYLD_*` after host is running does not affect later `dlopen` calls.
4. The launcher needs no special signature; the linker's ad-hoc signature is sufficient. The host publisher may re-sign according to its own policy.

**Document what is not protected:** `DYLD_INSERT_LIBRARIES` can inject code into the launcher itself, which runs before its `main` (E13). The launcher cannot prevent this; it is control of the process loader and outside the protection scope. Publishers that need this protection may sign the launcher with hardened runtime (`codesign -o runtime`, with no entitlements; it depends only on system libraries and is unaffected by library validation). This prevented injection in E13. rutis does not do this by default.

**Whether the host enables hardened runtime is the publisher's choice; rutis does not mandate it.** rutis supports the loader in either case and documents their differences.

| | Off | On |
| --- | --- | --- |
| Run host directly, bypassing launcher: DYLD_* | Takes effect (E7) | Ignored (E9) |
| Signature requirement | None | SDK, libstd, plugins, and native dependencies must share the host's Team ID, or host must have `com.apple.security.cs.disable-library-validation` (E10) |
| Notarization | Cannot be notarized | Required for notarization |

The supported entry point is the launcher either way (SDK design §5.4). With hardened runtime, dyld removes received DYLD_*; step 3 still works because host restores environment variables within its own process, independently of dyld. Bundled `rutis-cli` does not enable hardened runtime. Add a CI host variant with hardened runtime and `disable-library-validation` and verify reload tests.

**Set linker arguments at link time; do not post-process.** `install_name_tool` and `strip` do not break a linker's ad-hoc signature (E15; Apple tools re-sign automatically), but they change bytes, create a second artifact variant, and break L2 and reproducible builds. Set install name and rpath in build.rs (§3.3). A publisher re-signing with its own certificate (e.g. Developer ID) is the final release step and does not change SDK identity. L2 binds the built SDK and plugin bytes; re-signing changes those bytes, so re-signing is limited to host and launcher. Publishers needing SDK/plugin re-signing must do so before computing L2. CI verifies every file in the release directory with `codesign --verify`.

### 3.5 V2: effects of two-level namespaces

E5 answers the main SDK design V2 question: under `RTLD_LOCAL` and two-level namespaces, two versions of the same crate coexist without crossing, and the SDK remains singular. Three more points:

- **The global allocator is split (E14).** SDK design §4.3 says define the allocator only in SDK, relying on ELF symbol interposition on Linux so libstd allocations use it. With macOS two-level namespaces, libstd's internal `__rust_alloc` binds to its own default export, not SDK's allocator. SDK currently uses `System`, matching libstd's default; changing to mimalloc would make libstd allocations and frees by host/plugins use mismatched allocators, undefined behavior. **Rule: SDK's allocator must be `System` on every platform.** This is an upstream known, unfixed issue (§8 R1): on Linux, prefer-dynamic with jemalloc also crashes since 1.71. Add compile-time assertion that SDK's `#[global_allocator]` type is `std::alloc::System`, and retain E14's counter experiment as regression coverage. Change SDK design §4.3 from “switching to mimalloc/jemalloc is an SDK change” to “not allowed until upstream fixes this.”
- **Weak-definition coalescing.** dyld coalesces weak definitions across loaded images, so a v2 plugin's weak symbol could bind to v1. Packaging rejects weak definitions with **Rust-mangled names** (`_ZN` or `_R` prefix) in a plugin export table. Whitelist compiler-rt helpers such as `___isOSVersionAtLeast` and `___isPlatformVersionAtLeast` (E12; libstd itself has them). Fail packaging with any non-whitelisted weak symbol and list its name.
- **Same install name for both versions.** E5 verified separate loading. Keep generation-change coverage in `test-dylib.sh`; no version-specific install name is needed.

### 3.6 SDK identity, reproducible builds, and toolchain

- **SDK upgrade.** Both section names (§3.1) and build.rs-injected linker arguments (§3.3) change the SDK, so the Linux SDK hash changes and Linux plugins must be rebuilt, as required by SDK design §10.2 item 4. P3–P6 already changed `export_plugin!` (adding `rutis_plugin_config_schema`), while `rutis-sdk` remains 0.4.0. Make one minor release and update hard-coded `version = "0.4.0"` in `rutis-dylib` and `rutis-cli` too.
- **macOS build data is diagnostic only; exclude it from L1.** Artifact bytes also depend on deployment target (`minos`), MacOSX SDK version, and linker version, recorded in `LC_BUILD_VERSION` (e.g. `minos 11.0 / sdk 26.5 / ld 1267.0`). The packager reads these three values into `sdk.toml` and plugin manifest `[build]` for diagnosing L2 mismatch. Do not put them in `SDK_ID`: SDK design §5.2 says L1 includes only ABI-affecting inputs, and `ld -v` in build.rs may not be the linker rustc actually invokes. Set `MACOSX_DEPLOYMENT_TARGET` explicitly in release scripts (recommend 13.0) and pin Xcode with `xcode-select` in CI.
- **Cross-machine reproducibility (macOS portion of V1).** E6 covers only one machine. Add two macos-15 runners to the `sdk-repro` matrix with different source, target, and Cargo home paths; build against the host anchor (prerequisite 2) and compare SDK hashes. Different Xcode versions may produce different hashes; the diagnostics above explain this.
- **Debug info.** macOS debug maps (OSO stabs) record absolute `.o` paths, which `--remap-path-prefix` cannot fix. This is currently harmless because Cargo's default release profile is `debug = 0`, `strip = "debuginfo"`, and the repository does not explicitly declare `[profile.release]`. Declare both settings and have reproducibility tests assert there are no OSO entries in the SDK, so enabling line tables cannot silently break reproducibility.

### 3.7 Packaging and tests

Rewrite `pack-dylib-plugin.py` in Rust inside `rutis-xtask`, using `rutis-dylib-meta` so pack and load share the same logic. Keep `cargo xtask pack-plugin` arguments unchanged.

**Script portability.** Replace GNU-specific uses in `tools/test-dylib*.sh` and `build-dylib-bundle.sh`: `sed -i` (BSD needs `-i ''`), `find -printf`, `ldd`, hard-coded `.so`, and `sha256sum` (available in macOS 26 but not older versions). Add `tools/lib/dylib-common.sh` with helpers such as `sha256_of`, `dylib_name`, and `std_dylib`; have scripts use it and delegate dependency checks to xtask. System `/bin/bash` on macOS is 3.2; under `set -u`, expanding an empty array errors, so avoid that pattern.

Additional macOS tests beyond the existing Linux suite:

| Test | Expected result |
| --- | --- |
| Start via launcher with caller `DYLD_LIBRARY_PATH` pointing at a valid SDK with an initializer that writes a marker | Host loads release-directory SDK and no marker is written; host child can see original `DYLD_LIBRARY_PATH` |
| Start via launcher with caller `DYLD_INSERT_LIBRARIES` whose initializer records the process | May run in launcher (out of scope), but not in host |
| Plugin declares SDK dependency by absolute path (reproduce E3) | Rejected before `dlopen` for dependency mismatch |
| Plugin has LC_RPATH, flat lookup, or `@loader_path` dependency | Packaging fails; manually packaged plugin is rejected before load |
| Plugin dynamically links a system native library such as `/usr/lib/libz.1.dylib` | Loads normally; library appears in manifest `[plugin] native_deps` |
| Binary native dependencies differ from `[plugin] native_deps` | Rejected before load |
| Host uses hardened runtime + `disable-library-validation` | Generation-change test passes |
| Source has quarantine and cache is empty | Rejected before reading/writing cache; file does not appear in cache |
| Source has quarantine and cache already has a clean same-hash entry | Rejected (source check is not skipped on cache hit) |
| Source is clean but same-hash cache entry has quarantine | Rejected before `dlopen`, without hanging |
| Fat, x86_64, or iOS simulator plugin | Rejected before load with format, architecture, or platform error |
| SDK counting allocator (E14) | Host and plugin allocations pass through SDK; assert allocator type is `System` |
| Run `codesign --verify` on release directory | All files pass |

**CI.** Add `dylib-macos` on macos-15 arm64, running the same three scripts as `dylib-linux` and the `loader_host` example. Add two macos-15 runners to `sdk-repro`. Keep the macOS entry in `static-platforms` to continue checking default static builds.

### 3.8 Plugins signed by other developers

The SDK design targets first-party plugins. Plugins may also come from another team or company signed with its own Apple developer account. This is technically possible under these conditions.

**Signing.** Whether the plugin can load depends on host signing:

| Host | Plugin signed by another developer |
| --- | --- |
| No hardened runtime | Loads; ad-hoc signed plugins also load |
| Hardened runtime with `disable-library-validation` | Loads; host may still be notarized |
| Hardened runtime without that entitlement | Fails: system allows only libraries signed with the host's Team ID |

A downloaded plugin has quarantine; its author must notarize it, or the user must remove the attribute manually (§3.2).

**Optional Team ID allowlist.** Host may configure allowed Team IDs. If configured, before `dlopen` the loader uses Security (`SecStaticCodeCreateWithPath` + `SecStaticCodeCheckValidity`) on the actual cache file to validate the signature and check the signer's Team ID against the list. Otherwise reject, including ad-hoc signed plugins. If not configured, do not check and preserve current behavior. This is macOS-only and independent of L1/L2: L1/L2 establish plugin/SDK compatibility; Team ID establishes who signed the plugin. Native libraries (§3.3) are outside this check.

**Build requirements are the larger obstacle.** A plugin must link the exact SDK artifact used by the host: same rustc, lockfile, and feature set. Current packaging includes the host package in the same Cargo build as the plugin (SDK design §5.3), so external developers need the host's build anchor and lockfile and must rebuild on every SDK upgrade. Convenient third-party plugin builds need a published “SDK build bundle” (lockfile + build anchor + parameters) and proof that different Cargo dependency graphs produce identical SDK bytes, an unproven item in SDK design §5.3. See [#108](https://github.com/arcships/rutis/issues/108).

**Trust.** Dylib plugins run in the host process without isolation; a plugin bug can crash the host and access all host memory. A signature identifies the publisher but does not establish safety. Untrusted code should use protocol plugins (SDK design §1, non-goals).

### 3.9 Acceptance mapping

| #102 acceptance item | Coverage |
| --- | --- |
| Version replacement and consumer reload | `test-dylib.sh` (§3.7) |
| Reject wrong identity before initializer code | §3.1 + bad-boot test; add wrong dependency (§3.3) |
| Refuse startup if host, SDK, or libstd was modified | `test-dylib-launcher.sh`; add DYLD_* test (§3.4) |
| Reproducible SDK bytes | Fixed `test-dylib-repro.sh` (prerequisite 2) + cross-runner CI comparison (§3.6) |
| `DylibResolver` `loader_host` example | Prerequisite 1 |

## 4. Windows feasibility validation plan

Proceed in order; if an item is infeasible, stop and record the result in SDK design §11. Put experiments in `docs/probes/windows-dylib/`, on branch `probe/windows-dylib`, and run them on windows-2025 with a push-triggered CI job (`workflow_dispatch` can only trigger a workflow already on the default branch, so it is unsuitable for a temporary task). Consider only `x86_64-pc-windows-msvc`. W1 has no dependency on other changes and can start immediately.

| # | Validation | Method | Infeasible if |
| --- | --- | --- | --- |
| W1 | **Export count** | Build `rutis_sdk.dll` with current `rutis-sdk` and host anchor; use `object` to count PE exports and check for LNK1189 (import-library object limit). Measure release (opt-level 3) and dev (opt-level 0): dylib exports generic monomorphizations, and share-generics is on by default at opt-level 0/1, so dev exports many more (§8 R7). Repeat with common dependencies such as `tokio/full` and serde derive to estimate growth. | Current count exceeds limit or lacks margin for an ordinary dependency upgrade. Toolchain stays stable, so `-Z` is unavailable; release already disables share-generics and has no room to reduce exports |
| W2 | **Same-named DLL versions** | Use `LoadLibraryExW` with full paths for `<cache>/<hashA>/greeter.dll` and `<cache>/<hashB>/greeter.dll` | Second load returns first module and neither path nor cache naming can avoid it |
| W3 | **Dependency resolution** | Host's static SDK/std imports resolve by standard search order (application directory first). Load plugins with `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32` (not `DLL_LOAD_DIR`, which searches plugin cache like the rejected `@loader_path` in §3.3); SDK dependency should reuse loaded module. Put same-name DLLs in working directory, `PATH`, and `.local` redirection directory. | An unclosable search path makes host or plugin resolve SDK/std outside release directory. `SetDefaultDllDirectories` affects only later `LoadLibrary`, not host's own static imports |
| W4 | **std DLL and VC runtime** | Put `std-*.dll` in release directory and validate it with launcher. Prebuilt `std-*.dll` dynamically depends on VC runtime such as `vcruntime140.dll`, whether or not host uses `+crt-static`. | Not considered infeasible: user installs VC++ runtime; rutis neither distributes nor checks it, only documents it |
| W5 | **Loader lock** | Plugin initializer (`rutis_plugin_entry`) runs after `LoadLibrary` returns, outside loader lock. Test whether Rust std TLS callbacks and `.CRT$XCU` static initializers in private plugin dependencies (e.g. `ctor`, `inventory`) can deadlock under loader lock. | Common dependencies deadlock under loader lock and no rule can prohibit them |
| W6 | **Boot blob** | Check whether `#[link_section = ".rutism"]` + `#[used]` survives MSVC linker `/OPT:REF` and whether `object` can locate it | Cannot preserve it and has no alternative, e.g. export a data symbol and locate it in export table |
| W7 | **File locking** | Loaded DLL cannot be overwritten or deleted. Confirm content-addressed cache only creates, never overwrites, and reports a readable error if a corrupt entry is locked | Not expected to be infeasible |
| W8 | **Runtime behavior across DLLs** | Run greeter fixture tests on windows-2025 unchanged: TypeId, downcast, `tokio::spawn` using host runtime, singleton `thread_local!`, `catch_unwind`, and Drop. MSVC does not support importing TLS variables across DLLs with dllimport; since rustc 1.70, cross-crate TLS access for dylib uses shim functions (§8 R8), so it should work, but no real Rust-dylib + tokio Windows report was found. | Any test fails |

**Differences not mentioned in the issue but required for implementation** (record in validation report and include in implementation issue):

- **The launcher cannot `exec`.** Windows has no call to replace the current process. Launcher must create and wait for a child: forward exit code, ignore its own Ctrl+C (child in same console receives it), and use a Job Object so launcher exit ends the child too. Process IDs differ from Linux/macOS; tools depending on host PID must account for this.
- **Windows can enforce more than Unix.** After validation, launcher can hold non-writable, non-deletable shared file handles to host, SDK, and std until child exit, truly keeping release directory immutable while running rather than relying on convention.
- **Library filenames have no `lib` prefix**; import libraries (`.dll.lib`) do not belong in release directory.

## 5. Phases

| PR | Contents | Dependency |
| --- | --- | --- |
| 0a | [#104](https://github.com/arcships/rutis/pull/104): merge P3–P6 into main (merged) | — |
| 0b | Fix SDK reproducibility test to build against host anchor (prerequisite 2) | — |
| A1 | `rutis-dylib-meta` (`object`); use it to read boot blob on Linux; rewrite packager in Rust; common script helpers. Linux behavior and SDK bytes unchanged | 0a |
| A2 | Inject linker arguments per artifact through build.rs; dependency checks (§3.3); choose `export_plugin!` section by format; explicit `[profile.release]`; SDK minor upgrade | A1, 0b |
| B | macOS: `unix` module and platform cfg, SDK install name, launcher DYLD_* sanitizing/restoration, quarantine, optional Team ID check, allocator assertion, build diagnostics, `dylib-macos` CI and sdk-repro | A2 |
| C | Validate Windows W1–W8 and record in SDK design §11. W1–W5 and W7 can start immediately; W6/W8 use section name and fixtures from after A2 | W6/W8 depend on A2 |

A1 is a pure refactor; A2 changes SDK bytes and groups the SDK upgrade. B and C are independent.

Implementation status (2026-10-03): 0b [#113](https://github.com/arcships/rutis/pull/113), A1 [#114](https://github.com/arcships/rutis/pull/114), A2 [#115](https://github.com/arcships/rutis/pull/115), B [#116](https://github.com/arcships/rutis/pull/116), and C [#119](https://github.com/arcships/rutis/pull/119) (Windows feasible; implementation in [#118](https://github.com/arcships/rutis/issues/118)) have PRs open. Deviations from this document: manifest field is `native_deps` under `[plugin]`; Team ID API is `Loader::require_team_ids`; example host uses a macOS run path (target directory and toolchain libstd) to test hardened-runtime host; example plugin's initializer marker is placed in `__DATA,__mod_init_func` on macOS.

## 6. Decisions made (2026-10-03)

1. **Hardened runtime:** publisher chooses whether host and launcher enable it; rutis does not mandate it. Loader supports both, and `rutis-cli` leaves it off (§3.4). Launcher only prevents DYLD_* from reaching host; it does not protect against launcher injection.
2. **Native plugin dependencies:** allowed, with restrictions on dependency paths (§3.3).
3. **Windows VC++ runtime:** user installs it; rutis neither distributes nor checks it (W4).

## 7. Review record (2026-10-03)

An independent review repeated E13–E16 and other experiments on the same macOS arm64 machine. It found no issue invalidating the overall direction; all comments below have been incorporated.

| Level | Comment | Resolution |
| --- | --- | --- |
| P1 | `DYLD_INSERT_LIBRARIES` injects launcher before its main; original test expectation cannot hold | True. Under SDK design §5.4, launcher injection is control of process loader and out of scope. Launcher only keeps DYLD_* from host and host restores it as on Linux; test now expects it not to run in host (§3.4). An initial requirement for launcher hardened runtime was later withdrawn |
| P1 | libstd internal allocations on macOS do not use SDK allocator | Restrict SDK allocator to `System`; add assertion and regression test (§3.5, E14) |
| P1 | RUSTFLAGS rpath applies to all artifacts, conflicting with “plugin has no rpath”; injection mechanism omitted | Inject through each crate's build.rs; include in A2 that changes SDK bytes (§3.3, §5) |
| P1 | Existing reproducibility test builds static-std SDK variant, not release artifact | Added as prerequisite 2 and PR 0b (§1) |
| P1 | Windows validation omitted cross-DLL runtime behavior (TLS, tokio context) | Added W8 |
| P2 | Claim that `install_name_tool` / `strip` break signature was wrong | Corrected; rationale is reproducibility and one artifact (§3.4, E15) |
| P2 | Quarantine should check actual cache file; cache writing does not preserve extended attributes | Corrected (§3.2) |
| P2 | Dependency check was incomplete; should be allowlist and reject unknown entries | Completed Mach-O/ELF rules and exact libstd match (§3.3); native libraries later allowed per §6.2 |
| P2 | Rejecting all weak definitions affects compiler-rt helpers | Reject Rust-mangled names only; whitelist helpers (§3.5) |
| P2 | Wrong macOS L1 input | Read `LC_BUILD_VERSION`, diagnostic only (§3.6) |
| P2 | Rationale for separate `rutis-dylib-meta` crate was invalid | Changed rationale to keep xtask from linking SDK; added constant-consistency test (§3.1) |
| P2 | Custom build.rs cfg does not reach downstream | Use `cfg(any(linux, macos))` (§3.2) |
| P2 | Windows facts were wrong (`crt-static`, `-Z` args, `DLL_LOAD_DIR`, `workflow_dispatch`) | Corrected each (§4) |
| P2 | PR ordering: W1 can start now; PR A was too large | Split A into A1/A2; C does not wait for A (§5) |
| P2 | Need to distinguish the `/usr/bin/env` case in E7 | Added E16 |
| P3 | Linux has no E3-like bug; add `e_machine` / platform check, combine SDK version bump, explicit release profile, example-host rpath, Bash 3.2 | All incorporated |

## 8. External research (2026-10-03)

Research covered upstream issues, Apple/Microsoft documentation, and similar projects. No existing solution was better than this design. The allocator restriction must apply to every platform; other approaches were confirmed and some checks were added.

| # | Constraint | Research result | Impact |
| --- | --- | --- | --- |
| R1 | Allocator split (E14) | Known, unfixed upstream: [rust-lang/rust#100781](https://github.com/rust-lang/rust/issues/100781) (`global_allocator` incompatible with `-C prefer-dynamic`; Mach-O two-level namespaces and Windows libstd use System); [#114518](https://github.com/rust-lang/rust/issues/114518) (since 1.71, prefer-dynamic + jemalloc segfaults on macOS **and Linux**). Weak-symbol or function-pointer replacement for shim [#134522](https://github.com/rust-lang/rust/pull/134522) was not merged. Bevy users saw the same crash with mimalloc + `dynamic_linking` | Require `System` allocator on every platform (§3.5) |
| R1a | Statically include std in SDK so process has one allocator shim | Suggested by research, but **locally infeasible**: Cargo forces `-C prefer-dynamic` for a dylib dependency. Building SDK directly with rustc and static std still leaves host/plugin unable to link (`cannot satisfy dependencies so 'core' only shows up once`) | Rejected |
| R1b | `-flat_namespace`, `__DATA,__interpose` | Affect cross-image imports only; libstd's call to its own exported `__rust_alloc` is likely intra-image and unchanged. Flat namespace also causes global symbol conflicts and breaks multi-version coexistence | Rejected |
| R2 | Absolute default install name (E2) | Upstream default remains unchanged ([#28640](https://github.com/rust-lang/rust/issues/28640) open). `-C rpath` also sets `@rpath/<filename>` but writes build-directory LC_RPATH to every artifact. Cargo has no dylib-specific `rustc-link-arg-*`, only package-wide `rustc-link-arg` | Keep §3.3 build.rs design |
| R3 | DYLD_* injection (E7, E13) | Apple DTS confirms hardened runtime ignores and removes DYLD_* except with `allow-dyld-environment-variables` or `get-task-allow`. No specific note found for ad-hoc + runtime | Optional publisher hardening in §3.4; launcher must have neither entitlement |
| R4 | Quarantine (E11) | Apple docs: since 10.15, quarantined plugins need notarization or user approval in System Settings; headless evaluation appears hung. A single dylib cannot staple a notarization ticket. Audio-plugin hosts commonly tell users to run `xattr -d` or distribute notarized plugins | Keep reject-before-load plus `xattr -d` guidance (§3.2) |
| R5 | Same install name, different paths (E5) | Apple says dyld locates by path first, then checks loaded-image table; full-path `dlopen` of two files creates two images. Risk is plugin `@rpath/…` dependency, where dyld reuses loaded image with same name | §3.3 limits plugin deps to host-loaded SDK and libstd; this reuse is desired |
| R6 | Never `dlclose` | On macOS dyld already ignores `dlclose` for images that used TLS (Rust `print!` does); abi_stable also explicitly does not support unload | Confirms SDK design §9 |
| R7 | Windows export limit (W1) | Real issue: Bevy [#1110](https://github.com/bevyengine/bevy/issues/1110) (open since 2020), [#14930](https://github.com/bevyengine/bevy/issues/14930). Dylib exports generic monomorphizations; share-generics defaults on at opt-level 0/1. Bevy requires opt-level 3 for dependencies when dynamic linking on Windows. Stable alternatives unavailable: `-Zshare-generics=n` and `#[export_visibility]` ([#151425](https://github.com/rust-lang/rust/issues/151425)) are unstable | W1 measured in [#119](https://github.com/arcships/rutis/pull/119): release 1,597 exports, dev 14,545. Setting opt-level 2 only for rutis-sdk did nothing (dev still 14,543); setting it for all dependencies dropped count to 2,609. If reducing dev exports is needed, set `[profile.dev.package."*"] opt-level = 2` |
| R8 | Windows cross-DLL TLS (W8) | Since rustc 1.70 ([#108089](https://github.com/rust-lang/rust/pull/108089)), MSVC dylib cross-crate TLS access uses shim functions; 1.98 switched TLS destructors to FLS. If tokio is only in SDK, context should be singular | W8 risk lower, but still requires testing |
| R9 | Two same-named DLL versions (W2) | Microsoft docs: full path searches only that path; dependent DLLs resolve by module name and prefer already loaded modules | W2 expected to work; do not put another SDK copy in plugin directory |
| R10 | VC runtime (W4) | Research suggested rustup prebuilt std depends on vcruntime140.dll. **W4 disproved this:** `std-*.dll` has no VC runtime dependency; SDK, host, and plugin depend on `VCRUNTIME140.dll` and UCRT | Do not enable `crt-static`; user installs VC++ runtime (§6.3) |

## 9. PR review record (#105, 2026-10-03)

| Level | Comment | Resolution |
| --- | --- | --- |
| P2 | Checking quarantine only on copied cache file is bypassable: cache copies bytes without extended attributes, losing the flag on first load of downloaded plugin | Check source before reading, writing, or reusing cache regardless of hit; check actual cache file again before `dlopen`; add three acceptance tests (§3.2, §3.7) |
