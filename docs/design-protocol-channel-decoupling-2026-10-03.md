# 网络栈：协议与通道解耦（设计稿）

状态：D1 已实现（`crates/rutis-channel`，会话改跑在通道上，`Session` 从 `Process` 拆出，Node 侧分帧归通道）；D2、D3 未实现。日期：2026-10-03。
依据：[兼容层设计](design-protocol-plugin-mount.md)（§3 线协议）、[挂载 Cordis 插件：需求](requirements-protocol-plugins.md)。
使用方：[远程插件：跨框架节点与对称协议](design-remote-plugins-2026-10-03.md)（下称"节点稿"）。
基准：`main` `d68471f`。

## 一、要解决什么

节点稿把整件事看成：

```text
rutis ── 网络栈 ── rutis / cordis / pydis / …
```

本文讲中间那一层。**协议不能依赖承载它的通道**：协议层只知道"有一条有序、可靠的消息通道"；通道层只搬运不透明的消息，不认识帧。两边可以单独替换。

每个框架都要有自己的网络栈实现：Rust 是新 crate `rutis-channel`，JS 放在 cordis 桥的包里，库外的框架（例如以后的 pydis）各自实现。所以本文既是 Rust 侧的设计，也是一份跨语言规范：通道契约（§五）和 WebSocket 绑定（§九）。不同语言的实现按这份规范互通。

现状是协议和"Unix socket + 换行分帧 + JSON + 拉起进程后回拨"绑在一起（§三），后果有这些：

- **远程**：节点之间走网络，就得先把网络连接伪装成 Unix 字节流（远程稿第一版就是这么做的，见 §十四）。
- **库外的框架**：其他语言不进 rutis（#107），但库外的实现要接入时，也得照抄"临时目录 socket + 回拨"的启动方式。
- **测试**：没有进程内通道，也没有可注入故障的通道，半开连接、慢链路都测不了。
- **平台**：会话代码直接引用 `std::os::unix::net::UnixStream`，整个会话层只能在 Unix 上编译。

不在本文范围：

- 协议本身（会话层的对称化、框架操作）：见节点稿 §五。
- rutis-dev 的开发通道（另一个协议）。以后可以改用本文的通道层，本文不展开。
- Windows 上的通道实现（命名管道）。解耦之后它只是多一种通道。

## 二、结论先说

1. **分层，依赖只能向下**：连接器 → 通道 → 编码 → 会话 → 框架操作 → 桥（§四）。本文管前两层和编码层的边界。
2. **新 crate `rutis-channel`** 放通道契约、通道实现、连接器和装饰器，不依赖任何协议 crate。`rutis-interop` 只通过 `Channel` 使用通道，解耦由编译器保证。这些都是库；在框架里，连接器由传输插件包成服务，交给链接插件使用（节点稿 §4.3）。
3. **Rust 的通道接口是阻塞式的**，因为会话的同步调用要在任意线程上阻塞等待，读线程也刻意不依赖调用方的运行时。通道必须独立于调用方执行器推进；内部用异步栈的实现（WebSocket、TLS）自带线程（§5.3）。Node 的通道跑在 I/O worker 里，Python 跑在自己的线程里，是同一条约束。
4. **一帧对应一条通道消息**。分帧归通道（字节流用换行，以后二进制编码用长度前缀），编码归协议（JSON）。Node 的 `wire.encode` 不再附带换行（§六）。
5. **结束原因归通道**：进程退出状态、心跳超时、对端关闭时给出的原因，都作为通道的结束原因交给会话。会话不再接收 `disconnected` 闭包。
6. **身份来自连接器**：`ChannelInfo` 带连接器验证过的对端节点 id；握手里自报的节点 id 必须与它一致（节点稿 §5.1）。
7. **本机子节点用继承的 fd 连接**：把 socketpair 的一端作为 fd 3 交给子进程，去掉临时目录 socket；路径方式保留。Node 侧已验证可行（§七）。
8. **WebSocket 绑定是一份跨语言规范**（§九）：URL、子协议、鉴权、一帧一消息、心跳、关闭码、重连。Rust 和 JS 的实现按它互通。
9. **D1 是纯重构**：线格式不变（Unix socket 上仍是逐行 JSON），`rutisProtocol` 不变，现有测试全部通过，性能不退化。
10. **测试按矩阵组织**：通道契约测试跑遍每种通道实现；会话测试跑遍每种通道；WebSocket 绑定做跨实现测试（§十一）。

**完成的判据**：

- `rpc.rs`、`protocol.rs` 不再引用 `std::os::unix`，去掉 `cfg(unix)` 后能编译（`rpc.rs` 现在借用的 `server::native_error` 要挪出冻结的 `server.rs`）；
- 新增一种通道，不需要改 `rutis-interop` 的任何文件；
- 同一组会话测试在所有通道上通过；
- Rust 和 JS 的 WebSocket 实现能互相连通（Rust 监听、JS 拨号，以及反过来）；
- Unix 上的线格式不变。

## 三、现状：耦合点清单

| 位置 | 现在做什么 | 属于哪层 | 解耦后 |
| --- | --- | --- | --- |
| `rpc.rs` `Peer.writer: Mutex<UnixStream>`、`closer: UnixStream`、`Drop` 里 `shutdown` | 发送；从别的线程打断阻塞的写；结束 | 通道 | `Box<dyn Sender>`、`Arc<dyn Closer>` |
| `rpc.rs` 读线程：`BufReader::lines()` + `serde_json::from_str` | 按行切分并解码 | 分帧 + 编码 | `Receiver::recv()` + 编码层解码 |
| `rpc.rs` `write_locked`：`serde_json::to_vec` + `\n` + `write_all` | 编码、加换行、写出 | 编码 + 分帧 + 通道 | 编码层编码 + `Sender::send` |
| `rpc.rs` `Connection::connect(UnixStream)`、`connect_with(UnixStream, disconnected)` | 会话入口 | 入口 | `Connection::open(Channel, dispatch)`；`connect` 保留为 `Channel::unix(stream)` 的简写 |
| `process.rs` `Process::mount` | 建临时目录、`UnixListener::bind`、拉起 `node … runner.mjs <socket>`、`accept` | 连接器 | 本机子节点的 `spawn` 连接器，返回 `Channel` + 子进程句柄 |
| `process.rs` `Child::disconnected` | 断开时最多等 1 秒取得退出状态，拼成错误 | 结束原因 | 由 `spawn` 连接器的 `Receiver` 在 EOF 时给出 |
| `process.rs` `Process` | 会话、控制操作、子进程三者合一 | 混合 | 拆成会话部分 + 子进程句柄（§十） |
| `server.rs` `serve`（冻结的反方向） | 从 argv 取 socket 路径，`UnixStream::connect` | 连接器 | unix 连接器；行为不变 |
| 生成代码 | 代理里持有 `Arc<Process>`；调用 `Process::mount` | 使用方 | D1 不改（`Process` 外观保留） |
| `runtime.rs` `CordisRuntimePlugin`（#109） | 用 `Process::mount` 的 anchor 模式拉起运行时，提供 `CordisRuntime` | 使用方 | D1 不改（`Process` 外观保留） |
| `wire.mjs` `encode` | `JSON.stringify` + `'\n'` | 编码里混进了分帧 | 编码不带换行；字节流通道写出时加 |
| `io-worker.mjs` | `createConnection(socketPath)`，或监听后拉起；`createInterface` 按行切；`JSON.parse` | 连接器 + 通道 + 分帧 + 解码 | 通用 worker + 通道模块 |
| `client.mjs` `Process` | 把 `{ executable, socketPath }` 交给 worker | 连接器参数 | 改为通道规格（§5.4） |
| `runner.mjs` | argv 为 `<socket> <插件或 anchor>` | 启动参数 | `<通道> --id <id> <…>`，裸路径保持兼容（§十） |
| `peer.mjs` | 经 `send` / `receive` / `pump` 收发，与传输无关 | 会话 | 把 `encode` 换成不带换行的版本；文件改名为 `session.mjs`，避免和节点稿的 `Peer` 混淆 |
| `rpc.rs` 的 `struct Peer` | 一个会话的内部状态 | 会话 | 改名（例如 `SessionState`），理由同上 |
| `rpc/tests.rs`、`tests/rpc_callbacks.rs` | 直接用 `UnixStream::pair()`、手写逐行 JSON，或自己 bind `UnixListener` | 测试 | 改用 memory 通道和连接器 |

会话的核心逻辑（调用表、引用表、调用链路由、`SyncWaitCycle`）不需要动，Node 的 `Peer` 也早已和传输无关。要改的集中在会话的边缘，以及 `Process` 和 `io-worker`。

## 四、分层

| 层 | 职责 | 不得知道 | Rust | Node |
| --- | --- | --- | --- | --- |
| L0 连接器 | 建立通道：拉起子节点、监听、拨号、鉴权、重连；给出对端身份 | 帧、编码 | `rutis-channel`（网络连接器在 `websocket` 特性里），由传输插件对外提供 | worker 里的连接代码，由 cordis 的传输插件对外提供 |
| L1 通道 | 有序、可靠、保持边界的双向消息流；分帧、背压、存活、结束原因 | 帧、编码 | `rutis-channel` | cordis 桥包里的 `src/channel/*.mjs`（在 worker 里运行） |
| L2 编码 | 帧 ↔ 字节（JSON） | 通道种类 | `rutis-interop` 内部 | `src/codec.mjs` |
| L3 会话 | 握手、调用、引用、取消、调用链、错误 | 通道种类、身份来源 | `rpc.rs` | `session.mjs`（现在的 `peer.mjs`） |
| L4 框架操作 | 服务公告、代装插件、事件转发（节点稿 §5.2，对称） | 通道种类 | `rutis-interop` | cordis 桥 |
| L5 桥 | 一组插件：传输、身份、链接、导出、导入、代装、事件（节点稿 §4.3） | 通道种类（传输插件除外） | rutis 的桥插件 | cordis 的桥插件 |

```text
             rutis-loader · 生成代码 · 应用
                          │
                    rutis-interop
              L5 桥 · L4 框架操作 · L3 会话 · L2 编码
                          │  只经 Channel
                    rutis-channel
              L1 通道契约与实现 · 装饰器 · L0 连接器

  JS（cordis 桥包）、Python（pydis）各自实现 L0–L5，按本文和节点稿的规范互通
```

## 五、通道契约

### 5.1 协议对通道的要求

| 要求 | 协议依赖它的地方 |
| --- | --- |
| 有序 | 双方都校验调用号单调递增（"invalid or repeated invocation identity"）；读线程按到达顺序先登记帧里授予的引用，再处理后面的 `release`（`rpc/tests.rs` 的 `admitted_call_pins_its_target_before_a_following_counted_release`） |
| 可靠且不重复 | 引用按授予次数归还：丢一条 `release` 就泄漏，重复一条就会误删 |
| 保持消息边界 | 一帧对应一条消息，通道不得拆分或合并 |
| 全双工 | 同步调用等待回复期间，还要接收并执行反向调用 |
| 结束可观察，并带原因 | 在途调用要以 `Transport` 失败，并说明对端是怎么结束的 |
| 不依赖调用方执行器 | `current_thread` 运行时可能正被一次同步调用阻塞（`rpc.rs` 的读线程和 `process.rs` 的退出状态线程都基于这条约束） |
| 有背压，不无限缓冲 | 协议本身没有流控 |
| 能在有限时间内发现失联 | 可能静默失效的介质（网络）必须自带心跳；本机 socket 靠 EOF 就够了 |

不满足这些要求的介质（UDP、不保序的消息队列），要先在通道实现里补上序号、确认和重传，才能当通道用。

### 5.2 通道对协议的承诺

- 不解析、不修改、不注入、不丢弃消息；消息是不透明的字节；
- 关闭原因是给人看的文字，协议不根据它走不同分支；
- 身份信息只经 `ChannelInfo` 提供，不写进消息流。

### 5.3 Rust 接口

```rust
// rutis-channel（示意）
pub struct Channel {
    pub sender: Box<dyn Sender>,      // 会话在自己的发送锁里使用
    pub receiver: Box<dyn Receiver>,  // 归会话的读线程所有
    pub closer: Arc<dyn Closer>,      // 任何线程、任何时候都可以调用
    pub info: ChannelInfo,
}

pub trait Sender: Send {
    /// 发送一条消息；通道施加背压时阻塞。
    /// 按值传入：字节流通道可以就地加换行，不必再复制一次。
    fn send(&mut self, message: Vec<u8>) -> Result<(), ChannelError>;
}

pub trait Receiver: Send {
    /// 阻塞到下一条消息；`Ok(None)` 表示对端正常结束。
    fn recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError>;
}

pub trait Closer: Send + Sync {
    /// 幂等；必须唤醒正阻塞在 send 和 recv 上的线程。
    fn close(&self, reason: &str);
}

pub struct ChannelInfo {
    pub transport: &'static str,    // "unix" | "fd" | "memory" | "websocket" …
    pub peer: Option<String>,       // 连接器验证过的对端节点 id；本机子节点由父节点指定
    pub label: String,              // 用在错误信息里的名字，例如 "peer mac"
}

pub enum ChannelError {
    /// 通道已结束，包括对端异常退出、心跳超时、被对端或本端关闭。
    Closed { reason: String },
    /// 消息超出上限。发送方遇到时通道不关闭；接收方遇到时通道关闭。
    TooLarge { limit: usize, size: usize },
}
```

三个对象分开，对应现在 `UnixStream` 的三个克隆（写端、读端、`closer`），会话现有的锁设计因此保持不变：

- 会话持有发送锁时完成"编码 + 发送"，引用表的变化和发送顺序保持一致；
- `close` 先在锁外打断阻塞中的发送，再拿锁清理表（`rpc.rs` 现有的写法）。

**为什么是阻塞式**：会话的同步调用会在任意线程上阻塞等待回复，包括 `current_thread` 运行时自己的线程；读线程也刻意不依赖任何运行时。如果用 async 接口，就必须指定一个执行器来推进，这正好和同步等待冲突。内部基于异步栈的实现（WebSocket、TLS）自带一个线程和运行时，对外仍然提供阻塞接口；`rutis-channel` 提供把异步实现包成阻塞接口的适配器。

**会话侧的变化**：

- 读线程循环调用 `recv()`，解码后交给现有的 `receive`。`Closed { reason }` 映射为 `Error::Transport("<label>: <reason>")`；label 为空时不加前缀，所以本机通道的错误文本和以前一样。
- 发送超限按编码失败处理：请求直接向调用方返回错误；应答改发错误帧，现在的 `respond` 已经有这条路径。

### 5.4 Node 接口

```js
// cordis 桥包 src/channel/*.mjs（示意），在 I/O worker 里运行
// open(spec, { message(bytes), closed(reason) }) → { send(bytes), close(reason) }
```

- 规格：`unix:<path>`、`fd:<n>`、`wss://…`（拨号），以及裸路径（等同 `unix:`，兼容现状）。冻结反方向用的"监听后拉起"模式保留为一个连接器。cordis 节点要接受网络连接时，WebSocket 服务端需要 `ws` 这类依赖（Node 只内置了客户端）；节点通常是拨号方，这不是必需的。
- 主线程与 worker 之间沿用现有的 MessagePort + `Atomics` 交接。同步等待时主线程被阻塞，所以通道必须在 worker 里推进。这是 Node 版的"不依赖调用方执行器"，同一个 worker 也负责应答 WebSocket 心跳。
- 编码：会话模块用 `codec.encode`（不带换行），worker 用 `codec.decode`。在 worker 里解码是现有的性能安排，可以保留；关键是通道模块只负责分帧，不决定编码。

### 5.5 其他语言

契约相同：通道在独立线程上推进，字节流按换行分帧，WebSocket 按 §九。例如 pydis 作为本机子节点时，用 `socket.socket(fileno=3)` 打开继承的 fd。

## 六、分帧与编码

- **编码（协议层）**：JSON，紧凑输出。字符串里的换行在 JSON 中已经转义，所以按行分帧是安全的。编码先做成内部接缝，不开放替换，因为现在只有 JSON 一种。
- **分帧（通道层）**：字节流通道（Unix socket、继承的 fd）按换行分帧，与 v1 线格式一致。消息型通道（WebSocket）一条消息就是一帧，不加换行。
- **以后的二进制编码**：字节流通道改用长度前缀分帧，WebSocket 改用二进制消息。编码由连接器在建立会话时告诉两端，不在帧里协商，因为 `hello` 本身必须先能解码。

## 七、连接器

| 连接器 | 产出 | 用途 |
| --- | --- | --- |
| `spawn`（继承 fd） | `Channel` + 子进程句柄；fd 3 是 socketpair 的一端；子进程退出状态作为结束原因 | 本机子节点，D2 起作为默认 |
| `spawn`（路径） | 现状：临时目录里的 socket，子进程回拨 | 保留 |
| `unix::connect` / `unix::listen` | `Channel` | 冻结的反方向、工具 |
| `memory::pair` | 两条首尾相连的 `Channel` | 测试；同一进程里的两个节点 |
| WebSocket `dial` / `listen` | `Channel` + 验证过的对端节点 id；心跳在通道内部；`dial` 负责退避重连 | 节点之间的网络链接（§九） |

**继承 fd**：

- 好处：没有文件系统路径，所以不会暴露、不会残留，也没有 `accept` 竞争；stdout 仍留给插件输出（兼容层设计 §9 的工程防护照样成立）。
- 已验证：在 Node 22.13（macOS）上，worker 线程能把继承的 fd 3 打开为 `net.Socket`，双向收发逐行 JSON；主进程的 stdout 不受影响，双方都能正常退出。
- Rust 侧：用 `UnixStream::pair()`，在 `pre_exec` 里 `dup2` 到 fd 3。只让这一个 fd 被子进程继承，其余照常 `CLOEXEC`。
- 兼容：cordis 桥包在 `package.json` 里声明支持的通道（例如 `"rutisChannels": ["unix", "fd"]`），构建时和 `rutisProtocol` 一起核对；未声明 `fd` 的旧版本走路径方式。

**结束原因**：`spawn` 连接器的 `Receiver` 在 EOF 时最多等 1 秒取得退出状态（也就是现在的 `Child::disconnected`），然后返回 `Closed { reason: "Cordis process exited with signal: 9 (SIGKILL)" }`。会话拿到的错误和现在逐字相同。D1 里由 `Channel::with_end_reason` 实现：`Process::mount` 把它包在 Unix 通道外面。

**在框架里怎么用**：连接器不被业务代码直接调用，而是由传输插件包成 `Transport#<种类>` 服务，链接插件依赖它（节点稿 §4.3）：

- `rutis-bridge/local` 包装 `spawn`，`rutis-bridge/websocket` 包装 `dial` 和 `listen`，测试用的 `rutis-bridge/memory` 包装 `memory::pair`。加一种传输，就是多一个传输插件，链接插件和协议层都不用改。
- 监听由传输插件持有。验证身份后，传输插件把连接交给对应对端的链接插件；没有为这个对端配置链接插件的，直接拒绝。
- 卸掉一个传输插件，依赖它的链接按原生门控停下。

## 八、装饰器

- `limit(n)`：消息大小上限；
- `trace`：逐条记录消息，默认关闭，调试用；
- `fault`（仅测试）：延迟、丢弃后关闭、半开（停止转发但不关闭），用来测心跳和故障语义。

心跳不做成装饰器：它需要带外的控制消息，只能放在本身就有这种消息的传输里（WebSocket 的 ping/pong）。

一条连接只承载一个会话，所以不做复用；树形组合在框架层完成（节点稿 §4.5），也不需要帧级中继。

## 九、WebSocket 绑定（跨语言规范）

各语言的网络栈按这一节互通。

| 项 | 规定 |
| --- | --- |
| 地址 | 监听方配置，例如 `wss://main.example.com/rutis` |
| 子协议 | `rutis.<会话协议主版本>`，v2 即 `rutis.2`；不符时在升级握手阶段拒绝 |
| TLS | 非回环地址必须是 `wss`；可以由前置反向代理终止 TLS，监听方只听回环地址。拨号方校验对方证书（系统根证书或配置的 CA） |
| 鉴权 | 升级请求带 `Authorization: Bearer <token>`，或者用客户端证书。监听方经身份插件把凭据映射成对端节点 id，放进 `ChannelInfo.peer`。拨号方按配置知道自己连的是哪个节点，握手时核对对方 `hello` 里的节点 id |
| 消息 | 一帧一条文本消息（UTF-8 JSON，不带换行）；二进制消息留给以后的二进制编码 |
| 大小上限 | 默认 16 MiB，可配置；收到超限消息时以关闭码 1009 关闭 |
| 心跳 | 双方都发 ping，默认每 10 秒一次；30 秒内收不到任何消息即判定失联、关闭连接。同时打开 TCP keepalive |
| 关闭 | 关闭原因写进 close frame 的 reason（UTF-8，不超过 123 字节）。有序关闭用 1001；同一节点 id 的新连接接管旧连接时，旧连接以 4002 关闭，reason 为 "replaced by a new connection" |
| 重连 | 由拨号方负责：指数退避，从 0.5 秒开始，上限 30 秒，±20% 抖动，连接稳定 60 秒后重置。凭据被拒时按上限间隔重试并持续报错；子协议不符时停止重试 |

谁拨号由部署决定，通常是工作节点拨主节点（方便穿过 NAT）。拨号方向和链接两端的权限无关，权限只看两端各自为对方装了哪些功能插件（节点稿 §六）。

## 十、本机子节点：`Session` 与 `Process`

- **会话部分**（`rutis-interop` 的 `Session`）= `Connection` + 框架操作，可以建立在任意 `Channel` 上。
- **`Process`** = `spawn` 连接器给出的子进程句柄 + `Session`。保留现有的全部构造函数和方法作为外观，生成代码和现有测试都不用改。
- **启动参数**：`<程序> <通道> --id <id> <插件或 project>`。`<通道>` 是 `fd:3`、`unix:/path`，或者裸路径（等同 `unix:`，兼容现状）；`--id` 是父节点给子节点指定的节点 id（节点稿 §5.1）。
- 在节点模型里，本机子节点由 `rutis-bridge/local` 传输插件拉起；`Process` 是 D1 期间保留的外观，等生成代码改为建立在桥插件上（节点稿 N2）后，就不再需要它。
- 冻结的反方向（`server.rs`、`client.mjs` 的 `launch`）换用 unix 连接器，行为不变，也不增加能力。

## 十一、测试

- **通道契约测试**（`rutis-channel`，每种实现都要跑）：
  - 多线程并发发送时，消息仍按发送顺序到达；
  - 消息边界保持，包括接近上限的大消息；
  - 关闭能唤醒阻塞中的 send 和 recv；
  - 关闭原因能传到对端（WebSocket）；
  - 有背压；关闭是幂等的；
  - 在一个被同步调用阻塞的 `current_thread` 运行时里，发送和关闭都能完成。
- **会话矩阵**：
  - `rpc/tests.rs` 改为在 memory 通道上运行；
  - 互操作集成测试（真实的 Node 进程）按连接器参数化：路径方式（现状）、继承 fd、回环 WebSocket。
- **WebSocket 跨实现测试**：Rust 监听 ↔ JS 拨号，JS 监听 ↔ Rust 拨号；覆盖鉴权失败、子协议不符、心跳超时（用 `fault` 装饰器制造半开连接）、接管。
- **Node**：`peer` 的测试本来就用内存 harness；另外补上字节流通道（路径和 fd）的分帧测试。
- **性能**：用现有的 bench fixture 对比 D1 前后的 Unix 通道。退化超出噪声范围就回头修改设计。

## 十二、兼容与版本

- D1、D2 都不改线格式：Unix 和继承 fd 的字节流上仍是逐行 JSON，`rutisProtocol` 仍为 1。启动参数兼容裸路径；fd 方式按 §七 的声明启用。
- WebSocket 子协议跟着会话协议的主版本走（`rutis.2`）。通道层自身的变化不影响会话协议，反之亦然。

## 十三、分阶段

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| D1（已实现） | 新建 `rutis-channel`（契约、Unix 字节流、memory）；`rpc.rs` 改用 `Channel`；从 `Process` 拆出会话部分；会话层内部的 `Peer`（`rpc.rs` 的 struct、`peer.mjs`）改名；Node 侧编码去掉换行，worker 的通道代码模块化（路径方式） | 现有测试全部通过；线格式不变；性能不退化；`rpc.rs`、`protocol.rs` 去掉 `cfg(unix)` 后能编译 |
| D2 | 继承 fd 的 `spawn`（Rust 与 Node）；装饰器；通道契约测试；会话矩阵 | 契约测试与会话矩阵在所有本地通道上通过 |
| D3 | WebSocket 绑定的 Rust 实现（`rutis-channel` 的 `websocket` 特性）与 JS 实现（cordis 桥包）；`dial` / `listen` 连接器（由节点稿的 WebSocket 传输插件对外提供） | WebSocket 跨实现测试通过；会话矩阵在回环 WebSocket 上通过 |

节点稿的 N 阶段建立在这些阶段之上。

## 十四、不采用的方案

| 方案 | 不采用的原因 |
| --- | --- |
| socketpair 桥接（远程稿第一版） | 会话层仍绑定 Unix 字节流和换行分帧；每种新传输都得伪装成 Unix 流，在泵里再分一次帧；会话层永远离不开 Unix |
| 控制消息和协议帧共用连接，按顶层字段分流（同上） | 通道必须解析载荷，编码也被锁死成 JSON |
| 远端代理 + 帧级中继 + 复用（远程稿第二版） | 被节点模型取代：远端本身就是一个框架节点，需要转接时在框架层做 |
| 异步通道接口（async trait） | 会话的同步调用要能在任意线程上阻塞，读线程不能依赖调用方的运行时 |
| 现在就开放可插拔编码 | 只有 JSON 一种；先留内部接缝，有第二种时再开放 |
| 把心跳做进会话协议（ping 帧） | 存活是介质的属性：本机 socket 不需要心跳，网络传输自带；放进协议，等于让每种实现都去做 |
| 用 stdio 做本机通道 | 插件会往 stdout 打印，帧流会被破坏；继承 fd 3 同样不占用路径，又不和 stdout 冲突 |
| 把会话做成泛型（`Connection<C: Channel>`） | 类型参数会扩散到生成代码和所有代理类型里；trait 对象每帧多一次间接调用，开销可以忽略 |

## 十五、与其他文档的关系

- [兼容层设计](design-protocol-plugin-mount.md)：§3 的帧语义由节点稿改成对称；§9 的"帧走独立 socket"改由通道层保证。
- [节点稿](design-remote-plugins-2026-10-03.md)：会话层对称化、框架操作、桥、策略、远程插件的用法。
- 需求 §3 的约束不变：rutis 内核和 Cordis 都不改。
- [多语言决策记录](decision-multilang-2026-10-03.md)（#107）把"启动方式抽象"列为不做，理由是它只为其他语言铺路。本文要做协议与通道解耦，理由是远程 peer 要走网络，与其他语言无关；这项修订列在节点稿 §十四，需要评审确认。
