# rutis-bridge

把 rutis 接到其他进程和机器：link、身份、节点功能。

## 组成

- **承载**（`Transport`）：由承载插件按 `transport_key(种类)` 提供，link 只依赖这个接口，不依赖具体承载 crate。现有 [`local`](../rutis-transport-local)、[`memory`](../rutis-transport-memory)、[`websocket`](../rutis-transport-websocket)。
- **身份**（`Identity`）：持有本端出示的凭据，以及把对端出示的凭据映射到端点 id 的规则。`StaticIdentity::new(本端).present(对端, Credential::Bearer(..))` / `.accept_token(token, 对端)`；`IdentityPlugin` 提供它。
- **link**（`LinkPlugin` + `LinkConfig`）：`dial` 或 `listen` 一个对端，建立会话后提供 `Peer#<对端>`。
  - 断开后退避重连：0.5 秒起，上限 30 秒，±20% 抖动，稳定 60 秒后重置；`AuthRejected` 按 30 秒慢重试；`Incompatible` 停止。
  - `require(能力)` 在握手后校验对端的契约（`node`、`runtime`），缺少即停止。
  - `LinkConfig::local_runtime()` 是本机运行时用的模式：兼容协议、不重连。
  - `LinkPlugin::state()` 观察 `LinkState`。
- **Peer**：一次会话。功能按族（`plugins`、`services`、`events`…）在上面注册处理函数，并经 `link.offers` 告诉对端；`Peer::offers()` 观察对端提供了什么。重连后是新的 `generation`。

## 节点功能

| 插件 | 作用 |
| --- | --- |
| `ExportPlugin::new(对端, [名字])` | 把本端的服务（`host_key(名字)`）公告给对端 |
| `ImportPlugin::new(对端, [名字])` | 把对端公告的服务注册为本端服务；本端已有同名服务时拒绝 |
| `HostPlugin::new(对端, 目录)` | 替对端加载本机已安装的插件（`plugins.*`），按行的 `isolate` / `inject` 运行；只对可信对端开放 |
| `EventsPlugin::new(对端, 发出, 接收)` | 事件单向转发 |
| `RuntimeAccessPlugin::new(对端, 名字)` | 对端是一个运行时实例，提供 `RuntimeSession#<名字>` 给 `RuntimePlugin::remote` |
| `PeerPlugin::new(LinkConfig, Features)` | 把 link 和上面这些功能组合成一个插件；`PeerHandle::set` 改功能时只重启变化的部分，会话保留 |

服务调用跨节点保留调用链：同步调用期间的回调、回调里再调回去的重入调用，都能穿过多跳转发。

feature `conformance` 提供节点一致性测试（`conformance::node`），其他实现（例如 Node 的 `@arcships/rutis-interop/bridge`）可以拿它对照自己。

## 例子

```rust
// main：监听 mac，替它跑行，把 clock 给它
root.plugin(WebSocketPlugin::new(config)?);
root.plugin(IdentityPlugin::new("main", StaticIdentity::new(main.clone()).accept_token(token, mac.clone())));
root.plugin(PeerPlugin::new(
    LinkConfig::listen(mac, "websocket", "main", "public").require("node"),
    Features { export: vec!["clock".into()], ..Features::default() },
)?);
```

用 rutis-loader 配置时，`rutis-bridge/peer` 行（feature `peer`）就是一个 `PeerPlugin`，`peer:<对端>/<插件>` 行在对端加载插件。

设计见 [docs/design-remote-plugins-2026-10-03.md](../../docs/design-remote-plugins-2026-10-03.md)。
