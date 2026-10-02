# 一方插件 dylib SDK：Linux 使用说明

本能力对应 [设计稿](design-dylib-sdk-2026-09-24.md)。默认 `rutis-cli` 仍静态链接；只有启用 `dylib-plugins` 的发布包会加载一方可信插件。初版只支持 Linux ELF64 小端目标。插件代码与宿主处于同一进程，崩溃会带走宿主。

## 构建宿主发布包

使用固定的 Rust 1.98.1 工具链，在仓库根目录运行：

```sh
bash tools/build-dylib-bundle.sh
```

脚本以 `RUTIS_SDK_LOCKFILE` 指定本次解析实际使用的 `Cargo.lock`，再按宿主的完整 Cargo feature 图构建 SDK，计算 `librutis_sdk.so` 的 SHA-256，再把该哈希编入宿主。最后把宿主、SDK、动态 libstd 的文件名与哈希编入独立启动器。脚本输出一个新的 `target/dylib-bundles/<hash>/` 目录，包含公开入口 `rutis-cli`、内部宿主 `rutis-cli-host`、SDK、libstd 和 `sdk.toml`。它拒绝覆盖既有目录。正式部署应将整个目录安装到可信、不可原地修改的版本路径；更新时创建新目录。

只能从目录里的 `rutis-cli` 启动。启动器不链接 SDK 或动态 libstd，先对三个运行文件校验哈希，再以固定目录作为动态库搜索路径执行内部宿主。内部宿主启动后再次核对实际加载的 SDK，并在启动运行时线程前恢复调用者原有的 `LD_*` 环境变量，使子进程沿用调用者的库路径。

## 编写与打包插件

插件 crate 使用 `crate-type = ["dylib"]`，依赖同一 SDK 版本。配置类型是 `rutis_sdk::ConfigValue`（`serde_json::Value`）。工厂示例见 [greeter-v1](../tests/dylib-fixtures/greeter-v1/src/lib.rs)：

```rust
rutis_sdk::export_plugin! { id: "greeter", factory: Factory }
```

宏生成 Rust ABI 工厂入口、运行期元数据和加载前可解析的 ELF 引导节。插件不得定义自己的全局分配器，必须使用 `panic = "unwind"`。跨插件或宿主交换的自定义服务与事件类型必须由 SDK 中的接口 crate 提供；插件私有类型只留在插件内部。后台任务应观察 `ctx.cancelled()`；旧代 `Ctx` 的注册在换代后会被拒绝。

把 SDK 发布包中的 `sdk.toml` 和 `librutis_sdk.so` 提供给打包命令：

```sh
cargo xtask pack-plugin \
  --manifest-path tests/dylib-fixtures/greeter-v1/Cargo.toml \
  --sdk-manifest target/dylib-bundles/<hash>/sdk.toml \
  --sdk-file target/dylib-bundles/<hash>/librutis_sdk.so \
  --features export \
  --output /tmp/greeter-v1
```

发布清单会指定构建锚点 `rutis-cli/dylib-plugins`。打包器使用插件工作区的 `Cargo.lock` 设置 `RUTIS_SDK_LOCKFILE`，在同一 Cargo feature 图中分两次编译宿主与插件，先得出 SDK 产物哈希，再把它编入插件；宿主的临时构建结果无需重新发布。打包器复核 SDK 文件、插件 ELF 引导节、依赖重复版本和插件自定义分配器，再从实际二进制生成 `plugin.toml`。如插件位于另一个 Cargo 工作区，需在 SDK 发布流水线中用同一 feature 图构建，并用 `--prebuilt-library` 打包；单独构建产生不同 SDK 哈希时会被拒绝。

`rutis-cli` 的 dylib 变体可用 `--plugin <目录> --plugin-config '<JSON>'` 装载一个插件。需要代码换代的宿主调用 `Loader::load`、`Loader::spawn` 和 `Loader::swap`；`swap` 复用 rutis 的 `FiberView::update`，消费者会随服务卸载和重新提供而重载。`DylibConfig::new` 的模块身份、名称和依赖声明检查位于工厂内部，直接调用 `view.update` 也不能绕过。加载器每个插件 id 默认最多保留四个已映射版本；旧版本永不 `dlclose`，达到上限后须重启回收。

## 验证与边界

本仓库的 Linux 验证命令：

```sh
bash tools/test-dylib.sh
bash tools/test-dylib-launcher.sh
bash tools/test-dylib-repro.sh
cargo test --workspace
```

测试覆盖插件内 `tokio::spawn`、跨库 `Snapshot` 类型和 `String` 服务读取与 downcast、v1→v2 换代后的消费者重载与旧插件析构、错误 L1/L2 在 `dlopen` 前拒绝且 ELF 初始化函数未运行、模块身份变更经 `swap` 与直接 `update` 均被拒绝、版本保留上限、损坏缓存的原子修复与失败入口的同版本重试、宿主/SDK/libstd 文件改动时启动器拒绝、环境库路径覆盖下从自身目录启动、不同源码与 target 路径的 SDK 字节一致，以及旧代迟到注册在两种 tokio 运行时及 Failed 状态下被拒绝。CI 另在两个独立 runner 上构建 SDK 并比较产物哈希。

macOS 与 Windows 的动态加载路径尚未验证；这些平台保持静态构建。直接执行内部宿主没有启动前校验，不能作为 dylib 发布入口。发布目录与插件缓存必须由可信部署控制，运行期间不得原地改写文件。
