# Upgrading to rutis 0.7

[中文](migration-0.6-to-0.7.md) · [Release notes](releases/0.7.0.en.md)

This guide covers core 0.6.x, the old interop/loader packages, and the unpublished integrated development version numbered 0.3.0. The core and train packages are versioned 0.7.0 for this release, retaining their existing publication workflows.

## Rust core

Set `rutis = "0.7"` and update the lockfile. Typed plugins are optional; existing `Plugin` implementations remain usable. The 0.6.1 control-plane interfaces remain available. When upgrading directly from 0.6.0, also read the [control-plane guide](migration-0.6.0-to-0.6.1.en.md).

Update and rebuild third-party Rust plugins depending on core 0.6 too. Service types, `Ctx` and plugin traits in one host must use the same core version; Cargo resolving both 0.6 and 0.7 does not make their types interchangeable.

## Rust hosts and old interop packages

```toml
[dependencies]
rutis = "0.7"
rutis-loader = { version = "0.7", features = ["node", "python", "peer"] }
rutis-bridge = { version = "0.7", features = ["python", "websocket"] }
```

Enable only the features you use. Bridge enables `node` by default; `python`, `websocket`, `cordis` and `testing` are opt-in. Replace the loader's old `interop` feature with `node`, adding `python` / `peer` for Python or hosted-node rows.

| Old entry point | 0.7 entry point |
| --- | --- |
| Rust `rutis-interop` | `rutis-bridge` |
| `rutis_interop::rpc` | `rutis_bridge::session` |
| Cordis `Mount` / `Process` | `rutis_bridge::cordis::{Mount, Process}` (`cordis` feature) |
| `rutis_interop::build::from_manifest` | `rutis_bridge::cordis::build::from_manifest` |
| Intermediate standalone channel / transport crates | `rutis_bridge::channel` / `rutis_bridge::transport` |
| npm `@arcships/rutis-interop` | `@arcships/rutis-runtime` for execution; `@arcships/rutis` for leaf plugin authors |
| Python `rutis_runtime` imports | `rutis` |

Change build dependencies for Cordis `build.rs` to `rutis-bridge` with `cordis` enabled and regenerate bindings. A global crate-name substitution is insufficient: sessions, runtimes and Cordis bindings now live in separate modules. See [Cordis](guide/cordis.en.md) and [Rust hosts](guide/rust-host.en.md) for complete configurations. Configure runtime plugins and resolvers using the new guides; remote runtimes and node features share bridge connections.

## JS/TS and Python

After publication, upgrade the SDK, runtime and host together:

```bash
npm install @arcships/rutis@^0.7.0
npm install --save-dev @arcships/rutis-runtime@0.7.0 @arcships/rutis-host@0.7.0
uv add 'rutis>=0.7,<0.8'
uv add --dev 'rutis-host>=0.7,<0.8'
```

Leaf plugins import `definePlugin` from `@arcships/rutis` and testing tools from `@arcships/rutis/testing`. Existing Cordis plugins keep their Cordis interface and use the new runtime package; conversion to leaf plugins is optional. PyPI `rutis` contains the Python SDK and runtime; install `rutis[network]` for network transports.

`rutis-host new <name> --lang node|python` generates the matching 0.7 dependency ranges. Existing projects retain their own plugin versions while updating framework dependencies and lockfiles. Run plugin tests, `rutis-host check`, then verify reload with `rutis-host dev`.

## Protocols and deployment

Package version 0.7.0, the plugin API marker and session protocol versions are independent. Local sessions use protocol 2; endpoint sessions use protocol 3. Do not set `rutisProtocol` to 7 to match the package version. Deploy hosts and runtimes from the same train together.

Local runtimes and the host support Linux/macOS; use WSL on Windows. Node 24+ and Python 3.12+ are required. Check endpoint identity, certificate hostname, CA and authentication token for remote deployments. Verify reconnection and service withdrawal in staging before replacing a deployment.

## dylib

Changing the core and lockfile changes SDK identity. Regenerate the host/SDK bundles and rebuild all dynamic plugins against that SDK. Do not mix old SDK binaries or assume Rust semver implies dylib ABI compatibility. SDK and dylib tools retain independent package versions. See the [SDK build package design](design-sdk-build-package-2026-10-04.en.md).

## Release tags

Publish the core with `rutis-v0.7.0` first. Once it succeeds, push `v0.7.0` at the same commit to publish bridge, loader, host and npm/PyPI packages. `cli-v*` remains the independent CLI example binary release.
