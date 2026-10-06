# rutis-host 与 rutis.json

`rutis-host` 是不需要写 Rust 的宿主：它把内核、插件加载、本机的 Node / Python 运行时和网络连接组装好，由一份 `rutis.json` 驱动。插件作者用它在本地开发，也可以直接用它部署。需要自定义 Rust 服务的应用见 [在 Rust 应用里嵌入](rust-host.md)。

## 安装

| 方式 | 命令 |
| --- | --- |
| npm | `npx @arcships/rutis-host …`，或在项目里 `npm install -D @arcships/rutis-host` 后 `npx rutis-host …` |
| PyPI | `uvx rutis-host …`，或 `uv add --dev rutis-host` 后 `uv run rutis-host …` |
| 二进制 | GitHub Release 的 `rutis-host-<版本>-<平台>.tar.gz`（Linux、macOS 的 x64 / arm64） |
| crates.io | `cargo install rutis-host` |

npm 分发自带 Node 运行时（`@arcships/rutis-runtime`），PyPI 分发自带 Python 运行时（`rutis`）：项目里没有自己的运行时时用它们。

## 命令

| 命令 | 作用 |
| --- | --- |
| `rutis-host run [rutis.json]` | 按配置运行，打印每一行状态的变化；Ctrl-C 结束 |
| `rutis-host dev [目录]` | 在插件项目里运行这个插件（加上 `rutis.dev.json`），文件变化时重新加载 |
| `rutis-host check [rutis.json]` | 解析每一行，打印版本、依赖、提供的服务、配置 Schema；有不能运行的行时以非零状态退出。在没有 rutis.json 的插件项目里检查这个项目 |
| `rutis-host new <名字> --lang node\|python` | 创建插件项目 |

## rutis.json

```json
{
  "id": "main",
  "runtimes": {
    "node": { "project": "." },
    "py": { "project": "plugins", "python": ".venv/bin/python" },
    "remote": [{ "name": "gpu", "language": "python" }]
  },
  "listen": [
    { "name": "public", "address": "0.0.0.0:7443", "cert": "tls/server.pem", "key": "tls/server.key" }
  ],
  "rows": [
    { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "py:llm_gateway" },
    { "id": "local-tool", "name": "./tools/tool.ts" },
    { "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis", "export": ["weather"] } },
    { "id": "gpu", "name": "rutis-bridge/peer", "config": { "peer": "gpu", "dial": "wss://gpu.example.com/rutis", "runtime": "gpu" } },
    { "id": "embedder", "name": "gpu:embedder" }
  ]
}
```

文件里的相对路径都相对于文件所在的目录。

### id

这个节点的端点 id，连接到它的节点看到的名字。默认 `host`。

### runtimes

| 键 | 字段 | 作用 |
| --- | --- | --- |
| `node` | `project`（默认 `.`）、`runtime` | 启动 Node 运行时。插件包从 `project` 的 `package.json` 解析；`@arcships/rutis-runtime` 装在这里（或由 `runtime` 指定，或用 npm 版 rutis-host 自带的） |
| `py` | `project`（默认 `.`）、`python` | 启动 Python 运行时。解释器默认依次为 `$VIRTUAL_ENV/bin/python`、`<project>/.venv/bin/python`、`python3`，它的环境里要装 `rutis` 和插件 |
| `remote` | `name`、`language` | 别的机器上的运行时，经 `rows` 里 `"runtime": "<name>"` 的节点行连接，见 [连接节点](nodes.md) |

缺少运行时包时，启动失败并给出安装命令。

### listen

别的节点连进来用的 WebSocket 监听器。`cert` / `key` 是 TLS 证书和私钥（PEM），也可以由 `RUTIS_CERT` / `RUTIS_KEY` 给出。不带 TLS 时只能监听回环地址。

### rows

要运行的插件，每一行：

| 字段 | 含义 |
| --- | --- |
| `id` | 行的名字，在本文件内唯一 |
| `name` | 插件：npm 包名（或包的子路径）、`py:<名字>`（Python 入口点或模块）、`./相对路径`（插件文件）、`<远程运行时>:<模块>`、`rutis-bridge/peer`（节点，见 [连接节点](nodes.md)）、`peer:<节点>/<插件>`（在别的节点上运行的插件） |
| `config` | 插件的配置 |
| `inject` | 额外要等的服务名 |
| `isolate` | `{ "服务名": true }` 让这一行用自己的那份服务；`{ "服务名": "标签" }` 让同标签的行共用一份 |
| `disabled` | `true` 时不运行 |

服务按名字在所有插件之间共享：任何语言、任何节点上提供的 `weather`，所有声明了 `inject: ["weather"]` 的插件都能用。

## 凭据

凭据不写在文件里：

| 环境变量 | 作用 |
| --- | --- |
| `RUTIS_TOKEN` | 连接其他节点时出示的、以及接受其他节点连入时核对的 token |
| `RUTIS_TOKEN_<节点>` | 只对某个节点（大写，`-` 写作 `_`），优先于 `RUTIS_TOKEN` |
| `RUTIS_CA` | 额外信任的 CA 证书（PEM），用于 `wss://` 连接 |
| `RUTIS_CERT` / `RUTIS_KEY` | 监听器的证书和私钥（`listen` 里没有写时） |

## 部署

把 `rutis.json`、Node 项目（装好插件和 `@arcships/rutis-runtime`）、Python 环境（装好 `rutis` 和插件）放在一起，用进程管理器（systemd 等）运行 `rutis-host run /srv/app/rutis.json`。`rutis-host` 结束时，它启动的运行时进程随之结束。
