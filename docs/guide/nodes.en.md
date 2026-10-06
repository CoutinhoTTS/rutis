# Connect Nodes

A rutis host is a **node**. Nodes connect through a **link**: one side dials (`dial`) and the other listens (`listen`). Once connected, they can share services, run plugins on each other's behalf, and forward events. The connection reconnects automatically when interrupted.

The examples below configure rows in `rutis.json`. In Rust, use `rutis_bridge::PeerPlugin` for the same purpose (see [Embed in a Rust application](rust-host.en.md)).

## A link

```json
{ "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis" } }
```

The other side (`office`) listens:

```json
{
  "id": "office",
  "listen": [{ "name": "public", "address": "0.0.0.0:7443", "cert": "server.pem", "key": "server.key" }],
  "rows": [
    { "id": "main", "name": "rutis-bridge/peer", "config": { "peer": "main", "listen": "public" } }
  ]
}
```

Both sides use the same token. The dialing side presents `RUTIS_TOKEN` (or `RUTIS_TOKEN_OFFICE`); the listening side uses it to verify that the peer is `main`.

- `dial` accepts `wss://…` for remote connections (TLS required) or `ws://127.0.0.1…` for the local loopback interface.
- For a self-signed certificate, add its issuing CA to `RUTIS_CA` on the dialing side.
- After a disconnect, retries use backoff starting at 0.5 seconds and capped at 30 seconds. Rejected credentials (403 or an untrusted certificate) are retried every 30 seconds. An incompatible protocol stops retrying and reports an error.
- A dialing peer that arrives before the listening side's link is ready is asked to retry, so either side can start first.

## What a link can do

The node row's `config` determines what happens over the connection. Changing `export`, `import`, or `events` does not disconnect the link; only the corresponding feature restarts.

| Field | Purpose |
| --- | --- |
| `export: ["weather"]` | Makes this node's `weather` service available to the peer. |
| `import: ["calendar"]` | Registers the peer's `calendar` service on this node. It is rejected if this node already has a service with that name. |
| `events: { "out": ["tick"], "in": ["tock"] }` | Forwards events in one direction. |
| `host: true` | Allows the peer to run plugins installed on this node. Enable only for trusted peers. |
| `rows: true` | Runs rows named `peer:<peer>/<plugin>` from this file on the peer. |
| `runtime: "<name>"` | Identifies the peer as a language runtime; see below. |

Calls retain their call chain across nodes: callbacks during a synchronous call, including callbacks that call back again, can cross multiple nodes.

## Run a plugin on another node

When the peer enables `host: true` and this node enables `rows: true`, a `peer:<peer>/<plugin>` row loads that plugin on the peer:

```json
{ "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis", "rows": true } },
{ "id": "scanner", "name": "peer:office/scanner-plugin", "config": { "dpi": 300 } }
```

The plugin runs on the peer and uses that node's services. This node still controls its configuration and lifecycle. If the peer goes offline, the row waits and reloads the plugin when the peer returns. The peer executes the row's `isolate` and `inject` settings.

## A language runtime on another machine

For example, run a Python runtime on a GPU machine and manage its plugins from the main node:

```bash
# On the GPU machine: install rutis[network] and the plugins, then listen
RUTIS_TOKEN=secret RUTIS_CERT=server.pem RUTIS_KEY=server.key \
  python -m rutis listen:wss://0.0.0.0:7443/rutis --id gpu --peer main /srv/plugins
```

```json
{
  "id": "main",
  "runtimes": { "remote": [{ "name": "gpu", "language": "python" }] },
  "rows": [
    { "id": "gpu", "name": "rutis-bridge/peer", "config": { "peer": "gpu", "dial": "wss://gpu.example.com:7443/rutis", "runtime": "gpu" } },
    { "id": "embedder", "name": "gpu:embedder" }
  ]
}
```

A remote Python runtime row is named `<runtime-name>:<module>`; the module is resolved on the remote machine. A Node runtime can also run remotely: in the Node project on that machine, run `node node_modules/@arcships/rutis-runtime/src/runner.mjs listen:wss://… --id <name> --peer <controller> ./package.json` (plugins are resolved from that `package.json`) and set `language` to `node`. A remote runtime serves one controller at a time. When its controller disconnects, it unloads all plugins running for that controller.

## Connect a Cordis application as a node

An existing Cordis application can become a node through `@arcships/rutis-runtime/bridge`; see [Cordis](cordis.en.md).

## Security

- Use `wss://` for remote connections. A listener without TLS may bind only to a loopback address.
- Use one token per peer (`RUTIS_TOKEN_<NODE>`) and set it only on the nodes that need it.
- `host: true` lets a peer run any plugin installed on this node. Enable it only for trusted peers. It can load installed plugins, not file paths.
- Nodes share services by name, and only services listed in `export` are shared.
