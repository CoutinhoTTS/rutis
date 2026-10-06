# 连接节点

一个 rutis 宿主是一个**节点**。节点之间用 **link** 连接：一端拨号（`dial`），一端监听（`listen`），连上后在这条连接上共享服务、代为运行插件、转发事件。断开后自动重连。

下面用 `rutis.json` 的行来写；在 Rust 里用 `rutis_bridge::PeerPlugin` 做同样的事（见 [在 Rust 应用里嵌入](rust-host.md)）。

## 一条 link

```json
{ "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis" } }
```

另一端（`office`）监听：

```json
{
  "id": "office",
  "listen": [{ "name": "public", "address": "0.0.0.0:7443", "cert": "server.pem", "key": "server.key" }],
  "rows": [
    { "id": "main", "name": "rutis-bridge/peer", "config": { "peer": "main", "listen": "public" } }
  ]
}
```

两端用同一个 token：拨号方出示 `RUTIS_TOKEN`（或 `RUTIS_TOKEN_OFFICE`），监听方用它核对来者是 `main`。

- `dial` 是 `wss://…`（跨机器，必须 TLS），或 `ws://127.0.0.1…`（本机回环）。
- 自签证书：把签发它的 CA 放进拨号方的 `RUTIS_CA`。
- 断开后重连：从 0.5 秒开始退避，最长 30 秒；凭据被拒（403、证书不受信任）时每 30 秒重试一次；协议不兼容时停止并报告。
- 监听方的 link 还没就绪时连进来的拨号方会被要求重试，所以两端可以按任意顺序启动。

## 在 link 上做什么

节点行的 `config` 决定这条连接上做什么。`export`、`import`、`events` 改了不会断开连接，只重启对应的那部分。

| 字段 | 作用 |
| --- | --- |
| `export: ["weather"]` | 把本节点的服务 `weather` 给对端用 |
| `import: ["calendar"]` | 把对端的服务 `calendar` 作为本节点的服务；本节点已有同名服务时拒绝 |
| `events: { "out": ["tick"], "in": ["tock"] }` | 事件单向转发 |
| `host: true` | 允许对端在本节点上运行这里装着的插件（只对可信的对端打开） |
| `rows: true` | 本文件里 `peer:<对端>/<插件>` 的行在对端运行 |
| `runtime: "<名字>"` | 对端是一个语言运行时，见下文 |

服务跨节点时调用链保持：同步调用期间的回调、回调里再调回去，都能穿过多个节点。

## 在别的节点上运行插件

对端开了 `host: true`，本端开了 `rows: true` 时，`peer:<对端>/<插件>` 的行在对端加载那个插件：

```json
{ "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis", "rows": true } },
{ "id": "scanner", "name": "peer:office/scanner-plugin", "config": { "dpi": 300 } }
```

插件在对端运行，用对端的服务；它的配置、启停仍由本端决定。对端离线时这一行等待，回来后重新加载。行的 `isolate` / `inject` 交给对端执行。

## 别的机器上的语言运行时

在 GPU 机器上只运行一个 Python 运行时，由主节点管理它的插件：

```bash
# GPU 机器：装好 rutis[network] 和插件，监听
RUTIS_TOKEN=secret RUTIS_CERT=server.pem RUTIS_KEY=server.key \
  python -m rutis listen:wss://0.0.0.0:7443/rutis --id gpu --peer main /srv/plugins
```

```json
{
  "id": "main",
  "runtimes": { "remote": [{ "name": "gpu", "language": "python" }] },
  "rows": [
    { "id": "gpu", "name": "rutis-bridge/peer", "config": { "peer": "gpu", "dial": "wss://gpu.example.com:7443/rutis", "runtime": "gpu" } },
    { "id": "embedder", "name": "gpu:embedder" }
  ]
}
```

远程 Python 运行时的行名是 `<运行时名>:<模块>`，模块在远程机器上解析。Node 运行时同样可以远程运行：在那台机器的 Node 项目里执行 `node node_modules/@arcships/rutis-runtime/src/runner.mjs listen:wss://… --id <名字> --peer <控制方> ./package.json`（插件从这个 package.json 解析），`language` 写 `node`。远程运行时一次服务一个控制方；控制方断开时，它为这个控制方运行的插件全部卸载。

## 把 Cordis 应用接成节点

已有的 Cordis 应用用 `@arcships/rutis-runtime/bridge` 成为一个节点，见 [Cordis](cordis.md)。

## 安全

- 跨机器只用 `wss://`。不带 TLS 的监听器只能绑定回环地址。
- 每个对端一个 token（`RUTIS_TOKEN_<节点>`），只在需要的节点上设置。
- `host: true` 让对端能在本节点运行这里装着的任何插件，只对可信的对端打开；它只能加载已安装的插件，不能加载文件路径。
- 节点只按名字共享 `export` 里列出的服务。
