# rutis 挂载 Cordis 插件：兼容层设计

依据：[需求](requirements-protocol-plugins.md)。基准：Cordis `4.0.1`（`interop/node/package-lock.json` 锁定）。交付计划见[路线图](roadmap-native-plugin-mount.md)。

**原 TS 插件在真实 Cordis 中运行；rutis 侧拿到的是构建时生成的 Rust 类型，以普通 rutis 服务的形式注册。所有映射都在兼容层完成，只使用 rutis 和 Cordis 的公开 API。**

## 1. 组成

```text
rutis 应用进程                                   Node 进程
+------------------------------+                +---------------------------+
| ctx.plugin(bindings::Plugin) |                | runner.mjs                |
|   Projection                 |                |   真实 Cordis Context     |
|   -> provide_mut_as / 替换   |  Unix socket   |   原 TS 插件              |
|   Process + rpc::Connection  | <============> |   Peer（client.mjs）      |
|                              |  逐行 JSON 帧  |   I/O Worker              |
+------------------------------+                +---------------------------+
```

| 部件 | 位置 | 职责 |
| --- | --- | --- |
| 生成器 | `interop/node/src/generate.mjs`，由 `rutis_interop::build::cordis_plugin` 在 `build.rs` 中调用 | 用 TypeScript 检查器读取原插件，生成 Rust 的 Config、服务代理类型和挂载插件 |
| 挂载插件 | 生成代码 | 启动 Node 进程、注册清理 effect、把服务交给 `Projection` 发布 |
| `Projection` | `crates/rutis-interop/src/projection.rs` | 把 Cordis 服务槽位的变化映射为 rutis 服务的注册、替换和撤销 |
| `Process` / `rpc` | `crates/rutis-interop/src/{process,rpc,protocol}.rs` | 进程管理、线协议、调用与引用表 |
| runner / Peer | `interop/node/src/{runner,client,peer,io-worker,errors}.mjs` | 加载原插件，跟踪服务槽位，执行调用 |

rutis 内核和 Cordis 都没有为兼容做任何修改。

## 2. 构建期生成

`build.rs` 调用 `rutis_interop::build::cordis_plugin(插件路径, interop/node)`，生成结果写入 `OUT_DIR/cordis.rs`，由应用 `include!`。生成器读取的内容：

| 来源 | 生成 |
| --- | --- |
| `apply(ctx, config)` 的 config 类型 | `Config` 结构体（必填字段） |
| `ctx.provide('名字', 值)`（名字为字面量，调用解析到 Cordis） | 每个服务一个 Rust 代理类型，名字首字母大写 |
| 服务的公开方法 | 同名 snake_case 方法；同步方法仍同步，返回 Promise 的方法生成 `async fn`；均返回 `Result<T, rutis_interop::Error>` |

目前支持的类型：`number` / `string` / `boolean` / `void` 及其数组。以下情况在构建时报错并指出源码位置，不会静默丢掉成员：公开属性、重载、泛型方法、可选 / 默认 / 剩余参数、其他类型。生成器把原插件导入的文件都登记为 Cargo 的重新构建依赖。

## 3. 线协议

一个双向 Unix socket，逐行 JSON，协议版本 1。

| 帧 | 含义 |
| --- | --- |
| `hello { version }` | 握手；完成前不接受其他帧 |
| `invoke { id, path, target, method, args }` | 调用服务方法（`target` 为句柄）或控制操作（`target` 为空） |
| `call { id, path, reference, args }` | 调用对端传来的函数引用 |
| `await { id, path, reference }` | 等待对端传来的异步结果 |
| `return { id, value }` / `throw { id, error }` | 返回值 / 错误 |
| `release { reference, count }` | 归还引用的授予次数 |

- **值**：`undefined`、JSON 数据、列表、引用（函数或异步结果）。函数和 Promise / Future 以引用传递，保留身份，不序列化成快照。
- **引用计数**：发送方每授予一次加一；接收方收到时先登记持有再排队执行，本地代理全部释放后按收到的次数 `release`。旧的 release 与新的授予交叉时不会误删。会话关闭时整表清空，不等待 GC。
- **错误**：以对象图传输，保留 `name`、`message`、`cause`、AggregateError 的 `errors` 及其顺序、共享和循环引用。
- **调用链**：调用号为 `rust:n` / `node:n`，`path` 记录嵌套调用链，用于把反向调用交给正在同步等待的那一方执行。

### 3.1 同步与异步

- 生成的同步方法在调用线程上同步等待结果。等待期间，属于同一调用链的反向调用（例如 JS 同步调用 Rust 回调）在等待线程上执行，所以嵌套同步回调可以正常工作。
- 异步方法返回 Future；Node 侧返回 Promise。调用与等待是两个操作：`invoke` 返回的异步结果以引用形式传回，需要时再 `await`。
- **等待环**：如果同步调用链需要的结果只能由被它自己阻塞的执行器推进（Node 主线程上的定时器 / Promise，或 Rust `current_thread` 运行时上的 Future），直接返回 `SyncWaitCycle` 错误，不会卡死。
- 适配器可以用 `Connection::independent_future` 把确认与调用方执行器无关的异步工作放到每连接一个的后台执行器上。`Send` 不代表可以迁移，只有显式声明的工作才会放过去。

## 4. 服务投影

### 4.1 句柄

Node 侧为每个导出的服务槽位维护一串**对象句柄**：

- 槽位第一次出现的对象，句柄就是服务名（例如 `counter`）；之后每换一个对象，分配新句柄（`counter#2`、`counter#3`……）。
- 句柄永远指向创建时的那个对象。方法调用按句柄定位对象，不会重新读取槽位。
- 槽位变化时，Node 侧通过控制调用 `service(name, handle | null, version)` 通知 Rust；`version` 保证旧通知不会覆盖新通知。

槽位变化的检测只用 Cordis 公开入口：

| 变化 | 检测方式 |
| --- | --- |
| `ctx.provide` 发布、撤销，提供者进入 / 离开 ACTIVE | `internal/service` 事件 |
| 属性赋值 `ctx.counter = x` | `internal/set` 钩子，在 `next()` 之后重新读取 |
| 直接 `ctx.set('counter', x)` | Cordis 不发任何通知；兼容层在每次调用本进程的服务方法后重新读取槽位（边界规则 1） |

### 4.2 rutis 侧

`Projection` 只用 rutis 公开 API：

| 槽位状态 | rutis 操作 |
| --- | --- |
| 第一次可用 | `ctx.provide_mut_as` 注册代理，保留返回的 `ServiceWriter` |
| 换成新对象 | `ServiceWriter::set` 替换；已经取得的 `Arc` 快照仍指向旧对象 |
| 变为不可用 | 撤销注册；依赖它的 rutis 插件按原生依赖规则停止 |
| 再次可用 | 重新注册，依赖方按原生规则重新启动 |

通知在同步调用链内到达时，会在调用返回前处理，所以 `counter.swap(2)` 返回后，`ctx.get::<Counter>()` 已经是新代理。异步路径下允许短暂读到旧代理（需求 §4）。

发布在锁外执行，重入或并发的变化交给正在进行的发布循环处理。

### 4.3 句柄回收

每个生成的代理对应一个句柄。代理的最后一个 `Arc` 被释放时，发送 `release(handle)`；Node 侧只在句柄既被释放、又不再是槽位当前对象时才删除它。还没来得及生成代理就被替换掉的句柄由 `Projection` 直接释放。

## 5. 生命周期与故障

- **启动**：挂载插件启动 Node 进程，握手后发 `mount`；Node 侧加载原插件并等待 `fiber.await()`。原插件启动失败时挂载失败，不注册任何服务；服务暂不可用时挂载成功但不注册，依赖方保持等待。
- **清理顺序**：挂载插件先注册自己的清理 effect，再注册服务。rutis 逆序清理，所以先撤销服务、执行消费者的 disposer（此时仍可调用远端服务），最后才关闭 Node 进程。
- **先清理后排空**：`dispose` 同时启动 Cordis 插件卸载和在途调用排空，不先等调用结束；disposer 可能正是解除在途等待的动作。
- **故障**：Node 进程退出或连接断开时，所有在途调用和后续调用都返回 `Transport` 错误。不重试，不返回默认值，已发送但未返回的调用视为结果未知。

## 6. 事件（待实现）

跨边界事件按组转发，只用两侧的公开 API，不改 rutis 内核。

**事件声明**：生成器读取原插件对 Cordis `Events` 接口的声明，为每个事件生成 Rust 事件类型（实现 rutis `Event`，`NAME` 为事件名）；载荷目前只支持数据类型。

**Cordis → rutis**：原插件加载完成后，runner 为每个转发事件在 Cordis 中登记一个转发监听。事件的签名决定转发方式，不去猜调用方用的是哪种分发：

| 事件签名 | 转发监听的行为 | 在 rutis 侧 |
| --- | --- | --- |
| 返回 `void` / `Promise<void>` | 返回一个 Promise，在 rutis 侧处理完成后 resolve。`emit` 会忽略它（发出即忘），`parallel` / `serial` 会等待它 | `parallel` |
| 同步返回值 | 同步调用 Rust，返回结果；用于 `bail` | `bail_sync`，`Some(v)` 转成短路值，`None` 转成 `undefined` |
| 返回 `Promise<值>` | 异步调用 Rust；用于 `serial` | `serial` |

**rutis → Cordis**：挂载插件为每个转发事件登记一个 rutis 监听，收到后调用 Node 侧，以相同签名规则在 Cordis 中分发。

**防止回环**：两侧的转发监听都忽略由对方转发器自己发出的事件。

**顺序**：转发监听在原插件 `apply` 完成后登记，所以 Cordis 侧顺序为：apply 期间登记的监听 → rutis 一组 → 之后登记的监听。组内按 rutis 原生顺序执行。

**不转发**：`internal/*` 事件；waterfall 暂不转发，声明时在构建期报错。

**默认只转发纯 `emit` 事件**：`rutis-cordis` 在 dsh 上实测过，被动订阅一个 waterfall 事件（例如 `agent/request`）会让链在转发监听处中断，因为不调用 `next()` 的监听会否决整条链。事件是否转发由应用显式列出，列表只收已确认为 `emit` 的事件；其他分发方式需要签名能证明转发监听的行为与原链相容。

## 7. 反方向：Cordis 应用挂载 rutis 插件（冻结）

"Cordis / dsh 宿主使用 Rust"由已在用的 `rutis-cordis` + `host/` 负责。这里的实现保留代码和测试，不再增加能力：

- `rutis_interop::build::rutis_plugin` 用 syn 解析单个入口源码，生成 Rust 导出分发、Cordis 挂载插件（`rutis.mjs`）和 Context 类型声明（`rutis.d.mts`）。
- 只支持单文件中的公开具体插件、`&self` 方法和基本类型；遇到模块声明、宏、泛型、借用、公开字段时报错。
- 导出对象在挂载时捕获，不跟随换值。

## 8. 工程防护（待补）

以下做法已在 `rutis-cordis` 中验证，按路线图 W2 移植：调用超时与取消传播、迟到应答计数丢弃、握手时的能力协商与装载期缺失检查、断连时的在途调用记录。帧已经走独立 socket，插件向 stdout 打印不会破坏帧流。

## 9. 做不到或不支持的部分

| 项目 | 原因 | 处理 |
| --- | --- | --- |
| 直接 `ctx.set` 换值的即时可见 | Cordis 不产生任何通知 | 下一次调用后可见（边界规则 1） |
| 同步调用中等待需要被阻塞线程推进的结果 | 单线程事件循环 / `current_thread` 无法重入 | 返回 `SyncWaitCycle`（边界规则 6） |
| Cordis `emit` 返回时 rutis 监听已执行完 | 跨进程的语言栈差异 | 采用发出即忘（边界规则 2） |
| 跨侧逐个交错的监听顺序 | 两侧各有一张监听表 | 按组转发（边界规则 3、4） |
| 返回值含义不同的同名事件契约 | 框架契约不同 | 按签名显式转换，无法转换时报不兼容 |
| 未支持的类型（对象、回调参数等） | 生成器尚未实现 | 构建期报错，列入路线图 |

以下不再建设：共享内存版本页、提交边界审计、跨框架统一事件队列（[#74](https://github.com/arcships/rutis/pull/74)，已停止推进）、从 Rust 源码自动提取完整接口的 rustdoc / 编译器方案、分布式 GC。
