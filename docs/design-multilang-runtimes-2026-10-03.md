# 多语言插件：每种语言一个运行时插件（设计稿）

状态：设计稿，未实现。日期：2026-10-03。
依据：[多语言决策记录](decision-multilang-2026-10-03.md)（#107，本文修订其中几项，见 §十）、[Cordis 运行时插件化](design-cordis-runtime-plugin-2026-10-03.md)（#109）、[rutis-loader 设计](design-rutis-loader-2026-10-02.md)、[兼容层设计](design-protocol-plugin-mount.md)、调研报告 [plan/analysis/multilang](plan/analysis/multilang/README.md)。
基准：`main` `a078630`。

## 一、要解决什么

rutis 现在有两种动态加载插件的方式：dylib（Rust 插件，进程内）和 Cordis（JS 插件，经 `CordisRuntimePlugin` 在一个 Node 进程里运行）。两者都是 loader 的行，可以写在同一份配置里。

其他语言里也有值得接的东西：

- **Python**：AI/ML、数据处理、各种 SDK，agent 用的工具大多先有 Python 版。
- **Swift / Objective-C**：macOS 的系统能力，如日历、通讯录、辅助功能、屏幕录制、Core ML。
- **Go**：基础设施类库、云厂商 SDK、网络工具。

本文设计怎样接它们，同时避开 #107 担心的两件事：不在每种语言里重造一个插件框架，进程数也不随插件数增长。

不在本文范围：

- **Shell、PowerShell**：它们只能传数据，用不上服务、依赖门控和清理这套范式。它们的价值在于给 agent 当工具，应该另做"命令型工具运行时"（运行时发现、带 JSON Schema 的参数、可取消），不进插件协议。
- **远程**：插件跑在别的机器上是另一个问题，不和多语言绑在一起做（远程稿 #110 仍是草稿）。
- 各语言运行时的具体实现细节：本文只定模型、契约和分阶段。

## 二、结论先说

1. **一种语言一个运行时插件，一个运行时一个进程**。和 #109 的 `CordisRuntimePlugin` 一样：运行时插件拉起这门语言的进程，提供运行时服务；这门语言的插件都是依赖它的 loader 行，装进同一个进程。装十个 Python 插件也只有一个 Python 进程。
2. **范式只在 rutis 里有一份**。依赖门控、启动停止、重启、配置分层、热更新，都由 rutis 和 loader 负责。每个其他语言的插件在 rutis 里就是一个行，也就是一个 fiber。
3. **其他语言的插件是"叶子"**。语言那一侧只要一个很小的 SDK：插件有 `apply(ctx, config)`，在里面用服务、提供服务、返回清理函数。没有子插件、事件系统和本地依赖图，所以不需要在 Python、Swift 里复刻 Cordis 或 rutis。
4. **Node 仍是完整的 Cordis**。因为要复用 npm 上已有的 Cordis 插件，它们之间本来就按 Cordis 的方式互相依赖。这是唯一需要完整框架的一侧。
5. **同进程的调用直接完成**。同一个运行时里的两个插件互相用服务时，调用在本地直接完成，不走进程间通信。rutis 只管"谁依赖谁、谁先停"，不经手调用本身。只有跨语言的调用才走 IPC（同步调用约 30µs，是 Node 侧实测的数字）。
6. **隔离按需多开**。不稳定的插件可以单独放进另一个运行时实例。这和现在"需要隔离的插件放进不同挂载"是同一条规则，默认不这么做。
7. **两类运行时**：
   - **按名字加载代码的**（Node、Python、Swift）：一个进程按名字装载多个插件，Swift 用 `dlopen` 加载插件 bundle；
   - **插件就是编译好的程序的**（Go）：Go 不能在运行时可靠地加载代码，一组插件编译进一个可执行文件，作为一个运行时进程拉起；换代码就是换二进制、重启这组插件。
8. **契约复用现在的协议**，不为这件事升协议版本：帧、引用、取消、同步调用链和 `rows.*` 控制操作都照用（§五）。要补的是服务按名字投到 rutis，以及方法形状（同步还是异步）。
9. **先 Python，再 Swift，Go 等有具体需求再做**。

## 三、现状里能直接用的

| 现在有的 | 在本文里的角色 |
| --- | --- |
| `CordisRuntimePlugin`（#109）：拉起 Node 进程、注入宿主服务、提供 `CordisRuntime`、崩溃时撤销服务、不自动重启 | 运行时插件的样板，其他语言照这个模式各写一个 |
| `InteropResolver` + `JsRow`：按名字解析、行依赖运行时服务、`rows.load / update / unload / schema` | 行和运行时之间的控制操作，语言无关 |
| 线协议：帧、函数和异步结果引用、计数释放、`cancel`、`path` 调用链、`SyncWaitCycle` | 运行时进程和 rutis 之间的会话，语言无关 |
| `host_key` 宿主服务 | 其他语言的插件用 rutis 服务的方式 |
| Python runner 调研（`python.md`）：原型通过了嵌套同步回调、`SyncWaitCycle`、取消、无关调用延后，同步往返约 25–60µs | Python 运行时的会话层可以直接照这个原型做 |

协议里还有一处写死了 Node：Rust 侧只接受以 `node:` 开头的调用号。第一阶段把 `node:` 当作"运行时一侧"的前缀即可，其他语言的运行时也用它，不升协议版本；等真要改名时再随下一次协议升级处理。

## 四、模型

```text
rutis 宿主
 ├─ CordisRuntimePlugin ── Node 进程（完整 Cordis）── JS 行 × N
 ├─ PythonRuntimePlugin ── Python 进程（叶子 SDK）── Python 行 × N
 ├─ SwiftRuntimePlugin  ── Swift 辅助进程（叶子 SDK）── Swift 行 × N（bundle）
 ├─ GoRuntimePlugin     ── Go 可执行文件（一组插件编译在一起）
 ├─ dylib 行（进程内）
 └─ LoaderPlugin：所有行的期望状态
```

**运行时插件**（每种语言一个类型，可以有多个实例）：

- 配置：启动命令、项目位置（venv、bundle 目录、可执行文件）、提供给插件的宿主服务；
- 依赖：它提供给插件的宿主服务（照 #109：宿主服务没就绪就等，撤销就停）；
- 提供：这门语言的运行时服务，键带实例名（例如 `TypeKey::keyed_dynamic::<PythonRuntime>("py")`），这样同一种语言可以开多个实例；
- 进程崩溃时撤销服务、不自动重启，是否重启由应用决定（与 #109、#71 一致）。

**插件（行）**：

- 依赖：所在运行时的服务，加上插件自己声明的依赖（§六）；
- apply 时请运行时装载插件，清理时卸载；只改易变字段的更新就地提交（照 #101 的做法）；
- 插件提供的服务投到 rutis，别的语言、Rust 插件都能按名字使用（§五）。

**放置**：行名带上运行时前缀，例如 `py:weather.plugin`、`swift:Calendar.bundle`、`go:tools/ping`。不带前缀的 npm 包名照旧走 Cordis 运行时，`dylib:` 照旧。前缀后面的部分由运行时自己解析：Python 是模块名，Swift 是 bundle，Go 是"可执行文件里的插件名"。

## 五、运行时契约

运行时进程和 rutis 之间的契约，就是现在 Node 运行时实现的那份，加三处补充：

| 项 | 现状 | 补充 |
| --- | --- | --- |
| 启动 | 固定执行 `node --import tsx runner.mjs` | 运行时插件的配置里给出启动命令；参数仍是 socket 路径和项目位置 |
| 装载 | `rows.load(key, entry, config, isolate, inject)` | `entry` 由运行时自己解释（模块名、bundle、插件名）。`isolate` 只对 Cordis 有意义，叶子 SDK 可以忽略 |
| 描述 | `rows.schema(entry)` 返回配置的 JSON Schema | 改成同时返回插件声明的依赖（§六），以及它提供的服务及其方法形状 |
| 服务 | 行模式下，行提供的服务不投到 Rust（`InteropResolver` 的现状） | 运行时用现有的服务槽位通知报出行提供的服务；rutis 按名字注册成服务，键为 `TypeKey::keyed_dynamic::<RemoteService>(name)`，并登记进 loader 的服务名目录 |
| 方法形状 | 只有构建期生成的挂载知道方法是同步还是异步 | 运行时报服务时一并报出每个方法是同步还是异步：Python 用 `inspect`，Swift 由 SDK 登记时声明。Node 的做法见 §九 |

调用方向：

- **其他语言的插件用 rutis 服务、或用另一种语言的服务**：走现有的 `host:<名字>` 调用。rutis 按名字找到这个服务，如果它来自另一个运行时，就转发过去。转发要用到跨会话的引用转交（现在 `rpc.rs` 明确不支持），这是 M1 里唯一较大的会话层改动。
- **同一个运行时里的插件互相使用**：运行时在本地直接把对象交给使用方，不经过 rutis。

## 六、叶子 SDK

Python 的形状（示意）：

```python
# weather/plugin.py
inject = ["llm"]                 # 声明依赖：rutis 等它就绪才启动，它撤销就停下

def apply(ctx, config):
    llm = ctx.use("llm")         # 用宿主、其他插件或其他语言提供的服务
    ctx.provide("weather", Weather(llm, config["city"]))
    return lambda: ...           # 清理
```

Swift 的形状（示意）：

```swift
final class CalendarPlugin: RutisPlugin {
    static let inject = ["llm"]
    func apply(_ ctx: Context, config: Config) async throws -> Cleanup {
        ctx.provide("calendar", Calendar(store: EKEventStore()))
        return {}
    }
}
```

SDK 只做这几件事：

- 插件入口（`apply`、`inject`、配置类型，配置类型可以导出成 JSON Schema）；
- `ctx.use` / `ctx.provide`：同进程直接给对象，跨进程给代理；
- 运行时进程：实现 §五 的契约，以及同步等待期间的重入（Python 的做法见调研 §3：I/O 线程加主线程按调用链执行反向调用）。

依赖写在插件里（`inject`），由 `rows.schema` 报给 rutis，rutis 把它写进这一行的依赖声明，于是原生门控生效。插件之间的依赖、启动顺序和停止顺序，全由 rutis 决定，SDK 里没有依赖图。

## 七、各语言的要点

| 语言 | 运行时进程 | 要点 |
| --- | --- | --- |
| Python | 一个 Python 进程，按模块名装载 | 照调研原型实现会话层；每个运行时实例用一个锁定的环境（uv 项目），环境冲突时开第二个实例；需要 Python 3.12 或更高 |
| Swift / ObjC | 一个签名的辅助程序，用 `dlopen` 加载插件 bundle | 系统授权（TCC）记在辅助程序上，所以它必须有自己的签名和用途说明；ObjC 代码由 Swift 侧直接调用；只支持 Apple 平台 |
| Go | 一组插件编译进一个可执行文件 | 不用 Go 的 `plugin` 包；换代码要重新构建并重启这一组；构建由部署方负责，宿主不临时编译 |

## 八、进程与效率

- 进程数等于用到的运行时实例数：一种语言默认一个，Go 是一组一个，需要隔离时才多开。
- 同一个运行时里的调用不走 IPC。
- 跨语言的同步调用约 30µs（Node 实测），Python 调研给出的是 25–60µs。对热路径上的高频调用，应该把调用双方放进同一种语言，或者改用异步方法。

## 九、待定

- **Node 侧的方法形状**：行模式下没有构建期生成的信息。可选的做法有两种：随 npm 项目生成一份形状清单（用现有生成器读 `.d.ts`），或者把 Cordis 行提供的服务只开放给同进程的 JS 插件，暂不投到 rutis。倾向前者。
- **跨会话的引用转交**：§五 的转发需要它，要单独设计并配测试。
- **SDK 放在哪**：放在本仓库里单独的包（`runtimes/python`、`runtimes/swift`），还是放在单独的仓库。倾向放在本仓库，和一致性测试一起维护。
- **调用号前缀**：第一阶段沿用 `node:`（§三），改名时机待定。

## 十、与 #107 决策记录的关系

| #107 的结论 | 本文 |
| --- | --- |
| PowerShell、Bash、AppleScript/JXA 不在 rutis 内做 | 保留。Shell 类改为另做命令型工具运行时，不进插件协议 |
| Python（min_cordis）库外、以后再说 | 修订：做 Python，但不是 min_cordis 那样的完整框架，而是叶子 SDK 加运行时插件 |
| 中立接口描述不做 | 部分修订：只做方法形状（同步还是异步），用于跨语言调用，不做类型 IR 和代码生成 |
| 启动方式抽象不做 | 部分修订：运行时插件的启动命令可配置，仅此而已，不做通用的启动层 |
| `node:` 前缀放宽不做 | 保留：沿用 `node:` 作为运行时一侧的前缀 |
| 反方向继续冻结 | 保留 |

#107 担心的"每种语言都停在 demo 水平""一人维护五种运行时"，本文用两点回应：每种语言只有一个很小的叶子 SDK，范式只在 rutis 里实现一份；语言按真实需求逐个接。

## 十一、分阶段

| 阶段 | 内容 | 验收 |
| --- | --- | --- |
| M1 | 契约补充：运行时启动命令可配置；`rows.schema` 报出依赖和服务形状；行提供的服务按名字投到 rutis 并登记进服务名目录；跨会话的引用转交。先在 Node 运行时上做完 | JS 行提供的服务能被 Rust 行按名字 `inject`；JS 插件声明的依赖参与 rutis 门控；现有测试全部通过 |
| M2 | Python 运行时插件 + 叶子 SDK + 运行时契约的一致性测试 | 同一套一致性测试在 Node 和 Python 两个运行时上通过；Python 插件和 JS 插件互相使用服务；同进程调用不走 IPC |
| M3 | Swift 运行时插件（macOS）+ 叶子 SDK | 一个用 EventKit 的插件在签名辅助程序里运行，系统授权提示指向辅助程序 |
| M4 | Go（有具体需求时） | 一组 Go 插件编译成一个可执行文件，由运行时插件拉起，换二进制后这组插件重启 |
