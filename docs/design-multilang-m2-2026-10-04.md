# 多语言插件 M2：Python 运行时与叶子插件（实施记录）

状态：已实现。日期：2026-10-04。
依据：[多语言插件：每种语言一个运行时插件](design-multilang-runtimes-2026-10-03.md)（§十一 M2）、[多语言插件 M1](design-multilang-m1-2026-10-04.md)（§十一 JS/TS 叶子插件、§十三 留到 M2 的事）。

## 一、做了什么

| 部分 | 内容 |
| --- | --- |
| 运行时有名字 | `RuntimePlugin::named`，服务键 `Runtime::key(名字)`、`RuntimeRows::key(名字)`。Node 默认 `"node"`，Python 默认 `"py"`。一个应用里可以同时有多个运行时 |
| 启动命令可配置 | `Mount::launcher` / `Launcher { program, args, env, cwd }`，最后两个参数仍是 socket 路径和项目位置 |
| Python 运行时 | `interop/python/rutis_runtime`：协议层 `peer.py`（`peer.mjs` 的移植）、叶子插件的 runner、插件 SDK。`RuntimePlugin::python(sdk, project)` 启动它 |
| Python 行 | `InteropResolver::modules`：行名 `py:<模块名>` |
| JS/TS 叶子插件 | `@arcships/rutis-interop/plugin` 的 `definePlugin`；runner 认出标记后包成 Cordis 插件，装进现有 Node 进程 |
| 跨语言转发 | `RowService` 用调用方的会话（`rpc::caller`）做 `Connection::forward`，跨会话的同步调用链被改写 |
| 一致性测试 | `crates/rutis-loader/tests/multilang.rs`：同一组叶子插件各写一份 Python、一份 JS，两个运行时一起跑 |

## 二、实现时定下的几件事

**叶子运行时的依赖全部由 rutis 门控。** Python 运行时在 `mount` 时报告 `leaf` 特性。它没有自己的依赖解析，所以 resolver 把插件 `inject` 的每个名字都放进这一行的 rutis 依赖，不要求 `register_shared`。Node 运行时仍然只门控登记为共享的名字，其余交给 Cordis。

**Python 运行时可重入。** 冷启动时，JS 插件同步调用 Python 的服务、Python 插件同时同步调用 JS 的服务：两个进程都在同步等待，又都把对方的调用当作无关调用延后，于是互相卡死。一致性测试第一次运行就复现了。Node 的规则不变（只执行本调用链上的进来调用）；Python 在同步等待期间执行所有进来的调用。只要一方可重入，这种交叉等待就能解开。代价是 Python 插件的服务可能在它自己的同步调用之中被调用，README 写明了"调用 rutis 服务时不要持有锁"。两个 Node 运行时之间互相同步调用仍可能卡死，文档建议这种调用用异步方法。

**提供者比使用者后停。** 行卸载时先撤销它投到 rutis 的服务（`Projection::withdraw`，内核会先停下使用者），再让运行时卸载插件本身。之前是先卸载插件、异步撤销服务，一致性测试抓到了提供者的清理先于使用者执行。

**Python 插件配置变化即重启。** 叶子插件没有 volatile 字段，`rows.update` 等同于卸载后重新装载。

**语言之间解耦。** 每种语言是一个 Cargo feature（rutis-interop 的 `node`、`python`；rutis-loader 的 `node`、`python`，`interop` 等于两者），应用只编译它启用的语言；不挂运行时插件就不会有进程。共用的部分（协议、进程管理、服务投影、`RuntimePlugin`、`Launcher`）和语言无关。类型改成和语言无关的名字（`RuntimePlugin`、`Runtime`、`RuntimeRows`），旧的 `Cordis*` 名字保留为弃用别名。CI 分别检查只开一种语言时能否编译。

## 三、一致性测试覆盖的行为

两个运行时各有提供者、跨语言使用者、同进程使用者，同时冷启动：

1. 全部启动，运行时之间不互相等待；跨语言的同步、异步调用都到达对方；同进程使用者拿到的是提供者的对象本身；
2. 任一提供者移除时，只有它的使用者（两种语言的）停下，且都在提供者的清理之前停下；其他行照常运行；
3. 插件 `inject` 的服务（这里是 Rust 提供的 `llm`）在两种语言里都门控：未就绪时等待，提供后启动，撤销后停下；
4. Python 进程意外退出时，只有 Python 的行和用到 Python 服务的 JS 行停下，Node 运行时和其他 JS 行照常运行。

## 四、还没做的

- Swift（M3）、Go（M4），按总体稿等具体需求。
- 对象引用（带方法和属性的对象）转给 Node 侧：`peer.mjs` 仍不接受 Rust 导出的对象引用；Python 侧同样不接受。函数和异步结果可以转交。
- Python 插件没有配置的就地更新，也没有从类型注解生成配置 Schema（调研 `python.md` §五 的约定），目前 `Config` 直接写 JSON Schema。
