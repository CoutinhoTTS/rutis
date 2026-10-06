# rutis 指南

按你要做的事选：

| 我要… | 读 |
| --- | --- |
| 用 TypeScript / JavaScript 写插件 | [写一个 TypeScript 插件](typescript-plugin.md) |
| 用 Python 写插件 | [写一个 Python 插件](python-plugin.md) |
| 查插件能做什么、值怎样传递 | [插件 API](plugin-api.md) |
| 不写 Rust，直接运行插件 | [rutis-host 与 rutis.json](rutis-host.md) |
| 把多台机器连起来、在别的机器上运行插件 | [连接节点](nodes.md) |
| 在自己的 Rust 应用里加载插件 | [在 Rust 应用里嵌入](rust-host.md) |
| 让已有的 Cordis 应用和 rutis 互通，或在 Rust 里挂载 Cordis 插件 | [Cordis](cordis.md) |

## 包

| 谁装 | Rust（crates.io） | Node（npm） | Python（PyPI） |
| --- | --- | --- | --- |
| 插件作者 | `rutis-sdk`（dylib 插件） | `@arcships/rutis` | `rutis` |
| 宿主（运行插件） | `rutis`、`rutis-loader`、`rutis-bridge` | `@arcships/rutis-runtime` | `rutis` |
| 不写 Rust 的宿主 | `rutis-host` | `@arcships/rutis-host` | `rutis-host` |

除内核 `rutis` 外，这些包一起发布、版本号相同（发布列车）。内核和 `rutis-sdk` 等 dylib 工具独立发版；列车 0.7.x 基于内核 0.6.1。

用过 `rutis-interop` 的项目见[迁移指南](../migration-interop-to-0.7.md)。

## 环境

- Linux 或 macOS。本机运行插件依赖 Unix；Windows 上用 WSL。
- Node 24 或更高（TypeScript / JavaScript 插件）。
- Python 3.12 或更高（Python 插件）。
- Rust 1.85 或更高（只在 Rust 里嵌入时需要）。
