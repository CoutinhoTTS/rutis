# Packages, Tools, and Workflows for Developers

Date: 2026-10-06. Status: implemented (Section 14 records deviations from the design).

## 1. Why Do This Now?

Multilingual plugins (M1/M2) and the network stack (#145) are complete, but neither has been released. Packages have grown around implementation details rather than developers' tasks:

- The name `rutis-interop` does not explain what it is. It contains the cross-process session protocol, language-runtime management, static Cordis mounting, and code generation. The network stack is split across `rutis-channel`, `rutis-bridge`, three `rutis-transport-*` crates, and `rutis-runtime-local`. These splits prevent internal dependency cycles; developers do not need to know those layers, but currently have to choose among seven or eight crates.
- A Node leaf plugin has to import the entire Node runtime (`@arcships/rutis-interop`, which brings Cordis, tsx, typescript, and ws). Python's plugin SDK and runtime are mixed in the `rutis_runtime` package inside one repository, and have not been published.
- There is no general-purpose host program; running a plugin requires writing Rust.
- Plugin authors have no testing tools or publishing and compatibility conventions.

There are few downstream users, so the cost of a breaking reorganization is lowest now. This design settles all developer-facing packages, names, tools, and workflows. Tutorials come last, after these are ready.

## 2. Who Uses rutis?

| Role | Task | Tools |
|---|---|---|
| Plugin author | Write, test, and publish plugins | TypeScript/JavaScript, Python, Rust |
| Host author | Build an application that loads plugins | Embed with Rust, or write no code (use the general host) |
| Operator | Install plugins, write config, connect machines | Configuration files |
| Cordis application author | Connect an existing Cordis application to rutis | TypeScript/JavaScript |

## 3. Principles

1. **Organize packages by who installs them and why, not by internal layers.** Internal layers are modules; optional components are selected with features or optional dependencies. Split packages only when their dependencies differ substantially and their installers are different.
2. **Name packages by purpose.** Developers should know from a package name whether they need it.
3. **Plugin authors depend on one lightweight package.** The host installs what is needed to run plugins, so plugins do not pin its version.
4. Keep npm packages under the `@arcships` scope; use the `rutis` name on crates.io and PyPI. All names below were confirmed unclaimed (2026-10-06).

## 4. Packages

### 4.1 Overview

| Installer | Rust (crates.io) | Node (npm) | Python (PyPI) |
|---|---|---|---|
| Plugin author | `rutis-sdk` (existing, dylib plugins) | `@arcships/rutis` | `rutis` |
| Host (runs plugins) | `rutis`, `rutis-loader`, `rutis-bridge` | `@arcships/rutis-runtime` | Same `rutis` package |
| Host without Rust | `rutis-host` (binary) | `@arcships/rutis-host` | `rutis-host` |
| Cordis application author | — | `@arcships/rutis-runtime` (`/bridge`) | — |

Four Rust packages, three Node packages, and two Python packages.

### 4.2 Rust

| Crate | Purpose | Change |
|---|---|---|
| `rutis` | Kernel | Unchanged |
| `rutis-loader` | Load plugins from a config file (rows) | Depend on `rutis-bridge` (optional) |
| `rutis-bridge` | Connect rutis to other processes, languages, and machines | **Merge** `rutis-channel`, `rutis-interop`, `rutis-bridge`, `rutis-transport-local` / `-memory` / `-websocket`, and `rutis-runtime-local` |
| `rutis-host` | General-purpose host program (Section 6) | New |

`rutis-sdk` and dylib-related crates are outside this scope and remain unchanged.

**`rutis-bridge` features:**

| Feature | Contents | Default |
|---|---|---|
| (always available) | Channel contract, session protocol, link, identity, node functions (export / import / host / events), memory transport, local transport (Unix), remote language runtimes | — |
| `node` | Local Node runtime | On |
| `python` | Local Python runtime | Off |
| `websocket` | WebSocket transport (rustls, tungstenite) | Off |
| `cordis` | Static Cordis plugin mounting and build-time Rust binding generation (syn, quote, toml) | Off |
| `testing` | Channel contract tests, session/runtime/node consistency tests | Off |

Internally these are modules: `channel`, `session`, `link`, `transport::{local, memory, websocket}`, `runtime`, and `cordis`. Layers previously split to avoid cycles become dependencies among modules.

- Third parties implementing a new transport need only depend on `rutis-bridge` (`default-features = false`) and use `testing` to run the channel contract tests.
- Remove deprecated interfaces (`CordisRuntime*`, `RuntimePlugin::node` / `python` / `launcher`, etc.) directly; do not keep aliases.
- `rutis-loader` features: `node` and `python` for local runtimes, `peer` for rows running on other nodes. Each enables the corresponding `rutis-bridge` components.

### 4.3 Node

| Package | Installer | Contents | Dependencies |
|---|---|---|---|
| `@arcships/rutis` | Plugin author | `definePlugin`, TypeScript types, testing tools (`@arcships/rutis/testing`) | **None** |
| `@arcships/rutis-runtime` | Host (installed in the host's Node project); Cordis application author | Runtime process (runner, `listen:` daemon, transports, sessions, Cordis host); `/bridge` connects a Cordis application as a rutis node (`Link`, `Export`, `Import`, `Host`, `Events`); binding generation used at build time by `rutis-bridge`'s `cordis` feature | `@deepseek-ai/cordis`, `tsx`, `ws` |
| `@arcships/rutis-host` | Users without Rust (`npx`) | Distribution of host binaries. Platform binaries live in optional dependencies `@arcships/rutis-host-<platform>`, a common npm binary-distribution pattern (as with esbuild); users do not interact with these directly | `@arcships/rutis-runtime` |

The SDK must be its own package: plugin authors should not install Cordis, tsx, or ws just to write a plugin. `definePlugin` uses `Symbol.for` as its marker (as it does now), so the SDK and runtime do not need to share a module instance.

Remove the old `@arcships/rutis-interop` package directory, aliases, and publishing entry points directly; do not provide a migration layer or an old-package deprecation release.

### 4.4 Python

| Package (import name) | Contents | Dependencies |
|---|---|---|
| `rutis` (`rutis`) | `define_plugin`, types (`py.typed`), testing tools (`rutis.testing`); runtime process via `python -m rutis`; optional `network` extra (WebSocket remote runtime) | None; `network` requires `websockets>=13` |
| `rutis-host` | Wheel containing the host binary (`uvx rutis-host`, `uv add --dev rutis-host`) | `rutis` |

The Python runtime has no dependencies, so putting the SDK and runtime in one package does not make plugin authors install anything extra; there is no need to split them. A plugin depends on `rutis`, and the host's Python environment already has the plugin installed, so it has the runtime too.

### 4.5 Why Split Node but Not Python?

Only one question matters: when plugin authors install the SDK, are they forced to install things they do not need? Node's runtime depends on Cordis, tsx, and ws, so it is split. The Python runtime has no dependencies, so it stays in one package.

## 5. Plugin Conventions

### 5.1 Plugin API Version

The SDK marks each plugin with an integer `api` (starting at 1), indicating which plugin API it targets; the runtime declares the range it supports. On incompatibility, loading reports a clear error, such as “plugin weather requires plugin API 2, but this runtime supports only 1; upgrade @arcships/rutis-runtime.”

- Plugin API version is independent of package version. Increment it only for incompatible changes to plugin-visible interfaces (`ctx` methods, declaration format, value-passing rules).
- Cordis plugins that do not use the SDK are treated as API 1.
- The runtime supports one value: `PLUGIN_API` (currently 1). Reject plugins with `api > PLUGIN_API`; do not retain support for historical versions.
- `api` is carried in the `api` field of the object returned by Node's `definePlugin`, and in Python's `Plugin.api`.
- `rutisProtocol` is the cross-process wire-format version checked during handshake; `api` is the plugin-visible interface version checked at load time. They advance independently.

### 5.2 Node Plugins

- **Leaf plugin** (recommended): `export default definePlugin({ inject, provides, config, apply })`, structurally parallel to a Python plugin.
- **Cordis plugin:** write it as usual. To expose services to rutis, declare them in `package.json` as `"rutis": { "provides": { "weather": { "today": "sync" } } }`.
- **package.json:** depend on `@arcships/rutis`; set `"keywords": ["rutis-plugin"]` and `"type": "module"`.
- **Code form:** publish compiled JavaScript plus `.d.ts`; during development the runtime can load TypeScript source directly (tsx).
- **Row name:** package name, resolved from the host's Node project directory.

### 5.3 Python Plugins

- A module provides `apply(ctx, config)`, or defines `plugin = rutis.define_plugin(...)`.
- **pyproject.toml:** depend on `rutis`; register the plugin with an entry point:

  ```toml
  [project.entry-points."rutis.plugins"]
  weather = "weather_plugin"
  ```

- **Row name:** `py:<name>`. Look up by entry point first (which also provides the package version for detecting stale resolution), then by module name if not found; uninstalled modules in development can still be loaded.
- The runtime uses the interpreter configured by the host (Section 6.2); install the plugin and its dependencies in that environment.

### 5.4 Value and Behavior Rules

The two languages share the existing rules: data is passed by value, functions and objects by reference, calls may be re-entered synchronously, and config changes restart the plugin. Explain these consistently in the tutorials.

## 6. General-Purpose Host: rutis-host

A host that requires no Rust code, assembling the kernel, loader, and `rutis-bridge` and driven by one config file. It is a local development environment for plugin authors and can also be used for deployment; applications needing custom Rust services continue to embed rutis in Rust.

### 6.1 Commands

| Command | Purpose |
|---|---|
| `rutis-host run [rutis.json]` | Run from config |
| `rutis-host dev` | Development mode: run directly inside a plugin project; automatically add the current project as a row, watch files and reload, and enable the development channel (`rutis-dev`) |
| `rutis-host check [rutis.json]` | Validate config: resolve each row, print plugin config Schema, dependencies, and provided services, and report incompatibilities (plugin API, protocol version, missing runtime packages) |
| `rutis-host new <name> --lang node\|python` | Create a plugin project from a template |

### 6.2 `rutis.json` Configuration

```json
{
  "id": "main",
  "runtimes": {
    "node": { "project": "." },
    "py": { "project": ".", "python": ".venv/bin/python" }
  },
  "rows": [
    { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "py:fake_llm" },
    { "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis", "export": ["weather"] } }
  ]
}
```

- **`runtimes`:** local runtimes to start.
  - `node.project` is the Node project directory; plugin packages resolve from here, and `@arcships/rutis-runtime` is installed here too.
  - `py.python` is the interpreter. By default, try `$VIRTUAL_ENV/bin/python`, `./.venv/bin/python`, then `python3`; `rutis` must be installed in this environment.
  - If a runtime package is missing, startup fails with an installation command.
- **`rows`:** retain rutis-loader row format (`isolate`, `inject`, `peer:` rows, `rutis-bridge/peer` node rows, etc.).
- **Service names shared across languages:** any name declared by a row in `provides` is automatically registered as shared (`ServiceCatalog::share_by_name()`); there is no additional `shared` field. Plugin authors do not need to understand `register_shared`.
- **Credentials** are read from environment variables, not config: rename `RUTIS_INTEROP_TOKEN`, etc. to `RUTIS_TOKEN` / `RUTIS_CA` / `RUTIS_CERT` / `RUTIS_KEY`.
- The general host does not provide Rust services itself; plugins can share services with each other or access services on another node through a link.

### 6.3 Development Mode

- Run `rutis-host dev` from the plugin project directory: read `package.json` or `pyproject.toml` and add that plugin as a row. An optional project `rutis.dev.json` can add config, fake service rows (for example a local `fake_llm.py`), and other plugins.
- **Reload on file changes:**
  - Python: re-import that row's module (existing behavior).
  - Node leaf plugin: **new** per-row re-import; append a versioned URL to the entry module to bypass module cache, matching Python. Other modules it imports are not re-imported, also matching Python.
  - Node Cordis plugin: continue restarting the runtime.
- Prefix each runtime's stdout/stderr; show the row's status and reason when a plugin fails.
- **Later:** `rutis-host dev --join wss://…` lets a development machine connect as a node to a real application. The application loads the in-development plugin through a `peer:<developer-machine>/<plugin>` row and uses the application's real services. This composes existing link and host features and is a second-phase item.

### 6.4 Distribution

- GitHub Release binaries for Linux (x86_64 / aarch64) and macOS (x86_64 / aarch64).
- npm: `npx @arcships/rutis-host`; PyPI: `uvx rutis-host`; crates.io: `cargo install rutis-host`.
- Windows: local runtimes depend on Unix, so plugin development uses WSL; this version does not distribute a Windows binary.

## 7. Testing Tools

Include testing tools in the SDK; they need neither a host nor a runtime process:

```ts
import { load } from '@arcships/rutis/testing'
import plugin from '../src/index.ts'

const t = await load(plugin, {
  config: { city: 'Oslo' },
  services: { llm: { ask: async q => 'sunny' } },
})
assert.equal(await t.service('weather').today(), 'sunny in Oslo')
await t.unload()            // run cleanup and check that all provided services were revoked
```

```python
from rutis.testing import load

async def test_weather():
    async with load(weather_plugin, config={"city": "Oslo"}, services={"llm": FakeLlm()}) as t:
        assert await t.service("weather").today() == "sunny in Oslo"
```

The tools verify:

- Every declared `inject` service is provided, and the plugin cannot access undeclared services.
- Provided services match the shape declared by `provides`.
- Cleanup functions run on unload.
- In strict mode, arguments and return values crossing the boundary make a round trip under the real rules (data is copied, functions become references), exposing code that “works in-process but breaks across processes.”

Use `rutis-host` for integration tests: start the host in the test, or run `rutis-host check` in CI.

## 8. Complete Developer Workflows

### 8.1 Node / TypeScript Plugin Author

```bash
npx @arcships/rutis-host new weather --lang node   # Template: package.json, src/index.ts, test/, rutis.dev.json, CI workflow
cd weather && npm install                          # @arcships/rutis dependency; @arcships/rutis-host dev dependency
npm test                                           # node --test, using @arcships/rutis/testing
npx rutis-host dev                                 # Run locally; reload automatically after code changes
npm publish                                        # Template CI tests and publishes when a tag is pushed
```

### 8.2 Python Plugin Author

```bash
uvx rutis-host new weather --lang python           # Template: pyproject.toml, weather/, tests/, rutis.dev.json, CI workflow
cd weather && uv sync                              # rutis dependency; rutis-host dev dependency
uv run pytest                                      # Use rutis.testing
uv run rutis-host dev
uv build && uv publish
```

### 8.3 Operator: Use a Plugin

```bash
npm install weather-plugin                         # Install in the host's Node project (alongside @arcships/rutis-runtime)
uv pip install --python .venv weather-plugin       # Or install in the host's Python environment
# Add a row to rutis.json: { "id": "weather", "name": "weather-plugin", ... }
rutis-host check && rutis-host run
```

Rust host authors embed `rutis-loader` and `rutis-bridge`; row format is the same as `rutis.json`.

## 9. Versioning and Releases

### 9.1 rutis Packages

- **Release train:** except for the kernel `rutis` and dylib-related crates, all packages in Section 4 share one version and release together. Users only need to remember “use the same version.”
- **Compatibility is not enforced by version alignment:** the session protocol is checked during handshake and plugin API by the `api` marker. The train simply makes version numbers easy to understand.
- **Train starts at 0.3.0:** `rutis-loader` 0.2 is already released; the breaking merge becomes 0.3 and new package names launch at 0.3.0 with the loader.
- **Tags:** use `vX.Y.Z` for the train (the most common convention); change binary releases of `rutis-cli`, which currently use `v*`, to `cli-vX.Y.Z`; the kernel keeps `rutis-vX.Y.Z`.
- **One workflow:** `release.yml` publishes crates in dependency order (skip versions already on crates.io), then npm packages (publish platform host-binary packages first) and PyPI packages, and finally GitHub Release binaries. It replaces #147's `publish-bridge.yml` and existing `publish-interop.yml` / `publish-loader.yml`.
- **Remove old packages directly:** `rutis-interop` has no downstream users to support, so do not keep a compatibility layer, migration docs, or old-package publishing entry point. Do not publish a final README-only release or run `npm deprecate`. The train publishes only the new package structure; do not delete or yank historical registry versions.

### 9.2 Plugin Author Packages

- Follow each ecosystem's semver and depend on the SDK's major version (for example, `@arcships/rutis@^0.3`); the plugin API marker provides compatibility fallback.
- Templates include CI: tests, `rutis-host check`, and trusted publishing to npm or PyPI on tags.

## 10. Environment Requirements

| | Requirement | Notes |
|---|---|---|
| OS | Linux, macOS | Local runtimes require Unix; use WSL on Windows |
| Node | Currently says 26 | No Node 26-specific feature was found in the code. Test with Node 24 (LTS) during implementation and lower the requirement to 24 if it passes |
| Python | 3.12 or later | |
| Rust (embedded host) | MSRV 1.85 | Unchanged |

## 11. Documentation Structure

Write tutorials last and organize them by role under `docs/guide/`:

1. Write a TypeScript plugin: from `new` to release.
2. Write a Python plugin: from `new` to release.
3. Plugin API reference: `ctx`, declarations, value passing, re-entry, lifecycle.
4. Run the host: `rutis-host` and `rutis.json`.
5. Connect machines: nodes, remote runtimes, `peer:` rows, credentials, and TLS.
6. Embed in a Rust application: `rutis-loader`, `rutis-bridge`.
7. Connect a Cordis application as a node: `@arcships/rutis-runtime/bridge`.

Package READMEs on npm, PyPI, and crates.io should be in English and link to the relevant guides; write the guides in Chinese first.

## 12. Implementation Phases

| Phase | Scope | Completion criteria |
|---|---|---|
| P0 Merge and rename | Rust: merge into `rutis-bridge` (features in 4.2), remove old crates and deprecated interfaces. Node: split out `@arcships/rutis`, rename runtime to `@arcships/rutis-runtime`. Python: rename package/import to `rutis`. Rename environment variables. | All existing tests pass under the new structure; every feature combination compiles on its own |
| P1 SDK | Plugin API marker/check; test tools in both languages; types; Python entry points/version; per-row reload for Node leaf plugins; validate Node 24 | SDK has its own tests; `load(...)` can test example plugins in the repository |
| P2 Host | `rutis-host` run/dev/check/new; `rutis.json`; both templates; diagnostics for missing runtime packages | A template-created plugin can run `dev`, tests, and `check` without any Rust code |
| P3 Distribution/release | Release-train workflow; npm platform packages; maturin wheels; remove old publishing entry points | Complete release works once in test registries (Verdaccio, TestPyPI, crates.io dry run) |
| P4 Docs | Guides in Section 11 and package READMEs | Follow a tutorial from scratch without reading source code |
| Later | `rutis-host dev --join`; Windows binary | — |

Merge #147 (CI fixes, soak tests, smoke example, README) first, but do not release from it. Rewrite its release workflow and migration docs under P0/P3 to match the new structure.

## 13. Decisions

| # | Decision | Reason |
|---|---|---|
| 1 | Keep only `rutis`, `rutis-loader`, `rutis-bridge`, and `rutis-host` in Rust; optional pieces use features | Splits should serve user choice; internal layers belong in modules |
| 2 | Plugin-author packages: Node `@arcships/rutis`, Python `rutis`; Node runtime `@arcships/rutis-runtime`; host `rutis-host` | Plugin authors install one lightweight package named simply rutis |
| 3 | Make the general host an official product; first version covers development and simple deployment, while production operations (service management, log config, metrics) come later | It is the entry point for “use rutis without Rust,” but the first version need not cover every deployment concern |
| 4 | One release-train version, `vX.Y.Z` tags, one workflow; `rutis-cli` switches to `cli-v*` | Users align one version; reserve `v*` for the main product |
| 5 | Give plugin API its own integer version, starting at 1 | Package versions change for unrelated reasons and are unsuitable for deciding whether a plugin can load |
| 6 | Package READMEs in English; guides first in Chinese | Registry readers are global; write quality guides first, translate afterward |
| 7 | First release after P0–P3; do not release interop 0.3 / bridge 0.1 before then | Avoid immediate renaming; published names are hard to retract |

## 14. Implementation Record

The design was implemented in one pass. Deviations from the plan:

- **Unified version is 0.3.0:** `rutis-loader` 0.2 was already released; all train packages and new names use 0.3.0, and usage examples and migration docs were updated together.
- **Shared service names:** `rutis-host` uses `ServiceCatalog::share_by_name()` to share every service by name, so no `"shared"` field is needed and it was removed from config.
- **Development mode does not enable the dev channel** (`rutis-dev`): `rutis-host dev` watches files and reloads itself; add the channel if needed later.
- **Python projects in dev mode load modules via entry point** (`py:<module>`), so the project does not first need to be installed in the venv. After publishing, the host still loads by entry-point name.
- **Relative row paths:** `./…` and `../…` row names in `rutis.json` and `rutis.dev.json` are relative to the containing file.
- **Remote runtimes:** `runtimes.remote` in `rutis.json` declares a runtime elsewhere (Python row name `<name>:<module>`; Node rows resolve packages on that runtime).
- **Node 24:** all Node tests pass under Node 24, so `engines` is `>=24`; CI uses 24 on Linux and 26 on macOS.
- **Directories:** `node/rutis` (SDK), `node/rutis-runtime`, `node/rutis-host`, `node/baseline`, `python/rutis`; maturin config for `rutis-host` is in `crates/rutis-host/pyproject.toml`.
- **Publishing:** `release.yml` (tag `vX.Y.Z`) replaces `publish-interop.yml`, `publish-loader.yml`, and #147's `publish-bridge.yml`; `rutis-cli` binary releases use `release-cli.yml` (tag `cli-vX.Y.Z`). `scripts/train.mjs` checks train versions.
- **Do not enable tokio's `signal` feature workspace-wide:** it makes `rutis-dylib` examples fail to link (two copies of the standard library appear). `rutis-host` and smoke examples do not handle Ctrl-C; the process follows default behavior and runtime processes exit when the channel closes.
