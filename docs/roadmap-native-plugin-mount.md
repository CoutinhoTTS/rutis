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
| 已发布 dsh 插件：协议层与原生逐项对照；生成的类型化绑定装载与调用 | `crates/rutis-interop/tests/dsh_baseline.rs`、`examples/dsh-baseline/tests/typed.rs` |
| 组合挂载：组内依赖解析、缺失依赖点名 | `crates/rutis-interop/tests/group_mount.rs` |
| 宿主向插件提供服务：依赖门控、调用、注销函数、撤销与恢复 | `crates/rutis-interop/tests/host_services.rs`、`examples/dsh-baseline/tests/host.rs` |
| 投影与导出生命周期（评审回归） | `crates/rutis-interop/tests/projection_lifecycle.rs` |
| 回调参数、返回函数：真实插件 | `examples/dsh-baseline/tests/callbacks.rs` |
| 事件转发：emit / parallel 语义、真实插件 | `crates/rutis-interop/tests/event_forwarding.rs`、`examples/dsh-baseline/tests/events.rs` |
| 取消：丢弃 future 中止 AbortSignal、迟到应答丢弃 | `crates/rutis-interop/tests/cancellation.rs` |
| 活对象：实时属性、方法、身份、嵌在数据里、传回原对象 | `crates/rutis-interop/tests/live_objects.rs`、`interop/node/test/peer.test.mjs`、`examples/dsh-baseline/tests/typed.rs` |

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
| dsh-workspace（与 storage、storage-json、storage-domain、session-persistence-jsonl 组合挂载） | `workspaceRegistry` | 是 | 2 / 2 |

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

### W1.5 组合挂载（已完成）

W3 的实测暴露出比单个成员更大的缺口：已发布插件通常设计成组合使用，单独挂载时依赖无法满足（`dsh-workspace` 在原生 Cordis 中单独装载同样起不来）。现在一次挂载可以是一组插件，装进同一个 Cordis Context，依赖按原生规则解析；绑定用 `cordis_group` 生成。`dsh-workspace` + `dsh-storage` + `dsh-storage-json` + `dsh-storage-domain` + `dsh-session-persistence-jsonl` 作为一组挂载后，协议层与原生一致，类型化绑定可调用。测试：`crates/rutis-interop/tests/group_mount.rs`、`dsh_baseline.rs`、`examples/dsh-baseline/tests/typed.rs`。

### W1.6 宿主向 Cordis 插件提供服务（已完成）

被挂载的 Cordis 插件依赖 rutis 应用提供的服务（设计 §5）。这项需求原本在需求文档中，2026-09-29 改写需求时被误删，已补回并实现。`Bindings::provide` 为宿主服务生成 trait、`provide_*` 注册函数和分发代码；挂载插件 `injects` 这些服务；runner 在装载插件前注册代理。

验收目标为 `dsh-persona` 依赖 `systemPrompt`：宿主未提供时挂载等待，提供后注册提示词片段，撤销后插件停止并清理，重新提供后恢复，卸载时 Cordis 调用宿主返回的注销函数（`examples/dsh-baseline/tests/host.rs`、`crates/rutis-interop/tests/host_services.rs`）。

仍不支持：Cordis 插件依赖另一次挂载里的 Cordis 服务（需要放进同一组）；宿主服务的就地替换（撤销后按依赖重启整个挂载）。

### W2 工程防护（已完成主要部分）

- 取消与超时：丢弃异步调用的 future 即取消，Cordis 方法收到的 `AbortSignal` 随之中止；超时用 `tokio::time::timeout`。测试：`crates/rutis-interop/tests/cancellation.rs`。
- 迟到应答：已取消调用的应答被丢弃并计数（`Connection::orphans()`），不断开连接。
- 推迟：握手能力协商（生成器与 runner 同包发布，W4 分离后再做）；断连时逐个记录在途调用。

### W3 按缺口补能力（顺序由 W1 数据决定）

1. ~~**服务发现改为读取类型声明**~~（已完成）：从 `Context` 声明取服务，支持 `Service` 子类和 npm 包目录。
2. ~~**数据类型**~~（已完成）：品牌类型、数据对象、字面量联合、可空、可选参数、`Record`、动态 JSON；可省略的 `AbortSignal` 暂不暴露。结果：6 个插件全部生成类型化绑定并通过 L0；55 个成员中绑定 32 个（此前 1 个），`examples/dsh-baseline` 用生成的类型调用 credentials、fs、jobs，与原生行为一致。
3. ~~**活对象**~~（已完成）：协议增加对象引用、记录值、方法调用和属性读取；生成器为带方法的接口和类实例生成代理，属性实时读取。`dsh-workspace` 的 `create` / `get` / `list` / `resolveByPath` 可以通过类型化绑定使用。绑定覆盖率 39 / 55（此前 32）。
4. ~~**取消**~~（已完成）：`AbortSignal` 参数由 Rust future 的丢弃来中止；选项对象里的 `AbortSignal` 字段仍不传。绑定覆盖率 40 / 55。
5. ~~**回调参数、返回函数**~~（已完成）：回调参数生成 `impl Fn` 闭包参数（同步或返回 `BoxFuture`），返回的函数为 `RemoteFunction`，JS `Error` 值为 `JsError`。invariants 安装器、`modifyRecord`、`fs.watch`、`attachController` 在真实插件上验证（`examples/dsh-baseline/tests/callbacks.rs`）。绑定覆盖率 47 / 55。
6. ~~**服务属性**~~（已完成）：服务自身的属性生成实时 getter。绑定覆盖率 52 / 55，剩余为二进制（2）和流（1）。
7. ~~**事件（双向）**~~（已完成）：`Bindings::event` 选择 Cordis → rutis 的通知事件，生成 rutis 事件类型；Cordis `emit` 发出即忘，`parallel` 等待 rutis 监听。credentials 的两个事件在真实插件上验证（`examples/dsh-baseline/tests/events.rs`）。`Bindings::emit` 选择 rutis → Cordis 的事件，宿主服务用它发出接口层的事件（persona 示例发出 `system-prompt/change`）。同一事件只能选一个方向。waterfall / 有返回值的事件不转发。
8. `Uint8Array`、`AsyncIterable`：按需。

### W4 包级接入

用应用清单（`Cargo.toml` 的 `package.metadata`）配置插件包与版本，替代 `build.rs` 中的源码路径；按 npm 包解析插件入口和类型声明。

## 4. 验证

```sh
npm --prefix interop/node ci
npm --prefix interop/node test
npm --prefix interop/baseline ci
cargo test -p rutis-interop -p native-mount-example -p dsh-baseline
cargo clippy -p rutis-interop -p native-mount-example -p dsh-baseline --all-targets -- -D warnings
```

## 5. 性能记录

同步方法往返（2026-09-28，Node 26.8.1、Intel Core Ultra X7 358H，预热 200 次后调用 3,000 次）：

| Rust 构建 | p50 / p95 / p99（μs） | 均值（μs） |
| --- | --- | --- |
| debug | 65.3 / 100.3 / 181.0 | 70.3 |
| release | 32.7 / 59.9 / 101.4 | 36.6 |

复现：`cargo build --release -p native-mount-example --bin rutis-counter && node interop/node/bench/sync-call.mjs target/release/rutis-counter`。服务读取本身不经过 IPC；每次方法调用一次往返。W1 用真实插件负载评估是否需要优化。
