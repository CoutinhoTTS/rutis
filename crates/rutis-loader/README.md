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
| `rutis_dylib::DylibResolver` | `dylib:<目录>` | Linux 上的 dylib 插件（rutis-dylib 的 `loader` feature） |
| `InteropResolver` | npm 包名、包的子路径、文件路径 | JavaScript（Cordis）插件，共用一个 Node 进程与 Cordis Context（本 crate 的 `interop` feature，Unix） |
