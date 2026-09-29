# rutis 挂载 Cordis 插件：路线图

依据：[需求](requirements-protocol-plugins.md)、[设计](design-protocol-plugin-mount.md)。rutis 内核与 Cordis 不修改。每项工作都要有跨进程自动化测试，并与 Cordis 原生行为对照。

## 1. 分工

仓库里与 Cordis 互通的实现按方向分工，不合并：

| 方向 | 负责 | 状态 |
| --- | --- | --- |
| rutis 应用挂载 Cordis 插件（主） | `rutis-interop` + `interop/node`（本路线图） | 开发中 |
| Cordis / dsh 宿主使用 Rust 服务 | `rutis-cordis` + `host/`（[dsh 桥设计](design-dsh-bridge-2026-08-21.md)、[aimux 决策](decision-aimux-llm-plugin-2026-08-23.md)） | 在用，不受本路线图影响 |
| `rutis-interop` 的反方向（Cordis 应用挂载 rutis 插件） | `build/rust.rs`、`server.rs` | **冻结**：保留代码与测试，不再增加能力；与 `rutis-cordis` 方向重合 |

传输层暂不统一（两套各约 800 行）。`rutis-interop` 稳定后再评估是否合并。

## 2. 已完成

| 能力 | 测试 |
| --- | --- |
| 构建期生成 Rust 类型；同步 / 异步方法、业务错误、依赖就绪、清理顺序、启动失败、隔离挂载 | `examples/native-mount/src/main.rs` |
| 服务换值跟随：新读取得到新代理，旧快照不变，撤销与重新注册，句柄回收 | `crates/rutis-interop/tests/service_projection.rs` |
| 句柄固定指向原对象；先清理后排空；进程退出后调用失败 | `crates/rutis-interop/tests/process_exit.rs` |
| 协议 v1：invoke / await 分离、函数与异步结果引用、计数释放、同步调用链内的反向调用、`SyncWaitCycle`、后台执行器 | `crates/rutis-interop/tests/rpc_callbacks.rs`、`src/rpc/tests.rs`、`interop/node/test/peer.test.mjs` |
| 错误对象图往返 | `crates/rutis-interop/tests/error_shape.rs`、`interop/node/test/errors.test.mjs` |

## 3. 后续工作

### W1 真实插件基线（先做）

用真实插件代替夹具，找出实际缺口，再决定补什么。

- **目标插件**：npm 上已发布的 dsh 官方插件（`@deepseek-ai/dsh-settings`、`dsh-credentials`、`dsh-invariants`、`dsh-attachment` 等，版本 `0.1.1-rc.2`，依赖闭包无缺口）。能取得 `deepseek-harness/plugin-reference` 语料后再扩大样本。
- **分级**（沿用 [dsh 桥](design-dsh-bridge-2026-08-21.md) §九的口径）：L0 能生成绑定并装载；L1 服务可调用；L2 主要用法行为与原生一致；L3 事件与生命周期一致；L4 带活对象的载荷一致。
- **产出**：每个插件的分级结果和缺口清单（按"生成器不支持 / 协议不支持 / 边界规则之外"分类），写入本文。

已知的第一批缺口（来自对 `dsh-settings` 的初步查看）：`class X extends Service` 形式的服务注册、返回对象（`SettingsScope`）、泛型接口、schemastery schema 作为配置。

### W2 工程防护（与 W1 并行）

吸收 `rutis-cordis` 已验证的做法：

| 项 | 内容 |
| --- | --- |
| 超时与取消 | 调用方声明超时；取消传播到 Node 侧；迟到应答计数丢弃 |
| 握手能力协商 | 握手交换能力集；装载期检查插件所需服务，缺失时明确报错 |
| 断连记录 | 断连时记录在途调用和计数，便于排查 |

### W3 按缺口补能力

补什么由 W1 的结果决定，不预先排机制。候选项：

- **生成器**：`Service` 子类注册、返回有状态对象（协议新增 object 引用）、函数类型参数、更多数据类型、schemastery 配置。
- **事件**：按设计 §6 按组转发。默认只转发纯 `emit` 事件；waterfall 事件绝不被动订阅（`rutis-cordis` 实测：不调用 `next()` 的监听会否决整条链）。
- **活对象载荷**：参照 dsh 桥的替身表，逐个事件决定活对象的过线表示。

### W4 包级接入

用应用清单（`Cargo.toml` 的 `package.metadata`）配置插件包与版本，替代 `build.rs` 中的源码路径；按 npm 包解析插件入口和类型声明。

## 4. 验证

```sh
npm --prefix interop/node ci
npm --prefix interop/node test
cargo test -p rutis-interop -p native-mount-example
cargo clippy -p rutis-interop -p native-mount-example --all-targets -- -D warnings
```

## 5. 性能记录

同步方法往返（2026-09-28，Node 26.8.1、Intel Core Ultra X7 358H，预热 200 次后调用 3,000 次）：

| Rust 构建 | p50 / p95 / p99（μs） | 均值（μs） |
| --- | --- | --- |
| debug | 65.3 / 100.3 / 181.0 | 70.3 |
| release | 32.7 / 59.9 / 101.4 | 36.6 |

复现：`cargo build --release -p native-mount-example --bin rutis-counter && node interop/node/bench/sync-call.mjs target/release/rutis-counter`。服务读取本身不经过 IPC；每次方法调用一次往返。W1 用真实插件负载评估是否需要优化。
