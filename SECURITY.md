# Security policy · 安全策略

## Reporting a vulnerability

Please do not report security issues in public issues or pull requests. Instead, use GitHub's private reporting form: [report a vulnerability](https://github.com/arcships/rutis/security/advisories/new).

Include what you can of the following: the affected package and version, the impact, steps or code to reproduce it, and any fix you have in mind. We will acknowledge the report, keep you informed while we work on it, and credit you in the advisory unless you prefer otherwise.

Areas where reports are especially valuable: node links (authentication, TLS, the session protocol), the language runtimes and the processes they start, and dylib plugin loading.

## Supported versions

rutis is at 0.x. Fixes go into the latest release of each package: the `rutis` core, and the 0.7 release train (`rutis-bridge`, `rutis-loader`, `rutis-host` and the npm and PyPI packages).

## 报告漏洞

请不要在公开的 issue 或 pull request 里报告安全问题，而是通过 GitHub 的私密报告表单：[报告漏洞](https://github.com/arcships/rutis/security/advisories/new)。

请尽量提供：受影响的包和版本、影响范围、复现步骤或代码，以及你设想的修复方式。我们会确认收到，在处理过程中同步进展，并在公告中致谢（除非你希望匿名）。

特别欢迎以下方面的报告：节点互联（认证、TLS、会话协议）、语言运行时及其启动的进程、dylib 插件加载。

## 支持的版本

rutis 处于 0.x。修复会发布在各包的最新版本中：内核 `rutis`，以及 0.7 发布列车（`rutis-bridge`、`rutis-loader`、`rutis-host` 及 npm、PyPI 包）。
