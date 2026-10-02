# rutis-dsh：在 rutis 宿主里运行 dsh

`rutis-dsh up` 启动 dsh 的完整 web 界面。dsh 跑在 rutis 启动和管理的 Node 进程里，模型调用由同一进程中的 [aimux-llm](../aimux-llm) 提供。接入机制见 [rutis-interop](../rutis-interop/README.md)；需要 Unix 与 Node（CI 使用 Node 26）。

## 使用

```sh
npm --prefix crates/rutis-dsh/dsh ci
DEEPSEEK_API_KEY=... cargo run -p rutis-dsh -- up
```

dsh 在 stderr 打印带登录令牌的地址（`dsh web: http://127.0.0.1:3080/?token=…`），默认会打开浏览器。

```text
rutis-dsh up [--profile <name>] [dsh 选项...]
```

| 选项 | 说明 |
| --- | --- |
| `--profile <name>` | `$DSH_HOME/profiles` 下的 profile，默认 `rutis-web`；首次启动时以 dsh-base、dsh-web-app 和 aimux bundle 创建 |
| 其余参数 | 交给 dsh web，例如 `--port 3081`、`--host 0.0.0.0`、`--no-open` |

工作目录即 dsh 的工作区（读取其中的 `.env`）。Ctrl-C 或 dsh 自身退出（如 `--help`）时，rutis 卸载挂载并结束 Node 进程；Node 进程意外结束时 `rutis-dsh` 随之退出。

## 模型路由

模型选择器中的 **aimux (rutis)** 一组由 aimux 提供。路由在 dsh 模型页（设置段 `llm-aimux`）配置，修改即时生效：

| 字段 | 说明 |
| --- | --- |
| 路由名 | dsh 中的 provider 名，不能与其他适配器的路由重名（例如官方的 `deepseek-official`） |
| `provider` | aimux 的 provider，缺省为路由名 |
| `apiKeyEnv` | key 的凭据引用，每次请求经 dsh 凭据服务解析（模型页保存的 key 在这里）；没有凭据服务时读同名环境变量 |
| `displayName` | 显示名 |

不带 key 的路由（包括默认的 `aimux`）使用宿主的兜底 provider：`AIMUX_PROVIDER` / `AIMUX_MODEL`（默认 `deepseek` / `deepseek-chat`）及该 provider 的 key 环境变量（如 `DEEPSEEK_API_KEY`）。模型目录取自兜底 provider，界面中选择的模型按选择生效。

## 组成

| 部分 | 位置 | 作用 |
| --- | --- | --- |
| 启动器 | `dsh/launcher.ts`、`dsh/launcher/boot.ts` | 在挂载的 Cordis Context 中启动 dsh profile（沿用 dsh-app-boot 的启动步骤，不新建 Context、不接管信号与退出）；启动成功、失败和退出以事件告知 rutis |
| aimux bundle | `dsh/aimux`（npm 包 `@rutis/dsh-aimux`） | profile 中的 `llm-aimux` 行：向 dsh-llm 注册路由，模型调用经宿主服务 `aimux` 交给 Rust |
| 宿主服务 | `src/aimux.rs` | `aimux` 的 Rust 实现：每次调用是一个 aimux-llm 流，适配器分批读取；停止读取即取消 |
| 挂载 | `Cargo.toml` | `web`：web 界面；`agent`：不带界面的 dsh agent 组合（测试与从 Rust 驱动 agent） |

## profile 配置（rutis-loader）

`rutis_dsh::profile` 把 dsh 的 profile 配置接到 [rutis-loader](../rutis-loader)，不需要 Node：

| 部分 | 作用 |
| --- | --- |
| `profile::load` | 按 dsh-app-boot 的规则读出各层：bundle 的 patch 文件（解析不到、没有 `dsh.bundle`、版本不兼容的 bundle 跳过并说明原因）、用户层 `cordis.patch.yml`（可编辑）、`$DSH_HOME/cordis.patch.yml`、`--patch`、遥测开关；嵌套 include 展开为最后一层 |
| `profile::UserLayerStore` | 用户层的 `Persist`：持有与 dsh 相同的跨进程写锁（`<profile>/package.json.lock`），比对版本，只重写变化的 patch（其余 patch 的注释保留），读回校验后原子替换 |
| `profile::expr::JsSubset` | dsh 写在 `!!js` 里的 JavaScript 子集；`ctx` 只能读服务名目录里登记的服务 |
| `profile::watch::watch` | 轮询 profile 的文件，变化后重新读层并 reconcile |
| `rutis-dsh dump-config` | 打印合成后的 profile，对照 `dsh --dump-config` |

与 dsh 的一致性由对拍测试保证：YAML 方言对 js-yaml，表达式对 Node，分层对 dsh-app-boot（见 `tests/profile_*.rs`，需要安装 npm 项目）。

```sh
cargo run -p rutis-dsh -- dump-config --profile web
```

## 部署

二进制与 npm 项目一起分发：把 `crates/rutis-dsh/dsh`（含已安装的 `node_modules`，符号链接需展开）复制到目标机器，并用 `RUTIS_INTEROP_ROOT` 指向它（见 rutis-interop README 的“部署”）。仓库内的 npm 项目以 `file:` 依赖引用 `interop/node`；独立部署时可改为 npm 上的 `@arcships/rutis-interop`。

## 从旧桥迁移

此前的 `rutis-dsh up` 启动官方 `dsh` CLI，由插入其 profile 的 `rutis-bridge` 插件经 TCP 调用 Rust（`rutis-cordis` + `host/`）。现在 rutis 是宿主，不再需要单独安装 `dsh`、`rutis-bridge` npm 包和 `RUTIS_DSH_BIN`。路由配置的字段不变（`llm-aimux` 设置段），但保存在新的 profile（`rutis-web`）中，需在模型页重新添加；也可用 `--profile` 指定已有 profile。旧桥转发给 rutis 的 dsh 事件（`HostEvent`）改为由 interop 按声明生成类型化事件。

## 测试

```sh
cargo test -p rutis-dsh                          # web 界面启动 / 失败 / 停止，agent 组合，迁移部署
DEEPSEEK_API_KEY=... cargo test -p rutis-dsh --test agent -- --ignored   # 真实模型的 agent 回合
```
