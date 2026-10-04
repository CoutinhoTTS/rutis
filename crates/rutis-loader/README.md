# rutis-loader

rutis 的插件管理层：按数据决定装哪些插件、怎么配置。对应 cordis 的 `cordis-plugin-loader` + `cordis-plugin-include`。设计见 [docs/design-rutis-loader-2026-10-02.md](../../docs/design-rutis-loader-2026-10-02.md)。

- 输入是有序的 patch 层（期望状态），`reconcile` 让运行态向它收敛；
- 命令式修改（`create` / `update` / `set_disabled` / `rename_module` / `move_to` / `remove`）只改可编辑层，等树稳定后返回，启动失败自动回滚；
- 改动经 `Persist` 钩子保存，版本冲突时在最新内容上重放待保存队列；
- 不读写任何文件。插件组合写死在代码里的项目直接用 `ctx.plugin`，不需要它。

```rust
use rutis::Ctx;
use rutis_loader::{Builtins, Editable, Layer, LoaderOptions, LoaderPlugin, Version};

let mut builtins = Builtins::new();
builtins.register::<MyConfig, _>("my-plugin", MyFactory);

let plugin = LoaderPlugin::new(builtins, LoaderOptions::default());
let loader = plugin.handle();
root.plugin(plugin).await?;

let report = loader
    .reconcile(
        vec![Layer::new("defaults", defaults), Layer::new("user", user)],
        Some(Editable::new("user", Version::default())),
    )
    .await?;
loader.update("my-row", serde_json::json!({ "level": 2 })).await?;
```

配置里的 `inject` / `isolate` 用 `ServiceCatalog` 把服务名对应到 `TypeKey`；`{ "__jsExpr": .. }` 表达式由 `LoaderOptions::expressions` 求值，loader 自己不带求值器（dsh 的在 rutis-dsh）。没登记的服务名、没装求值器时，相关行状态为 `Unresolved`。

插件生命周期：

- **volatile 字段**：配置 schema 中带 `"x-volatile": true` 的字段（schemars：`#[schemars(extend("x-volatile" = true))]`）只改了它们时不重启，loader 存下新配置并向插件发 `VolatileUpdate`；插件在 apply 里 `ctx.events().on(ctx, &volatile_key(ctx), ...)` 接收。
- **插件卸载自己**：插件调 `ctx.dispose_self()`，loader 把该行设为 disabled 写进可编辑层，并发 `LoaderChanged::SelfDisposed`。

插件来源（`Resolver`）：

| 来源 | 名字 | 说明 |
| --- | --- | --- |
| `Builtins` | 注册时给的任意名字 | 编译进宿主的插件 |
| `rutis_dylib::DylibResolver` | `dylib:<目录>` | dylib 插件（rutis-dylib 的 `loader` feature，Linux、macOS、Windows x64/MSVC） |
| `InteropResolver` | npm 包名、包的子路径、文件路径 | JavaScript（Cordis）插件，装进 rutis-interop 的 `RuntimePlugin`（一个 Node 进程与 Cordis Context）；各行依赖 `RuntimeRowsPlugin` 提供的 `RuntimeRows`，运行时没就绪或进程退出时等待（本 crate 的 `interop` feature，Unix）。服务按名字与 rutis 共享，见下文 |

其他语言的插件按语言启用 feature，应用只编译、只启动它启用并挂载的运行时：

| feature | 行 |
| --- | --- |
| `node` | JavaScript/TypeScript：Cordis 插件和 `definePlugin` 叶子插件，`InteropResolver::node` |
| `python` | Python 叶子插件，行名 `py:<模块名>`，`InteropResolver::modules` |
| `interop` | 两者都要 |

JavaScript 行与 rutis 按名字共享服务，键都是 `rutis_interop::host_key(名字)`（`dyn HostDispatch`）：

- 插件 `inject` 的服务名，在 catalog 里用 `register_shared` 登记过的，由 rutis 门控：服务就绪才启动这一行，撤销就停下，运行期间把它注册进 Cordis。没登记的名字仍交给 Cordis 自己门控（同一个 Node 进程里插件之间的依赖）。
- 插件所在包的 `package.json` 里 `rutis.provides` 声明的服务（`{ "名字": { "方法": "sync" | "async" } }`）会投到 rutis，注册在这一行的 fiber 上，Rust 插件和其他行可以按名字 inject。同一个 Node 进程里的行用它时直接拿 Cordis 里的原生对象。
- 应用依次挂载 `RuntimePlugin`、`LoaderPlugin`、`RuntimeRowsPlugin`，并让后两者共用同一个 `InteropResolver`：

```rust
let mut catalog = ServiceCatalog::new();
catalog.register_shared("llm").register_shared("weather");
let runtime = RuntimePlugin::node(node_package, anchor);
let resolver = Arc::new(InteropResolver::node(runtime.handle()).with_catalog(&catalog));
root.plugin(runtime);
let options = LoaderOptions { catalog, ..LoaderOptions::default() };
root.plugin(LoaderPlugin::new(Chain::new().with_shared(resolver.clone()), options)).await?;
root.plugin(RuntimeRowsPlugin::new(resolver));
```

`RuntimeRowsPlugin` 在运行时启动后先重新解析运行时启动前解析的行（以及包版本变了的行），拿到插件声明的依赖，然后才提供 `RuntimeRows`，所以行启动时依赖声明是完整的。

其他语言的插件也是行。Python 运行时（`RuntimePlugin::python`）的行名是 `py:<模块名>`，用 `InteropResolver::modules(运行时句柄)` 解析，同样配一个 `RuntimeRowsPlugin`。Python 插件 `inject` 的每个名字都在 rutis 里门控（Python 那边没有自己的依赖解析），不需要 `register_shared`；但 Rust 插件或 JavaScript 插件要用 Python 插件提供的服务时，那个名字仍要登记为共享。

```rust
let python = RuntimePlugin::python("interop/python", "plugins/py");
let python_rows = Arc::new(InteropResolver::modules(python.handle()).with_catalog(&catalog));
root.plugin(python);
// Chain 里同时放 Node 和 Python 两个 resolver，各配一个 RuntimeRowsPlugin。
```

JavaScript/TypeScript 也可以写成和 Python 一样的叶子插件，不用接触 Cordis：

```ts
import { definePlugin } from '@arcships/rutis-interop/plugin'
export default definePlugin({
  inject: ['llm'],
  provides: { weather: { today: 'async' } },
  apply(ctx, config) {
    ctx.provide('weather', new Weather(ctx.use('llm'), config.city))
    return () => {}
  },
})
```

它装进 Node 运行时的 Cordis Context，和 Cordis 插件在同一个进程里，互相用服务不走进程间通信。

行卸载时，先撤销它投到 rutis 的服务、等用到这些服务的插件都停下，再卸载插件本身，所以提供者总是比它的使用者后停。
