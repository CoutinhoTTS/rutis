# External Plugin Development Guide: Build rutis dylib Plugins with an SDK Bundle

This guide is for plugin authors developing **outside the rutis repository**: you have your own repository and release schedule, and someone else distributes the host (the rutis application). For architecture and boundaries, see [dylib SDK design](design-dylib-sdk-2026-09-24.en.md) and [SDK bundle design](design-sdk-build-package-2026-10-04.en.md); this document covers operations only.

The host publisher provides two artifacts, and you need both:

| Artifact | Contents | What you use it for |
|---|---|---|
| **Runtime release directory** (distributed to host users) | `rutis-cli`, host, SDK, dynamic libstd, launcher | Run your plugin locally |
| **sdk-bundle** (distributed to plugin authors) | Prebuilt SDK, dependency-closure rlibs, `sdk.toml`, `bundle.toml`, toolchain, `GUIDE.md` | Compile and package your plugin |

Both are tied to the same SDK artifact (`artifact_sha256` in `sdk.toml`). Your plugin links only that SDK, and the host loads that same build at runtime. Different bytes are rejected.

## Set up a workspace once

```text
my-plugin/
  .cargo/config.toml     # copy from sdk-bundle and change two paths
  rust-toolchain.toml    # copy from sdk-bundle (required: plugin and SDK use the same rustc)
  Cargo.toml
  src/lib.rs
```

In `.cargo/config.toml`, `--extern` and `-L dependency` point to the extracted sdk-bundle. Two important notes are at the top of that file; repeated here:

- **The `RUSTFLAGS` environment variable replaces this configuration entirely**; it is not appended. If it is set in your shell or CI, `cargo check` may misleadingly report `E0463: can't find crate for rutis_sdk`. Clear it before building.
- rust-analyzer completion and navigation may not resolve `rutis_sdk::` paths because it is not in Cargo's crate graph. `cargo check` and flycheck work normally. This is a known limitation, not a configuration error.

## Write a plugin

`Cargo.toml`—note that there is **no** `rutis-sdk` dependency:

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["dylib"]
test = false
doctest = false

[features]
export = []

[dependencies]
# Add private dependencies as needed, but do not list rutis / tokio /
# tokio-util / serde_json / rutis-sdk. The packager rejects these names.
# Use shared crates through rutis_sdk re-exports:
# rutis_sdk::{rutis, tokio, serde_json}.
```

`src/lib.rs`:

```rust
use rutis_sdk::rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory};
use rutis_sdk::ConfigValue;

struct Factory;
struct MyPlugin;

impl PluginFactory<ConfigValue> for Factory {
    fn name(&self) -> &str {
        "my-plugin"
    }
    fn build(&self, config: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        let _ = config;
        Ok(Box::new(MyPlugin))
    }
}

impl Plugin for MyPlugin {
    fn name(&self) -> &str {
        "my-plugin"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            // Use the Tokio re-export: the task runs on the host's runtime.
            rutis_sdk::tokio::spawn(async {})
                .await
                .map_err(|e| CordisError::PluginFailed(e.into()))?;
            ctx.provide("hello from my-plugin".to_string())?;
            Ok(Effect::Done)
        })
    }
}

rutis_sdk::export_plugin! { id: "my-plugin", factory: Factory }
```

Use private dependency types internally, but **do not expose them across plugin/host boundaries**. Types exchanged in services, events, and configuration must come from the `rutis_sdk` re-exports. To pass structured data to the host, construct `ConfigValue` with `rutis_sdk::serde_json::json!({...})`. Do not derive `Serialize` for your own types and pass them to the SDK's serde_json: your private copy's trait and the one in the SDK dependency closure are different, producing compile error E0277.

## Package

```sh
# Run from a rutis repository clone (the packager is currently a repository xtask command):
cargo xtask pack-plugin --bundle <sdk-bundle directory> \
  --manifest-path <your-plugin>/Cargo.toml \
  --features export \
  --output <distribution directory>
```

The packager verifies every sdk-bundle file hash and the full rustc version; rejects direct dependencies on shared crates; injects the SDK build; checks the allocator, weak exports, native-library dependencies, and bootstrap-section identity; then produces a plugin directory (binary + `plugin.toml`). Give that directory to the host publisher or end user.

Local verification with the runtime release directory supplied by the host publisher:

```sh
<runtime directory>/rutis-cli --scripted --load-only --plugin <distribution directory> --plugin-config '{}'
```

`--load-only` loads the plugin, runs `apply`, and exits without starting the terminal UI (safe in a headless environment). Exit code 0 means loading succeeded.

## Troubleshooting

| Symptom | Cause and action |
|---|---|
| `E0463: can't find crate for rutis_sdk` (configuration unchanged) | `RUSTFLAGS` is set and replaced `.cargo/config.toml`; clear it. If the sdk-bundle path changed, verify both paths in the config. |
| `E0514: found crate rutis_sdk compiled by an incompatible version of rustc` | The toolchain differs from the one fixed by the bundle. Copy its `rust-toolchain.toml` into the workspace; rustup will switch automatically. |
| Packaging fails with `colliding StableCrateId`, “overlaps the SDK's dependency closure,” or E0463/E0277 pointing at a `tokio`/`serde` crate | A private dependency pulled a shared crate into your graph and your code uses it. The packager explains the `colliding` form; the E0463 form (cannot find the private crate itself) is not annotated but has the same cause. Remove that dependency or use the `rutis_sdk::` re-export. |
| Packaging fails with “declares … as a direct dependency” | `Cargo.toml` directly lists a shared crate. Use the `rutis_sdk::` re-export instead. |
| Packaging fails with “bundle is missing …” / “differs from the bundle manifest” | The bundle is incomplete or modified; download it again. |
| E0277: a trait on a private type is not satisfied (for example, `Serialize`) | Traits from separate copies do not interoperate. Pass data via `json!` / `ConfigValue` as described above. |

## SDK upgrades

After the host publisher upgrades the SDK, old plugins are rejected by the new host because L1/L2 do not match, so users cannot load them. Get the new sdk-bundle (its `sdk.toml` has a different `artifact_sha256`) and package your plugin again. Your source normally needs no changes unless the SDK API changed. Replace the sdk-bundle in your workspace and update the paths in `.cargo/config.toml`.

## Known limitations

- Packaging currently uses `cargo xtask` from the rutis repository. You need a rutis clone or a packaging service from the host publisher; a standalone packager is future work.
- rust-analyzer completion for `rutis_sdk::` paths is unreliable (see above).
- Plugins run in the same process as the host; a crash takes down the host, so plugin code must be trusted. Untrusted code should use protocol plugins (`rutis-interop`).
