# 发布

## 发布列车

除内核 `rutis`（`rutis-v*`，publish-rutis.yml）、`rutis-cli`（`cli-v*`，release-cli.yml）和 dylib 相关的 crate 之外，这些包同一个版本、一起发布：

| 注册表 | 包 |
| --- | --- |
| crates.io | `rutis-bridge`、`rutis-loader`、`rutis-host` |
| npm | `@arcships/rutis`、`@arcships/rutis-runtime`、`@arcships/rutis-host`（及 `@arcships/rutis-host-{linux,darwin}-{x64,arm64}`） |
| PyPI | `rutis`、`rutis-host`（各平台的 wheel） |
| GitHub Release | `rutis-host` 二进制 |

## 步骤

1. 改版本号：`crates/rutis-bridge`、`crates/rutis-loader`、`crates/rutis-host` 的 Cargo.toml（以及它们之间的依赖版本），`node/rutis`、`node/rutis-runtime`、`node/rutis-host` 的 package.json（`@arcships/rutis-host` 依赖的运行时和平台包版本），`python/rutis/pyproject.toml`，`crates/rutis-host/pyproject.toml` 里 `rutis` 的范围。`node scripts/train.mjs` 检查它们一致，CI 也会跑。
2. 合并到 main，CI 的 `release-dry-run` 通过（各包都能打包）。
3. 发布前在两台机器上跑一次冒烟（下文）。
4. 打 tag `vX.Y.Z` 并推送。release.yml：核对版本 → 构建四个平台的二进制和 wheel → 发布 crate、npm 包、PyPI 包 → 创建 GitHub Release。各步只发布注册表里还没有的版本，中途失败时修好后重新运行即可。

需要的配置：GitHub environment `release`（`CARGO_TOKEN`、`NPM_TOKEN`）和 `pypi`（PyPI 上为 `rutis`、`rutis-host` 配置 trusted publisher，指向 release.yml）。

插件 API（`PLUGIN_API`，SDK 与运行时各有一份）只在插件看到的接口不兼容时增加；会话协议版本（`rutisProtocol` 与 `rutis_bridge::session::PROTOCOL`）在线格式不兼容时增加。

## 冒烟

```text
# 监听方（证书对应它的主机名）
cargo run -p rutis-bridge --features websocket --example smoke -- \
    listen 0.0.0.0:7443 --cert server.pem --key server.key --token secret

# 拨号方
cargo run -p rutis-bridge --features websocket --example smoke -- \
    dial wss://<监听方主机名>:7443/rutis --ca ca.pem --token secret
```

要看到：拨号方每秒打印 `clock: <n>`；断网后 30 秒内两边报告心跳超时并等待重连，恢复后 `ready, session <n+1>`；重启监听方后拨号方退避重连；错误的 token 是 `AuthRejected … 403`，不信任的 CA 是 `AuthRejected … UnknownIssuer`。

再用发布的包从零走一遍 [写一个 TypeScript 插件](guide/typescript-plugin.md) 和 [写一个 Python 插件](guide/python-plugin.md)。

每晚的 stress 工作流还跑两个浸泡测试（link 反复断开重连、进程反复拉起结束），检查文件描述符、线程数不增长、进程都被回收。
