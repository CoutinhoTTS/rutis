# 多语言插件调研（2026-10-03，存档）

**结论：不在 rutis 内做其他语言的接入。** 完整的思考过程见[决策记录](../../../decision-multilang-2026-10-03.md)，需求文档的改动见 [§8](../../../requirements-protocol-plugins.md)。

理由：

- rutis 的价值在 Cordis 范式：依赖门控、服务换值、清理树、依赖驱动重载。PowerShell、Bash、AppleScript 没有这套范式，接入后只剩“带类型的命令调用”，范式本身用不上。
- 脚本最可能的使用者是 agent，它要的是运行时发现和动态调用，不需要构建期的类型化绑定。
- 五种运行时各有平台限制和难以收尾的问题，维护面与收益不成比例。具体问题包括：取消无法确认、PowerShell 的线程亲和性、macOS 的自动化授权（TCC）、bash 3.2 的限制。
- min_cordis（Python）同属 Cordis 范式，但目前没有任何插件需要挂载，归入“库外、以后再说”。

确有需要时，这些语言的支持应作为 rutis 之外的插件实现：一个运行时插件提供执行服务，每个脚本作为 inject 它的插件或 loader 行。模式与 [cordis-runtime 插件](../../../design-cordis-runtime-plugin-2026-10-03.md)相同。

## 报告（英文原文）

| 文件 | 内容 |
| --- | --- |
| [runner-contract.md](runner-contract.md) | 从代码整理的 rutis-interop 协议 v1 runner 契约，含真实 `runner.mjs` 的帧跟踪；可复用 / 需泛化 / Node 专用部分的分类 |
| [python.md](python.md) | 基于 min_cordis 的 Python runner：钩子映射、同步重入、类型提取、部署 |
| [powershell.md](powershell.md) | pwsh 托管方式、runspace 语义、序列化陷阱、取消与清理、AST 提取 |
| [bash.md](bash.md) | 每次调用一个进程、fd 3 结果通道、进程组与取消、argc 注解 |
| [applescript.md](applescript.md) | 常驻 OSA host、OSAKit 状态与取消、TCC 授权、注解 |
| [precedents.md](precedents.md) | Neovim、Azure Functions、Pulumi、Nushell 等约 40 个跨语言插件系统的经验 |

报告中提到的实验脚本（`scratchpad/experiments/...`、`X/...`）没有提交；报告里保留了命令和关键输出。

其中两点对 Cordis 挂载本身也有用：

- runner-contract §12.2 列出了 runner 必须满足的隐含约定，例如：Rust future 是惰性的，`event` 的确认必须 await；`path` 传错会导致死锁。
- 请求 id 前缀在 Rust 侧写死为 `node:`（`rpc.rs:822,1165,1194`）。
