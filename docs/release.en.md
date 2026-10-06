# Releases

## Release train

Except for the `rutis` core (`rutis-v*`, `publish-rutis.yml`), `rutis-cli` (`cli-v*`, `release-cli.yml`), and dylib-related crates, these packages share a version and are released together:

| Registry | Packages |
| --- | --- |
| crates.io | `rutis-bridge`, `rutis-loader`, `rutis-host` |
| npm | `@arcships/rutis`, `@arcships/rutis-runtime`, `@arcships/rutis-host` and `@arcships/rutis-host-{linux,darwin}-{x64,arm64}` |
| PyPI | `rutis`, `rutis-host` (wheels for each platform) |
| GitHub Release | `rutis-host` binaries |

## Steps

1. Update versions in `crates/rutis-bridge`, `crates/rutis-loader`, and `crates/rutis-host` Cargo.toml files (including dependency versions); `node/rutis`, `node/rutis-runtime`, and `node/rutis-host` package.json files (including the runtime and platform package versions used by `@arcships/rutis-host`); `python/rutis/pyproject.toml`; and the `rutis` range in `crates/rutis-host/pyproject.toml`. `node scripts/train.mjs` checks that they match, and CI runs it too.
2. Merge to `main` and confirm the CI `release-dry-run` passes (all packages can be built).
3. Run the smoke test on two machines before publishing (see below).
4. Create and push a tag such as `vX.Y.Z`. `release.yml` checks versions, builds binaries and wheels for four platforms, publishes crates/npm/PyPI packages, and creates a GitHub Release. Each step skips versions already present in its registry, so after fixing a mid-release failure you can rerun it.

Required configuration: GitHub environment `release` with `CARGO_TOKEN` and `NPM_TOKEN`, and environment `pypi` with trusted publishers configured on PyPI for `rutis` and `rutis-host`, pointing to `release.yml`.

Increment `PLUGIN_API` (present in both the SDK and runtime) only when the interface visible to plugins becomes incompatible. Increment the session protocol version (`rutisProtocol` and `rutis_bridge::session::PROTOCOL`) when the wire format becomes incompatible.

## Smoke test

```text
# Listener (the certificate must match its hostname)
cargo run -p rutis-bridge --features websocket --example smoke -- \
    listen 0.0.0.0:7443 --cert server.pem --key server.key --token secret

# Dialer
cargo run -p rutis-bridge --features websocket --example smoke -- \
    dial wss://<listener-hostname>:7443/rutis --ca ca.pem --token secret
```

Expected behavior: the dialer prints `clock: <n>` every second. After a network outage, both sides report a heartbeat timeout and wait to reconnect within 30 seconds; after recovery, they report `ready, session <n+1>`. Restarting the listener makes the dialer retry with backoff. A wrong token returns `AuthRejected … 403`; an untrusted CA returns `AuthRejected … UnknownIssuer`.

Then use the published packages to follow [Write a TypeScript plugin](guide/typescript-plugin.en.md) and [Write a Python plugin](guide/python-plugin.en.md) from a clean environment.

The nightly stress workflow also runs two soak tests (repeated link disconnect/reconnect and repeated process start/exit) and checks that file descriptor and thread counts do not grow and all processes are reaped.
