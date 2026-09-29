# rutis 挂载 Cordis 插件：路线图

依据：[需求](requirements-protocol-plugins.md)、[设计](design-protocol-plugin-mount.md)。按能力的实际价值分切片交付；每个切片都要有跨进程自动化测试，并与 Cordis 原生行为对照。rutis 内核与 Cordis 不修改。

## 1. 切片

| 切片 | 内容 | 状态 |
| --- | --- | --- |
| S1 服务方法 | 构建期生成 Rust 类型；同步 / 异步方法、业务错误、依赖就绪、先清理后排空、进程故障 | 已完成 |
| S2 协议基础 | 线协议 v1：invoke / await 分离、函数与异步结果引用、计数释放、同步调用链内的反向调用、`SyncWaitCycle`、错误对象图 | 已完成（协议层）；生成的绑定尚未使用回调参数 |
| S3 换值跟随 | 服务对象句柄；`internal/service` / `internal/set` / 调用后检查；rutis 侧 `provide_mut_as` + `ServiceWriter` 替换、撤销与重新注册；句柄回收 | 已完成 |
| S4 事件转发 | 生成 Rust 事件类型；按设计 §6 双向按组转发；按签名映射 emit / parallel / bail / serial；防回环 | 下一步 |
| S5 回调与对象进入绑定 | 生成器支持函数类型参数、返回有状态对象（新增 object 引用类型） | 待做 |
| S6 真实插件验收与包级接入 | 选定目标 Cordis 插件逐个接通；用应用清单配置插件包，替代 `build.rs` 中的源码路径 | 待做，先确定目标插件列表 |

反方向（Cordis 应用挂载 rutis 插件）只做维护，不新增能力。

## 2. 当前验收

| 覆盖 | 测试 |
| --- | --- |
| 生成的方法形状、依赖门控、清理顺序、启动失败、隔离挂载 | `examples/native-mount/src/main.rs` |
| 换值后新读取得到新代理、旧快照不变、撤销与重新注册、句柄回收 | `crates/rutis-interop/tests/service_projection.rs` |
| 句柄固定指向原对象、先清理后排空、进程退出 | `crates/rutis-interop/tests/process_exit.rs` |
| 双向同步回调、保存回调、异步结果、等待环、后台执行器、关闭 | `crates/rutis-interop/tests/rpc_callbacks.rs` |
| 错误对象图往返 | `crates/rutis-interop/tests/error_shape.rs`、`interop/node/test/errors.test.mjs` |
| 引用受理与释放交叉、握手、关闭时中断阻塞写 | `crates/rutis-interop/src/rpc/tests.rs`、`interop/node/test/peer.test.mjs` |
| 生成器诊断 | `interop/node/test/generate.test.mjs`、`crates/rutis-interop/src/build/rust.rs` |
| 反方向（Cordis 挂载 rutis 插件） | `examples/native-mount/tests/cordis_mount.rs` |

```sh
npm --prefix interop/node ci
npm --prefix interop/node test
cargo test -p rutis-interop -p native-mount-example
cargo clippy -p rutis-interop -p native-mount-example --all-targets -- -D warnings
```

## 3. 性能记录

同步方法往返的样本（2026-09-28，Node 26.8.1、Intel Core Ultra X7 358H，预热 200 次后调用 3,000 次）：

| Rust 构建 | p50 / p95 / p99（μs） | 均值（μs） |
| --- | --- | --- |
| debug | 65.3 / 100.3 / 181.0 | 70.3 |
| release | 32.7 / 59.9 / 101.4 | 36.6 |

复现：

```sh
cargo build --release -p native-mount-example --bin rutis-counter
node interop/node/bench/sync-call.mjs target/release/rutis-counter
```

服务读取本身不经过 IPC（代理在 rutis 注册表中）；每次方法调用一次往返。S6 用真实插件负载评估是否需要优化。
