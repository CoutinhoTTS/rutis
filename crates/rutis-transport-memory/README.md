# rutis-transport-memory

进程内的承载：同一进程里有界的内存通道，用于测试和进程内的 link。应用只在显式配置时使用。

- `pair()`：两条互相连接的通道。
- `MemoryPlugin` 提供 `Transport#memory`。`MemoryTransport::listen(名字)` 开一个监听器，`dial(名字)` 连到它。
- `MemoryTransport::endpoint(名字, 本端 id)` 声明一个按身份路由的端点：link 在上面注册（`LinkConfig::listen`），拨号方出示 bearer token，由监听方的 `Identity` 验证后交给对应的 link，行为和 WebSocket 监听器一致（含未注册时的 `Retryable`）。
- 卸载插件会关闭它建立的所有通道和监听器。

```rust
let transport = Arc::new(MemoryTransport::default());
transport.endpoint("main-in", PeerId::new("main")?);
main.plugin(MemoryPlugin::with_transport(transport.clone()));
mac.plugin(MemoryPlugin::with_transport(transport));
// main: LinkConfig::listen(mac, "memory", "main", "main-in")
// mac:  LinkConfig::dial(main, "memory", "mac", "main-in")
```

每个方向缓冲 64 条消息（`CAPACITY`），满了 `send` 阻塞。它满足 `rutis-channel` 的通道契约，测试多节点行为时可以用它代替网络。
