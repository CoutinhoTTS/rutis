# rutis 0.6.0 → 0.6.1：插件控制面

0.6.1 起，rutis 应用可以用数据驱动的方式管理插件：装哪些插件、怎么配置都写在配置里，运行时可以增删插件、改配置、热更新。这些功能放在新 crate `rutis-loader` 0.1.0 里。内核 `rutis` 只为它补了几个接口，没有破坏性变更。设计见 [design-rutis-loader](design-rutis-loader-2026-10-02.md)。

本次一起发布的包：

| 包 | 版本 | 要不要改代码 |
| --- | --- | --- |
| `rutis` | 0.6.0 → 0.6.1 | 不用改 |
| `rutis-interop`（crate）与 `@arcships/rutis-interop`（npm） | 0.1.0 → 0.2.0，协议版本 1 → 2 | 两者必须一起升级；手写的 `Mount { .. }` 要补一个字段 |
| `rutis-loader` | 新发布 0.1.0 | 想用控制面时才需要接入 |

## 只用 rutis 内核

把依赖升到 `rutis = "0.6.1"` 即可，代码不用改。新增的接口都是可选的：

- `impl Plugin for Box<dyn Plugin>`：运行时才决定装哪个插件时（例如从配置构造出一组插件），装箱的插件可以直接交给 `ctx.plugin`。
- `Ctx::view(plugin_id)`：按诊断或事件里拿到的 `PluginId` 找到该插件的 `FiberView`。
- `Ctx::dispose_self()`：插件卸载自己，相当于 cordis 的 `ctx.fiber.dispose()`。调用后立即返回，插件在当前 apply 或回调结束后卸载，不要在插件内部等待它。根上下文不能这样卸载（用 `Ctx::shutdown`）；重启之后，旧一代的上下文调用会得到 `InactiveEffect`，不会卸掉新一代。
- `FiberView::instance()`：插件实例的 `InstanceId`。
- `FiberView::set_config(config)`：只存新配置、不重启。插件自己就地应用变化时用（cordis 的 volatile 字段）；其他情况仍用 `update`。
- 事件 `ServiceChanged`：服务绑定注册或最终移除时，在提供方上下文的总线上发出（`change` 为 `Provided` / `Removed`）。行为上唯一的变化是总线上多了这一种事件；不订阅就没有影响。

## 使用 rutis-interop

### 两个包一起升级

```toml
# Cargo.toml
rutis-interop = "0.2"
```

```json
// npm 项目的 package.json
"@arcships/rutis-interop": "0.2.0"
```

升级后运行一次 `npm install`（或 `npm ci`）。

协议版本升到了 2：crate 与 npm 运行时版本不一致时，构建会报 `... speaks protocol 1, this rutis-interop speaks 2: install a matching @arcships/rutis-interop`，进程握手也会拒绝。只升一边时按提示把另一边升上来即可。

### 手写的 `Mount`

`Mount` 新增字段 `anchor`（不带插件时启动空的 Cordis Context，见下文）。用结构体字面量写全所有字段的地方要补上它：

| 0.1 | 0.2 |
| --- | --- |
| `Mount { plugins, services, observer, hosts, events, emits }` | `Mount { plugins, services, observer, hosts, events, emits, anchor: None }` |
| `Mount { plugins, ..Mount::default() }` | 不用改 |

由 `build.rs`（`rutis_interop::build::from_manifest`）生成的绑定不用改：它们由 npm 运行时里的生成器生成，两个包升级到 0.2 后会自动重新生成。

### 新增

- 生成的配置结构体同时实现 `Deserialize`，可以从 JSON（例如 loader 一行的配置）构造。
- 逐个装载：`Mount { anchor: Some(package_json), .. }` 不带插件时启动空的 Cordis Context，之后用 `Process::load_row` / `update_row` / `unload_row` 逐个装载、更新、卸载插件，`row_schema` 读取插件 schemastery `Config` 转成的 JSON Schema。
- `CordisRuntimePlugin`：把这个 Node 进程和 Cordis Context 作为普通插件挂进 rutis，生命周期跟随 fiber 树。见 [rutis-interop README](../crates/rutis-interop/README.md#逐个装载rows)。

## 接入 rutis-loader（可选）

插件组合写死在代码里、不需要运行时增删的应用，继续直接用 `ctx.plugin`，不需要 rutis-loader。

需要时，把原来的一串 `root.plugin(...)` 改成“注册插件来源 + 用配置描述装哪些”：

```toml
rutis-loader = "0.1"
```

```rust
use rutis_loader::{Builtins, Layer, LoaderOptions, LoaderPlugin};

// 原来：root.plugin_with(MyFactory, MyConfig { level: 1 }).await?;
let mut builtins = Builtins::new();
builtins.register::<MyConfig, _>("my-plugin", MyFactory);

let plugin = LoaderPlugin::new(builtins, LoaderOptions::default());
let loader = plugin.handle();
root.plugin(plugin).await?;

let rows: Vec<rutis_loader::Patch> = serde_json::from_value(serde_json::json!([
    { "insert": [{ "id": "main", "name": "my-plugin", "config": { "level": 1 } }] }
]))?;
loader.reconcile(vec![Layer::new("app", rows)], None).await?;

// 运行时修改：改配置、停用、删除……
loader.update("main", serde_json::json!({ "level": 2 })).await?;
```

插件本身不用改：工厂（`PluginFactory`）照常构造实例。要让 loader 拿到配置 schema，注册时给出 schema（`Builtins::register` 对实现了 `JsonSchema` 的配置自动生成）。插件可以选择支持：

- **volatile 字段**：schema 中带 `"x-volatile": true` 的字段只改了它们时不重启，插件在 apply 里 `ctx.events().on(ctx, &volatile_key(ctx), ...)` 接收 `VolatileUpdate`。
- **卸载自己**：`ctx.dispose_self()`，loader 会把该行设为停用并写进可编辑层。

插件来源除了编译进宿主的 `Builtins`，还有 Linux 上的 dylib 插件（`rutis-dylib` 的 `loader` feature，`dylib:<目录>`）和 JavaScript（Cordis）插件（本 crate 的 `interop` feature）。旧版 `rutis-dylib` 编译的 dylib 插件不用重新编译；要让 loader 拿到配置 schema，用 `export_plugin!(..., schema: ...)` 重新编译。

配置分层、可编辑层与持久化、include、表达式等见 [rutis-loader README](../crates/rutis-loader/README.md)。
