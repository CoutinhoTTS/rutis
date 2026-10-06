# 在 Rust 应用里嵌入

应用要给插件提供自己的 Rust 服务、或者把 rutis 放进已有的 Rust 程序时，直接用 crate 组装宿主。不需要这些时用 [rutis-host](rutis-host.md) 即可，配置的行格式相同。

## 依赖

```toml
[dependencies]
rutis = "0.7"
rutis-loader = { version = "0.7", features = ["node", "python", "peer"] }
rutis-bridge = { version = "0.7", features = ["python", "websocket"] }   # node 默认开
```

| crate / feature | 内容 |
| --- | --- |
| `rutis` | 内核：插件、依赖、服务 |
| `rutis-loader` | 按数据（行）加载插件。`node` / `python`：这两种语言的行；`peer`：节点行和 `peer:` 行 |
| `rutis-bridge` | 连接其他进程、语言、机器。`node`（默认）、`python`：本机运行时；`websocket`：WebSocket 承载；`cordis`：挂载 Cordis 插件并生成 Rust 绑定（见 [Cordis](cordis.md)）；`testing`：一致性测试 |

只用其中一种语言时只开那一个 feature，应用只编译、只启动它用到的部分。

## 给插件提供 Rust 服务

插件按名字使用的服务，在 Rust 里是 `host_key(名字)` 下的 `dyn HostDispatch`：

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
    // 方法形状：插件按它决定同步还是异步调用。
    fn methods(&self) -> Option<serde_json::Value> {
        Some(json!({ "now": "sync" }))
    }
}

root.provide_as::<dyn HostDispatch>(host_key("clock"), Arc::new(Clock))?;
```

插件提供的服务同样以 `host_key(名字)` 出现在 rutis 里，Rust 插件可以按名字使用它们。

## 运行时与加载器

```rust
use std::sync::Arc;
use rutis_bridge::runtime::LocalRuntime;
use rutis_loader::{
    Chain, Layer, LoaderOptions, LoaderPlugin, Patch, RuntimeResolver, RuntimeRowsPlugin, ServiceCatalog,
};

// 服务名到键：所有名字都按名字跨语言共享（rutis-host 的做法）；
// 也可以只登记需要的：catalog.register_shared("clock")。
let mut catalog = ServiceCatalog::new();
catalog.share_by_name();

// 本机的 Node 运行时：@arcships/rutis-runtime 所在的目录，和插件解析所用的 package.json。
let node = LocalRuntime::node("app/node_modules/@arcships/rutis-runtime", "app/package.json");
let node_rows = Arc::new(RuntimeResolver::node(node.handle()).with_catalog(&catalog));
// 本机的 Python 运行时：插件模块所在的目录，装着 rutis 的解释器。
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

- 运行时是一个插件，它的进程随插件启动、随插件卸载而结束。进程意外结束时，运行时停下，它的行等待；`RuntimeHandle::state()` 是 `Down(原因)`，原因说明进程怎样结束。是否重启由应用决定：对运行时的 fiber 调用 `restart`。
- 行的完整格式（分组、`isolate`、`inject`、表达式、命令式修改与持久化）见 [rutis-loader 设计](../design-rutis-loader-2026-10-02.md)。
- 改了插件代码：`RuntimeResolver::invalidate_all()` 后 `Loader::reload(行)`，新代码生效（Node 与 Python 都只重新导入插件的入口模块）。
- 设置 `RUTIS_TRACE` 时，运行时通道上的每条消息在 stderr 记一行（方向和长度，不含内容）。

## 节点

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

用 rutis-loader 配置时，`rutis_loader::register_peer_node` 注册 `rutis-bridge/peer` 行（与 rutis.json 相同），`PeerResolver` 解析 `peer:` 行。远程运行时：`RuntimePlugin::remote(名字)` 加一个 `runtime` 设为同名的节点行，行名由 `RuntimeResolver::modules(handle)` 解析为 `<名字>:<模块>`。概念与安全见 [连接节点](nodes.md)。
