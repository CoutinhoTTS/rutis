# 面向开发者的包、工具与流程

日期：2026-10-06。状态：设计，待确认。

## 1. 为什么现在做

多语言插件（M1/M2）和网络栈（#145）已经完成，但都还没发布。现在的包是按实现长出来的，不是按开发者要做的事划分的：

- `rutis-interop` 这个名字说不出它是什么。它同时装着跨进程会话协议、语言运行时管理、Cordis 静态挂载和代码生成；网络栈又拆成了 `rutis-channel`、`rutis-bridge`、三个 `rutis-transport-*` 和 `rutis-runtime-local`。这些拆分是为了让内部依赖不成环，开发者并不需要知道这些层次，却得在七八个 crate 里挑。
- 写一个 Node 叶子插件要 `import` 整个 Node 运行时（`@arcships/rutis-interop`，带着 Cordis、tsx、typescript、ws）；Python 的插件 SDK 和运行时混在一个仓库内的 `rutis_runtime` 包里，也没有发布。
- 没有通用的宿主程序：不写 Rust 就跑不起来一个插件。
- 插件作者没有测试工具，也没有发布和兼容的约定。

我们还没有多少下游，破坏性调整的代价最低就在现在。这份设计确定所有面向开发者的包、命名、工具和流程；教程放在最后，等这些都就绪再写。

## 2. 谁在用 rutis

| 角色 | 要做的事 | 用什么 |
| --- | --- | --- |
| 插件作者 | 写插件、测试、发布 | TypeScript/JavaScript、Python、Rust |
| 宿主作者 | 做一个能加载插件的应用 | Rust 嵌入，或者不写代码（用通用宿主） |
| 运维 | 安装插件、写配置、把多台机器连起来 | 配置文件 |
| Cordis 应用作者 | 让已有的 Cordis 应用和 rutis 互通 | TypeScript/JavaScript |

## 3. 原则

1. **包按"谁来装、为什么装"划分，不按内部分层划分。** 内部层次是模块；可选的部分用 feature 或可选依赖来选。只有依赖分量差别很大、安装者又不同的时候才拆成两个包。
2. **按用途命名。** 开发者看到包名就知道该不该装它。
3. **插件作者只依赖一个轻量的包。** 运行插件需要的东西由宿主安装，插件不会锁死它的版本。
4. npm 包保持 `@arcships` scope；crates.io 和 PyPI 用 `rutis` 名字。下面用到的名字都已确认未被占用（2026-10-06）。

## 4. 包

### 4.1 总览

| 谁装 | Rust（crates.io） | Node（npm） | Python（PyPI） |
| --- | --- | --- | --- |
| 插件作者 | `rutis-sdk`（已有，dylib 插件） | `@arcships/rutis` | `rutis` |
| 宿主（运行插件） | `rutis`、`rutis-loader`、`rutis-bridge` | `@arcships/rutis-runtime` | （同上，`rutis`） |
| 不写 Rust 的宿主 | `rutis-host`（二进制） | `@arcships/rutis-host` | `rutis-host` |
| Cordis 应用作者 | — | `@arcships/rutis-runtime`（`/bridge`） | — |

Rust 四个，Node 三个，Python 两个。

### 4.2 Rust

| crate | 是什么 | 变化 |
| --- | --- | --- |
| `rutis` | 内核 | 不变 |
| `rutis-loader` | 按配置加载插件（行） | 依赖改为 `rutis-bridge`（可选） |
| `rutis-bridge` | 把 rutis 接到其他进程、其他语言、其他机器 | **合并**：`rutis-channel`、`rutis-interop`、`rutis-bridge`、`rutis-transport-local` / `-memory` / `-websocket`、`rutis-runtime-local` |
| `rutis-host` | 通用宿主程序（第 6 节） | 新增 |

`rutis-sdk` 和 dylib 相关的 crate 不在这次范围内，不变。

**`rutis-bridge` 的 features：**

| feature | 内容 | 默认 |
| --- | --- | --- |
| （总是有） | 通道契约、会话协议、link、身份、节点功能（export / import / host / events）、memory 承载、本机承载（Unix）、远程语言运行时 | — |
| `node` | 本机 Node 运行时 | 开 |
| `python` | 本机 Python 运行时 | 关 |
| `websocket` | WebSocket 承载（rustls、tungstenite） | 关 |
| `cordis` | 静态挂载 Cordis 插件与构建期 Rust 绑定生成（syn、quote、toml） | 关 |
| `testing` | 通道契约测试、会话 / 运行时 / 节点一致性测试 | 关 |

内部是模块：`channel`、`session`、`link`、`transport::{local, memory, websocket}`、`runtime`、`cordis`。原来为了不成环而拆开的层次，现在只是模块之间的依赖。

- 实现新承载的第三方只需依赖 `rutis-bridge`（`default-features = false`），用 `testing` 跑通道契约测试。
- 弃用的接口（`CordisRuntime*`、`RuntimePlugin::node` / `python` / `launcher` 等）直接删除，不再保留别名。
- `rutis-loader` 的 features：`node`、`python`（本机运行时的行），`peer`（在其他节点上运行的行），各自打开 `rutis-bridge` 对应的部分。

### 4.3 Node

| 包 | 谁装 | 内容 | 依赖 |
| --- | --- | --- | --- |
| `@arcships/rutis` | 插件作者 | `definePlugin`、TypeScript 类型、测试工具（`@arcships/rutis/testing`） | **无** |
| `@arcships/rutis-runtime` | 宿主（装在宿主的 Node 项目里）；Cordis 应用作者 | 运行时进程（runner、`listen:` 守护、各种通道、会话、Cordis 宿主）；`/bridge`：把 Cordis 应用接成 rutis 节点（`Link`、`Export`、`Import`、`Host`、`Events`）；`rutis-bridge` 的 `cordis` feature 在构建期使用的绑定生成 | `@deepseek-ai/cordis`、`tsx`、`ws` |
| `@arcships/rutis-host` | 不写 Rust 的人（`npx`） | 宿主二进制的分发；平台二进制放在可选依赖 `@arcships/rutis-host-<平台>` 里，这是 npm 分发二进制的通行做法（esbuild 同理），用户不会直接接触 | `@arcships/rutis-runtime` |

SDK 必须单独成包：插件作者不应该因为写一个插件而装上 Cordis、tsx、ws。`definePlugin` 用 `Symbol.for` 做标记（现在就是如此），SDK 和运行时之间不需要共享模块实例。

`@arcships/rutis-interop` 在 npm 上标记弃用，说明指向新包。

### 4.4 Python

| 包（导入名） | 内容 | 依赖 |
| --- | --- | --- |
| `rutis`（`rutis`） | `define_plugin`、类型（`py.typed`）、测试工具（`rutis.testing`）；运行时进程：`python -m rutis`；可选依赖 `network`（WebSocket，远程运行时） | 无；`network` 需要 `websockets>=13` |
| `rutis-host` | 宿主二进制的 wheel（`uvx rutis-host`、`uv add --dev rutis-host`） | `rutis` |

Python 的运行时没有任何依赖，SDK 和运行时放在同一个包里不会让插件作者多装任何东西，所以不拆。插件依赖 `rutis`，宿主的 Python 环境里装了插件，也就有了运行时。

### 4.5 为什么 Node 拆、Python 不拆

只看一点：插件作者装 SDK 时，会不会被迫装上不需要的东西。Node 运行时依赖 Cordis、tsx、ws，所以拆；Python 运行时什么都不依赖，所以不拆。

## 5. 插件的约定

### 5.1 插件 API 版本

SDK 在插件上打一个整数标记 `api`（从 1 开始），表示插件按哪一版插件 API 编写；运行时声明自己支持的范围。加载时不兼容给出明确的错误，例如"插件 weather 需要插件 API 2，这个运行时只支持 1；请升级 @arcships/rutis-runtime"。

- 插件 API 的版本与包版本无关，只在插件看到的接口（`ctx` 的方法、声明格式、值的传递规则）发生不兼容变化时才增加。
- 不经过 SDK 的 Cordis 插件视为 API 1。

### 5.2 Node 插件

- **叶子插件**（推荐）：`export default definePlugin({ inject, provides, config, apply })`，与 Python 插件同构。
- **Cordis 插件**：照常编写；要提供给 rutis 的服务，在 `package.json` 里声明 `"rutis": { "provides": { "weather": { "today": "sync" } } }`。
- **package.json**：依赖 `@arcships/rutis`；`"keywords": ["rutis-plugin"]`；`"type": "module"`。
- **代码形式**：发布编译好的 JavaScript 加 `.d.ts`；开发时运行时可以直接加载 TypeScript 源码（tsx）。
- **行名**：包名，从宿主的 Node 项目目录解析。

### 5.3 Python 插件

- 模块提供 `apply(ctx, config)`，或者 `plugin = rutis.define_plugin(...)`。
- **pyproject.toml**：依赖 `rutis`；用入口点注册插件：

  ```toml
  [project.entry-points."rutis.plugins"]
  weather = "weather_plugin"
  ```

- **行名**：`py:<名字>`。先按入口点查找（这样也能拿到包的版本，用来判断解析结果是否过期），找不到再按模块名，开发中未安装的模块仍可以加载。
- 运行时用宿主配置的解释器（第 6.2 节），插件及其依赖装在那个环境里。

### 5.4 值与行为规则

两种语言相同，已有文档的内容不变：数据按值传递，函数和对象按引用传递，同步调用期间可能被重入，配置变化导致插件重启。教程里统一讲一遍。

## 6. 通用宿主：rutis-host

一个不需要写 Rust 的宿主，把内核、loader 和 `rutis-bridge` 组装好，由一份配置驱动。它是插件作者的本地开发环境，也可以直接用于部署；需要自定义 Rust 服务的应用照旧用 Rust 嵌入。

### 6.1 命令

| 命令 | 作用 |
| --- | --- |
| `rutis-host run [rutis.json]` | 按配置运行 |
| `rutis-host dev` | 开发模式：在插件项目里直接运行；自动把当前项目作为一行，监听文件变化并重载；开启开发通道（`rutis-dev`） |
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

- **`runtimes`**：要启动的本机运行时。
  - `node.project` 是 Node 项目目录，插件包从这里解析，`@arcships/rutis-runtime` 也装在这里。
  - `py.python` 是解释器，默认依次尝试 `$VIRTUAL_ENV/bin/python`、`./.venv/bin/python`、`python3`，`rutis` 必须装在这个环境里。
  - 缺少运行时包时，启动失败并给出安装命令。
- **`rows`**：沿用 rutis-loader 的行格式（`isolate`、`inject`、`peer:` 行、`rutis-bridge/peer` 节点行等不变）。
- **跨语言共享的服务名**：任何一行在 `provides` 里声明的名字自动登记为共享；需要额外共享的名字写在 `"shared": [...]`。插件作者不需要理解 `register_shared`。
- **凭据**不写在配置里，从环境变量读取：`RUTIS_INTEROP_TOKEN` 等改名为 `RUTIS_TOKEN` / `RUTIS_CA` / `RUTIS_CERT` / `RUTIS_KEY`。
- 通用宿主自己不提供 Rust 服务；插件之间共享服务，或者经 link 使用其他节点的服务。

### 6.3 开发模式

- 在插件项目目录运行 `rutis-host dev`：读取 `package.json` 或 `pyproject.toml`，把这个插件作为一行。项目里可选的 `rutis.dev.json` 可以添加配置、假服务行（例如一个本地的 `fake_llm.py`）以及其他插件。
- **文件变化后的重载**：
  - Python：重新导入这一行的模块（现有行为）。
  - Node 叶子插件：**新增**按行重新导入，入口模块用带版本号的 URL 绕过模块缓存，与 Python 对齐；它导入的其他模块不重新导入，这一点也与 Python 相同。
  - Node 的 Cordis 插件：仍然重启运行时。
- 每个运行时的 stdout/stderr 带前缀输出；插件失败时显示行的状态和原因。
- **以后**：`rutis-host dev --join wss://…`，让开发机作为节点连到一个真实的应用，应用用 `peer:<开发机>/<插件>` 行加载正在开发的插件，直接用上应用的真实服务。这只是组合已有的 link 和 host，放在第二阶段。

### 6.4 分发

- GitHub Release：Linux（x86_64 / aarch64）与 macOS（x86_64 / aarch64）的二进制。
- npm：`npx @arcships/rutis-host`；PyPI：`uvx rutis-host`；crates.io：`cargo install rutis-host`。
- Windows：本机运行时依赖 Unix，插件开发用 WSL；这一版不分发 Windows 二进制。

## 7. 测试工具

放在 SDK 里，不需要宿主，也不需要运行时进程：

```ts
import { load } from '@arcships/rutis/testing'
import plugin from '../src/index.ts'

const t = await load(plugin, {
  config: { city: 'Oslo' },
  services: { llm: { ask: async q => 'sunny' } },
})
assert.equal(await t.service('weather').today(), 'sunny in Oslo')
await t.unload()            // 运行清理，检查提供的服务都已撤销
```

```python
from rutis.testing import load

async def test_weather():
    async with load(weather_plugin, config={"city": "Oslo"}, services={"llm": FakeLlm()}) as t:
        assert await t.service("weather").today() == "sunny in Oslo"
```

工具检查：

- `inject` 声明的服务都已提供，没有声明的服务插件拿不到；
- 提供的服务与 `provides` 声明的形状一致；
- 卸载时清理函数都运行了；
- 严格模式下，跨边界的参数和返回值按真实规则往返一次（数据复制，函数变成引用），提前暴露"在进程内能用、跨进程就坏"的写法。

集成测试用 `rutis-host`：在测试里启动宿主，或者在 CI 里运行 `rutis-host check`。

## 8. 开发者的完整流程

### 8.1 Node / TypeScript 插件作者

```bash
npx @arcships/rutis-host new weather --lang node   # 模板：package.json、src/index.ts、test/、rutis.dev.json、CI 工作流
cd weather && npm install                          # 依赖 @arcships/rutis；开发依赖 @arcships/rutis-host
npm test                                           # node --test，使用 @arcships/rutis/testing
npx rutis-host dev                                 # 本地运行，改代码自动重载
npm publish                                        # 模板里的 CI 在打 tag 时测试并发布
```

### 8.2 Python 插件作者

```bash
uvx rutis-host new weather --lang python           # 模板：pyproject.toml、weather/、tests/、rutis.dev.json、CI 工作流
cd weather && uv sync                              # 依赖 rutis；开发依赖 rutis-host
uv run pytest                                      # 使用 rutis.testing
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

Rust 宿主作者用 `rutis-loader` 和 `rutis-bridge` 嵌入，行的格式与 `rutis.json` 相同。

## 9. 版本与发布

### 9.1 rutis 自己的包

- **发布列车**：除内核 `rutis` 和 dylib 相关的 crate 之外，第 4 节的所有包使用同一个版本号，一次一起发布。用户只需记住"用同一个版本"。
- **兼容不靠版本号对齐**：会话协议在握手时检查，插件 API 由 `api` 标记检查。列车只是让版本号易于理解。
- **新名字从 0.1.0 开始**：`rutis-bridge` 本来就还没发布过；`rutis-loader` 继续它自己的序号，并入列车后取列车的版本号。
- **tag**：列车用 `vX.Y.Z`（最常见的写法）；现在占用 `v*` 的 `rutis-cli` 二进制发布改为 `cli-vX.Y.Z`；内核保持 `rutis-vX.Y.Z`。
- **一个工作流**：`release.yml` 按依赖顺序发布 crate（crates.io 已有的版本跳过），然后发布 npm 包（各平台的宿主二进制包先发）和 PyPI 包，最后上传 GitHub Release 的二进制。它取代 #147 里的 `publish-bridge.yml` 以及现有的 `publish-interop.yml`、`publish-loader.yml`。
- **旧包**：`rutis-interop` 不再发新版本。npm 上标记弃用；crates.io 上发布最后一个只含 README 的版本，指向 `rutis-bridge`。

### 9.2 插件作者的包

- 遵循各自生态的 semver，依赖 SDK 的主版本（例如 `@arcships/rutis@^0.1`），由插件 API 标记兜底兼容。
- 模板自带 CI：测试、`rutis-host check`、打 tag 时用 trusted publishing 发布到 npm 或 PyPI。

## 10. 环境要求

| | 要求 | 备注 |
| --- | --- | --- |
| 操作系统 | Linux、macOS | 本机运行时依赖 Unix；Windows 用 WSL |
| Node | 当前写的是 26 | 代码里没有找到依赖 26 的特性。实施时用 Node 24（LTS）跑一遍测试，能过就放宽到 24 |
| Python | 3.12 及以上 | |
| Rust（嵌入宿主） | MSRV 1.85 | 不变 |

## 11. 文档结构

教程最后写，按角色组织在 `docs/guide/`：

1. 写一个 TypeScript 插件：从 `new` 到发布
2. 写一个 Python 插件：从 `new` 到发布
3. 插件 API 参考：`ctx`、声明、值的传递、可重入、生命周期
4. 运行宿主：`rutis-host` 与 `rutis.json`
5. 连接多台机器：节点、远程运行时、`peer:` 行、凭据与 TLS
6. 在 Rust 应用里嵌入：`rutis-loader`、`rutis-bridge`
7. 把 Cordis 应用接成节点：`@arcships/rutis-runtime/bridge`

各包在 npm、PyPI、crates.io 上的 README 用英文，指向对应的指南；指南先写中文。

## 12. 实施阶段

| 阶段 | 内容 | 完成标准 |
| --- | --- | --- |
| P0 合并与改名 | Rust：合并为 `rutis-bridge`（features 见 4.2），删除旧 crate 与弃用接口；Node：拆出 `@arcships/rutis`，运行时改名为 `@arcships/rutis-runtime`；Python：包名与导入名改为 `rutis`；环境变量改名 | 所有现有测试在新结构下通过；每个 feature 组合都能单独编译 |
| P1 SDK | 插件 API 标记与检查；测试工具（两种语言）；类型；Python 入口点与版本；Node 叶子插件按行重载；验证 Node 24 | SDK 有自己的测试；`load(...)` 能测仓库里的示例插件 |
| P2 宿主 | `rutis-host` 的 run / dev / check / new；`rutis.json`；两套模板；运行时包缺失时的诊断 | 用模板新建的插件不写 Rust 就能 `dev`、测试、`check` |
| P3 分发与发布 | 发布列车工作流；npm 平台包；maturin wheel；旧包弃用 | 在测试用的 registry 上（Verdaccio、TestPyPI、crates.io dry run）走通一次完整发布 |
| P4 文档 | 第 11 节的指南与各包 README | 按教程从零走一遍，不看源码也能完成 |
| 之后 | `rutis-host dev --join`；Windows 二进制 | — |

#147（CI 修复、浸泡测试、冒烟示例、README）先合并，但不按它发布；它的发布工作流和迁移说明在 P0/P3 中按新结构重写。

## 13. 决定

| # | 决定 | 理由 |
| --- | --- | --- |
| 1 | Rust 只保留 `rutis`、`rutis-loader`、`rutis-bridge`、`rutis-host`；可选部分用 feature | 拆分应当服务于使用者的选择；内部层次用模块表达 |
| 2 | 插件作者的包：Node `@arcships/rutis`，Python `rutis`；Node 运行时 `@arcships/rutis-runtime`；宿主 `rutis-host` | 插件作者只装一个最轻的包，名字就是 rutis 本身 |
| 3 | 通用宿主作为正式产品；第一版覆盖开发和简单部署，生产运维能力（服务管理、日志配置、指标）放到后续版本 | 它是"不写 Rust 也能用 rutis"的入口，但第一版不必包揽部署 |
| 4 | 发布列车：一个版本号，tag `vX.Y.Z`，一个工作流；`rutis-cli` 改用 `cli-v*` | 用户只需对齐一个版本号；`v*` 留给主产品 |
| 5 | 插件 API 用独立的整数版本，从 1 开始 | 包版本会因为与插件无关的修改而变化，不适合用来判断插件能否加载 |
| 6 | 各包 README 用英文，指南先写中文 | registry 上的读者面向整个生态；指南先保证质量，再翻译 |
| 7 | P0–P3 完成后第一次发布；在此之前不发布 interop 0.3 / bridge 0.1 | 避免刚发布就改名；名字一旦发布就很难收回 |
