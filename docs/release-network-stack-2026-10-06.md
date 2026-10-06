# 发布：多语言插件与网络栈（interop 0.3、loader 0.2、bridge 0.1）

> 发布暂停：#148 / #149 的开发者包方案替代本文的独立包发布方案。#147 可以先合并以保留修复和验证，但不要按下列旧 tag 发布；最终发布以开发者包方案的版本和工作流为准。

## 发布哪些包

| 包 | 版本 | tag | 工作流 |
| --- | --- | --- | --- |
| `rutis-channel` | 0.1.0（新） | 随 `interop-v0.3.0` | publish-interop |
| `rutis-interop` 与 npm `@arcships/rutis-interop` | 0.3.0 | `interop-v0.3.0` | publish-interop |
| `rutis-bridge`、`rutis-transport-memory`、`rutis-transport-local`、`rutis-transport-websocket`、`rutis-runtime-local` | 0.1.0（新） | `bridge-v0.1.0` | publish-bridge |
| `rutis-loader` | 0.2.0 | `loader-v0.2.0` | publish-loader |

`rutis` 内核不变（0.6.1），不发。Python 包 `rutis_runtime` 这次不发 PyPI，仍从仓库的 `interop/python` 安装。

## 顺序

crates.io 要求依赖（包括可选依赖）先发布，所以按依赖顺序打 tag，每一步的工作流成功后再打下一个：

1. `interop-v0.3.0`：先发 `rutis-channel`（crates.io 上还没有时），再发 `rutis-interop` 和 npm 包。
2. `bridge-v0.1.0`：按顺序发 bridge、memory、local、websocket、runtime-local；crates.io 已有的版本跳过。它会先检查 rutis、rutis-channel、rutis-interop 已在 crates.io 上。
3. `loader-v0.2.0`：它的 `peer` feature 依赖 rutis-bridge，工作流同样先检查。

仅 publish-bridge 对列出的 crate 跳过已发布版本；publish-interop 和 publish-loader 不保证整条流程可重复执行。失败后先核对注册表与成功步骤，不删除或移动已发布 tag，也不直接重跑整条发布链。

## 发布前的验证

CI 已覆盖：Linux 上全部测试，macOS 上网络栈和运行时的测试（`network-macos`），Windows 和 macOS 的编译检查。每晚的 `stress` 工作流还跑两个浸泡测试（每个默认 10 分钟）：link 反复断开重连、进程反复拉起结束，检查文件描述符、线程数不增长、进程都被回收。

发布前再手动做一次两台机器的冒烟（`crates/rutis-transport-websocket/examples/smoke.rs`）：

```text
# 监听方（证书对应它的主机名）
cargo run -p rutis-transport-websocket --example smoke -- \
    listen 0.0.0.0:7443 --cert server.pem --key server.key --token secret

# 拨号方
cargo run -p rutis-transport-websocket --example smoke -- \
    dial wss://<监听方主机名>:7443/rutis --ca ca.pem --token secret
```

要看到的：

- 拨号方每秒打印一次 `clock: <n>`；
- 拔掉网线（或防火墙挡住端口）：30 秒内两边打印 `waiting … after Retryable`（心跳超时），恢复网络后自动 `ready, session <n+1>`，`clock` 继续；
- 重启监听方：拨号方退避重连，重连后 `clock` 从 0 重新开始；
- 错误的 token：`AuthRejected … 403`；不信任的 CA：`AuthRejected … UnknownIssuer`，都按 30 秒慢重试。

## 发布后

- 在 GitHub 上为三个 tag 写 release notes，链接迁移说明：[rutis-interop 0.2 → 0.3](migration-interop-0.2-to-0.3.md)、[rutis-loader 0.1 → 0.2](migration-loader-0.1-to-0.2.md)。
- 确认 crates.io 页面显示了各 crate 的 README。
