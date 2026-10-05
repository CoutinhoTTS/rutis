# 一方插件 dylib SDK：Linux、macOS 与 Windows 使用说明

本能力对应 [设计稿](design-dylib-sdk-2026-09-24.md)。默认 `rutis-cli` 仍静态链接；只有启用 `dylib-plugins` 的发布包会加载一方可信插件。支持 Linux（ELF64 小端）、macOS（arm64，Mach-O）和 Windows（x64，`x86_64-pc-windows-msvc`，PE）；macOS 和 Windows 部分的设计见 [design-dylib-macos-windows](design-dylib-macos-windows-2026-10-03.md)，Windows 的可行性验证记录在设计稿 §十一。插件代码与宿主处于同一进程，崩溃会带走宿主。

## 构建宿主发布包

使用固定的 Rust 1.98.1 工具链，在仓库根目录运行：

```sh
bash tools/build-dylib-bundle.sh
```

脚本以 `RUTIS_SDK_LOCKFILE` 指定本次解析实际使用的 `Cargo.lock`，再按宿主的完整 Cargo feature 图构建 SDK，计算 `librutis_sdk.so`（macOS 为 `librutis_sdk.dylib`）的 SHA-256，再把该哈希编入宿主。最后把宿主、SDK、动态 libstd 的文件名与哈希编入独立启动器。脚本输出一个新的 `target/dylib-bundles/<hash>/` 目录，包含公开入口 `rutis-cli`、内部宿主 `rutis-cli-host`、SDK、libstd 和 `sdk.toml`。它拒绝覆盖既有目录。正式部署应将整个目录安装到可信、不可原地修改的版本路径；更新时创建新目录。

只能从目录里的 `rutis-cli` 启动。启动器不链接 SDK 或动态 libstd，先对三个运行文件校验哈希，再以固定目录作为动态库搜索路径执行内部宿主。内部宿主启动后再次核对实际加载的 SDK，并在启动运行时线程前恢复调用者原有的 `LD_*` 环境变量，使子进程沿用调用者的库路径。

## 编写与打包插件

插件 crate 使用 `crate-type = ["dylib"]`，依赖同一 SDK 版本。配置类型是 `rutis_sdk::ConfigValue`（`serde_json::Value`）。工厂示例见 [greeter-v1](../tests/dylib-fixtures/greeter-v1/src/lib.rs)：

```rust
rutis_sdk::export_plugin! { id: "greeter", factory: Factory }
```

宏生成 Rust ABI 工厂入口、运行期元数据和加载前可解析的引导节（ELF 为 `.note.rutis.meta`，Mach-O 为 `__DATA,__rutis_meta`，PE 为 `.rutism`）。插件不得定义自己的全局分配器，必须使用 `panic = "unwind"`。SDK 的分配器只能是 `System`（上游 rust-lang/rust#100781、#114518）。

插件可以动态链接系统里的原生库（例如 `libz.so.1`），但只能依赖宿主那一份 SDK 和 libstd，不能依赖其他 Rust dylib，也不能带 `RUNPATH`/`RPATH`。宿主和 SDK 的 `$ORIGIN` 由各自的 `build.rs` 设置，不要写进 `RUSTFLAGS`，否则插件也会带上。跨插件或宿主交换的自定义服务与事件类型必须由 SDK 中的接口 crate 提供；插件私有类型只留在插件内部。后台任务应观察 `ctx.cancelled()`；旧代 `Ctx` 的注册在换代后会被拒绝。

把 SDK 发布包中的 `sdk.toml` 和 `librutis_sdk.so` 提供给打包命令：

```sh
cargo xtask pack-plugin \
  --manifest-path tests/dylib-fixtures/greeter-v1/Cargo.toml \
  --sdk-manifest target/dylib-bundles/<hash>/sdk.toml \
  --sdk-file target/dylib-bundles/<hash>/librutis_sdk.so \
  --features export \
  --output /tmp/greeter-v1
```

发布清单会指定构建锚点 `rutis-cli/dylib-plugins`。打包器使用插件工作区的 `Cargo.lock` 设置 `RUTIS_SDK_LOCKFILE`，在同一 Cargo feature 图中分两次编译宿主与插件，先得出 SDK 产物哈希，再把它编入插件；宿主的临时构建结果无需重新发布。打包器复核 SDK 文件、插件引导节、插件的动态依赖、依赖重复版本和插件自定义分配器，再从实际二进制生成 `plugin.toml`；其中 `native_deps` 列出插件链接的原生库，加载器在 `dlopen` 前核对二进制与它一致。如插件位于另一个 Cargo 工作区，需在 SDK 发布流水线中用同一 feature 图构建，并用 `--prebuilt-library` 打包；单独构建产生不同 SDK 哈希时会被拒绝。

`rutis-cli` 的 dylib 变体可用 `--plugin <目录> --plugin-config '<JSON>'` 装载一个插件。需要代码换代的宿主调用 `Loader::load`、`Loader::spawn` 和 `Loader::swap`；`swap` 复用 rutis 的 `FiberView::update`，消费者会随服务卸载和重新提供而重载。`DylibConfig::new` 的模块身份、名称和依赖声明检查位于工厂内部，直接调用 `view.update` 也不能绕过。加载器每个插件 id 默认最多保留四个已映射版本；旧版本永不 `dlclose`，达到上限后须重启回收。

## 外部构建：SDK 构建包

面向插件作者的完整指南（工作区搭建、代码示例、排错）见
[外部插件开发指南](external-plugin-guide.md)；本节概要工具链视角。上面的流程需要宿主源码（锚点包进入同一次 Cargo 构建）。没有宿主源码的外部开发者用 **sdk-bundle**：发布流水线在构建运行发布目录后运行

```sh
cargo xtask pack-sdk-bundle --bundle-dir target/dylib-bundles/<hash> --output <sdk-bundle>
```

它复制预编译 SDK（Windows 另带 `rutis_sdk.dll.lib` 导入库）、按 `sdk.toml` 的 `packages` 清单收集依赖闭包的编译期产物（**含 proc-macro 的 `.so`**——rustc 递归加载闭包 crate 的完整 rmeta 依赖链），再用一个探针插件逐个变体试删收缩到最小集（Linux 实测 41 个产物、约 60 MB），最后写 `bundle.toml` 文件级哈希清单。`sdk.toml`、`Cargo.lock`、`rust-toolchain.toml`、`cargo-config.toml` 模板与 `GUIDE.md` 一并放入。

插件侧：`Cargo.toml` 不声明 `rutis-sdk`（也不能直接依赖 `rutis`、`tokio`、`tokio-util`、`serde_json`——共享 crate 只能经 `rutis_sdk::` 的再导出使用），共享类型由注入的 `--extern` 解析。打包命令：

```sh
cargo xtask pack-plugin --bundle <sdk-bundle> \
  --manifest-path <plugin>/Cargo.toml --features export --output <dist>
```

打包器先核对 `bundle.toml` 全部文件哈希、`rustc` 完整版本与 `sdk.toml` 一致（版本不符时 rustc 会在元数据加载期报 E0514，早于任何检查）、以及插件 manifest 的直接依赖黑名单（声明共享 crate 在构建前被拒绝，而不是依赖 rustc 的错误码——注入的 `--extern` 出现在 RUSTFLAGS 时，SDK 的 build.rs 白名单会先报"未归类的 RUSTFLAGS 参数"）。环境里已设置 `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` 时打包器直接失败：注入就是通过它们传递的，静默覆盖调用者的 flags 不安全。构建成功后照常执行分配器、weak 导出、`native_deps`、引导节 L1/L2 与 `sdk.toml` 的交叉核对，并按平台核验链接产物（Linux `DT_NEEDED`、macOS `@rpath` 且无 run path、Windows 导入表）。

开发体验：把构建包的 `cargo-config.toml` 放进插件工作区 `.cargo/config.toml` 并改好路径后，`cargo check`/`cargo build` 可用；rust-analyzer 的补全可能不解析 `rutis_sdk::` 路径（注入的 crate 不在 cargo 的 crate graph 里），flycheck 仍走 `cargo check`。SDK 升级后旧插件因 L1/L2 不符被宿主拒绝，须用新构建包重编。Linux 与 macOS 的完整流程由 `tools/test-sdk-bundle.sh` 覆盖（E1/E3/E6/E9），Windows 走 `dylib-windows` 工作流的相同脚本。

## 验证与边界

本仓库的 Linux 验证命令：

```sh
bash tools/test-dylib.sh
bash tools/test-dylib-launcher.sh
bash tools/test-dylib-repro.sh
bash tools/test-sdk-bundle.sh
cargo test --workspace
```

测试覆盖插件内 `tokio::spawn`、跨库 `Snapshot` 类型和 `String` 服务读取与 downcast、v1→v2 换代后的消费者重载与旧插件析构、错误 L1/L2 在 `dlopen` 前拒绝且 ELF 初始化函数未运行、模块身份变更经 `swap` 与直接 `update` 均被拒绝、版本保留上限、损坏缓存的原子修复与失败入口的同版本重试、宿主/SDK/libstd 文件改动时启动器拒绝、环境库路径覆盖下从自身目录启动、不同源码与 target 路径的 SDK 字节一致，以及旧代迟到注册在两种 tokio 运行时及 Failed 状态下被拒绝。CI 另在两个独立 runner 上构建 SDK 并比较产物哈希。

直接执行内部宿主没有启动前校验，不能作为 dylib 发布入口。发布目录与插件缓存必须由可信部署控制，运行期间不得原地改写文件。

## macOS 的差别

- **库搜索路径**：SDK 的 install name 是 `@rpath/librutis_sdk.dylib`，宿主和 SDK 的 run path 是 `@loader_path`，都由各自的 `build.rs` 在链接时设定，构建后不做任何修改。插件不带 run path。
- **启动器**：它不设置任何 `DYLD_*`，而是把调用者的 `DYLD_*` 改名为 `RUTIS_ORIG_DYLD_*`，再启动宿主；宿主启动后恢复原值，宿主启动的子进程看到的环境与调用者一致。有人用 `DYLD_INSERT_LIBRARIES` 把代码注入启动器本身时，启动器无法阻止，这不在它的防护范围内；需要防这一点的发布方可以给启动器加 hardened runtime 签名。
- **quarantine**：插件源文件或缓存中的文件带 `com.apple.quarantine` 属性时，加载器在 `dlopen` 前拒绝，错误信息里给出 `xattr -d com.apple.quarantine <文件>`。加载器不自动清除这个属性。
- **签名**：宿主是否开 hardened runtime 由发布方决定。开了以后，SDK、libstd 和插件必须与宿主用同一个 Team ID 签名，或者宿主带 `com.apple.security.cs.disable-library-validation`。宿主可以调用 `Loader::require_team_ids` 只接受指定 Team ID 签名的插件，这时只有 ad-hoc 签名的插件会被拒绝。
- **插件的原生库依赖**必须写成绝对路径（例如 `/usr/lib/libSystem.B.dylib`），不能用 `@rpath`、`@loader_path`、`@executable_path` 或相对路径。插件还必须使用两级命名空间，不能用 `-undefined dynamic_lookup`，导出符号里也不能有 Rust 符号的 weak 定义。
- **分配器**：SDK 的分配器只能是 `System`。macOS 上 libstd 内部的分配不经过 SDK 的分配器。
- **构建**：脚本把 `MACOSX_DEPLOYMENT_TARGET` 固定为 13.0。Xcode 版本不同时 SDK 字节也不同，发布流水线应固定 Xcode。

## Windows 的差别

- **文件名**：没有 `lib` 前缀。发布目录里是 `rutis-cli.exe`（启动器）、`rutis-cli-host.exe`、`rutis_sdk.dll` 和 `std-<hash>.dll`（取自工具链的 `bin\`）；插件是 `<id>.dll`。导入库 `rutis_sdk.dll.lib` 不进发布目录。
- **VC++ 运行库**：SDK、宿主和插件依赖 `VCRUNTIME140.dll` 和 UCRT（`api-ms-win-crt-*`，Windows 10 起是系统组件）。VC++ 运行库由用户自行安装，rutis 不随包分发，启动器也不检查。std DLL 本身不依赖它。
- **启动器**：Windows 没有 `exec`，启动器先校验三个运行文件的存在和哈希，再把宿主作为子进程启动并等待，转发它的退出码。校验时以只允许读取的共享模式打开这三个文件，并一直持有到宿主退出，期间它们不能被写入、删除或改名。启动器把自己放进一个设置了 `KILL_ON_JOB_CLOSE` 的 Job Object，启动器退出（包括被强制结束）时宿主和它启动的进程一起结束；需要留下进程的宿主可以用 `CREATE_BREAKAWAY_FROM_JOB` 启动它。启动器忽略 Ctrl+C，由同一控制台上的宿主自己处理。宿主的进程 ID 与启动器不同。
- **库搜索路径**：宿主对 SDK 和 std 的导入先在宿主所在目录查找，启动器已确认两者都在那里，所以工作目录和 `PATH` 中的同名文件不会被用到；启动器不修改任何环境变量。不要从缺文件的目录启动内部宿主：缺文件时 Windows 会继续按 `PATH` 查找。
- **插件加载**：加载器以完整路径调用 `LoadLibraryExW`，标志为 `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`，插件复用宿主已加载的 SDK 和 std，缓存目录不在搜索范围内。从核对缓存文件哈希到 `LoadLibraryExW` 返回，加载器以只读共享模式持有该文件。已加载的 DLL 可以被改名，但不会被覆盖。加载器从不 `FreeLibrary`。
- **插件的原生库依赖**只能写 DLL 文件名，不能带路径；导入表和延迟加载导入表都会检查。DLL 名不区分大小写，`plugin.toml` 的 `native_deps` 中记为小写。
- **静态初始化**：插件中的静态初始化（`.CRT$XCU`，例如 `ctor` crate）在 `LoadLibraryExW` 返回前、持有加载锁时运行。其中不得等待其他线程（`join`、阻塞地等 channel 或锁）：新线程要等加载锁释放后才能运行，等待会卡住。分配内存、加锁、访问 `thread_local!` 不受影响。`rutis_plugin_entry` 在 `LoadLibraryExW` 返回后才调用，不受此限。
- **导出数**：一个 DLL 最多导出 65535 个符号。release 构建的 `rutis_sdk.dll` 约 1600 个，`build-dylib-bundle.sh` 在超过 30000 时失败。dev 构建（opt-level 0）会导出泛型实例，当前约 14500 个，依赖大量增加时可能接近上限；需要时给 SDK 的全部依赖设 opt-level ≥ 2，例如 `[profile.dev.package."*"] opt-level = 2` 加上对 `rutis` 等工作区成员的单独设置。只给 `rutis-sdk` 一个包设置没有效果。
- **构建**：SDK 和宿主由各自的 `build.rs` 加链接参数 `/Brepro`，去掉链接器写入的时间戳；连同路径重映射，SDK 在不同目录构建时字节相同。
- **测试**：上面三个脚本在 Git Bash 中运行。Windows 上另外测试：缓存目录、工作目录和 `PATH` 中放同名 DLL 不被使用；宿主运行期间发布目录的三个文件不能修改、删除或改名；强制结束启动器后宿主随之结束；缺少任一运行文件时启动器拒绝。
- **CI**：Windows 的 dylib 测试约 30 分钟，放在单独的 `dylib-windows` workflow 里，只在 PR 改动 dylib 相关代码（SDK、`rutis-dylib*`、xtask、测试夹具和脚本）、推送发布 tag（`v*`、`rutis-v*`、`loader-v*`）或手动触发时运行。内核或依赖升级同样可能影响 Windows 插件，合并这类改动前可以手动运行一次。
