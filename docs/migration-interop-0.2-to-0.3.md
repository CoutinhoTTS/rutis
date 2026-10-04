# rutis-interop 0.2 → 0.3：服务按名字跨语言共享

这一版是多语言插件 M1（设计见 [多语言插件 M1](design-multilang-m1-2026-10-04.md)）：用 rutis-loader 动态加载的 JavaScript 插件，可以和 rutis 按名字共享服务。内核 `rutis` 不变，协议版本仍是 2。

| 包 | 版本 | 要不要改代码 |
| --- | --- | --- |
| `rutis-interop`（crate）与 `@arcships/rutis-interop`（npm） | 0.2.0 → 0.3.0 | 两者必须一起升级 |
| `rutis-loader` | 下一个次版本 | 用 `InteropResolver` 的应用要改，见下文 |

只用构建期生成的静态挂载（`include_mounts!`）的应用，升级版本号即可，代码不用改：静态挂载仍在启动时一次注册宿主服务。

## 两个包一起升级

```toml
rutis-interop = "0.3"
```

```json
"@arcships/rutis-interop": "0.3.0"
```

旧的 npm 包不报告新功能。Rust 侧用到新接口（`describe_row`、`lease_host`、带导出的 `load_row_exporting`）时会报错“需要 @arcships/rutis-interop 0.3.0 或更高”。

## 用 `InteropResolver` 加载 JavaScript 行的应用

1. **挂上 `RuntimeRowsPlugin`。** 行现在依赖它提供的 `CordisRuntimeRows`，不挂的话行会一直等待；诊断里的等待原因会指向 `CordisRuntimeRows`。它要和 loader 共用同一个 `InteropResolver`，所以把 resolver 放进 `Arc`，用 `Chain::with_shared` 加入：

   ```rust
   let resolver = Arc::new(InteropResolver::new(runtime.handle()).with_catalog(&catalog));
   root.plugin(LoaderPlugin::new(Chain::new().with_shared(resolver.clone()), options)).await?;
   root.plugin(RuntimeRowsPlugin::new(resolver));
   ```

2. **宿主服务的名字登记为共享。** 以前 `.host(名字, 方法)` 让运行时等这个服务，并在启动时注册进 Cordis。现在运行时不等任何服务，宿主服务由用到它的行去等、在运行期间注册进 Cordis。行判断"用到"的依据是插件自己的 `inject`，并且只认 catalog 里 `register_shared` 登记过的名字：

   ```rust
   catalog.register_shared("probe");
   ```

   没登记的话，插件在 Cordis 里等不到这个服务，会一直不启动。

3. **行为变化。** 宿主服务被撤销或替换时，以前整个运行时连同所有行一起重启；现在只有用到它的行重启，运行时和其他行不受影响。

`.host(名字, 方法)` 仍然保留，只用来声明方法形状。宿主服务的实现也可以自己报出形状（`HostDispatch::methods`），这时不需要 `.host`。

## 新增

- `Process::describe_row`（替代 `row_schema`）：插件的配置 Schema、`inject` 的服务名，以及包 `package.json` 里 `rutis.provides` 声明的服务。
- `Process::load_row_exporting`、`row_projection`、`RowService`：行提供的服务注册为 `host_key(名字)` 下的 `dyn HostDispatch`。
- `Process::lease_host` / `HostLease`：按行注册宿主服务，计数，最后一个释放时撤销。
- `HostDispatch::methods`、`HostDispatch::origin`：都有默认实现，现有实现不用改。
- `Projection::service_keyed`：投影到任意键。
- rutis-loader：`ServiceCatalog::register_shared` / `is_shared`、`InteropResolver::with_catalog`、`Chain::with_shared`、`RuntimeRowsPlugin`、`CordisRuntimeRows`。
