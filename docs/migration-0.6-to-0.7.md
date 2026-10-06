# 升级到 rutis 0.7

[English](migration-0.6-to-0.7.en.md) · [发布说明](releases/0.7.0.md)

本指南适用于 `rutis` 0.6.x、旧 `rutis-interop` / loader 包，以及仓库中曾使用 0.3.0 版本号的未发布整合版。本次内核与 bridge、loader、host、npm/PyPI 包版本均为 0.7.0，沿用已有发布流程。

## 只使用 Rust 内核

把依赖改成 `rutis = "0.7"` 并更新锁文件。类型化插件是可选接口；已有 `Plugin` 实现可以继续使用。0.6.1 的控制面接口继续保留。若从 0.6.0 直接升级，可一并阅读[控制面说明](migration-0.6.0-to-0.6.1.md)。

依赖 0.6 的第三方 Rust 插件也要更新依赖并重编译；同一宿主的服务类型、`Ctx` 和插件 trait 必须来自同一份内核，不能把 Cargo 同时解析出的 0.6 与 0.7 类型混用。

## Rust 宿主与旧互操作包

```toml
[dependencies]
rutis = "0.7"
rutis-loader = { version = "0.7", features = ["node", "python", "peer"] }
rutis-bridge = { version = "0.7", features = ["python", "websocket"] }
```

只启用应用需要的 feature。bridge 默认启用 `node`；`python`、`websocket`、`cordis` 和 `testing` 按需开启。loader 旧的 `interop` feature 改为 `node`；需要 Python 或远程节点行时另加 `python` / `peer`。

| 旧入口 | 0.7 入口 |
| --- | --- |
| Rust `rutis-interop` | `rutis-bridge` |
| `rutis_interop::rpc` | `rutis_bridge::session` |
| Cordis 挂载的 `Mount` / `Process` | `rutis_bridge::cordis::{Mount, Process}`（`cordis` feature） |
| `rutis_interop::build::from_manifest` | `rutis_bridge::cordis::build::from_manifest` |
| 中间开发版独立 channel / transport crate | `rutis_bridge::channel` / `rutis_bridge::transport` |
| npm `@arcships/rutis-interop` | 运行时用 `@arcships/rutis-runtime`；叶子插件 SDK 用 `@arcships/rutis` |
| Python `rutis_runtime` 导入 | `rutis` |

Cordis 的 `build.rs` 依赖也要换成带 `cordis` feature 的 `rutis-bridge`，并重新生成绑定。不要仅全局替换 crate 名：会话、运行时、Cordis 绑定现在分属不同模块。完整例子见 [Cordis](guide/cordis.md)及 [Rust 宿主](guide/rust-host.md)。运行时插件和 resolver 按新指南配置；远程运行时与节点功能共用 bridge 连接。

## JS/TS 与 Python

发布后同步升级插件 SDK、运行时和宿主：

```bash
npm install @arcships/rutis@^0.7.0
npm install --save-dev @arcships/rutis-runtime@0.7.0 @arcships/rutis-host@0.7.0
uv add 'rutis>=0.7,<0.8'
uv add --dev 'rutis-host>=0.7,<0.8'
```

JS/TS 叶子插件从 `@arcships/rutis` 导入 `definePlugin`，测试工具从 `@arcships/rutis/testing` 导入。已有 Cordis 插件继续遵循 Cordis 插件接口，运行它们使用新 runtime 包；不必改成叶子插件。Python SDK 和运行时合在 PyPI 的 `rutis` 包；网络传输额外安装 `rutis[network]`。

`rutis-host new <name> --lang node|python` 生成对应 0.7 的依赖范围。已有项目保留自己的插件版本，只更新上述框架依赖和锁文件。之后运行插件测试、`rutis-host check`，再用 `rutis-host dev` 验证重载。

## 协议与部署

包版本 0.7.0、插件 API 标记和会话协议号是三个不同概念。本地会话协议为 2，带 endpoint 身份的会话协议为 3。不要为匹配包版本手动把 `rutisProtocol` 改成 7；宿主和运行时按同一列车一起部署。

Linux/macOS 支持本地语言运行时和 host；Windows 使用 WSL。Node 要求 24+，Python 要求 3.12+。远程部署还需检查 endpoint 标识、证书主机名、CA 和认证 token；先在测试环境验证断线重连和服务撤销，再替换运行中的部署。

## dylib

升级内核及锁文件会改变 SDK 身份。重新生成宿主 bundle / SDK bundle，再基于该 SDK 构建所有动态插件；不要混用旧 SDK 二进制或把 Rust semver 当作 dylib ABI 兼容保证。SDK、dylib 工具本身仍有独立包版本。见 [SDK 构建包设计](design-sdk-build-package-2026-10-04.md)。

## 发布标签

先推送 `rutis-v0.7.0` 发布内核，成功后在同一提交推送 `v0.7.0` 发布 bridge、loader、host 及 npm/PyPI 包。`cli-v*` 仍是独立 CLI 示例的二进制发布。
