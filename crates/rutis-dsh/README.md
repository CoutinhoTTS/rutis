# rutis-dsh: Running dsh inside a rutis host

`rutis-dsh up` launches the full dsh web interface. dsh runs in a Node process started and managed by rutis, and model calls are served by [aimux-llm](../aimux-llm) in the same process. For the integration mechanism see the [Cordis guide](../../docs/guide/cordis.md); requires Unix and Node 24 or later.

## Usage

```sh
npm --prefix crates/rutis-dsh/dsh ci
DEEPSEEK_API_KEY=... cargo run -p rutis-dsh -- up
```

dsh prints an address with a login token to stderr (`dsh web: http://127.0.0.1:3080/?token=…`) and opens a browser by default.

```text
rutis-dsh up [--profile <name>] [dsh options...]
```

| Option | Description |
| --- | --- |
| `--profile <name>` | Profile under `$DSH_HOME/profiles`, default `rutis-web`; created on first start from the dsh-base, dsh-web-app, and aimux bundles |
| Other arguments | Passed to `dsh web`, e.g. `--port 3081`, `--host 0.0.0.0`, `--no-open` |

The working directory is dsh's workspace (`.env` inside it is read). On Ctrl-C or when dsh exits on its own (e.g. `--help`), rutis unloads the mount and terminates the Node process; if the Node process dies unexpectedly, `rutis-dsh` exits with it.

## Model Routing

The **aimux (rutis)** group in the model selector is provided by aimux. Routing is configured on the dsh models page (settings section `llm-aimux`) and takes effect immediately:

| Field | Description |
| --- | --- |
| Route name | Provider name within dsh; must not collide with routes of other adapters (e.g. the official `deepseek-official`) |
| `provider` | The aimux provider; defaults to the route name |
| `apiKeyEnv` | Credential reference for the key, resolved per request through the dsh credential service (keys saved on the models page live here); without a credential service, the environment variable of the same name is read |
| `displayName` | Display name |

Routes without a key (including the default `aimux`) use the host's fallback provider: `AIMUX_PROVIDER` / `AIMUX_MODEL` (default `deepseek` / `deepseek-chat`) and the key environment variable for that provider (e.g. `DEEPSEEK_API_KEY`). The model catalog comes from the fallback provider; the model picked in the UI takes effect as selected.

## Components

| Part | Location | Role |
| --- | --- | --- |
| Launcher | `dsh/launcher.ts`, `dsh/launcher/boot.ts` | Starts the dsh profile inside the mounted Cordis Context (reuses dsh-app-boot's startup steps; does not create a Context or take over signals/exit); reports success, failure, and exit to rutis as events |
| aimux bundle | `dsh/aimux` (npm package `@rutis/dsh-aimux`) | The `llm-aimux` row in the profile: registers routes with dsh-llm; model calls go through the host service `aimux` into Rust |
| Host service | `src/aimux.rs` | Rust implementation of `aimux`: each call is one aimux-llm stream, read by the adapter in batches; stopping reading cancels it |
| Mounts | `Cargo.toml` | `web`: the web interface; `agent`: a UI-less dsh agent composition (for tests and driving the agent from Rust) |

## Profile Configuration (rutis-loader)

`rutis_dsh::profile` connects dsh's profile configuration to [rutis-loader](../rutis-loader); no Node needed:

| Part | Role |
| --- | --- |
| `profile::load` | Reads the layers following dsh-app-boot's rules: bundle patch files (bundles that can't be resolved, lack `dsh.bundle`, or have incompatible versions are skipped with a reason), the user layer `cordis.patch.yml` (editable), `$DSH_HOME/cordis.patch.yml`, `--patch`, and the telemetry switch; nested includes are expanded into the final layer |
| `profile::UserLayerStore` | `Persist` for the user layer: holds the same cross-process write lock as dsh (`<profile>/package.json.lock`), compares versions, rewrites only the patches that changed (comments on other patches are preserved), and atomically replaces after read-back verification |
| `profile::expr::JsSubset` | The JavaScript subset dsh writes in `!!js`; `ctx` can only read services registered in the service-name directory |
| `profile::watch::watch` | Polls the profile's files; re-reads the layers and reconciles on change |
| `rutis-dsh dump-config` | Prints the composed profile, for comparison against `dsh --dump-config` |

Consistency with dsh is guaranteed by cross-checking tests: YAML dialect against js-yaml, expressions against Node, layering against dsh-app-boot (see `tests/profile_*.rs`, which require the npm project to be installed).

```sh
cargo run -p rutis-dsh -- dump-config --profile web
```

## Deployment

The binary is distributed together with the npm project: copy `crates/rutis-dsh/dsh` (with `node_modules` installed, symlinks dereferenced) to the target machine and point `RUTIS_CORDIS_ROOT` at it (see "Deployment" in the [Cordis guide](../../docs/guide/cordis.md)). Inside the repository the npm project references `node/rutis-runtime` via a `file:` dependency; for standalone deployment you can switch to `@arcships/rutis-runtime` on npm.

## Migrating from the Old Bridge

Previously, `rutis-dsh up` launched the official `dsh` CLI, with a `rutis-bridge` plugin inserted into its profile calling Rust over TCP (`rutis-cordis` + `host/`). Now rutis is the host, so installing `dsh` and the `rutis-bridge` npm package separately, and setting `RUTIS_DSH_BIN`, are no longer needed. The routing configuration fields are unchanged (the `llm-aimux` settings section) but are stored in a new profile (`rutis-web`) and must be re-added on the models page; you can also point `--profile` at an existing profile. dsh events formerly forwarded to rutis by the old bridge (`HostEvent`) are now generated by interop as typed events from declarations.

## Testing

```sh
cargo test -p rutis-dsh                          # web UI start / failure / stop, agent composition, migration deployment
DEEPSEEK_API_KEY=... cargo test -p rutis-dsh --test agent -- --ignored   # agent turns against a real model
```
