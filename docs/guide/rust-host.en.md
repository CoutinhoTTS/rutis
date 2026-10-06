# Embed rutis in a Rust Application

Use the crates directly when your application needs to provide its own Rust services to plugins or when you want to embed rutis in an existing Rust program. If you do not need this, use [rutis-host](rutis-host.en.md); both use the same row configuration format.

## Dependencies

```toml
[dependencies]
rutis = "0.6"
rutis-loader = { version = "0.7", features = ["node", "python", "peer"] }
rutis-bridge = { version = "0.7", features = ["python", "websocket"] }   # node is enabled by default
```

| Crate / feature | Contents |
| --- | --- |
| `rutis` | Core: plugins, dependencies, and services. |
| `rutis-loader` | Loads plugins from data rows. `node` / `python` enable rows for those languages; `peer` enables node rows and `peer:` rows. |
| `rutis-bridge` | Connects processes, languages, and machines. `node` (default) and `python` enable local runtimes; `websocket` enables the WebSocket transport; `cordis` mounts Cordis plugins and generates Rust bindings (see [Cordis](cordis.en.md)); `testing` enables compatibility tests. |

Enable only the language features you use so the application compiles and starts only the required components.

## Provide Rust services to plugins

In Rust, a service used by name from a plugin is a `dyn HostDispatch` registered under `host_key(name)`:

```rust
use std::sync::Arc;
use rutis_bridge::session::{host_key, HostDispatch, Reply, Value};
use serde_json::json;

struct Clock;

impl HostDispatch for Clock {
    fn invoke(&self, method: &str, _args: Value) -> Reply {
        match method {
            "now" => Ok(json!(42).into()),
            other => Err(rutis_bridge::session::Error::Value(format!("no method {other}"))),
        }
    }
    // The plugin uses this method shape to decide whether to call synchronously or asynchronously.
    fn methods(&self) -> Option<serde_json::Value> {
        Some(json!({ "now": "sync" }))
    }
}

root.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock))?;
```

Services provided by plugins also appear in rutis under `host_key(name)`, so Rust plugins can use them by name.

## Runtime and loader

```rust
use std::sync::Arc;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
};

// Share every service name across languages, as rutis-host does. You can also register only selected names:
// catalog.register_shared("clock").
let mut catalog = ServiceCatalog::new();
catalog.share_by_name();

// Local Node runtime: the directory containing @arcships/rutis-runtime and the package.json used for resolution.
let node = LocalRuntime::node("app/node_modules/@arcships/rutis-runtime", "app/package.json");
let node_rows = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
// Local Python runtime: plugin module directory and an interpreter with rutis installed.
let python = LocalRuntime::python("app/plugins").interpreter("app/.venv/bin/python");
let python_rows = Arc::new(RuntimeResolver::modules(python.handle()).with_catalog(&catalog));
root.plugin(node);
root.plugin(python);

let loader = LoaderPlugin::new(
    Chain::new().with_shared(python_rows.clone()).with_shared(node_rows.clone()),
    LoaderOptions { catalog, ..LoaderOptions::default() },
);
let handle = loader.handle();
root.plugin(loader).await?;
root.plugin(RuntimeRowsPlugin::new(node_rows));
root.plugin(RuntimeRowsPlugin::new(python_rows));

let rows: Vec<Patch> = serde_json::from_value(json!([{ "insert": [
    { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "py:llm_gateway" }
] }]))?;
handle.reconcile(vec![Layer::new("app", rows)], None).await?;
```

- A runtime is a plugin. Its process starts with the plugin and exits when the plugin is unloaded. If the process exits unexpectedly, the runtime stops and its rows wait; `RuntimeHandle::state()` reports `Down(reason)`, including why the process ended. The application decides whether to restart it by calling `restart` on the runtime fiber.
- For the full row format (groups, `isolate`, `inject`, expressions, imperative updates, and persistence), see the [rutis-loader design](../design-rutis-loader-2026-10-02.en.md).
- To pick up plugin code changes, call `RuntimeResolver::invalidate_all()` and then `Loader::reload(row)`. Node and Python both reimport only the plugin's entry module.
- With `RUTIS_TRACE` set, each runtime-channel message produces a line on stderr with its direction and length, but not its contents.

## Nodes

```rust
use rutis_bridge::transport::websocket::{Config, WebSocketPlugin};
use rutis_bridge::{Credential, Features, IdentityPlugin, LinkConfig, PeerId, PeerPlugin, StaticIdentity};

let (main, office) = (PeerId::new("main")?, PeerId::new("office")?);
root.plugin(WebSocketPlugin::new(Config::new())?).await?;
root.plugin(IdentityPlugin::new(
    "main",
    StaticIdentity::new(main).present(office.clone(), Credential::Bearer(token)),
));
root.plugin(PeerPlugin::new(
    LinkConfig::dial(office, "websocket", "main", "wss://office.example.com/rutis"),
    Features { export: vec!["clock".into()], import: vec!["calendar".into()], ..Features::default() },
)?);
```

For rutis-loader configuration, `rutis_loader::register_peer_node` registers the `rutis-bridge/peer` row (the same format as `rutis.json`), and `PeerResolver` resolves `peer:` rows. For a remote runtime, add `RuntimePlugin::remote(name)` and a node row whose `runtime` is the same name; `RuntimeResolver::modules(handle)` resolves row names as `<name>:<module>`. See [Connect nodes](nodes.en.md) for concepts and security.
