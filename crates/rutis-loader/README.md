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

P1 尚未支持（P2 补上）：配置里的 `inject` / `isolate`、`!!js` 表达式；用到它们的行状态为 `Unresolved`。
