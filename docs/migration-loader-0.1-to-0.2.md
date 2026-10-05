# rutis-loader 0.1 → 0.2

这一版随 rutis-interop 0.3 一起发布：语言运行时的行与 rutis 按名字共享服务，以及在其他节点上运行的行。内核 `rutis` 不变。

| 依赖 | 版本 |
| --- | --- |
| `rutis` | 0.6（不变） |
| `rutis-interop`（可选） | 0.2 → 0.3 |
| `rutis-bridge`、`rutis-channel`（可选，feature `peer`） | 新增，0.1 |

## 不用 `InteropResolver` 的应用

不用改代码，升级版本号即可。

## 用 `InteropResolver` 的应用

要改，见 [rutis-interop 0.2 → 0.3](migration-interop-0.2-to-0.3.md)：

- 挂 `RuntimeRowsPlugin`，并用 `Chain::with_shared` 和它共用同一个 `InteropResolver`；
- 宿主服务的名字用 `ServiceCatalog::register_shared` 登记；
- 本机运行时改用 `rutis-runtime-local` 的 `LocalRuntime`。

## features

| feature | 内容 |
| --- | --- |
| `node` | `InteropResolver::node`：JavaScript（Cordis）插件作为行 |
| `python` | `InteropResolver::modules`：Python 叶子插件，行名 `py:<模块名>` |
| `interop` | 以上两者；0.1 的 `interop` 配置不用改 |
| `peer` | 在其他节点上运行的行，见下文 |

## 新增

- `RuntimeRowsPlugin`、`RuntimeRows`（`CordisRuntimeRows` 是弃用的别名）。
- `ServiceCatalog::register_shared` / `is_shared`；`Chain::with_shared`；`Loader::resolve`。
- `InteropResolver::with_catalog`、`invalidate` / `invalidate_all`（包内容变了但版本号没变时手动失效）。
- feature `peer`：
  - `peer:<对端>/<插件>` 行在对端加载插件（经 rutis-bridge 的 `HostPlugin`）。`PeerResolver` 解析它们，`PeerRowsPlugin` 在对端提供宿主后才放行这些行；对端不在线时行等待，不算解析失败。
  - 行的 `isolate` / `inject` 交给对端执行，名字按对端 loader 的服务目录映射。
  - `register_peer_node`：`rutis-bridge/peer` 行，在配置里声明一个节点（link 与 export / import / events / host / runtime）；改 export、import、events 只重启对应功能，会话保留。`node_schema` 是它的配置 Schema，`LoaderCatalog` 让对端加载本 loader 能解析的插件。
