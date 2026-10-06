# rutis-host and rutis.json

`rutis-host` is a host that requires no Rust code. It assembles the core, plugin loader, local Node / Python runtimes, and network connections, all driven by a `rutis.json` file. Plugin authors can use it for local development or deployment. For applications that need custom Rust services, see [Embed in a Rust application](rust-host.en.md).

## Installation

| Method | Command |
| --- | --- |
| npm | `npx @arcships/rutis-host …`, or install with `npm install -D @arcships/rutis-host` and run `npx rutis-host …` |
| PyPI | `uvx rutis-host …`, or install with `uv add --dev rutis-host` and run `uv run rutis-host …` |
| Binary | `rutis-host-<version>-<platform>.tar.gz` from GitHub Releases (Linux and macOS, x64 / arm64) |
| crates.io | `cargo install rutis-host` |

The npm distribution includes the Node runtime (`@arcships/rutis-runtime`), and the PyPI distribution includes the Python runtime (`rutis`). They are used when the project does not provide its own runtime.

## Commands

| Command | Purpose |
| --- | --- |
| `rutis-host run [rutis.json]` | Runs the configuration and prints each row's status changes. Press Ctrl-C to stop. |
| `rutis-host dev [directory]` | Runs a plugin project with its `rutis.dev.json` file and reloads it when files change. |
| `rutis-host check [rutis.json]` | Resolves each row and prints its version, dependencies, provided services, and configuration schema. Exits with a nonzero status if a row cannot run. Without `rutis.json`, checks the plugin project in the current directory. |
| `rutis-host new <name> --lang node\|python` | Creates a plugin project. |

## `rutis.json`

```json
{
  "id": "main",
  "runtimes": {
    "node": { "project": "." },
    "py": { "project": "plugins", "python": ".venv/bin/python" },
    "remote": [{ "name": "gpu", "language": "python" }]
  },
  "listen": [
    { "name": "public", "address": "0.0.0.0:7443", "cert": "tls/server.pem", "key": "tls/server.key" }
  ],
  "rows": [
    { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "py:llm_gateway" },
    { "id": "local-tool", "name": "./tools/tool.ts" },
    { "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis", "export": ["weather"] } },
    { "id": "gpu", "name": "rutis-bridge/peer", "config": { "peer": "gpu", "dial": "wss://gpu.example.com/rutis", "runtime": "gpu" } },
    { "id": "embedder", "name": "gpu:embedder" }
  ]
}
```

Relative paths in the file are resolved from the directory containing the file.

### `id`

The endpoint ID of this node. Connected nodes see this name. Defaults to `host`.

### `runtimes`

| Key | Fields | Purpose |
| --- | --- | --- |
| `node` | `project` (default `.`), `runtime` | Starts the Node runtime. Plugin packages are resolved from `project`'s `package.json`. Install `@arcships/rutis-runtime` there, specify it with `runtime`, or use the copy included with the npm distribution of rutis-host. |
| `py` | `project` (default `.`), `python` | Starts the Python runtime. The interpreter defaults to `$VIRTUAL_ENV/bin/python`, then `<project>/.venv/bin/python`, then `python3`. Its environment must contain `rutis` and the plugins. |
| `remote` | `name`, `language` | A runtime on another machine, connected through a node row with `"runtime": "<name>"`; see [Connect nodes](nodes.en.md). |

If a runtime package is missing, startup fails and prints an installation command.

### `listen`

WebSocket listeners used by other nodes to connect. `cert` and `key` are a TLS certificate and private key in PEM format; they can also be supplied through `RUTIS_CERT` and `RUTIS_KEY`. A listener without TLS can bind only to a loopback address.

### `rows`

Each row runs a plugin:

| Field | Meaning |
| --- | --- |
| `id` | Row name, unique within this file. |
| `name` | Plugin identifier: an npm package (or package subpath), `py:<name>` (Python entry point or module), `./relative-path` (plugin file), `<remote-runtime>:<module>`, `rutis-bridge/peer` (a node; see [Connect nodes](nodes.en.md)), or `peer:<node>/<plugin>` (a plugin running on another node). |
| `config` | Plugin configuration. |
| `inject` | Additional service names to wait for. |
| `isolate` | `{ "service": true }` gives this row its own instance of the service; `{ "service": "label" }` shares one instance among rows with that label. |
| `disabled` | When `true`, the row does not run. |

Services are shared by name across all plugins. Any plugin that declares `inject: ["weather"]` can use a `weather` service provided in any language or on any node.

## Credentials

Do not put credentials in the file:

| Environment variable | Purpose |
| --- | --- |
| `RUTIS_TOKEN` | Token presented when connecting to other nodes and checked when accepting connections. |
| `RUTIS_TOKEN_<NODE>` | Token for a specific node (uppercase, with `-` written as `_`); takes precedence over `RUTIS_TOKEN`. |
| `RUTIS_CA` | Additional trusted CA certificate in PEM format for `wss://` connections. |
| `RUTIS_CERT` / `RUTIS_KEY` | Listener certificate and key when they are not specified under `listen`. |

## Deployment

Place `rutis.json`, the Node project (with plugins and `@arcships/rutis-runtime` installed), and the Python environment (with `rutis` and plugins installed) together. Run `rutis-host run /srv/app/rutis.json` under a process manager such as systemd. When `rutis-host` exits, the runtime processes it started exit as well.
