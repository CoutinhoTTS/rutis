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

### W1 真实插件基线（已完成首轮，2026-09-29）

装置在 `interop/baseline/`：锁定 npm 上已发布的 dsh 官方插件（`0.2.0-rc.1`，Cordis `4.0.4`）。同一份场景清单分别在原生 Cordis 和经 interop 从 Rust 执行，逐项比对结果（`crates/rutis-interop/tests/dsh_baseline.rs`，CI 中运行）；`classify.mjs` 静态统计每个服务成员需要的绑定能力。

```sh
npm --prefix interop/baseline ci
cargo test -p rutis-interop --test dsh_baseline -- --nocapture
node interop/baseline/classify.mjs
```

**协议层结果**（绕过生成器，按 JSON 直接调用）：

| 插件 | 服务 | 装载 / 可用性与原生一致 | 数据调用与原生一致 |
| --- | --- | --- | --- |
| dsh-invariants | `invariants` | 是 | 无纯数据方法 |
| dsh-credentials-local | `credentials` | 是 | 7 / 7 |
| dsh-fs-local | `fs` | 是 | 8 / 8（含 `FsError` 业务错误） |
| dsh-jobs-local | `jobs` | 是 | 2 / 2（含业务错误） |
| dsh-commands | `commands` | 是 | 无纯数据方法 |
| dsh-workspace | `workspaceRegistry` | 是（缺依赖，两边都不发布服务） | — |

**绑定层结果**：生成器对 6 个插件全部停在 L0，原因相同：它们都用 `class X extends Service` 注册，服务名和类型写在 `declare module '@deepseek-ai/cordis' { interface Context { ... } }` 里，而生成器只识别 `ctx.provide('名字', 值)`。

**能力统计**（6 个插件、55 个公开成员）：现有生成器能处理 1 个；协议已支持、只差生成器的 28 个；其余需要协议扩展。

| 能力 | 成员数 | 生成器 | 协议 |
| --- | --- | --- | --- |
| 品牌字符串 / 数字 | 42 | 否 | 是 |
| 数据对象（含字面量联合、可空） | 31 | 否 | 是 |
| 可选参数 | 19 | 否 | 是 |
| `AbortSignal` 参数 | 13 | 否 | 否 |
| 活对象（有方法的对象 / 类实例） | 12 | 否 | 否 |
| 回调参数 / 返回函数 | 5 / 5 | 否 | 是 |
| 公开属性 | 5 | 否 | 否 |
| `Uint8Array` / `AsyncIterable` | 2 / 1 | 否 | 否 |

事件：纯通知 5 个，waterfall 3 个（`fs/write-intent`、`fs/edit-intent`、`workspace/session-activity`），有返回值 1 个。

### W2 工程防护（与 W1 并行）

吸收 `rutis-cordis` 已验证的做法：

| 项 | 内容 |
| --- | --- |
| 超时与取消 | 调用方声明超时；取消传播到 Node 侧；迟到应答计数丢弃 |
| 握手能力协商 | 握手交换能力集；装载期检查插件所需服务，缺失时明确报错 |
| 断连记录 | 断连时记录在途调用和计数，便于排查 |

### W3 按缺口补能力（顺序由 W1 数据决定）

1. **服务发现改为读取类型声明**：从 `Context` 接口扩展取服务名与类型，支持 `Service` 子类；用 `package.json` 的 `types` 找声明文件。这是 6 个插件都卡在 L0 的原因。
2. **数据类型**：品牌类型、数据对象（生成 serde 结构体）、字面量联合、可空、可选参数。只需要生成器，协议已支持；完成后预计 28 个成员可用。
3. **取消**：`AbortSignal` 参数映射为取消（与 W2 的取消传播共用机制），13 个成员。
4. **回调参数、返回函数**：生成器接入协议已有的函数引用，10 个成员。
5. **活对象与属性**：协议增加 object 引用类型，17 个成员；先确认实际用法再定范围。
6. **事件**：纯通知事件按设计 §6 转发；waterfall 事件暂不转发。
7. `Uint8Array`、`AsyncIterable`：按需。

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
