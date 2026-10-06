# 面向开发者的包、工具与流程

日期：2026-10-06。状态：设计，待确认。

## 1. 为什么现在做

多语言插件（M1/M2）和网络栈（#145）已经完成，但都还没发布。现在的包是按实现长出来的，不是按开发者要做的事划分的：

- `rutis-interop` 这个名字说不出它是什么。它同时装着跨进程会话协议、语言运行时管理、Cordis 静态挂载和代码生成，网络栈的 crate 也依赖它。
- 写一个 Node 叶子插件要 `import` 整个 Node 运行时（`@arcships/rutis-interop`，带着 Cordis、tsx、typescript、ws）；Python 的插件 SDK 和运行时混在一个仓库内的 `rutis_runtime` 包里，也没有发布。
- 没有通用的宿主程序：不写 Rust 就跑不起来一个插件。
- 插件作者没有测试工具，也没有发布和兼容的约定。

我们还没有多少下游，破坏性调整的代价最低就在现在。这份设计确定所有面向开发者的包、命名、工具和流程；教程放在最后，等这些都就绪再写。

## 2. 谁在用 rutis

| 角色 | 要做的事 | 用什么语言 |
| --- | --- | --- |
| 插件作者 | 写插件、测试、发布 | TypeScript/JavaScript、Python、Rust |
| 宿主作者 | 做一个能加载插件的应用 | Rust（嵌入），或者不写代码（用通用宿主） |
| 运维 | 安装插件、写配置、把多台机器连起来 | 配置文件 |
| Cordis 应用作者 | 让已有的 Cordis 应用和 rutis 互通 | TypeScript/JavaScript |

本设计主要服务前两类，并让运维的工作只剩安装包和写配置。

## 3. 命名原则

1. **按用途命名**：开发者看到包名就知道该不该装它。
2. **同一个概念在各语言里同名**：写插件用的包在 Rust、Node、Python 里都叫 `rutis-sdk`。
3. **"运行时"只指语言运行时进程**：运行插件的那个 Node 或 Python 进程。Rust 一侧管理这些进程的 crate 不叫 runtime，避免和"异步运行时"以及运行时进程本身混淆。
4. npm 包保持 `@arcships` scope；PyPI 和 crates.io 没有 scope，用 `rutis-` 前缀。以上名字都已确认未被占用（2026-10-06）。

## 4. 包的划分

### 4.1 总览

| 用途 | Rust（crates.io） | Node（npm, `@arcships/`） | Python（PyPI） |
| --- | --- | --- | --- |
| 写插件（SDK） | `rutis-sdk`（已有，dylib 插件） | `rutis-sdk` | `rutis-sdk` |
| 运行插件的语言运行时进程 | — | `rutis-runtime` | `rutis-runtime` |
| 通用宿主程序 | `rutis-host`（二进制） | `rutis-host`（分发二进制） | `rutis-host`（分发二进制） |
| 把 Cordis 应用接成节点 | — | `rutis-bridge` | — |
| 内核 | `rutis` | — | — |
| 在 Rust 宿主里加载插件 | `rutis-loader` | — | — |
| 在 Rust 宿主里管理语言运行时 | `rutis-polyglot` | — | — |
| 节点之间的连接 | `rutis-bridge`、`rutis-transport-local` / `-memory` / `-websocket` | — | — |
| 跨进程会话协议、通道契约 | `rutis-session`、`rutis-channel` | （在 `rutis-runtime` 里） | （在 `rutis-runtime` 里） |
| 静态挂载 Cordis 插件、生成 Rust 绑定 | `rutis-cordis` | （在 `rutis-runtime` 里） | — |

### 4.2 Rust crate：按依赖分层

```
rutis-channel  →  rutis-session  →  rutis-bridge  →  rutis-transport-*  →  rutis-polyglot  →  rutis-loader  →  rutis-host
                                                                              └→  rutis-cordis
```

每一层只依赖它左边的层，不再有"bridge 依赖 interop、interop 的测试又依赖 bridge"这样的反向牵连。

| crate | 内容 | 来自 |
| --- | --- | --- |
| `rutis-channel` | 通道契约，不变 | — |
| `rutis-session`（新） | 会话协议：`Connection`、`Value`、帧编解码、握手与会话格式、错误、`HostDispatch` 与 `host_key`；feature `conformance` 提供会话一致性测试 | `rutis-interop` 的 `rpc`、`protocol`、`objects`、`services` 和 `conformance::session` |
| `rutis-bridge` | link、身份、Peer、节点功能（export / import / host / events）、`PeerPlugin` | 不变；`RuntimeAccessPlugin` 移出，bridge 从此与语言无关 |
| `rutis-transport-*` | 三种承载，不变 | — |
| `rutis-polyglot`（新） | 语言运行时：`RuntimePlugin`（本机与远程）、`RuntimeHandle`、行与宿主服务接口（`RuntimeClient`，即原 `Process` 中与行相关的部分）、`Launcher`、`RuntimeAccessPlugin`、本机运行时 `LocalRuntime`；features `node`、`python`、`local`（默认开，仅 Unix）；运行时一致性测试 | `rutis-interop` 的 `runtime`、`rows`、`Process` 的行部分；`rutis-runtime-local`（并入）；bridge 的 `RuntimeAccessPlugin` |
| `rutis-cordis`（新） | 静态挂载 Cordis 插件：构建期代码生成（`build`）、`include_mounts!`、`Mount` / 挂载外观、投影、事件转发、生成代码的分发 | `rutis-interop` 的 `build`、`Process` 的挂载部分、`projection`、`events`、`server`，及其私有的进程拉起代码 |
| `rutis-loader` | 依赖改为 `rutis-polyglot`、`rutis-bridge` | — |
| `rutis-host`（新） | 通用宿主程序，见第 6 节 | — |

`rutis-interop` 和 `rutis-runtime-local` 不再存在。弃用的接口（`CordisRuntime*`、`RuntimePlugin::node` / `python` / `launcher` 等）随这次改名直接删除，不再保留别名。

关于 `rutis-polyglot` 这个名字：备选还有 `rutis-lang` 和 `rutis-runtime`。不选 `rutis-runtime`，因为它会和 Node/Python 的 `rutis-runtime`（运行时进程本身）同名而角色不同；`rutis-lang` 偏短、含义不够确定。`polyglot`（多语言）是业界通用说法，例如 GraalVM Polyglot。

### 4.3 Node 包

| 包 | 内容 | 依赖 |
| --- | --- | --- |
| `@arcships/rutis-sdk` | `definePlugin`、TypeScript 类型、测试工具（`@arcships/rutis-sdk/testing`） | **无** |
| `@arcships/rutis-runtime` | 运行时进程：runner、`listen:` 守护、各种通道、会话、Cordis 宿主；`rutis-cordis` 构建期使用的绑定生成 | `@deepseek-ai/cordis`、`tsx`、`ws` |
| `@arcships/rutis-bridge` | 把 Cordis 应用接成 rutis 节点：`Link`、`Export`、`Import`、`Host`、`Events` | `@arcships/rutis-runtime`（使用它的会话与通道） |
| `@arcships/rutis-host` | 宿主二进制的 npm 分发，`npx @arcships/rutis-host` | `@arcships/rutis-runtime`；按平台的可选依赖 `@arcships/rutis-host-<平台>` |

插件只依赖 `@arcships/rutis-sdk`。运行时由宿主安装，插件不会把运行时版本锁死。`definePlugin` 用 `Symbol.for` 做标记（现在就是如此），所以 SDK 与运行时之间不需要共享模块实例。

`@arcships/rutis-interop` 发布最后一个版本，在 npm 上标记为弃用，说明指向新包。

### 4.4 Python 包

| 包（导入名） | 内容 | 依赖 |
| --- | --- | --- |
| `rutis-sdk`（`rutis_sdk`） | `define_plugin`、类型（`py.typed`）、测试工具（`rutis_sdk.testing`） | 无 |
| `rutis-runtime`（`rutis_runtime`） | 运行时进程：`python -m rutis_runtime`；可选依赖 `network`（WebSocket，远程运行时） | 无；`network` 需要 `websockets>=13` |
| `rutis-host` | 宿主二进制的 wheel（`uvx rutis-host`、`uv add --dev rutis-host`） | `rutis-runtime` |

两个包都发布到 PyPI，要求 Python 3.12 或更高。

## 5. 插件的约定

### 5.1 插件 API 版本

SDK 在插件上打一个整数标记 `api`（从 1 开始），表示插件按哪一版插件 API 编写；运行时声明自己支持的 API 范围。加载时不兼容会给出明确的错误，例如"插件 weather 需要插件 API 2，这个运行时只支持 1；请升级 @arcships/rutis-runtime"。

- 插件 API 的版本与包版本无关，只在插件看到的接口（`ctx` 的方法、声明的格式、值的传递规则）发生不兼容变化时才增加。
- 不经过 SDK 的 Cordis 插件视为 API 1。

### 5.2 Node 插件

- **叶子插件**（推荐）：`export default definePlugin({ inject, provides, config, apply })`，与 Python 插件同构。
- **Cordis 插件**：照常编写；要提供给 rutis 的服务，在 `package.json` 里声明 `"rutis": { "provides": { "weather": { "today": "sync" } } }`。
- **package.json**：`"keywords": ["rutis-plugin"]`（便于搜索）；`"type": "module"`；依赖 `@arcships/rutis-sdk`。
- **代码形式**：发布编译好的 JavaScript 加 `.d.ts`；开发时运行时可以直接加载 TypeScript 源码（tsx）。
- **行名**：包名，例如 `weather-plugin` 或 `@org/weather`，从宿主的 Node 项目目录解析。

### 5.3 Python 插件

- 模块提供 `apply(ctx, config)`，或者 `plugin = define_plugin(...)`，写法不变。
- **pyproject.toml**：依赖 `rutis-sdk`；用入口点注册插件：

  ```toml
  [project.entry-points."rutis.plugins"]
  weather = "weather_plugin"
  ```

- **行名**：`py:<名字>`，先按入口点查找（这样也能拿到包的版本，用来判断解析结果是否过期），找不到再按模块名。开发中未安装的模块仍可以按模块名加载。
- 运行时用宿主配置的解释器（第 6.2 节），插件及其依赖装在那个环境里。

### 5.4 值与行为规则

两种语言相同，已有文档的内容不变：数据按值传递、函数和对象按引用传递、同步调用期间可能被重入、配置变化导致插件重启。教程统一讲一遍。

## 6. 通用宿主：rutis-host

一个不需要写 Rust 的宿主。它把内核、loader、本机运行时、承载和 link 组装好，由一份配置驱动。它既是插件作者的本地开发环境，也可以直接在生产中使用；需要自定义 Rust 服务的应用照旧用 Rust 嵌入。

### 6.1 命令

| 命令 | 作用 |
| --- | --- |
| `rutis-host run [rutis.json]` | 按配置运行 |
| `rutis-host dev` | 开发模式：在插件项目里直接运行；自动把当前项目作为一行加载，监听文件变化并重载；开启开发通道（`rutis-dev`） |
| `rutis-host check [rutis.json]` | 校验配置：解析每一行，打印插件的配置 Schema、依赖和提供的服务，报告不兼容（插件 API、协议版本、缺少的运行时包） |
| `rutis-host new <名字> --lang node\|python` | 从模板创建插件项目 |

### 6.2 配置 `rutis.json`

```json
{
  "id": "main",
  "runtimes": {
    "node": { "project": "." },
    "py": { "project": ".", "python": ".venv/bin/python" }
  },
  "rows": [
    { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "py:fake_llm" },
    { "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis", "export": ["weather"] } }
  ]
}
```

- **`runtimes`**：要启动哪些本机运行时。`node.project` 是 Node 项目目录，插件包从这里解析，`@arcships/rutis-runtime` 也必须装在这里。`py.python` 是解释器，默认依次尝试 `$VIRTUAL_ENV/bin/python`、`./.venv/bin/python`、`python3`，`rutis-runtime` 必须装在这个环境里。缺少运行时包时，启动失败并给出安装命令。
- **`rows`**：沿用 rutis-loader 的行格式（`isolate`、`inject`、`peer:` 行、`rutis-bridge/peer` 节点行等不变）。
- **跨语言共享的服务名**：任何一行在 `provides` 里声明的名字都自动登记为共享；需要额外共享的名字写在 `"shared": [...]`。这样插件作者不需要理解 `register_shared`。
- **凭据**不写在配置里，从环境变量读取（`RUTIS_INTEROP_TOKEN` 等，改名为 `RUTIS_TOKEN` / `RUTIS_CA` / `RUTIS_CERT` / `RUTIS_KEY`）。
- 通用宿主自己不提供任何 Rust 服务；插件之间共享服务，或者经 link 使用其他节点的服务。

### 6.3 开发模式的行为

- 在插件项目目录运行 `rutis-host dev`：读取 `package.json` 或 `pyproject.toml`，把这个插件作为一行；项目里的 `rutis.dev.json`（可选）可以添加配置、假服务行（例如 `py:fake_llm` 或一个本地 JS 文件）以及其他插件。
- **文件变化后的重载**：
  - Python：重新导入这一行的模块（现有行为）。
  - Node 叶子插件：**新增**按行重新导入，入口模块用带版本号的 URL 绕过模块缓存，与 Python 对齐；它导入的其他模块不重新导入，这一点与 Python 相同。
  - Node 的 Cordis 插件：仍然重启运行时。
- 每个运行时的 stdout/stderr 带前缀输出；插件失败时显示行的状态和原因。
- **以后**：`rutis-host dev --join wss://…`，让开发机作为节点连到一个真实的应用，应用通过 `peer:<开发机>/<插件>` 行加载正在开发的插件，直接用上应用的真实服务。这只需要组合已有的 link 和 host，放在第二阶段。

### 6.4 分发

- GitHub Release：Linux（x86_64 / aarch64，gnu 与 musl）和 macOS（x86_64 / aarch64）的二进制。
- npm：`@arcships/rutis-host`，按平台拆成可选依赖（与 esbuild 的方式相同），它依赖 `@arcships/rutis-runtime`，所以 `npx @arcships/rutis-host dev` 开箱即用。
- PyPI：`rutis-host`，用 maturin 的二进制 wheel，依赖 `rutis-runtime`。
- crates.io：`cargo install rutis-host`。
- Windows：本机运行时依赖 Unix，Windows 上的插件开发用 WSL；宿主本身能在 Windows 上通过 link 连接远程运行时，但这一版不分发 Windows 二进制。

## 7. 测试工具

放在各自的 SDK 里，不需要宿主，也不需要运行时进程：

```ts
import { load } from '@arcships/rutis-sdk/testing'
import plugin from '../src/index.ts'

const t = await load(plugin, {
  config: { city: 'Oslo' },
  services: { llm: { ask: async q => 'sunny' } },
})
assert.equal(await t.service('weather').today(), 'sunny in Oslo')
await t.unload()            // 运行清理，检查提供的服务都已撤销
```

```python
from rutis_sdk.testing import load

async def test_weather():
    async with load(weather_plugin, config={"city": "Oslo"}, services={"llm": FakeLlm()}) as t:
        assert await t.service("weather").today() == "sunny in Oslo"
```

工具会检查：

- `inject` 声明的服务都已提供，没有声明的服务插件拿不到；
- 提供的服务与 `provides` 声明的形状一致；
- 卸载时清理函数都运行了；
- 严格模式下，跨边界的参数和返回值按真实规则往返一次（数据复制、函数变成引用），提前暴露"在进程内能用、跨进程就坏"的写法。

集成测试用 `rutis-host`：在测试里启动宿主，或者在 CI 里运行 `rutis-host check`。

## 8. 开发者的完整流程

### 8.1 Node / TypeScript 插件作者

```bash
npx @arcships/rutis-host new weather --lang node   # 模板：package.json、src/index.ts、test/、rutis.dev.json、CI 工作流
cd weather && npm install
npm test                                           # node --test，使用 @arcships/rutis-sdk/testing
npx rutis-host dev                                 # 本地运行，改代码自动重载
npm publish                                        # 模板里的 CI 在打 tag 时测试并发布
```

### 8.2 Python 插件作者

```bash
uvx rutis-host new weather --lang python           # 模板：pyproject.toml、weather/、tests/、rutis.dev.json、CI 工作流
cd weather && uv sync                              # 开发依赖包含 rutis-sdk、rutis-runtime、rutis-host
uv run pytest                                      # 使用 rutis_sdk.testing
uv run rutis-host dev
uv build && uv publish
```

### 8.3 运维：使用插件

```bash
npm install weather-plugin                         # 装进宿主的 Node 项目（与 @arcships/rutis-runtime 同一处）
uv pip install --python .venv weather-plugin       # 或者装进宿主的 Python 环境
# 在 rutis.json 加一行 { "id": "weather", "name": "weather-plugin", ... }
rutis-host check && rutis-host run
```

Rust 宿主作者用 `rutis-loader` 加 `rutis-polyglot` 嵌入，配置的行格式与 `rutis.json` 相同。

## 9. 版本与发布

### 9.1 rutis 自己的包

- **发布列车**：除内核 `rutis` 和 dylib 相关的 crate 之外，第 4 节的所有包（三种语言的 SDK、运行时、宿主，以及 bridge、承载、polyglot、loader、cordis、session、channel）使用同一个版本号，一次一起发布。用户只需记住"用同一个版本"。
- **兼容不靠版本号对齐**：会话协议在握手时检查，插件 API 由 `api` 标记检查。列车只是让版本号易于理解。
- **新名字从 0.1.0 开始**。`rutis-interop`（crates.io 0.2.0、npm 0.2.0）不再发布新版本：npm 上标记弃用；crates.io 上更新 README 指向新包。
- **一个工作流**：`release-train.yml`，tag 为 `train-vX.Y.Z`，按依赖顺序发布所有 crate（crates.io 已有的版本跳过），然后发布 npm 包（各平台的宿主二进制包先发）和 PyPI 包，最后上传 GitHub Release 的二进制。它取代 #147 里的 `publish-bridge.yml` 和现有的 `publish-interop.yml`、`publish-loader.yml`。
- `rutis-cli` 的二进制发布现在用 `v*` tag，保持不变；内核仍用 `rutis-v*`。

### 9.2 插件作者的包

- 遵循各自生态的 semver。依赖 `rutis-sdk` 的主版本（例如 `^0.1` 或 `^1`），由插件 API 标记兜底兼容。
- 模板自带 CI：测试、`rutis-host check`、打 tag 时发布到 npm 或 PyPI（使用 trusted publishing）。

## 10. 环境要求

| | 要求 | 备注 |
| --- | --- | --- |
| 操作系统 | Linux、macOS | 本机运行时依赖 Unix；Windows 用 WSL |
| Node | 当前写的是 26 | 代码里没有找到依赖 26 的特性，**实施时用 Node 24（LTS）跑一遍测试，能过就放宽到 24** |
| Python | 3.12 及以上 | |
| Rust（嵌入宿主） | MSRV 1.85 | 不变 |

## 11. 文档结构

教程最后写，按角色组织在 `docs/guide/`：

1. 写一个 TypeScript 插件：从 `new` 到发布
2. 写一个 Python 插件：从 `new` 到发布
3. 插件 API 参考：`ctx`、声明、值的传递、可重入、生命周期
4. 运行宿主：`rutis-host` 与 `rutis.json`
5. 连接多台机器：节点、远程运行时、`peer:` 行、凭据与 TLS
6. 在 Rust 应用里嵌入：`rutis-loader`、`rutis-polyglot`
7. 把 Cordis 应用接成节点：`@arcships/rutis-bridge`

各包在 npm、PyPI、crates.io 上的 README 用英文（面向生态里的读者），各指向对应的指南。指南先写中文。

## 12. 实施阶段

| 阶段 | 内容 | 完成标准 |
| --- | --- | --- |
| P0 拆分与改名 | Rust：`rutis-session`、`rutis-polyglot`（并入 runtime-local 和 RuntimeAccess）、`rutis-cordis`，删除 `rutis-interop` 与弃用接口；Node：`rutis-sdk`、`rutis-runtime`、`rutis-bridge`；Python：`rutis-sdk`、`rutis-runtime`；环境变量改名 | 所有现有测试在新结构下通过；依赖图符合 4.2 |
| P1 SDK | 插件 API 标记与检查；测试工具（两种语言）；类型；Python 入口点与版本；Node 叶子插件按行重载 | SDK 有自己的测试；`load(...)` 能测仓库里的示例插件 |
| P2 宿主 | `rutis-host` 的 run / dev / check / new；`rutis.json`；两套模板；运行时包缺失时的诊断 | 用模板新建的插件不写 Rust 就能 `dev`、测试、`check` |
| P3 分发与发布 | 发布列车工作流；npm 平台包；maturin wheel；弃用旧包 | 在测试用的 registry 上（Verdaccio、TestPyPI、crates.io dry run）走通一次完整发布 |
| P4 文档 | 第 11 节的指南；各包 README | 按教程从零走一遍，不看源码也能完成 |
| 之后 | `rutis-host dev --join`；Windows 二进制 | — |

#147（CI 修复、浸泡测试、冒烟示例、README）先合并，但不按它发布；它的发布工作流和迁移说明在 P0/P3 中按新名字重写。

## 13. 待确认

1. `rutis-polyglot` 作为管理语言运行时的 crate 名（备选 `rutis-lang`）。
2. 写插件的包在三种语言里都叫 `rutis-sdk`；运行时进程的包叫 `rutis-runtime`；通用宿主叫 `rutis-host`。
3. 通用宿主正式作为产品（可以直接用于生产，而不仅是开发工具）。
4. 发布列车：一个版本号、一个 `train-v*` tag、一个工作流。
5. 插件 API 用独立的整数版本标记（`api`）。
6. 各包 README 用英文，指南先写中文。
7. 第一次发布在 P0–P3 完成之后；在此之前不发布 interop 0.3 / bridge 0.1。
