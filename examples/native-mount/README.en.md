# Cross-Process Mount Example

The primary direction is **mounting existing Cordis plugins in a rutis application**. Rust types are generated from the original TypeScript plugin at build time; the plugin runs in a real Cordis instance and is used as a regular service from rutis. See the [compatibility layer design](../../docs/design-protocol-plugin-mount.en.md).

Run from the repository root:

```sh
npm --prefix node/rutis-runtime ci
cargo run -p native-mount-example
# Reverse direction (frozen; no further capabilities will be added): Cordis app mounts a rutis plugin from src/lib.rs.
cargo test -p native-mount-example --test cordis_mount -- --nocapture
```

There is no separate generation step. Mounts are declared in `[package.metadata.rutis-cordis]` in [Cargo.toml](Cargo.toml); [build.rs](build.rs) calls `from_manifest()` during a normal Cargo build. Generated files go in the build directory and are not committed or maintained by hand. See the [Cordis guide](../../docs/guide/cordis.en.md) for integration details.

| Direction | Original plugin | How it is used |
| --- | --- | --- |
| rutis mounts Cordis (primary) | [counter.ts](../../node/rutis-runtime/test/fixtures/counter.ts) | `ctx.plugin(bindings::Plugin::new(config))`, then `ctx.require::<bindings::Counter>()?` |
| Cordis mounts rutis (frozen) | [src/lib.rs](src/lib.rs) | `ctx.plugin(plugin(executable), config)`, then `ctx.counter.add(1)` |

The original plugin has no protocol imports or annotations.

| Verified | Behavior |
| --- | --- |
| Method shapes | Sync methods return synchronously; async methods return a Future / Promise. TypeScript exceptions map to `Result`; non-finite numbers produce explicit errors. |
| Dependencies and cleanup | Consumers wait for services to be ready. On unload, consumers clean up first (while they can still call remote services), then the remote process closes. |
| Following replacement | After Cordis replaces a service, new rutis reads get a new proxy while existing proxies still point to the old object. After revocation, consumers stop; after the service returns, they resume. |
| In-flight calls | Unload starts the disposer before waiting for in-flight calls. |
| Isolation and failure | Two isolated mounts do not share state. If the remote process exits, both in-flight and later calls fail. |

Currently supported method signatures use `number`, `string`, `boolean`, `void`, and arrays of these. Other types fail at build time. Event forwarding, callback arguments, and object return values are covered by the [roadmap](../../docs/roadmap-native-plugin-mount.en.md).
