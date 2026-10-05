# 远程插件：运行时接入与节点互联

状态：远程能力设计稿，未实现。更新：2026-10-05。

配套规范：[网络栈](design-protocol-channel-decoupling-2026-10-03.md)、[兼容层](design-protocol-plugin-mount.md)、[loader](design-rutis-loader-2026-10-02.md)。多语言基线：[总体设计 #121](https://github.com/arcships/rutis/pull/121)、[M1 设计 #127](https://github.com/arcships/rutis/pull/127)、[M1a #129](https://github.com/arcships/rutis/pull/129)、[M1b #128](https://github.com/arcships/rutis/pull/128)、[M1c #130](https://github.com/arcships/rutis/pull/130)、[M2 #131](https://github.com/arcships/rutis/pull/131)。PR 中的本机运行时实现不代表已支持网络部署。

## 1. 端点模型

运行位置与管理归属独立：承载决定本机或远程，上层接入方式决定插件由谁管理。

| 端点 | 管理归属 | 执行端要求 |
| --- | --- | --- |
| 语言运行时 | 控制方 rutis + loader 管理行、依赖、配置和生命周期 | 加载、执行、调用、清理；叶子运行时不需要 rutis 或本地依赖图 |
| 完整框架节点 | 各节点管理自己的插件；对端可通过 host 代装 | 完整 rutis、Cordis 或兼容框架，按需安装桥插件 |

```text
本地 rutis + loader
  ├─ 运行时接入 ─ link / Session ─ 承载 ─ Python / Node runtime
  └─ 节点桥    ─ link / Session ─ 承载 ─ rutis / Cordis 节点
                                               └─ 当地语言运行时
```

- Node 运行时保留完整 Cordis，用于 npm 插件及其内部依赖；Python 使用叶子 SDK。Node 可作为受控运行时或独立节点，角色由装配决定，不由语言决定。
- 本仓库维护 rutis、Cordis 接入与 Python 叶子运行时；不要求为 Python 实现完整框架。
- 框架内核、loader 核心不改。JS 运行在真实 Node.js 与 Cordis，不嵌入 JS 引擎。
- 不包含代码分发、插件市场、部署平台、进程自动重启、会话恢复、失联宽限期、操作系统沙箱和独立强制终止。

## 2. 模块与插件

| 模块 | 职责 | 所属 |
| --- | --- | --- |
| Channel / 连接器 | 单次连接、分帧、背压、承载鉴权、存活、关闭原因 | 契约归 `rutis-channel`；具体实现归各 `rutis-transport-*` crate；各语言提供对应实现 |
| Session / codec | 握手、调用、引用、取消、调用链、错误、编码 | Rust `rutis-interop`；各语言对应实现 |
| 承载插件 | 配置与持有连接器、监听器、本机子进程 | `rutis-transport-local` / `rutis-transport-websocket` / `rutis-transport-memory` / … |
| 身份插件 | 凭据、验证规则、端点身份映射 | `rutis-bridge`：identity |
| 链接插件 | 连接与会话生命周期、身份校验、重连、旧会话替换、会话就绪 | `rutis-bridge`：link |
| 运行时接入插件 | 运行时契约、服务租用与投影 | `rutis-interop`：RuntimePlugin；`rutis-bridge`：Peer 到运行时的接入 Adapter |
| 节点功能插件 | 导入、导出、代装、事件 | `rutis-bridge`：import / export / host / events |
| 行集成插件与解析器 | 描述查询、依赖解析、刷新、行就绪、行装卸 | `rutis-loader` |
| 组合插件 | 装配已有插件和配套实现 | 不涉及 loader 的组合归 `rutis-bridge`；含行集成的组合归 `rutis-loader` |

- 承载、身份、link 和节点桥使用 `rutis-bridge/` 前缀；运行时保留多语言设计的 Runtime 命名。
- Channel、Session、codec 是机制库，不单独插件化。
- WebSocket 是承载协议的一种，不是固定架构层。新增承载只增加 Adapter 与承载插件，不修改会话、运行时契约或节点功能插件。
- `rutis-interop`、`rutis-bridge` 不依赖 `rutis-loader`。link 不查询 schema、不刷新行、不提供 RuntimeRows 或 PeerRows。
- 叶子执行端实现所需的承载、会话和运行时处理器，不要求实现完整插件框架或节点桥插件组。叶子侧没有 link，相应职责在既有叶子 SDK 内补齐：
  - 已有（`interop/python/rutis_runtime`）：本机会话、`rows.*` / `hosts.*` 运行时处理器、断链后清理全部行并退出；进程由父进程以 `<socket> <project>` 拉起，回拨父进程 Unix socket，一个进程只服务一个会话。
  - 网络接入新增：监听与承载鉴权（配置控制方身份及验证规则，只接受验证通过的控制方连接）；进程常驻，依次服务多个控制会话，同一时刻只有一个活动会话，接管顺序见 §4.4；断链清理租约后继续监听，不退出。
  - 网络承载下叶子只监听，不持有重连退避，重连由控制方 link 负责；本机子进程仍按既有方式回拨或使用继承 fd。

### crate 归属与依赖

全部在 rutis 仓库内开发、联调与测试，以独立 crate 按需启用。核心 `rutis` 不增加网络、运行时或重连概念，也不反向依赖这些扩展。

- `rutis-channel`：Channel、连接器契约、连接元信息与结构化错误；不依赖 rutis、bridge 或 interop，不包含具体承载实现。
- `rutis-interop`：Session、codec、运行时契约及 RuntimePlugin；通过通用会话入口接入，不依赖 bridge 的 Peer 类型或具体承载。
- `rutis-bridge`：Transport 服务及注册接口、identity、link、节点功能插件、Peer 到运行时的 Adapter；不依赖任何具体 transport crate。
- `rutis-transport-local`：LocalPlugin，内部实现 Unix socket、继承 fd、spawn 与退出监控；不按内部机制继续拆 crate。
- `rutis-transport-websocket`：WebSocketPlugin，内部实现拨号、监听、TLS 接入、心跳与关闭处理。
- `rutis-transport-memory`：MemoryPlugin，内部实现有界内存通道，支持测试与进程内互联。
- `rutis-loader`：两类行集成、解析器及其组合。应用装配层选择具体承载，loader 不固定绑定承载清单。

承载 crate 依赖 `rutis-channel`、`rutis-bridge` 和核心插件 API，以原生 rutis 插件为主要交付入口，不将插件包装设为附属的 `plugin` feature。插件校验配置、提供 Transport、持有资源，并随卸载撤销服务和清理所属资源；依赖 link 通过原生门控停止。

组合通过统一 Transport 服务引用具体承载；需要自建承载的便捷组合在应用装配层完成，不能使 bridge 反向依赖 transport。运行时通用会话入口由 interop 定义，bridge Adapter 将 Peer 转为该入口，避免 crate 循环依赖。

只用核心不带入扩展；只用本机运行时不带入 WebSocket；只接远端执行端不要求安装本机 Node/Python 环境。memory 默认仅由测试或明确配置的应用选用。新增外部承载 crate 实现同一契约，无需修改 bridge。

## 3. 共享连接与会话

### 3.1 服务与依赖

- 承载插件提供 `Transport#<种类>`，配置键为 `transport`。
- 网络 link 依赖 Transport 和 Identity；本机执行端身份由父进程指定。
- link 完成身份及协议握手后提供 `Peer#<id>`，仅表示会话就绪，不表示某种管理功能已授权。
- bridge 的运行时接入 Adapter 与节点功能插件依赖 Peer，登记各自控制操作；RuntimePlugin 使用 interop 定义的通用会话入口，不直接依赖 Peer 类型。处理器随所属插件卸载注销。
- Peer 提供端点 ID、Session、操作注册及对端功能宣告观察接口。会话仅归 link 所有。
- 同一链接明确选择运行时或节点接入契约；不得对同一插件实例同时启用两套管理归属。
- 本机运行时的会话来源：N2 起，RuntimePlugin 的本机来源为 `local` 承载 + link（身份由父进程指定），与远程来源走同一入口；`Process` 外观仅为生成代码和既有调用方保留的兼容层，内部改由同一 Session 实现，不再是 RuntimePlugin 的另一条接入路径。迁移完成并通过兼容验收前，`Process` 不删除。

### Session 共享与资源归属

**Session 按运行时实例或节点 link 建立，不按插件建立。插件的装卸不创建或关闭共享 Session。**

| 接入模式 | 活动会话边界 | 共用会话的对象 |
| --- | --- | --- |
| 语言运行时 | 每个 Runtime 实例一个活动控制 Session；远程接入由所属 link 持有 | 该实例内的全部受控插件行、宿主服务代理与跨端调用 |
| 完整节点 | 每个配置的节点 link 一个活动 Session | 该 link 的 host/import/export/events，以及通过 host 代装的多个业务插件 |

- Runtime 接入和节点桥功能使用所属会话，不为每个插件、服务或事件另建 Session。
- 插件实例通过实例 key 区分；调用、服务与对象引用按各自 ID 路由，资源归属记录到插件实例或桥功能。实例 key 和引用只在所属会话的命名空间内解释。
- 卸载业务插件只清理其资源；卸载桥功能只注销其操作并清理所属资源。两者均不关闭共享 Session，也不清理无关插件。
- link 结束时统一关闭 Session。运行时接入清理该控制会话租约下的全部行；节点互联清理该链接代装和导入导出的资源，不影响远端独立管理的插件。
- 同运行时服务仍可交付原生对象；共享控制 Session 不要求运行时内部调用经过网络。
- 插件共享 Session 是运行时与节点接入的管理模型；多个逻辑 Channel 共享物理连接是 Adapter 的承载策略，二者独立。

### 3.2 连接注册与重试

- 每个 Session 使用一个逻辑 Channel。物理连接的池化、复用、多路复用与维护归 Transport Adapter；link 不要求会话独占物理连接。
- 连接器每次调用只返回一次逻辑 Channel 建立结果；拨号侧 link 唯一持有会话重建的退避状态。Adapter 不另起会话重试循环，不能恢复已终止的 Channel 或重放结果未知的调用。
- `Retryable`、`AuthRejected`、`Incompatible` 等结构化结果决定重试策略；诊断文字不参与判断。参数见网络栈规范。
- 共享监听器的验证规则与接收路由通过可撤销注册句柄交给 link 持有；无有效注册直接拒绝。
- 承载握手结束、移交连接前复核身份及注册代次；旧注册不得移交迟到连接。
- link 停止或 Identity 撤销时，取消重试、撤销注册、关闭所属会话及握手中的逻辑通道；不关闭其他链接使用的共享物理连接。底层凭据撤销使物理连接整体失效时，Adapter 通知所有受影响通道。
- 身份与授权绑定逻辑 Channel；复用不能串用权限。物理连接故障终止受影响通道，由各 link 分别处理；关闭单个 Channel 不影响同连接的其他通道。
- 承载卸载时关闭其连接，依赖链接按原生门控停下。

### 3.3 组合与更新

- 组合入口只组合已有实现，不另做协议、授权或重连逻辑。
- 自建实例归组合子树；引用的共享承载或身份实例不归该子树。
- 按子插件差异更新配置。仅修改导出、导入或事件列表时，保留 link 和原会话，不重建整棵组合子树。
- 本机进程启动与远程连接只改变会话来源，不改变运行时行的管理语义。

## 4. 语言运行时接入

### 4.1 管理与执行

- `RuntimePlugin` 接入具名运行时，会话来源可为本机承载或已连接的远端承载。
- 一个运行时实例承载多个插件；需要隔离时配置多个实例。运行时实例名与会话端点 ID 分开管理。
- 运行时插件只依赖执行端接入条件，不依赖业务行提供的服务。
- 叶子插件声明 apply、inject、provides、配置和清理；不提供子插件树、事件系统或独立依赖图。
- 同运行时服务直接交付原生对象；跨运行时或 Rust 调用经控制方 rutis 路由。
- 远端运行时不得自行决定依赖满足、启动顺序或重启策略。

### 4.2 运行时契约

复用多语言运行时操作，不强行改名为节点操作。

| 操作 / 信息 | 语义 |
| --- | --- |
| mount / features | 校验 `rows.v2`、`hosts`、`leaf` 等运行时功能 |
| `rows.schema` | 返回配置 schema、inject、provides 及方法形状 |
| `rows.load / update / unload` | 装载、更新、卸载指定行 |
| `hosts.provide / withdraw` | 在运行时中注册、撤销宿主服务代理 |
| 行服务通知与调用 | 将行提供的服务投到 rutis，按运行时契约访问 |

- 行服务使用 `host_key(name)` / `dyn HostDispatch`，方法形状由 `methods()` 提供，`origin()` 标记来源运行时。`origin()` 现返回 `Option<&Process>`；N2 改为与会话来源无关的运行时标识（运行时实例名 + 当前会话代次），同运行时判断按该标识进行，不依赖本机进程对象。
- 运行时功能（`rows.v2`、`hosts`、`leaf` 等）由 Runtime 服务在契约校验后公开；loader 侧行集成从 Runtime 读取，不再直接访问 `Process`。
- 宿主服务按消费行租用：首个租用注册，最后一个释放撤销；操作串行，租用返回前注册完成。
- 同运行时原生服务不重复注册代理；重复服务导出按冲突失败。
- `leaf` 运行时的全部 inject 由 rutis 门控；Cordis 仅将 `register_shared` 名字交给 rutis，其余依赖由 Cordis 管理。
- 行卸载先撤销服务投影并等待消费者停止，再卸载提供者插件。
- 更新按运行时能力执行；Python 叶子插件无 volatile，就地更新请求按卸载后重装处理，不承诺不断实例。

### 4.3 两阶段就绪

1. link 提供 Peer，RuntimePlugin 校验运行时契约后提供 `Runtime::key(实例名)`。
2. loader 侧 `RuntimeRowsPlugin` 依赖 Runtime 与 Loader，异步刷新离线解析及声明变化的行；完成后提供 `RuntimeRows::key(实例名)`。
3. 行依赖 RuntimeRows 和解析出的服务依赖；声明完整前不得启动。

解析刷新在独立任务中执行，不在进行中的 reconcile 内同步等待 reload。运行时失联撤销 Runtime，依赖门控撤销 RuntimeRows，相关行与服务投影停止。

### 4.4 远端租约

- 远程运行时只接受配置并验证过的控制方管理；每个运行时实例只有一个活动控制会话。
- 装载行、注入代理、导出引用均属于该会话租约。控制会话断开后，执行端清理租约内全部行和代理，不要求部署远端 rutis。
- 运行时守护进程可保持监听；本机父进程拉起的执行端断链后清理并退出。均不自动重启进程。
- 新控制会话接管前先结束旧租约；迟到旧会话指令不能作用于新租约。执行端按以下顺序处理新连接：承载鉴权 → 关闭旧会话通道，不再读取其后续帧 → 清理旧租约内全部行和代理 → 回应新会话 `hello`。清理完成前不回应新会话，因此新租约不会与旧租约并存。
- 网络承载下远程叶子运行时只作为监听方，由控制方 link 拨号并负责重连；需要穿越 NAT 时使用反向代理或隧道，本设计不提供叶子主动拨号。
- 重连后重新查询声明并按期望状态装载，不恢复旧引用，不重放结果未知的调用。

## 5. 完整节点互联

### 5.1 节点功能插件

| 插件 | 职责 | 依赖 |
| --- | --- | --- |
| export | 公告指定本地服务，跟踪换值和撤销 | Peer、被导出服务 |
| import | 将指定远端服务注册为本地原生服务 | Peer |
| host | 接受代装请求，装出的插件归自身子树 | Peer |
| events | 按事件名和方向转发通知 | Peer |

- 节点各自持有配置、服务和依赖图；功能插件只依赖会话就绪，不等待远程行。
- 默认不安装功能插件；安装哪些功能插件就是该链接的授权。
- 功能登记与注销更新 `link.offers`，由 link 转达，不让 link 解释行管理语义。
- 导入同名服务视为配置错误：先注册者保留，后注册的 import 失败并同时标明双方链接。冷启动时两者注册先后不确定，不承诺哪一方保留；配置应为不同链接的同名服务指定不同本地名。

### 5.2 节点控制操作

节点操作双向可用，未注册操作返回错误，会话继续。

| 操作 | 语义 / 处理方 |
| --- | --- |
| `services.announce { name, service, shape, version }` | 公告服务对象引用 / import |
| `services.withdraw { name, version }` | 撤销服务 / import |
| `plugins.describe(插件)` | 返回 `{ schema, version, integrity }` / host |
| `plugins.load(key, 插件, config, isolate, inject)` | 代装 / host |
| `plugins.update(key, config)`、`plugins.unload(key)` | 更新、卸载 / host |
| `events.forward { name, args }` | 以 parallel 发通知，等待监听完成 / events |
| `link.offers { families, version }` | 宣告已注册功能族，版本递增，忽略旧宣告 / link |

- 服务通过 Session 对象 call/get 访问；换值公告新引用，按 version 排序，旧引用按计数释放。
- Cordis 服务经 export 的导出 fiber 读取，调用产生的 effect 归该 fiber。
- 仅转发通知事件；同链接、同事件名只允许一个转发方向。
- 节点可组成树；跨链接导入再导出要求引用转发与调用链路由能力。

### 5.3 loader 侧代装集成

与 §4.3 相同采用两阶段就绪：

1. link 提供 Peer，对端宣告 plugins 功能族。
2. loader 侧 `PeerRowsPlugin` 依赖 Peer 与 Loader，异步刷新解析后提供 `PeerRows#<id>`。
3. `PeerRow` 依赖 PeerRows；声明完整前不得启动。

- `PeerResolver` 解析 `peer:<id>/<插件>`，目标节点解释插件名。
- `PeerRowsPlugin` 位于 rutis-loader，依赖 Peer 和 Loader；观察 plugins 宣告，查询 schema、刷新解析后提供 `PeerRows#<id>`。
- 对端无 plugins 或离线时，解析结果标记离线、schema 为空；行等待 PeerRows，不标为 Unresolved。
- 刷新以独立任务使用 `entries()` + `reload(id)`；门控开放前不能启动旧解析，每行只启动一次。
- `PeerRow` 依赖 PeerRows；apply 发 load，disposer 发 unload；已断链的卸载按成功处理。
- 对端撤回 plugins 时，PeerRowsPlugin 撤销 PeerRows；Peer 和其他功能保持运行。
- 会话登记表供无 ctx 的解析器查询；行集成和解析器由组合入口接入，不放进 link。
- 完整节点的插件依赖由目标框架处理；不将其内部依赖图复制到控制方 rutis。

## 6. 会话与兼容

### 6.1 契约隔离

- 共享会话机制，不等于共享控制操作或授权。运行时使用 rows/hosts，节点使用 plugins/services/events。
- 配置明确端点契约；启动时校验版本和能力，不靠调用失败猜测端点种类，不静默切换模式。
- 现有多语言实现的 `PROTOCOL = 2` 不等于本文拟扩展的节点会话格式。新增端点身份握手与调用号格式属于不兼容变更，必须分配不同主版本；具体发布版本号在实施前确定。
- WebSocket 子协议随实际会话主版本变化；不能把新格式继续标记为既有 `rutis.2`。
- 本机兼容路径保留现有运行时操作与线格式；新网络会话使用明确协商的格式，旧执行端不支持时直接报不兼容。
- 两种格式在过渡期并存：Rust interop 与 Node/Python SDK 需同时支持 `PROTOCOL = 2` 与新主版本；格式由会话来源决定（本机兼容路径用 2，网络会话用新版本），握手时确定，会话内不切换。本机路径迁移到新格式另行决定，不在本设计内。

### 6.2 目标会话能力

- 双方握手声明会话版本、端点 ID、实现名称/版本及能力；节点实现与运行时实现均可使用，不要求 framework 对象。
- 网络端点 ID 必须匹配 ChannelInfo 中验证过的身份；本机 ID 由父进程指定。部署内端点 ID 唯一，仅含小写字母、数字、`-`。
- 新格式调用号为 `<端点 id>:<n>`，校验前缀和递增序号；引用 origin 与调用链 path 保持一致。
- 兼容运行时路径保留现有会话标签和调用链 rebase，不能仅替换调用号而丢失跨会话路由。
- 两种格式间的引用与调用链转换只发生在 rutis 中继（`rpc/relay.rs`）：中继在转发时把兼容会话的标签路径改写为新格式的端点 id 路径，反之亦然。执行端不感知另一种格式；网络多跳（N4）只在新格式会话之间定义。
- 函数、异步结果、对象引用按能力声明；对象首次授予附形状。Node/Python 未实现对象接收时不得宣告支持。
- 取消双向落到 AbortSignal、Rust 取消令牌或语言等价物；丢弃 future 发送取消。
- 转发按（会话，引用号）保持身份，释放沿中继链传递；复用 M1a 的中继机制，补充网络多跳契约与测试。
- 能力包含 objects、signals、reentrant-sync、plugins、events、volatile、forwarding；能力不授予权限，缺少必需能力时使用前拒绝。
- 不支持同步重入时，同步等待期间的同链反向调用立即返回 SyncWaitCycle，不得排队。

### 6.3 服务形状与同步

- 形状描述方法的 sync/async、属性及对象形状；无声明成员不得跨端点暴露，不运行时猜测异步性。
- 运行时保留现有 provides 方法形状；节点服务可从 Rust trait derive、TS 声明清单或 Python inspect 生成。动态互通不强制类型化绑定。
- 放置不改变同步/异步方法类型。远程同步调用至少阻塞一个 RTT；暴露次数与耗时，不自动添加调用超时。
- 有截止时间的操作使用异步方法并由调用方设超时。
- Python 运行时同步等待可执行其他进来调用；插件不得在调用外部服务时持有会阻塞重入的锁。
- 两个 Node 运行时的无关调用链交叉同步等待仍有死锁限制；共享承载不解除该限制，此类调用使用异步方法。

## 7. 生命周期与安全

| 事件 | 运行时接入 | 节点互联 |
| --- | --- | --- |
| 会话就绪 | 校验契约后提供 Runtime，再由 loader 提供 RuntimeRows | 功能插件启动；loader 刷新后提供 PeerRows |
| 断链 | link 撤销 Peer；Runtime/RuntimeRows 级联撤销；控制方投影撤销，执行端清理租约 | link 撤销 Peer；loader 撤销 PeerRows；import 服务撤销，host 子树卸载 |
| 重连 | 新租约、新声明、新行实例 | 新会话、重新宣告、解析、代装 |
| 本机进程退出 | 报退出状态，不自动重启 | 同左 |
| 替换旧会话 | 清理旧租约后接入新会话 | link 结束旧会话及其功能子树，再接入新会话 |

- 在途调用断链后以 Transport 失败，结果未知，不自动重试；旧引用返回 session closed。
- 完整节点断链只清理该链接拥有的资源，不卸载节点独立管理的插件；本机最小子节点可按进程契约清理后退出。
- 信任模型：插件可信，网络和远端机器不完全可信。进程外执行不是沙箱。
- 非回环网络必须使用 TLS，允许前置反向代理终止；承载必须验证身份，权限与拨号方向无关。
- 节点 host 只能装当地已安装插件；开放 host 等同授予节点插件管理权，仅向可信对端开放。
- 远程运行时控制权仅授予配置的控制方；只能加载执行环境中已部署的插件，不提供代码上传。
- 远程加载默认拒绝 file: 和绝对路径插件名；显式本机运行时可保留 profile 的 file:// 兼容路径。
- 求值后的配置发送到执行机器，可能含密钥；诊断和追踪不得记录秘密。
- 通道限制消息大小；本设计不增加调用数和引用数配额。

## 8. 放置与配置

| 场景 | 配置 |
| --- | --- |
| 本机语言插件 | 保留 `py:<模块>`、npm 名及具名 Runtime 绑定；运行时会话来自 local |
| 远程语言插件 | 同一语言行解析规则，绑定远程 Runtime 实例；运行时会话来自网络 link |
| 远程节点代装 | `peer:<id>/<插件>`，使用节点 host 契约 |
| 远端 rutis 内的语言插件 | 外层节点互联，远端 rutis 自行装配 Runtime 和语言行 |

- 语言前缀只决定解析器；运行时实例配置决定执行位置。不另引入远程语言前缀。
- 不带前缀的 npm 名按语言行解析到解析器绑定的 Node Runtime 实例；原“默认节点”由该具名 Runtime 取代，不再有隐式的本机节点。
- 现有 npm 解析按本机 Runtime anchor 查找入口文件。远程 Runtime 的插件文件位于执行端，控制方不得按本地文件系统判断存在性；远程实例的入口解析由执行端在 `rows.schema` 中完成，未找到返回结构化结果，行标为 Unresolved。
- Runtime 的远程会话来源是拟新增配置能力，不宣称现有 launcher 已支持网络连接。
- 配置表达式在控制方求值，插件在执行方解释；表达式环境和插件读写环境分别属于两端。
- isolate 标签只在同一执行框架实例内共享；叶子运行时按自身契约处理。更换执行实例重建行。
- import 的服务注册到原生服务系统和 loader 目录；运行时投影继续使用 host_key，不另造平行共享服务键。

## 9. 实施阶段

| 阶段 | 交付 |
| --- | --- |
| D1 | 通道解耦，保留本机多语言契约与线格式 |
| N1 | 公共 link / 身份接入、端点契约与版本校验；新会话身份及能力格式 |
| N2 | Runtime 接入可选会话来源（本机经 local + link）；`origin()` 与运行时功能改为与 `Process` 无关；loader 侧两类行就绪及远程入口解析；独立节点功能插件与组合入口 |
| N3 | 网络栈 D2–D3；远程 Python / Node 运行时与 rutis / Cordis 节点均可用；租约和重连清理 |
| N4 | 网络多跳服务转出、引用与调用链路由 |
| N5 | 公共会话、运行时、节点三组一致性套件；其他端点按契约接入 |
| 后续 | 二进制编码、类型绑定扩展、Windows 承载 |

旧反方向 server.rs、build/rust.rs 保持冻结；删除需另行确认。既有多语言 M1/M2 能力复用，不因节点互联而强制迁移为完整框架。

## 10. 验收

### 公共机制

- 运行时和节点模式均装载至少两个插件，确认共用一个活动 Session；装载不新增会话，卸载其中一个不关闭会话且另一插件仍可调用。
- 节点 link 同时启用多种桥功能，撤销其中一种仅清理其资源；关闭 link 才统一清理所属会话资源。
- 同一会话测试覆盖 memory、本机 IPC、网络承载；新增承载不增加管理操作。
- local/websocket/memory 各自通过原生插件装载、配置校验、服务提供和卸载测试；卸载承载时依赖 link 停止并清理资源。
- 核心、interop、bridge、loader 和各承载 crate 无循环依赖；bridge 不依赖具体承载，纯本机组合不编译 WebSocket 依赖。
- 身份、版本或端点契约不符时拒绝；重连按结构化错误处理，不解析文字。
- link 不依赖 loader，任何行未解析时仍可建立会话。
- link / Identity 撤销阻止旧注册和迟到握手接入，不影响共享监听器的其他链接。
- 半开连接在心跳期限内清理；旧引用失效；本机进程崩溃不自动重启。
- Rust、Node、Python 同时通过 `PROTOCOL = 2` 与新主版本的会话测试；跨格式引用经 rutis 中继往返后身份与调用链保持。
- 同步回调在阻塞的 current_thread 上仍可推进；支持的引用、取消、错误对象图通过跨语言测试，不支持的能力明确拒绝。

### 远程运行时

- 远端机器仅运行 Python 叶子执行端，无 rutis，仍可由本地 loader 管理行、依赖、配置与清理。
- 本机与远程运行时使用同一组行测试；同运行时调用取得原生对象，跨运行时经 rutis 路由。
- 冷启动声明刷新后每行仅启动一次；提供者先撤销投影、消费者先停止；运行时崩溃仅影响相关行。
- 断链清理执行端租约；新控制会话不得复用旧行、引用或接收旧指令。
- 旧会话未断开时新控制方接入：执行端先关闭旧会话并清理租约，再回应新 `hello`；未验证的控制方被拒绝。
- 远程 Node 运行时的 npm 名在控制方本地不存在时仍可解析并装载；执行端不存在的插件使行 Unresolved。
- Python 配置更新重启实例；Node 仅在能力支持时使用 volatile。

### 节点互联

- 远程 rutis / Cordis 支持服务导入导出、代装、事件及本地原生依赖门控。
- 无 host 时行等待 PeerRows；开放后启动，撤回后停止，其他功能不受影响。
- 双方互相代装同时冷启动、重连不死锁；host 不等待行就绪。
- 断链只清理链接拥有的插件和服务，节点独立插件保持运行。
- 组合与单独装配权限和行为一致；修改导出配置保留原会话与无关功能。
