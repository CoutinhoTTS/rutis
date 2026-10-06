# @arcships/rutis-runtime

The Node runtime of [rutis](https://github.com/arcships/rutis): the process that runs TypeScript, JavaScript and Cordis plugins for a rutis host. Plugin authors depend on [`@arcships/rutis`](https://www.npmjs.com/package/@arcships/rutis) instead; hosts install this package next to the plugins they run.

- `src/runner.mjs` runs plugins in a Cordis `Context`, over an inherited socket, a Unix socket or a WebSocket. `listen:wss://… --id <id> --peer <controller> <package.json>` runs a runtime a rutis host on another machine controls.
- `@arcships/rutis-runtime/bridge` makes a Cordis application a rutis node: `Link`, `Export`, `Import`, `Host`, `Events`.
- `src/generate.mjs` generates Rust bindings for Cordis plugins mounted with rutis-bridge's `cordis` feature.

The runtime and the rutis-bridge crate speak the same protocol version (`rutisProtocol`); builds check it. Linux and macOS; Node 24 or later.

Guides (Chinese): [Cordis](https://github.com/arcships/rutis/blob/main/docs/guide/cordis.md), [nodes](https://github.com/arcships/rutis/blob/main/docs/guide/nodes.md).
