# 参与 rutis

[English](CONTRIBUTING.en.md)

谢谢你愿意花时间在 rutis 上。无论是报告一个 bug、指出文档里读不懂的一句话，还是提交一段代码，都很有价值。

## 提问与反馈

- **Bug**：请[新建 issue](https://github.com/arcships/rutis/issues/new/choose)，写清楚版本、平台、最小复现和期望的行为。能附一个失败的测试最好。
- **功能想法**：先开 issue 讲讲你要解决的问题。设计上有分歧的改动，先对齐方向再写代码，能省下双方的时间。
- **安全问题**：请不要公开提 issue，按 [安全策略](SECURITY.md) 私下报告。

## 仓库结构

| 目录 | 内容 |
| --- | --- |
| `crates/rutis` | 内核：插件、fiber、服务、事件 |
| `crates/rutis-loader` | 数据驱动的插件控制面 |
| `crates/rutis-bridge` | 跨进程、跨语言、跨机器：通道、会话、语言运行时、节点 |
| `crates/rutis-host` | 不写 Rust 的宿主 |
| `crates/rutis-sdk`、`crates/rutis-dylib*` | dylib 插件工具链 |
| `node/` | npm 包：插件 SDK `@arcships/rutis`、运行时、宿主 |
| `python/rutis` | PyPI 包 `rutis`：Python 插件 SDK 与运行时 |
| `docs/` | 指南、设计记录、迁移说明 |

## 开发环境

- Rust：仓库的 `rust-toolchain.toml` 固定了工具链，`rustup` 会自动安装。
- Node 24 或更高，Python 3.12 或更高（改动语言运行时或宿主时需要）。
- Linux 或 macOS。Windows 上请使用 WSL。

## 运行测试

```bash
cargo test --workspace                                  # 内核与大部分 crate
cargo test -p rutis-bridge --all-features               # 通道、会话、运行时、节点
cargo test -p rutis-loader --features node,python,peer  # loader 的各种插件行
npm --prefix node/rutis test
npm --prefix node/rutis-runtime ci && npm --prefix node/rutis-runtime test
(cd python/rutis && python3 -m unittest discover -s tests)
```

只改了一部分时，跑相关的那几项即可，完整的检查由 CI 完成。

## 提交改动

1. 从 `main` 拉一个分支。
2. 保持改动聚焦：一个 PR 解决一件事。
3. 行为变化要配测试；修 bug 时，先写一个能复现它的测试。
4. 提交前运行 `cargo fmt`，并确认相关测试通过。
5. 提交信息使用 [Conventional Commits](https://www.conventionalcommits.org/) 风格，例如 `fix(loader): …`、`feat(bridge): …`、`docs: …`。
6. 改动公共 API 或用户可见的行为时，同步更新文档；不兼容的变化请在 PR 里说明迁移方式。

### 文档

文档以中文为主，英文版放在同名的 `.en.md` 文件里。修改其中一份时请尽量同步另一份；只会写其中一种语言也没关系，在 PR 里说一声，我们来补。

## 发布

发布由维护者完成，流程见 [docs/release.md](docs/release.md)。

## 许可

提交到本仓库的贡献以 [MIT](LICENSE) 许可发布。
