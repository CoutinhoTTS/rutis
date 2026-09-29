# rutis 挂载 Cordis 插件：兼容层设计

依据：[需求](requirements-protocol-plugins.md)。基准：Cordis `4.0.4`（`interop/node/package-lock.json` 锁定）。交付计划见[路线图](roadmap-native-plugin-mount.md)。

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
| runner / Peer | `interop/node/src/{runner,client,peer,io-worker,errors}.mjs` | 按顺序加载一个或一组原插件，跟踪服务槽位，执行调用 |

rutis 内核和 Cordis 都没有为兼容做任何修改。

## 2. 构建期生成

`build.rs` 调用 `rutis_interop::build::cordis_module(插件, interop/node, 模块名)`，生成结果写入 `OUT_DIR/{模块名}.rs`，由应用 `include!`（`cordis_plugin` 是模块名为 `cordis` 的简写）。插件可以是 TypeScript 源文件，也可以是已安装的 npm 包目录；包按 `package.json` 的 `types` 分析，按运行时入口加载。

**组合挂载**：已发布的插件通常设计成组合使用，例如 `dsh-workspace` 依赖 `dsh-storage`、`dsh-storage-domain` 和一个会话持久化实现提供的服务。`cordis_group(模块名, &[(名字, 插件), ...], interop/node)` 为一组插件生成一份绑定：这组插件按给定顺序装进同一个 Node 进程的同一个 Cordis Context，彼此的依赖按 Cordis 原生规则解析；组内所有插件的服务都导出到 rutis，同名服务在构建时报错；每个插件的配置是组合 `Config` 的一个字段，字段名即给定的名字。

**服务发现**

| 插件形式 | 服务来源 |
| --- | --- |
| 默认导出 `Service` 子类（已发布插件的常见形式） | `declare module '@deepseek-ai/cordis' { interface Context { ... } }` 中类型为该类或其基类的成员；接口层声明 `credentials: CredentialProvider`，实现包继承它即可被发现 |
| 导出 `apply` 的函数插件，TypeScript 源码 | 字面量服务名的 `ctx.provide('名字', 值)`，类型优先取 Context 声明 |
| 导出 `apply` 的函数插件，只有声明文件 | 包自己声明的 Context 成员 |

配置类型取 `Service` 子类构造函数或 `apply` 的第二个参数；全部字段可选时 `Config` 实现 `Default`。

**类型映射**

| TypeScript | Rust |
| --- | --- |
| `number` / `string` / `boolean` / `void` | `f64` / `String` / `bool` / `()` |
| 品牌类型 `string & { __brand }`（带别名） | 同名 newtype，`#[serde(transparent)]`，可 `From<&str>` |
| 字符串字面量联合 | 同名枚举，变体按原字符串重命名 |
| 数据接口 / 对象字面量 | 同名结构体，字段 snake_case，按原名序列化；可选字段为 `Option` 且不序列化 `None` |
| `T \| undefined` / `T \| null` | `Option<T>` |
| 数组 / 只读数组 | `Vec<T>`；参数位置借用为 `&[T]` |
| `Record<string, T>` | `BTreeMap<String, T>` |
| `any` / `unknown`，以及其他联合（如带判别字段的对象联合） | `serde_json::Value`：数据原样过线，只是没有生成静态结构 |

方法：同步方法仍同步，返回 Promise 的生成 `async fn`，都返回 `Result<T, rutis_interop::Error>`。可选参数为 `Option<T>`，`None` 以 JS `undefined` 传递（不是 `null`）。可省略的 `AbortSignal` 参数和选项字段暂不暴露，调用时不带取消信号。

**不支持的成员**：属性、重载、泛型方法、回调参数、返回函数、活对象（带方法的对象或类实例）、`Uint8Array`、流等，不生成对应方法。每一项都在构建时以 `cargo:warning` 报出源码位置和原因，并列在服务类型的文档注释里；插件的其他成员照常生成，不会因为一个成员而整体失败。

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

**读取方式**：runner 为每个导出服务装载一个导出 fiber，它像普通原生消费者一样 `inject` 这个服务，并在自己的作用域里读取。

- 服务是否可用由 Cordis 原生门控决定，包括提供者状态和 `Service.check()`；`check` 不通过时不导出。
- 服务方法通过调用方 Context 创建的 effect（例如 `this.ctx.effect(...)`）属于导出 fiber，卸载时随它一起清理。
- Cordis 每次读取 `Service` 实例都会新建一个追踪代理，所以判断是否换值时，先用 `Symbol.for('cordis.original')` 取出原对象再比较；只读调用不会产生新句柄。

槽位变化的检测只用 Cordis 公开入口：

| 变化 | 检测方式 |
| --- | --- |
| `ctx.provide` 发布、撤销，提供者进入 / 离开 ACTIVE | `internal/service` 事件 |
| 属性赋值 `ctx.counter = x` | `internal/set` 钩子，在 `next()` 之后重新读取 |
| 直接 `ctx.set('counter', x)` | Cordis 不发任何通知；兼容层在每次调用本进程的服务方法后重新读取槽位，方法抛错时同样刷新（边界规则 1） |

### 4.2 rutis 侧

`Projection` 只用 rutis 公开 API：

| 槽位状态 | rutis 操作 |
| --- | --- |
| 第一次可用 | `ctx.provide_mut_as` 注册代理，保留返回的 `ServiceWriter` |
| 换成新对象 | `ServiceWriter::set` 替换；已经取得的 `Arc` 快照仍指向旧对象 |
| 变为不可用 | 撤销注册；依赖它的 rutis 插件按原生依赖规则停止 |
| 再次可用 | 重新注册，依赖方按原生规则重新启动 |

通知在同步调用链内到达时，会在调用返回前处理，所以 `counter.swap(2)` 返回后，`ctx.get::<Counter>()` 已经是新代理。异步路径下允许短暂读到旧代理（需求 §4）。

发布规则：

- 发布在锁外执行，重入或并发的变化交给正在进行的发布循环处理。
- 撤销完成之后才允许重新注册，避免"撤销后立刻再提供"时与尚未撤掉的旧注册冲突。
- 发布失败不记为已应用，槽位保持待发布状态，下一次变化时重试。
- 正在发布中的句柄不会因为重入的换值被提前释放。

### 4.3 句柄回收

每个生成的代理对应一个句柄。代理的最后一个 `Arc` 被释放时，发送 `release(handle)`；Node 侧只在句柄既被释放、又不再是槽位当前对象时才删除它。还没来得及生成代理就被替换掉的句柄由 `Projection` 直接释放。

挂载插件卸载时，`Projection::close()` 丢弃持有 `ServiceWriter` 的发布闭包，断开 `Process → Projection → ServiceWriter → 代理 → Process` 的引用环，使 `Process` 能被回收。

## 5. 宿主向 Cordis 插件提供服务（待实现）

被挂载的 Cordis 插件可以依赖 rutis 应用提供的服务，例如宿主把 aimux-llm 作为 `llm` 交给 dsh 插件。这与 §4 方向相反，但仍是 rutis 应用做宿主，不属于 §8 冻结的反方向。

**声明与类型**：应用在生成绑定时列出由 rutis 提供的服务名。服务的接口取自插件自己的 Context 声明（插件声明了 `llm: LlmRuntime`，接口就是 `LlmRuntime`）。生成器从这个 TS 接口生成：

| 生成物 | 作用 |
| --- | --- |
| Rust trait（例如 `LlmRuntimeHost`） | 应用实现它；同步方法为 `fn`，返回 Promise 的方法为 `async fn`；类型映射同 §2 |
| 分发代码 | 把来自 Node 的调用路由到应用注册的 trait 对象 |
| 挂载插件的依赖声明 | 挂载插件 `injects` 这些服务；rutis 侧未就绪时挂载插件按原生规则等待 |

不支持的成员沿用 §2 的规则：构建时警告，不生成 trait 方法；Node 侧调用这些成员时明确报错。

**Node 侧**：runner 在装载插件组之前，用 `ctx.provide(名字, 代理)` 注册代理对象，代理的方法经协议调用 Rust；Cordis 的依赖按原生规则解析。代理不是插件声明的那个类的实例（边界规则 7）。

**变化与清理**：

- rutis 侧服务撤销或换成新实例时，挂载插件作为依赖方按 rutis 原生规则停止或重启，Node 进程随之关闭或重建。第一版不做单个服务的就地替换。
- 清理顺序由两边的原生依赖自然保证：rutis 侧的提供者在它的依赖方（挂载插件）清理完之后才撤掉；挂载插件清理时先卸载 Cordis 插件组，再撤掉 Node 侧的代理，所以 Cordis 插件的 disposer 仍能调用 rutis 服务。
- JS 同步调用 rutis 服务时，Node 主线程同步等待 Rust 返回，沿用 §3.1 的同步规则和 `SyncWaitCycle` 检测。

## 6. 生命周期与故障

- **启动**：挂载插件启动 Node 进程，握手后发 `mount`；Node 侧加载原插件并等待 `fiber.await()`。runner 使用插件自己解析到的 Cordis（插件的 `Service` 子类与 `Context` 必须来自同一模块实例），插件入口可以导出 `apply`，也可以默认导出 `Service` 子类。原插件启动失败时挂载失败，不注册任何服务。一个 Node 进程只运行这次挂载的插件（单个或一组），rutis 侧也暂不能为它们提供服务，所以组内满足不了的必需依赖永远不会到位：挂载直接失败，错误列出是哪个插件缺哪些服务。挂载之后服务变为不可用（例如被插件自己撤销）时，按 §4.2 撤销注册。
- **清理顺序**：挂载插件先注册自己的清理 effect，再注册服务。rutis 逆序清理，所以先撤销服务、执行消费者的 disposer（此时仍可调用远端服务），最后才关闭 Node 进程。
- **先清理后排空**：`dispose` 同时启动 Cordis 插件卸载和在途调用排空，不先等调用结束；disposer 可能正是解除在途等待的动作。
- **故障**：Node 进程退出或连接断开时，所有在途调用和后续调用都返回 `Transport` 错误。不重试，不返回默认值，已发送但未返回的调用视为结果未知。

## 7. 事件（待实现）

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

## 8. 反方向：Cordis 应用挂载 rutis 插件（冻结）

"Cordis / dsh 宿主使用 Rust"由已在用的 `rutis-cordis` + `host/` 负责。这里的实现保留代码和测试，不再增加能力：

- `rutis_interop::build::rutis_plugin` 用 syn 解析单个入口源码，生成 Rust 导出分发、Cordis 挂载插件（`rutis.mjs`）和 Context 类型声明（`rutis.d.mts`）。
- 只支持单文件中的公开具体插件、`&self` 方法和基本类型；遇到模块声明、宏、泛型、借用、公开字段时报错。
- 导出对象在挂载时捕获，不跟随换值。

## 9. 工程防护（待补）

以下做法已在 `rutis-cordis` 中验证，按路线图 W2 移植：调用超时与取消传播、迟到应答计数丢弃、握手时的能力协商与装载期缺失检查、断连时的在途调用记录。帧已经走独立 socket，插件向 stdout 打印不会破坏帧流。

## 10. 做不到或不支持的部分

| 项目 | 原因 | 处理 |
| --- | --- | --- |
| 直接 `ctx.set` 换值的即时可见 | Cordis 不产生任何通知 | 下一次调用后可见（边界规则 1） |
| 宿主提供的服务在 Cordis 侧的 `instanceof` 判断 | 代理对象不是插件声明的类的实例 | 按接口调用一致（边界规则 7） |
| 同步调用中等待需要被阻塞线程推进的结果 | 单线程事件循环 / `current_thread` 无法重入 | 返回 `SyncWaitCycle`（边界规则 6） |
| Cordis `emit` 返回时 rutis 监听已执行完 | 跨进程的语言栈差异 | 采用发出即忘（边界规则 2） |
| 跨侧逐个交错的监听顺序 | 两侧各有一张监听表 | 按组转发（边界规则 3、4） |
| 返回值含义不同的同名事件契约 | 框架契约不同 | 按签名显式转换，无法转换时报不兼容 |
| 未支持的成员（回调参数、活对象、属性、二进制、流等） | 生成器或协议尚未实现 | 构建期警告并跳过该成员，其余成员照常生成；列入路线图 |

以下不再建设：共享内存版本页、提交边界审计、跨框架统一事件队列（[#74](https://github.com/arcships/rutis/pull/74)，已停止推进）、从 Rust 源码自动提取完整接口的 rustdoc / 编译器方案、分布式 GC。
