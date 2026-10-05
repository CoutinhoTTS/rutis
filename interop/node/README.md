# @arcships/rutis-interop

The Node side of [rutis-interop](https://github.com/arcships/rutis/tree/main/crates/rutis-interop): it lets a [rutis](https://github.com/arcships/rutis) application mount published Cordis plugins across processes, with Rust bindings generated from the plugins' type declarations.

- `src/generate.mjs` generates the Rust bindings during the application's Cargo build.
- `src/runner.mjs` runs the mounted plugins in a real Cordis `Context`, talking to the rutis process over a Unix socket.
- `@arcships/rutis-interop/plugin` exports `definePlugin` for leaf plugins: a plugin that only uses services (`ctx.use`), provides services (`ctx.provide`) and returns a cleanup, without Cordis. rutis-loader loads it into the same Cordis `Context` as Cordis plugins.

Install it in the npm project next to your rutis application, together with the plugins you mount; the `rutis-interop` crate finds it at `node_modules/@arcships/rutis-interop`. The crate and this package must speak the same protocol version (`rutisProtocol`); the build checks it.

Usage, supported types and boundary rules: see the [rutis-interop guide](https://github.com/arcships/rutis/blob/main/crates/rutis-interop/README.md) (Chinese). Unix only; Node 26 or later.
