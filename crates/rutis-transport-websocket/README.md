# rutis-transport-websocket

WebSocket 承载：跨机器的 rutis link。

`WebSocketPlugin` 提供 `Transport#websocket`，并持有 link 注册的监听器。

- **拨号**：每次调用连一次。`ws://` 只允许回环地址，其他地址必须 `wss://`。服务端证书按 `Trust` 验证（默认系统根证书，`Trust::only(ca)` 只信任给定 CA）。
- **监听**：`ListenerConfig::new(名字, 地址, 本端 id)`，可加 `.path(..)` 和 `.tls(ServerTls { certificate_pem, key_pem, client_ca_pem })`。link 用 `LinkConfig::listen` 在监听器上注册；连接按出示的凭据（`Authorization: Bearer` 或客户端证书）由 `Identity` 验证，交给对应的 link。
- **报文**：每条连接一条通道，消息是 UTF-8 JSON 文本；会话协议是子协议（`rutis.<版本>`），不符时在握手阶段拒绝（`Incompatible`）。
- **失败类别**：401/403（凭据或证书不对）是 `AuthRejected`；400/404/405/426 是 `Incompatible`；503（凭据有效但还没有 link 在监听它）和其他错误是 `Retryable`。
- **存活与上限**（`Limits`）：双方每 10 秒 ping，30 秒无消息即断开；单条消息上限 16 MiB，超过以 1009 关闭；被新连接接管以 4002 关闭，卸载以 1001 关闭。
- 承载跑在自己的线程上，所以无论调用方的执行器在做什么（例如阻塞在同步调用里），通道都能推进。

```rust
let websocket = WebSocketPlugin::new(Config::new().listener(
    ListenerConfig::new("public", "0.0.0.0:7443".parse()?, PeerId::new("main")?)
        .tls(ServerTls { certificate_pem, key_pem, client_ca_pem: None }),
))?;
root.plugin(websocket);
root.plugin(IdentityPlugin::new("main", StaticIdentity::new(main).accept_token(token, mac)));
root.plugin(LinkPlugin::new(LinkConfig::listen(mac, "websocket", "main", "public")));
```

Node（`@arcships/rutis-interop` 的 `ws:` / `wss:` / `listen:` 通道）和 Python（`rutis_runtime` 的 `network` 可选依赖）说同一套绑定，可以互连。
