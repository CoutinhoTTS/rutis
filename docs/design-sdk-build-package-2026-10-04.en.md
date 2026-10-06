# SDK Build Bundle: Build External dylib Plugins Without Rebuilding the SDK (2026-10-04)

For [#108](https://github.com/arcships/rutis/issues/108). Preconditions and background: [dylib SDK design](design-dylib-sdk-2026-09-24.md) §5.2/§5.3 and [macOS/Windows design](design-dylib-macos-windows-2026-10-03.md) §3.8.

Revised after the 2026-10-04 review (see §10): the build bundle now includes the dependency-closure rlibs, injection uses `-L dependency=`, coexistence rules have been rewritten, and acceptance semantics have been updated.

## 1. Conclusion Up Front

1. **External builds use a precompiled SDK; they do not attempt to reproduce identical bytes from a different dependency graph.** The build bundle carries SDK artifacts compiled by the release pipeline. Plugin builds do not rebuild the SDK; they link against the released copy. This removes the feature-graph problem because SDK bytes do not come from the plugin build.
2. **Why abandon cross-graph reproducibility:** Cargo unifies features over the entire graph of one invocation. A plugin's private dependency changes the unified result—and thus SDK bytes—if it touches a crate in the SDK tree such as `tokio` or `serde_json` (discovered 2026-09-25 in SDK design §5.3). The workaround “enable every SDK feature to saturate the SDK” is unreliable: features can be mutually exclusive, feature-added dependencies can drift the closure, and every dependency upgrade would require auditing the whole tree again.
3. **Keep first-party builds as they are.** Anchor builds (`[build] anchor_package/anchor_features` in `sdk.toml`) continue serving in-repository plugins and the release pipeline. `--prebuilt-library` (package without building) is unchanged.
4. **Precompiled does not mean “ship only the dylib.”** `--extern rutis_sdk=<dylib>` lets rustc read SDK metadata from the dylib, but that metadata declares dependencies on `rutis`, `tokio`, `tokio-util`, `serde_json`, `std`, and others. Plugin code necessarily uses those types (`ConfigValue` is `serde_json::Value`), so rustc must find matching rlib metadata on its search path or return `E0463`. The build bundle must therefore include **the rlib dependency closure of the SDK**, injected with `-L dependency=` (confirmed by review experiments; see §10). Closure rlibs and the dylib come from one build, so their crate disambiguators match and symbol hashes align naturally.
5. **Overlap between a plugin-private graph and the SDK closure is not “safe by default.”** Experiments show that directly using an independently resolved crate with the same name (such as `serde_json`) fails **at compile time** with `colliding StableCrateId values`. For an indirect dependency whose features happen to exactly match the SDK copy, the disambiguators match and link behavior is undefined. W2 proves coexistence of two plugin versions, not coexistence between the SDK closure and a plugin-private graph. The rule is therefore narrowed: **shared crates must be used through re-exports from `rutis_sdk::`**. Reject direct dependencies in a pre-build check; expose indirect overlap during compilation or reject it in the checker, and have the packager relay a readable diagnostic (§4, §5).

## 2. Current State and Gaps

| Current state | Gap |
|---|---|
| Default `pack-plugin` mode requires the anchor package in the same Cargo graph, so it needs host source code | External developers do not have host source; this path is unavailable |
| `--prebuilt-library` only validates; the plugin binary must be built elsewhere (in the release pipeline) | External developers lack the build environment |
| `build-dylib-bundle.sh` copies only runtime files (the `cp` list starting at line 53) | It is a **runtime** bundle: no SDK dependency-closure rlibs, Windows import library, pinned toolchain, or injection config |
| `sdk.toml` contains `[sdk]` version/id/artifact_sha256/target/rustc/packages and `[build]` anchor | Missing std fields, normalized RUSTFLAGS, remapping rules, and deployment target. `[build]` is appended by shell, and `--sdk-info` cannot generate all fields (build.rs normalization is not written into `identity.rs`; remapping commands cannot be recovered later) |
| `check_shared_duplicates` uses `cargo tree -d` to detect only **different versions with the same name** | A plugin's direct dependency on a shared crate such as `tokio` (same version) is not blocked; indirect overlap with the SDK closure is not diagnosed |

## 3. SDK Build Bundle (`sdk-bundle`)

The release pipeline produces one bundle per SDK version and platform. It **does not include a host or launcher** and is distributed separately from the runtime release directory; the two are associated by SDK `version + L2` (the same byte-identical SDK artifact):

```text
sdk-bundle-<target>-<sdk-version>/
  bundle.toml        Bundle manifest (see below)
  sdk.toml           SDK identity and build diagnostics
  lib/               librutis_sdk.so / .dylib / rutis_sdk.dll
                     Windows: rutis_sdk.dll.lib beside and named after the .dll
  deps/              Build-time artifacts for the SDK dependency closure (E0
                     measurements in §6: both rlibs and proc-macro .so files;
                     rustc recursively loads full rmeta dependency chains of
                     closure crates, including proc-macros their dependencies
                     use. std metadata comes from the pinned toolchain sysroot
                     and is not included)
  Cargo.lock         SDK build lockfile (for diagnostics and package review)
  rust-toolchain.toml pins 1.98.1
  cargo-config.toml  .cargo/config.toml injection template
  GUIDE.md           Plugin author guide
```

**Collect and reduce the closure (settled by E0):** the collector takes package names from `sdk.toml`'s `packages` and copies every same-named variant from the anchor build's `target/release/deps` (`.rlib` and proc-macro `.so`; rustc selects among multiple variants by crate disambiguator, without conflicts). It then runs a **reduction pass**: remove variants one at a time and retry the build; delete one if the build still succeeds. The release artifact retains only selected variants (E0 reduced the set to 41 files, 60.1 MB, including 4 proc-macro `.so` files). Run this pass in the release pipeline; roughly a few hundred incremental builds, taking minutes.

**`bundle.toml` (bundle-level manifest):** `format_version` (the build-bundle format version), associated SDK `version/L1/L2`, and the sha256 of **every file** (including closure rlibs, import libraries, and templates). Integrity relies on this manifest, not `version + L2`: dependency closure, import libraries, and config templates can change while SDK bytes remain unchanged, and the SDK hash alone cannot detect missing or mismatched files. Upgrade or rollback by publishing a new bundle directory (new manifest hash) and stopping distribution of the old one.

**Collecting `sdk.toml` fields:** current `--sdk-info` exposes only compile-time constants in `identity.rs`. For each new field, specify who writes it and when; do not promise that existing `--sdk-info` alone can generate all fields.

| Field | Collector and timing |
|---|---|
| `[sdk]` version/id/artifact_sha256/target/rustc/packages | `--sdk-info` (`identity.rs`, already available) |
| `std_file` / `std_sha256` | Release script records these while copying std; host `--sdk-info` separately reads `std_reference` from SDK binary and cross-checks it |
| Normalized RUSTFLAGS | `build.rs` writes to `identity.rs` at compile time (extend existing whitelist normalization) |
| Path remapping rules, `MACOSX_DEPLOYMENT_TARGET`, `/Brepro` | Release step records the actual build arguments (cannot be recovered after the fact) |
| Xcode/linker version | Release step reads from artifact `LC_BUILD_VERSION` |
| `[build]` anchor_package/anchor_features | Release step (consumed only in anchor mode) |

The `cargo-config.toml` template contains `--extern rutis_sdk=<lib/SDK>`, `-L dependency=<deps/>`, `-L native=<lib/>`; macOS also sets `MACOSX_DEPLOYMENT_TARGET=13.0`. The header warns that environment `RUSTFLAGS` **replaces the entire config rustflags value** rather than appending to it. Any existing RUSTFLAGS silently disables injection and causes a misleading `E0463`; the packager detects this explicitly (§4 step 3), and GUIDE recommends clearing it during builds.

**Size:** closure rlibs cost tens to hundreds of megabytes; this is the price of correctness. Establish the minimum set with E0, then consider compression; do not trim it beforehand.

## 4. Plugin-Side Build (`pack-plugin --bundle`)

The plugin `Cargo.toml` **does not declare** a `rutis-sdk` dependency; injected `--extern` resolves `use rutis_sdk::...`. This mode is mutually exclusive with default anchor mode and `--prebuilt-library`. **The direct-dependency denylist applies only to this mode** (anchor mode must declare a rutis-sdk dependency, as fixtures and implementation docs show; applying the denylist there would reject all existing workflows).

1. Read `bundle.toml` and `sdk.toml`: verify hashes of **all** bundle files against the manifest; verify `target` matches the local triple; verify the `rustc` version recorded by `--sdk-info` exactly matches the full version from the actual `rustc -vV` (fail immediately on mismatch—another rustc first reports `E0514`, before any packaging checks).
2. **Pre-build check** (parse manifest and dependency graph; do not depend on rustc error codes): if the plugin's **direct dependencies** include `rutis-sdk`, `rutis`, `tokio`, `tokio-util`, or `serde_json`, fail and name the package and dependency path. Reject even if declared but unused, and reject transitive inclusion of rutis-sdk source. Relying on a rustc error after injection experimentally hit the SDK `build.rs` RUSTFLAGS whitelist panic first (unclassified `--extern`), producing unreadable output; reject before building.
3. Build the plugin: the packager **takes explicit control of the environment**. If the caller set `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS`, fail immediately and explain why. Then inject `--extern rutis_sdk=<bundle lib/SDK>`, `-L dependency=<bundle deps/>`, `-L native=<bundle lib/>` (on Windows `--extern` points to `rutis_sdk.dll`, and same-directory `rutis_sdk.dll.lib` supplies linking, confirmed by E0/E5); set `RUTIS_SDK_ARTIFACT_SHA256` to L2 from `sdk.toml`; explicitly set working directory and `--manifest-path` (do not assume the plugin workspace config is read).
4. Reuse existing checks: custom allocator, weak Rust exports, `native_deps`, and boot-section L1/L2 cross-check against `sdk.toml`. For std consistency, read `std_reference` from the **bundle SDK binary** (`rutis_dylib_meta::std_reference`, existing mechanism) and compare against the plugin artifact's dynamic std reference; cross-check `std_file/std_sha256` in `sdk.toml` too.
5. **Check link outputs per platform** (not only the boot section): on Linux, `DT_NEEDED` may contain only the SDK soname, dynamic libstd, and `native_deps`; on macOS, dependency is `@rpath/librutis_sdk.dylib`, with no `LC_RPATH` and no flat lookup; on Windows, the import and delay-import tables may contain only `rutis_sdk.dll`, std, and allowed `native_deps` (lowercase filenames).
6. **Diagnose indirect overlap:** if an indirect dependency duplicated in the SDK closure triggers a compile-time error (such as `colliding StableCrateId`), relay rustc output as a readable message: “crate X overlaps the SDK closure; use its re-export from `rutis_sdk::` or remove that dependency.”
7. Generate `plugin.toml` from the boot section (including `native_deps`) and emit the plugin directory.

Developer experience: after adding the `cargo-config.toml` template to the plugin workspace, `cargo check`/`cargo build` work (provided `RUSTFLAGS` does not interfere). rust-analyzer flycheck uses `cargo check` and works, but the injected `--extern` crate is not in RA's crate graph, so completion/navigation for `rutis_sdk::` may show unresolved; measure and record this (E8c), but do not promise it as acceptance criteria.

## 5. Why Identity Holds

- **L1:** `SDK_ID` is a compile-time const inlined from the released dylib metadata and therefore automatically equals the release value. Linking a different SDK file is caught by the hash check in steps 1/4.
- **L2 and symbols:** every plugin reference to the SDK (including generic instances and vtables) uses the disambiguator in released dylib metadata. Closure rlibs and the dylib come from one build; each crate's disambiguator in the closure matches the SDK. Under `-L dependency=`, rustc finds the same metadata used for the release, so symbol resolution is guaranteed (confirmed by E0).
- **The closure is part of identity:** closure rlib metadata determines plugin type resolution and symbol references. A tampered closure can produce an ABI-incompatible plugin even if L1/L2 both pass. Hash every closure file in `bundle.toml` and validate each file (step 1 of §4); E6 covers tampering.
- **libstd:** the plugin uses the pinned toolchain, and std metadata comes from the same toolchain as std used to build the SDK dylib. The plugin's `libstd-<hash>` reference matches the std file in the runtime release directory; the loader's existing check is a backstop. E0 confirms rustc accepts sysroot metadata to satisfy the SDK dylib's std dependency.
- **Coexistence rules (rewritten):** remove the blanket “different metadata hashes naturally stay isolated.” Observed facts:
  - Direct use of an independently resolved same-name crate by the plugin → **compile-time** `colliding StableCrateId`; cannot link.
  - Indirect inclusion with exactly the same features as the SDK copy → same disambiguator and symbol names for both copies; link binding order is undefined, so reject or warn (E2 records behavior).
  - Indirect inclusion with different features → different symbols; under `RTLD_LOCAL` they do not bind to each other (W2 proves only plugin coexistence; E2 provides evidence for this case).

  Converged rule: use shared types only through re-exports from `rutis_sdk::`; reject direct dependencies before build, and surface indirect overlap through compile-time conflict or packaging checks. GUIDE says that same-name crates statically linked indirectly do **not share runtime context with the host** (for example, `tokio::spawn` will not find the host runtime unless tokio is re-exported by the SDK).
- **Changed acceptance semantics for #108:** see §8.

## 6. Verification (E Series)

Decision: if E0 or E1 fails, record the result in this document. If E1 fails, fall back to external builds using only release-pipeline `--prebuilt-library`, and close #108 with the limitation documented.

| # | Scope | Acceptance |
|---|---|---|
| E0 | **Minimum closure and injection form (three platforms):** measure the rlibs rustc recursively requires using the released SDK (metadata dependency graph); whether `.rmeta` suffices (expected not in build mode); availability of sysroot std metadata; Windows linking with `--extern` pointing to `.dll` plus same-directory `.dll.lib`; closure size | Record each result here; use the closure set as collector specification |
| E1 | Linux: independent workspace (no host source, outside repository); build the bundle and a greeter-equivalent plugin with `--bundle`; load with release host; inspect link outputs from §4 step 5. **Scope note:** v1→v2 replacement and consumer reload are covered by `test-dylib.sh` on shared Loader path. This test loads v1/v2 separately with `rutis-cli --load-only` to prove external artifacts pass all host identity, dependency, and lifecycle checks | Load succeeds and all checks pass |
| E2 | Private-dependency matrix: crate outside SDK tree (`base64`) → normal; direct `serde_json` dependency → pre-build rejection; indirect same-version/different-feature dependency → record behavior; indirect dependency with identical features → expected reject/warn, record behavior | Record each case in §5 |
| E3 | Pre-checks: declare `rutis-sdk` (including unused declaration and `package =` rename), transitively include SDK source, directly depend on shared crate → fail before build with package name and dependency path | Rejection occurs before invoking rustc |
| E4 | macOS: equivalent to E1 (arm64, `@rpath`, quarantine, no run path, two-level namespace) | Same as E1 |
| E5 | Windows: equivalent to E1 (import library from bundle; inspect import table) | Same as E1 |
| E6 | Tampering: change one byte in SDK dylib, one byte in closure rlib, missing/mismatched bundle file/hash, or impersonate SDK built in anchor mode → reject all with readable errors | Reject before build/`dlopen` |
| E7 | Toolchain: build with anything other than 1.98.1 → step 1 rustc-version check fails and instructs use of bundled `rust-toolchain.toml`; no E0514 | Version check precedes all other checks |
| E8 | Split developer experience: E8a clean environment (no RUSTFLAGS), `cargo check`/`build` pass; E8b environment with `RUSTFLAGS`, packager fails immediately and explains replacement behavior; E8c measure and record rust-analyzer behavior (no completion guarantee) | E8a/E8b are acceptance; E8c is informational |
| E9 | Missing/damaged closure (delete an rlib) → readable error naming missing crate (not misleading E0463 for rutis_sdk) | Readable error |

CI: add `tools/test-sdk-bundle.sh` to each of the three dylib jobs (produce bundle → build plugin in independent temporary workspace → load with `rutis-cli --load-only` → assert rejection paths).

### E0 Record (2026-10-04, Linux x64, SDK 0.5.0 / L2 `a1064b9e…`)

- Injection works: `--extern rutis_sdk=<released dylib>` + `-L dependency=<deps/>`; plugin Cargo.toml does not declare rutis-sdk, independent-workspace compile/link passes. Artifact `DT_NEEDED` contains only `librutis_sdk.so`, `libstd-<hash>.so`, and system libraries; no run path; boot-section L1/L2 matches `sdk.toml`.
- **The closure must recursively include the complete rmeta dependency chain, including proc-macro artifacts:** with only rlibs, E0463 occurs when rustc loads `futures_macro`, `tokio_macros`, `thiserror_impl`, etc. A missing item produces only the misleading “can't find crate for rutis_sdk” with no note; the packager must check bundle completeness against `bundle.toml` itself (readable error required by E9).
- Multiple same-name variants coexist without conflict; rustc selects by disambiguator. Reduced minimum: **41 artifacts (37 rlibs + 4 proc-macro .so), 60.1 MB** (see collection/reduction in §3).
- Sysroot std metadata satisfies the SDK dylib's std dependency (pinned toolchain is enough; do not bundle std).
- rustc version is checked first: under 1.97.1, the first error is `E0514`, before any E0463.
- E1-equivalent path passes: a plugin built from the reduced set loads in the release host (`rutis-cli --plugin`); L1/L2/boot checks pass, and initialization/apply run (including `rutis_sdk::tokio::spawn` on the host runtime).

**Additional platform checks (2026-10-04, CI, E4/E5):** end-to-end flows passed on macOS and Windows. Platform-specific handling based on measurements: proc-macro artifacts are `lib<crate>-<hash>.dylib` on macOS and `<crate>-<hash>.dll` (no `lib` prefix) on Windows; dynamic std is `std-<hash>.dll` on Windows; bundle includes `rutis_sdk.dll.lib`, and `--extern` points to `.dll` while the colocated import library links it; Windows reduction leaves 46 artifacts. Also found and fixed: `rutis-cli`'s TUI main loop never exits on a Windows console without input, so `--load-only` was added (load and apply, then return before creating TUI) as a plugin validation mode.

### E2 Record (2026-10-04, Linux x64)

| Case | Result |
|---|---|
| Private dependency outside SDK tree (`base64`) | Compile, package, and load in release host all pass |
| Private `serde` (derive), copy coexists with closure | Compile/load pass; **traits cannot be mixed across copies**—private type's `serde::Serialize` is not the trait required by closure `serde_json`; compile-time E0277 rejects it (evidence for §5: cross-boundary data uses `json!`/`ConfigValue`; private types do not appear at SDK API boundary) |
| Private `serde` (no derive) | Works; SDK closure contains only `serde_core`/`serde_json`, with zero overlap with plugin-side `serde` |
| Private dependency indirectly brings `tokio` (`tokio-stream`) and **code references its API** | Compile-time failure. **Error form varies:** `colliding StableCrateId` (first conflicts with `pin_project_lite`: transitive dependency features all match, hence same disambiguator; packager adds an explanation) or unannotated `E0463` (cannot find the private crate itself); both mean the same thing |
| Same, but dependency is declared and its API is unused | The copy's metadata is not loaded, so no conflict; build passes. Conflict detection follows actual use |

Conclusion: coexistence rules and rejection semantics in §5 work as designed; indirect overlap is rejected by rustc's compile-time conflict. The packager adds a next-step explanation to `colliding StableCrateId`; `E0463` cannot be reliably distinguished from other missing-crate cases and gets no annotation (E2a/E2d assert both forms and require the overlapping crate to be named; B/C behavior is recorded here).

## 7. Implementation Steps

1. **E0 experiment:** settle closure and injection form; record conclusions (including size) in §3/§5.
2. Have `build.rs` write normalized RUSTFLAGS to `identity.rs`; make `--sdk-info` report std cross-check; collect the fields in §3 during release.
3. Produce bundle: closure collector (recurse over metadata dependency graph and exclude host proc-macro artifacts) + `bundle.toml` manifest, on all three platforms.
4. Implement `pack-plugin --bundle`: pre-check (§4 step 2), environment takeover, injection, rustc-version check, link-output inspection (§4 step 5), indirect-overlap diagnostics (§4 step 6).
5. Add E1–E3 and E9 after they pass; E4–E8 follow platform jobs.
6. Docs: add “external build” to `dylib-sdk-implementation.md`; update #108 acceptance item 2 per §8; GUIDE includes Windows static-initializer restrictions, TLS-context explanation, RUSTFLAGS warning, and deployment target.

## 8. Acceptance Mapping (#108)

| #108 acceptance | Coverage |
|---|---|
| Independent workspace without host source; use only build bundle to build a plugin with private dependencies; L2 matches released SDK; release host loads it and completes replacement | E1, E2 (non-overlapping private dependency), E4; Windows E5 exceeds requirement but is included |
| “Verify that a different dependency graph can build identical SDK bytes” | **Semantics updated; request wording change.** In precompiled mode, private dependencies cannot change SDK features. The relevant rejection is “overlaps SDK closure → build fails and identifies dependency”: direct dependency is rejected before build (E3); indirect overlap is rejected by relayed compile conflict or packager check (E2). Feature differences are no longer observable: either symbols differ (coexist) or symbols match (reject), and both outcomes must be readable |
| Linux and macOS both pass | E1, E4 |

Remove #108 item 2, “check whether different dependency graphs can build identical SDK bytes,” from the work items: first-party builds already use anchors, and this design replaces that premise for external builds.

## 9. Explicitly Out of Scope

- Proving that arbitrary Cargo graphs can reproduce SDK bytes (§1 item 2).
- Publishing `rutis-sdk` to crates.io: an installable source form would encourage plugins to declare it as a dependency and rebuild from source.
- Promising full rust-analyzer support: injected `--extern` crates are absent from its crate graph; record completion/navigation behavior from E8c.
- Changing the trust boundary: dylib plugins remain a trusted same-process solution; signing and Team ID follow macOS design §3.8. Use protocol plugins for untrusted code.
- Changing runtime release directory contents or launcher validation; the build bundle is an independent artifact linked by `bundle.toml` and `version + L2`.
- Shrinking the build bundle in advance: closure completeness comes first; let E0 reduction establish the minimum set, then evaluate compression.

## 10. Review Record (2026-10-04)

GLM 5.3 and GPT-6-sol performed independent parallel reviews, each with a minimal reproduction and without modifying the repository. Both requested changes. Key points adopted in v2:

| Finding (source) | Revision |
|---|---|
| With only dylib in bundle, injected `--extern` returns `E0463` (both reproduced: missing closure metadata such as `rutis`; `.rmeta` is insufficient in build mode; `-L native=` does not search Rust crates) | §3 `deps/` rlib closure; §4 step 3 `-L dependency=`; E0, E9 |
| Plugin directly using independently resolved `serde_json` gets `colliding StableCrateId` (GPT reproduction); identical features give same disambiguator and undefined link behavior (GLM analysis) | Rewrite §1 item 5 and §5 coexistence rules; E2 matrix |
| Environment `RUSTFLAGS` replaces `.cargo/config.toml` rustflags in full (both reproduced), silently disabling injection and causing misleading E0463 | §3 template warning; §4 step 3 environment takeover; E8b |
| “Declaring rutis-sdk always yields E0464” is inaccurate: experiment first hits the RUSTFLAGS whitelist panic in SDK `build.rs:163` (GPT) | Pre-check in §4 step 2 replaces dependence on error code; E3 |
| Adding the direct-dependency denylist to anchor mode contradicts itself: anchor mode must declare rutis-sdk (GLM) | §4: denylist applies only in `--bundle` mode |
| Wrong rustc version reports E0514 before std checks; pinning a toolchain does not guarantee it is actually used (GPT) | Pre-build version check in §4 step 1; E7 |
| Two-stage “swap only the first stage” is too simplistic; bundle mode must inspect linked artifacts (import table, install name, run path, DT_NEEDED) (GPT) | §4 step 5; E1/E4/E5 acceptance |
| `version + L2` does not identify the whole bundle; missing/mismatched files go undetected (both) | Per-file `bundle.toml` manifest in §3; E6 |
| `--sdk-info` cannot produce all `[build]` fields: normalized RUSTFLAGS not in `identity.rs`, remapping cannot be recovered afterward (GPT) | Field collection table in §3; §7 step 2 |
| §4 referred to an SDK-recorded std file, but no such `sdk.toml` field existed (GLM) | `std_file/std_sha256` in §3 + read from SDK binary in §4 step 4 |
| macOS deployment target missing from injection template (GLM); Windows `--extern` target form unclear (GLM) | §3 template, §4 step 3 |
| Windows GUIDE omitted “static initialization must not wait on threads” (GPT, W5); indirect tokio's TLS context not shared should be stated (GLM) | GUIDE entries in §5 and §7 step 6; E5 |
| “Completion works” for rust-analyzer was an overclaim (GLM) | Developer experience in §4, E8c, §9 |
