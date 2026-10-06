# 插件 API

TypeScript / JavaScript 插件（`@arcships/rutis`）和 Python 插件（`rutis`）的写法与规则相同。当前插件 API 版本：**1**。

## 声明

| 声明 | TypeScript（`definePlugin({...})`） | Python（`define_plugin(apply, ...)` 或模块变量） | 含义 |
| --- | --- | --- | --- |
| 用到的服务 | `inject: ['llm']` | `inject=["llm"]` | 都就绪才启动；任何一个撤销，插件停下，等它回来再启动 |
| 提供的服务 | `provides: { weather: { today: 'async' } }` | `provides={"weather": Weather}` 或 `{"weather": {"today": "async"}}` | 名字和每个方法的种类（`sync` / `async`）。只有声明过的方法能被别的插件调用 |
| 配置 | `config: { type: 'object', ... }` | `config={...}`（模块变量写 `Config`） | JSON Schema；宿主据此校验和展示配置 |
| 启动 | `apply(ctx, config)` | `apply(ctx, config)` | 可以是 async；返回清理函数或不返回 |

## ctx

| 方法 | 作用 |
| --- | --- |
| `ctx.use(name)` | `inject` 里声明的服务。同一个运行时进程里的插件提供的，是对象本身；其他进程、其他语言、其他机器提供的，是代理 |
| `ctx.provide(name, value)` | 提供服务，直到插件卸载，或调用返回的函数 |
| `ctx.effect(cleanup)` | 插件卸载时运行 `cleanup`（可以是 async） |

清理按注册的相反顺序运行：`apply` 返回的清理函数最先，然后是 `ctx.effect` 注册的。

## 生命周期

插件不决定自己什么时候运行，宿主决定：

- `inject` 的服务都就绪时启动（`apply`）；
- 任何一个撤销时停下（运行清理），它回来时再启动；
- 配置变了：重启（运行清理，再用新配置 `apply`）；
- 插件提供的服务被撤销时，用到它的插件先停下，提供者后停。

所以 `apply` 里拿到的服务在插件运行期间一直有效，不需要处理"服务中途消失"。

## 值怎样传递

插件和它用到的服务可能不在同一个进程。跨进程时：

| 值 | 怎样传递 |
| --- | --- |
| `null` / `None`、布尔、数字、字符串 | 复制 |
| 数组 / list、普通对象 / 字符串键的 dict | 递归复制 |
| Python dataclass | 复制为 dict |
| `Error` / 异常 | 复制名字和消息；抛出时另一端得到同名的错误 |
| 函数、Promise / 协程 | 按引用：另一端调用或 await 的是原来那个 |
| 带方法的对象、类的实例 | 按引用：另一端拿到代理，调用的是原对象 |
| symbol、bigint、Map、Set、二进制、循环引用 | 不能跨进程（TypeScript 中 Map、Set、Date 按 JSON 规则复制，内容会丢失） |

同一个进程里没有复制，所以"传过去之后改原对象，另一端也看到"这类写法在单元测试里可能成立、跨进程就不成立。测试工具的严格模式（默认）按上表传值，提前暴露这种问题。

## 同步调用与可重入

声明为 `sync` 的方法同步返回结果，跨进程时调用方阻塞等待。期间：

- 被调用的一方如果反过来调用调用方（回调），调用能到达正在等待的调用方并执行；
- Python 运行时在同步等待期间也执行其他进来的调用，所以 Python 插件的服务可能在它自己正处于一次同步调用之中时被调用：**调用别的服务时不要持有锁**；
- Node 运行时在同步等待期间只执行属于这条调用链的调用，两个 Node 运行时互相同步调用可能卡死。跨运行时、可能交叉的调用请用 `async` 方法。

需要等待事件循环推进的结果（例如在同步方法里等一个 Promise）不能放在同步方法里，会得到 `SyncWaitCycle` 错误：改成 `async`。

## 取消

async 方法的调用方放弃等待（例如超时）时，调用被取消：被调用的 Python 协程被取消；JavaScript 方法在调用方传了 `AbortSignal` 参数时收到 abort（Rust 绑定调用 Cordis 方法时会传）。没有这两种情况时，被调用的方法照常运行完，结果被丢弃。

## 版本

插件上标记着它所用的插件 API 版本（`definePlugin` / `define_plugin` 自动标记）。运行时比插件旧时，加载失败并说明需要升级什么；插件依赖 SDK 的大版本范围即可，不需要与宿主版本对齐。
