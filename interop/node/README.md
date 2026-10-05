# @arcships/rutis-interop

The Node side of [rutis-interop](https://github.com/arcships/rutis/tree/main/crates/rutis-interop): it lets a [rutis](https://github.com/arcships/rutis) application mount published Cordis plugins across processes, with Rust bindings generated from the plugins' type declarations.

- `src/generate.mjs` generates the Rust bindings during the application's Cargo build.
- `src/runner.mjs` runs the mounted plugins in a real Cordis `Context`, talking to the rutis process over an inherited socket, a Unix socket or a WebSocket.
- `@arcships/rutis-interop/plugin` exports `definePlugin` for leaf plugins: a plugin that only uses services (`ctx.use`), provides services (`ctx.provide`) and returns a cleanup, without Cordis. rutis-loader loads it into the same Cordis `Context` as Cordis plugins.
- `@arcships/rutis-interop/bridge` links a Cordis application to rutis nodes (or other Cordis applications) as a full node, over WebSocket. `Link` keeps the session with one far end and provides it as the service `rutisPeer.<id>`; the features gate on it, so each session gets them anew:

  ```js
  import { Link, Export, Import, Host, Events } from '@arcships/rutis-interop/bridge'
  ctx.plugin(Link, { peer: 'main', id: 'mac', dial: 'wss://main.example.com/rutis', token })  // or listen: 'wss://…'
  ctx.plugin(Export, { peer: 'main', services: { calendar: { today: 'sync', later: 'async' } } })
  ctx.plugin(Import, { peer: 'main', services: ['clock'] })
  ctx.plugin(Host, { peer: 'main' })        // the peer may load the npm plugins installed here
  ctx.plugin(Events, { peer: 'main', out: ['tock'], in: ['tick'] })
  ```

  Opening `Host` gives the peer the management of the plugins installed here: open it only to trusted peers.
- `src/runner.mjs listen:wss://… --id <id> --peer <controller>` runs a runtime a remote rutis controls: it stays up, serves one controller at a time, and cleans each session's rows up when it ends.

Install it in the npm project next to your rutis application, together with the plugins you mount; the `rutis-interop` crate finds it at `node_modules/@arcships/rutis-interop`. The crate and this package must speak the same protocol version (`rutisProtocol`); the build checks it.

Usage, supported types and boundary rules: see the [rutis-interop guide](https://github.com/arcships/rutis/blob/main/crates/rutis-interop/README.md) (Chinese). Unix only; Node 26 or later.
