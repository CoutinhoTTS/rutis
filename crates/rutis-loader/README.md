# rutis-loader

Data-driven plugin management for [rutis](https://github.com/arcships/rutis): which plugins run, with which config, decided by ordered layers of rows rather than code. The counterpart of Cordis' `cordis-plugin-loader` and `cordis-plugin-include`.

- `reconcile` converges the running plugins to the layers; imperative edits (`create`, `update`, `set_disabled`, `move_to`, `remove`, …) change the editable layer, wait for the tree to settle and roll back on failure; `Persist` saves edits.
- `inject` / `isolate` name services through a `ServiceCatalog`; `{ "__jsExpr": … }` expressions are evaluated by `LoaderOptions::expressions`.
- Volatile config fields change without a restart; a plugin can unload itself.

Plugin sources (`Resolver`):

| source | row name | feature |
| --- | --- | --- |
| `Builtins` | any registered name | |
| `RuntimeResolver::node` | npm package, subpath or file | `node` |
| `RuntimeResolver::modules` | `py:<module>` (or `<runtime>:<module>`) | `python` |
| `PeerResolver`, `register_peer_node` | `peer:<node>/<plugin>`, `rutis-bridge/peer` | `peer` |
| `rutis_dylib::DylibResolver` | `dylib:<dir>` | rutis-dylib's `loader` |

```rust
let plugin = LoaderPlugin::new(Chain::new().with(builtins), LoaderOptions::default());
let loader = plugin.handle();
root.plugin(plugin).await?;
loader.reconcile(vec![Layer::new("defaults", defaults)], None).await?;
```

Guides (Chinese): [embedding in Rust](https://github.com/arcships/rutis/blob/main/docs/guide/rust-host.md); design: [docs/design-rutis-loader-2026-10-02.md](https://github.com/arcships/rutis/blob/main/docs/design-rutis-loader-2026-10-02.md).
