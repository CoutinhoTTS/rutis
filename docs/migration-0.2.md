# 升级到 0.2（发布列车）

从已发布的 `rutis-interop` 0.2 / `@arcships/rutis-interop` 0.2、`rutis-loader` 0.1 升级到 0.2。内核 `rutis` 不变。

这一版重新划分了包：插件作者、宿主、不写 Rust 的人各装各的包；除内核外的包一起发布、版本号相同（0.2.0）。新增的内容见 [指南](guide/README.md)；设计见 [面向开发者的包与流程](design-developer-packages-2026-10-06.md)。

## 包

| 以前 | 现在 |
| --- | --- |
| `rutis-interop`（crate） | `rutis-bridge`：会话协议在 `session`，语言运行时在 `runtime`，Cordis 静态挂载在 `cordis`（feature `cordis`） |
| `@arcships/rutis-interop`（npm，运行时） | `@arcships/rutis-runtime` |
| `@arcships/rutis-interop/plugin`（`definePlugin`） | `@arcships/rutis`（插件作者唯一需要的包，无依赖） |
| 仓库里的 Python 包 `rutis_runtime` | PyPI 上的 `rutis`（`import rutis`，`python -m rutis`） |
| `rutis-loader` 0.1 | `rutis-loader` 0.2 |
| — | `rutis-host`：不写 Rust 的宿主（crates.io、npm `@arcships/rutis-host`、PyPI） |

`@arcships/rutis-interop` 在 npm 上标记为弃用。

## 在 Rust 里挂载 Cordis 插件（静态挂载）

```toml
[dependencies]
rutis-bridge = { version = "0.2", features = ["cordis"] }

[build-dependencies]
rutis-bridge = { version = "0.2", features = ["cordis"] }

[package.metadata.rutis-cordis]        # 以前是 [package.metadata.rutis-interop]
npm = "cordis"
```

| 以前 | 现在 |
| --- | --- |
| `rutis_interop::build::from_manifest()`（build.rs） | `rutis_bridge::cordis::build::from_manifest()` |
| `rutis_interop::include_mounts!()` | `rutis_bridge::include_mounts!()` |
| `rutis_interop::Error`、`ObjectRef`、`RemoteFunction`、`JsError` | `rutis_bridge::cordis::…`（也在 `rutis_bridge::session`） |
| `rutis_interop::server::Dispatch` | `rutis_bridge::cordis::server::Dispatch` |
| `RUTIS_INTEROP_ROOT` | `RUTIS_CORDIS_ROOT` |
| npm 项目里的 `@arcships/rutis-interop` | `@arcships/rutis-runtime` |

生成的绑定在构建时重新生成，不需要手改。

## 语言运行时

| 以前 | 现在 |
| --- | --- |
| `CordisRuntimePlugin::new(包, anchor)` | `rutis_bridge::runtime::LocalRuntime::node(包, anchor)` |
| `CordisRuntime` | `rutis_bridge::runtime::Runtime` |
| `rutis_interop::HostDispatch`、`host_key` | `rutis_bridge::session::{HostDispatch, host_key}` |
| `rutis_interop::rpc::{Connection, Value, Reply, …}` | `rutis_bridge::session::{Connection, Value, Reply, …}` |
| `Process`、`Mount`、`Launcher` | `rutis_bridge::runtime::{Process, Mount, Launcher}` |

- 本机运行时现在由本机承载启动进程、经 link 接入，与远程运行时（`RuntimePlugin::remote`）走同一个入口；行完全一样。
- 进程意外结束时，`RuntimeHandle::state()` 是 `Down(原因)`，原因说明进程怎样结束；重启由应用对 `LocalRuntime` 的 fiber 调用 `restart`。
- 运行时有名字（`.named(..)`，Node 默认 `"node"`），服务键是 `Runtime::key(名字)`；直接 `ctx.require::<CordisRuntime>()` 的代码改为 `ctx.require_as::<Runtime>(Runtime::key("node"))`。
- Python：`LocalRuntime::python(插件目录)`，解释器用 `.interpreter(路径)` 指定，它的环境里要装 `rutis`。

## rutis-loader

| 以前 | 现在 |
| --- | --- |
| feature `interop` | `node`、`python`（按语言开）；`peer`：节点行和 `peer:` 行 |
| `InteropResolver::new(handle)` | `RuntimeResolver::node(handle)`；Python：`RuntimeResolver::modules(handle)` |
| `CordisRuntimeRows` | `RuntimeRows` |
| `ServiceCatalog::key(..)` 返回 `Option<&TypeKey>` | 返回 `Option<TypeKey>` |

用 `RuntimeResolver` 加载 JavaScript 行的应用还要：

1. **挂上 `RuntimeRowsPlugin`**，并让它和 loader 共用同一个 resolver（`Chain::with_shared`）。行依赖它提供的 `RuntimeRows`，不挂的话行会一直等待。

   ```rust
   let resolver = Arc::new(RuntimeResolver::node(runtime.handle()).with_catalog(&catalog));
   root.plugin(LoaderPlugin::new(Chain::new().with_shared(resolver.clone()), options)).await?;
   root.plugin(RuntimeRowsPlugin::new(resolver));
   ```

2. **宿主服务的名字登记为共享**：`catalog.register_shared("probe")`，或者 `catalog.share_by_name()` 让所有名字都按名字共享（rutis-host 的做法）。以前运行时等待 `.host(..)` 声明的服务并在启动时注册进 Cordis；现在由用到它的行去等，宿主服务撤销或替换时只有用到它的行重启。

完整的写法见 [在 Rust 应用里嵌入](guide/rust-host.md)。

## 节点与网络（新增）

连接多台机器、远程运行时、把 Cordis 应用接成节点，见 [连接节点](guide/nodes.md)。凭据的环境变量为 `RUTIS_TOKEN`、`RUTIS_CA`、`RUTIS_CERT`、`RUTIS_KEY`。

## 插件作者

- TypeScript / JavaScript：依赖 `@arcships/rutis`，`import { definePlugin } from '@arcships/rutis'`；测试用 `@arcships/rutis/testing`。
- Python：依赖 `rutis`，`from rutis import define_plugin`；测试用 `rutis.testing`；打包的插件在入口点组 `rutis.plugins` 下注册。
- 插件 API 版本为 1；运行时比插件旧时明确报错。

见 [写一个 TypeScript 插件](guide/typescript-plugin.md)、[写一个 Python 插件](guide/python-plugin.md)。
