# 外部插件开发指南：用 SDK 构建包开发 rutis dylib 插件

面向**不在 rutis 仓库里**开发插件的作者：你有自己的仓库、自己的版本节奏，宿主（rutis
应用）由别人发布。设计与边界见 [dylib SDK 设计](design-dylib-sdk-2026-09-24.md) 与
[SDK 构建包设计](design-sdk-build-package-2026-10-04.md)；本文只讲操作。

宿主方发布两类东西，你都需要：

| 产物 | 内容 | 给你做什么用 |
| --- | --- | --- |
| **运行发布目录**（宿主方分发给用户） | `rutis-cli`、宿主、SDK、动态 libstd、启动器 | 本地跑你的插件 |
| **sdk-bundle**（宿主方分发给插件作者） | 预编译 SDK、依赖闭包 rlib、`sdk.toml`、`bundle.toml`、工具链、`GUIDE.md` | 编译和打包你的插件 |

两者通过同一份 SDK 产物（`sdk.toml` 里的 `artifact_sha256`）关联。你构建的插件只链接
那一份 SDK；宿主运行时加载的也是那一份，字节不同会被拒绝。

## 一次搭好工作区

```text
my-plugin/
  .cargo/config.toml     # 从 sdk-bundle 的 cargo-config.toml 复制，改两处路径
  rust-toolchain.toml    # 从 sdk-bundle 复制（必须，插件与 SDK 用同一个 rustc）
  Cargo.toml
  src/lib.rs
```

`.cargo/config.toml` 里 `--extern` 与 `-L dependency` 指向你解压的 sdk-bundle 位置。
两个注意点写在文件头部注释里，这里再强调：

- **环境变量 `RUSTFLAGS` 会整体覆盖这份配置**（不是追加）。shell 或 CI 里设置过它，
  `cargo check` 就会报误导性的 `E0463: can't find crate for rutis_sdk`。构建前清掉。
- rust-analyzer 的补全和跳转可能不解析 `rutis_sdk::` 路径（它不在 cargo 的 crate graph
  里）；`cargo check` 与 flycheck 正常。这是已知限制，不是你的配置错了。

## 写插件

`Cargo.toml`——注意**没有** `rutis-sdk` 依赖：

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["dylib"]
test = false
doctest = false

[features]
export = []

[dependencies]
# 私有依赖随便加，但 rutis / tokio / tokio-util / serde_json / rutis-sdk
# 五个名字不能出现在这里（打包器会拒绝）。共享 crate 从 rutis_sdk 的
# 再导出使用：rutis_sdk::{rutis, tokio, serde_json}。
```

`src/lib.rs`：

```rust
use rutis_sdk::rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, PluginFactory};
use rutis_sdk::ConfigValue;

struct Factory;
struct MyPlugin;

impl PluginFactory<ConfigValue> for Factory {
    fn name(&self) -> &str {
        "my-plugin"
    }
    fn build(&self, config: &ConfigValue) -> Result<Box<dyn Plugin>, CordisError> {
        let _ = config;
        Ok(Box::new(MyPlugin))
    }
}

impl Plugin for MyPlugin {
    fn name(&self) -> &str {
        "my-plugin"
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Effect, CordisError>> {
        Box::pin(async move {
            // tokio 直接用再导出：任务跑在宿主的运行时上。
            rutis_sdk::tokio::spawn(async {})
                .await
                .map_err(|e| CordisError::PluginFailed(e.into()))?;
            ctx.provide("hello from my-plugin".to_string())?;
            Ok(Effect::Done)
        })
    }
}

rutis_sdk::export_plugin! { id: "my-plugin", factory: Factory }
```

私有依赖的类型可以随便用，但**不能出现在跨插件/跨宿主的边界上**：服务、事件、配置里
交换的类型必须来自 `rutis_sdk` 的再导出。给宿主传结构化数据用
`rutis_sdk::serde_json::json!({...})` 构造 `ConfigValue`——不要试图给你自己的类型 derive
`Serialize` 再喂给 SDK 的 `serde_json`（你私有副本的 trait 和 SDK 闭包里的不是同一个，
编译期报 E0277）。

## 打包

```sh
# 在 rutis 仓库里（目前打包器是仓库的 xtask 命令）：
cargo xtask pack-plugin --bundle <sdk-bundle 目录> \
  --manifest-path <你的插件>/Cargo.toml \
  --features export \
  --output <分发目录>
```

打包器会：校验整个构建包的文件哈希、核对 rustc 完整版本、拒绝共享 crate 的直接依赖、
注入 SDK 构建、检查分配器/weak 导出/原生库依赖/引导节身份，然后产出插件目录（二进制 +
`plugin.toml`）。把这个目录交给宿主方或用户即可。

本地验证（用宿主方给的运行发布目录）：

```sh
<运行目录>/rutis-cli --scripted --load-only --plugin <分发目录> --plugin-config '{}'
```

`--load-only` 加载插件、跑完 apply 即退出，不启动终端界面（无控制台的环境也安全）。退出码 0 即加载成功。

## 排错

| 症状 | 原因与处理 |
| --- | --- |
| `E0463: can't find crate for rutis_sdk`（且没动过配置） | 环境里有 `RUSTFLAGS`，覆盖了 `.cargo/config.toml`；清掉再试。若刚改过 sdk-bundle 位置，检查 config 里的两个路径 |
| `E0514: found crate rutis_sdk compiled by an incompatible version of rustc` | 工具链不是构建包固定的那个；把包里的 `rust-toolchain.toml` 放进工作区（rustup 会自动切换） |
| 打包失败，提到 `colliding StableCrateId` 或"overlaps the SDK's dependency closure" | 某个私有依赖把 `tokio`/`serde` 系拉进了你的图，而且你的代码用到了它。去掉该依赖，或改用 `rutis_sdk::` 的再导出 |
| 打包失败，"declares … as a direct dependency" | `Cargo.toml` 里直接写了共享 crate；换成 `rutis_sdk::` 再导出 |
| 打包失败，"bundle is missing …" / "differs from the bundle manifest" | 构建包缺件或被改动；重新下载 |
| E0277：私有类型的 trait 不满足（比如 `Serialize`） | 跨副本 trait 不互通；数据走 `json!`/`ConfigValue`（见上文） |

## SDK 升级

宿主方升级 SDK 后，旧插件在新宿主上因 L1/L2 不符被拒绝，用户侧不可加载。拿到新的
sdk-bundle（`sdk.toml` 的 `artifact_sha256` 变了）后重新打包插件即可；你的源码通常不用
改（除非 SDK 的 API 变了）。工作区里只需替换 sdk-bundle 并更新 `.cargo/config.toml`
的路径。

## 已知限制

- 打包命令目前是 rutis 仓库的 `cargo xtask`——你需要一份 rutis 仓库的克隆，或由宿主方
  提供打包服务。独立打包工具是后续工作。
- rust-analyzer 对 `rutis_sdk::` 路径的补全不可靠（见上）。
- 插件与宿主同进程运行：崩溃会带走宿主，插件代码必须可信。这是 dylib 插件的定位；
  不受控代码应走协议插件（`rutis-interop`）。
