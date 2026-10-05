# rutis-channel

rutis 会话所跑的通道契约。一条 `Channel` 是有序、可靠、双工的消息流：每条消息恰好到达一次、按顺序、保持边界。它只搬运不透明的字节，不认识协议帧或编码。

这个 crate 只有契约本身和连接错误，具体实现（分帧、大小上限、存活检测）在各承载 crate 里：[`rutis-transport-local`](../rutis-transport-local)、[`rutis-transport-memory`](../rutis-transport-memory)、[`rutis-transport-websocket`](../rutis-transport-websocket)。

| 类型 | 含义 |
| --- | --- |
| `Channel { sender, receiver, closer, info }` | 一条已建立的通道；sender、receiver 各在一个线程上用，closer 可从任何地方调用 |
| `ChannelInfo { transport, peer, label }` | 承载名、已验证的对端（`PeerId`）、诊断标签 |
| `ChannelError::Closed { reason }` | 通道结束；原因只用于诊断 |
| `ConnectError` | 建立失败的类别：`Retryable`（退避重试）、`AuthRejected`（慢重试）、`Incompatible`（停止） |
| `Closer::replaced()` | 被新的连接接管（WebSocket 关闭码 4002） |
| `PeerId` | 端点 id |

`trace::trace(channel, sink)` 给通道加一层记录：每条消息记一行方向和长度，不含内容。

## 给承载实现者

feature `testing` 提供契约测试，新的承载应当跑通它：

```rust
rutis_channel::testing::contract(|| my_transport_pair());
```

`testing::fault` 是可注入故障的通道包装（延迟、断开），用来测试上层在通道出问题时的行为。

设计见 [docs/design-protocol-channel-decoupling-2026-10-03.md](../../docs/design-protocol-channel-decoupling-2026-10-03.md)。
