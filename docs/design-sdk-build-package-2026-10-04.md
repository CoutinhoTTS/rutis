# SDK 构建包：外部构建 dylib 插件不重建 SDK（2026-10-04）

对应 [#108](https://github.com/arcships/rutis/issues/108)。前提与背景见
[dylib SDK 设计](design-dylib-sdk-2026-09-24.md) §5.2/§5.3 和
[macOS/Windows 设计](design-dylib-macos-windows-2026-10-03.md) §3.8。

## 一、结论先说

1. **外部构建走预编译 SDK，不走"不同依赖图复现相同字节"。** SDK 构建包直接携带发布流水线
   编译好的 SDK 产物；插件构建时不重建 SDK，只链接发布的那一份。feature 图问题因此消失：
   SDK 字节根本不来自插件的构建。
2. **放弃跨图复现的原因**：Cargo 单次调用的 feature 统一跨全图，插件私有依赖只要碰到
   `tokio`、`serde_json` 等 SDK 树内的 crate，统一结果就变，SDK 字节随之改变（SDK 设计稿
   §5.3 的 2026-09-25 发现）。补救方案"SDK 饱和启用全部 feature"不可靠：互斥 feature、
   feature 引入新依赖导致闭包漂移、任何一次依赖升级都要重新审计全树。这条路不值得证明，
   因为预编译路径不需要它。
3. **一方流水线维持现状。** 锚点构建（`sdk.toml` 的 `[build] anchor_package/anchor_features`，
   宿主包纳入同一次 Cargo 构建）继续服务仓库内插件与发布流水线；`--prebuilt-library`
   （只打包不构建）不变。
4. **技术支点已经存在**：`rutis-sdk` 是 `crate-type = ["dylib"]` 的单形态 crate，rustc 本来
   就从 dylib 读元数据（含符号哈希）并让插件链接同一个文件。构建包模式只是把
   `--extern rutis_sdk=` 指向的文件从"cargo 刚重建的"换成"发布包里的"，没有第二份元数据，
   没有哈希匹配问题。

## 二、现状与缺口

| 现状 | 缺口 |
| --- | --- |
| `pack-plugin` 默认模式要求锚点包在同一 Cargo 图中，即需要宿主源码 | 外部开发者没有宿主源码，此路不通 |
| `--prebuilt-library` 只做校验，插件二进制须在别处（发布流水线）构建 | 外部开发者没有构建环境 |
| 发布目录（`build-dylib-bundle.sh`）含宿主、SDK、libstd、启动器、`sdk.toml` | 它是**运行**包，不是**构建**包：缺构建所需的导入库（Windows）、工具链固定、注入配置 |
| `sdk.toml` 有 `[sdk]` version/id/artifact_sha256/target/rustc/packages 与 `[build]` anchor | 缺 #108 要求的：锁文件本体、规范化 RUSTFLAGS、重映射规则、macOS deployment target；且 `[build]` 是 shell 脚本追加的，不是 `--sdk-info` 生成 |
| `check_shared_duplicates` 用 `cargo tree -d` 只查**同名不同版本** | 插件直接依赖 `tokio` 等共享 crate（同版本）时静态双份链接不被拦截；现状靠 L2 失败兜底，错误不可读 |

## 三、SDK 构建包（sdk-bundle）

每个 SDK 版本、每个平台一份，由发布流水线产出（`build-dylib-bundle.sh` 之后新增一步，
或 `cargo xtask pack-sdk-bundle`）。内容与运行发布目录分开，**不含宿主与启动器**：

| 文件 | 用途 |
| --- | --- |
| `librutis_sdk.so` / `librutis_sdk.dylib` / `rutis_sdk.dll` | 预编译 SDK 本体（与运行发布目录中同一份产物） |
| `rutis_sdk.dll.lib`（仅 Windows） | 链接用导入库。运行发布目录不收它（实现文档"Windows 的差别"），构建包必须收 |
| `sdk.toml` | L1（`id`）、L2（`artifact_sha256`）、version、target、rustc、packages，以及新增的 `[build]` 诊断字段（见下） |
| `Cargo.lock` | SDK 构建的锁文件。插件构建不消费它（SDK 不重建），用于诊断与复核 `packages` |
| `rust-toolchain.toml` | 固定 1.98.1。插件自身的 libstd 必须与运行发布目录中的动态 libstd 一致，工具链不同会在打包时被 std 引用检查拒绝 |
| `cargo-config.toml` | `.cargo/config.toml` 模板：`--extern rutis_sdk=<SDK 路径>` 与 `-L native=<目录>` 注入，使 `cargo check`/`cargo build`/rust-analyzer 在插件工作区直接可用 |
| `GUIDE.md` | 插件作者指南摘录：禁则（自定义分配器、`panic = "abort"`、直接依赖共享 crate、边界上用私有类型）、SDK 升级须重编 |

`sdk.toml` 改为由 `rutis-cli-host --sdk-info` 一次性生成（含 `[build]` 段），shell 脚本不再
追加。`[build]` 段新增诊断字段（不进 L1，口径同 macOS 设计稿 §3.6）：规范化后的 RUSTFLAGS
（L1 实际采用的白名单结果）、路径重映射规则、`MACOSX_DEPLOYMENT_TARGET`、Xcode/链接器版本
（从产物 `LC_BUILD_VERSION` 读）、Windows 的 `/Brepro` 标记。L2 不一致时这些字段用于给出原因。

构建包版本与 SDK 版本一致（`sdk.toml` 的 `version` + L2 唯一确定内容）。分发方式不定
（GitHub Release 附件或内部制品库均可），不属于本设计。

## 四、插件侧构建（`pack-plugin --bundle`）

插件 `Cargo.toml` **不声明** `rutis-sdk` 依赖，`use rutis_sdk::...` 由注入的 `--extern`
解析。声明了会怎样：cargo 从源码重建一份 SDK（dylib-only crate，任何工作区都能构建，但
L1 不含锁文件依赖树、字节与发布产物不同），rustc 收到两个 `rutis_sdk` 候选直接报 E0464。
打包器把这个错误提前为可读信息："外部构建请删除 rutis-sdk 依赖，使用构建包注入"。

新模式 `cargo xtask pack-plugin --bundle <dir>`（与默认锚点模式、`--prebuilt-library` 互斥）：

1. 读 `<dir>/sdk.toml`；校验包内 SDK 文件哈希等于 L2，`target` 与本机三元组一致。
2. 构建插件（两阶段不变，只是第一阶段换成校验）：RUSTFLAGS 注入
   `--extern rutis_sdk=<包内 SDK>` 与 `-L native=<dir>`；
   `RUTIS_SDK_ARTIFACT_SHA256` 设为 `sdk.toml` 的 L2；工具链用包内 `rust-toolchain.toml`。
3. 复用现有全部检查：自定义分配器、weak Rust 导出、`native_deps`、动态 libstd 引用、
   引导节 L1/L2 与 `sdk.toml` 交叉核对。
4. 新增检查（同时补进锚点模式）：
   - 插件图的**直接依赖**含 `rutis`、`tokio`、`tokio-util`、`serde_json`、`rutis-sdk`
     任一项 → 失败并指出包名与引入路径（间接传递不查：它们静态链接进插件、元数据哈希
     与 SDK 内副本不同、符号名不同，`RTLD_LOCAL` 下互不绑定——W2 已证同名 crate 双版本
     共存可行；类型跨界则编译期即错）。
   - 插件引用的动态 libstd 与构建包 `sdk.toml` 记录的 std 文件不一致 → 失败，提示用包内
     `rust-toolchain.toml` 重建。
5. 从引导节生成 `plugin.toml`（含 `native_deps`），输出插件目录。

开发体验：把构建包的 `cargo-config.toml` 放进插件工作区 `.cargo/config.toml`（路径改成本机
构建包位置）后，`cargo check`、`cargo build`、rust-analyzer 都直接工作；`pack-plugin` 不依赖
用户配置，注入由它自己完成。

## 五、身份为什么成立

- **L1**：`SDK_ID` 是编译期 const，内联自发布 dylib 的元数据，自动等于发布值；插件不可能
  编出别的 L1，除非链接了别的 SDK 文件——那会被第 1/3 步的哈希核对拦下。
- **L2**：由 `sdk.toml` 注入并写进引导节；打包器从实际二进制读回，与包内 SDK 文件哈希、
  `sdk.toml` 三方核对（现有逻辑不变）。
- **符号**：插件对 SDK 的全部引用（含泛型实例、vtable）使用发布 dylib 元数据中的哈希；
  运行期宿主加载的是同一文件，符号必然解析。
- **libstd**：插件与 SDK 共用运行发布目录中的动态 libstd；工具链固定保证插件产出的
  `libstd-<hash>` 引用与目录中文件一致，加载器现有检查兜底。
- **原有验收语义变化**：#108 验收第 2 条"私有依赖改变 SDK feature 时打包失败"在预编译模式
  下前提不存在（私有依赖影响不到 SDK）。对应的拒绝变为第 4 步的直接依赖检查与 L2 核对。

## 六、验证（E 系列）

判定：任何一项失败，把结论记回本稿；若 E1 失败则外部构建退回"仅发布流水线
`--prebuilt-library`"，#108 以文档化限制关闭。

| # | 内容 | 判定 |
| --- | --- | --- |
| E1 | Linux：独立工作区（无宿主源码、不在仓库内），用构建包 + `--bundle` 构建 greeter 等价插件；被发布宿主加载，v1→v2 换代、消费者重载 | 与 `test-dylib.sh` 同口径 |
| E2 | E1 的插件带私有依赖：一个 SDK 树外的 crate（如 `base64`）+ 一个与 SDK 树同名的间接依赖（如私有依赖引入 `serde`）；加载与换代正常，weak 导出检查通过 | 静态副本共存不串线 |
| E3 | 插件 `Cargo.toml` 直接依赖 `tokio`（或声明 `rutis-sdk`） | 打包失败，信息含包名与引入路径 |
| E4 | macOS：E1 等价（arm64，`@rpath` 依赖、quarantine 流程不变） | 同 E1 |
| E5 | Windows：E1 等价（导入库来自构建包；导入表只含 `rutis_sdk.dll`、std、允许的 `native_deps`） | 同 E1 |
| E6 | 篡改：构建包内 SDK 文件改一字节 → 第 1 步拒绝；用锚点模式自建的 SDK 冒充构建包 → L2 核对拒绝，信息可读 | 拒绝在 `dlopen` 前 |
| E7 | 工具链不符：用非 1.98.1 构建插件 | 打包被 std 引用检查拒绝，提示用包内 `rust-toolchain.toml` |
| E8 | 开发体验：构建包 `cargo-config.toml` 放入工作区后 `cargo check` 与 rust-analyzer 可用，无需运行打包器 | 编译与补全正常 |

CI：`dylib-linux`、`dylib-macos` 增加 bundle 模式作业（产出构建包 → 独立临时工作区构建
夹具插件 → `loader_host` 加载换代）；Windows 走 `dylib-windows` 的现有触发条件。

## 七、实施步骤

1. `--sdk-info` 收编 `[build]` 段并新增诊断字段；`build-dylib-bundle.sh` 停止追加。
2. 构建包产出（xtask `pack-sdk-bundle` 或脚本新步骤），三平台。
3. `pack-plugin --bundle`：注入构建、第 4 步新检查（同时补进锚点模式）、可读诊断
   （L1 按 version/rustc/target/packages 逐项 diff；L2 不一致时提示"链接了重建的 SDK"）。
4. E1–E3 通过后进 CI；E4–E8 随平台作业。
5. 文档：`dylib-sdk-implementation.md` 增"外部构建"一节；#108 验收第 2 条措辞按 §五 更新。

## 八、验收映射（#108）

| #108 验收 | 对应 |
| --- | --- |
| 无宿主源码的独立工作区，只用构建包构建带私有依赖的插件，L2 与发布 SDK 一致，被发布宿主加载并完成换代 | E1、E2；macOS 为 E4（#102 已完成，无阻塞） |
| 插件私有依赖影响 SDK 时打包失败并指出哪个依赖哪个 feature | 语义更新（§五）：直接依赖共享 crate 或 rutis-sdk → 失败并指出包名与引入路径，E3 |
| Linux 和 macOS 都通过 | E1、E4；Windows（E5）超出 #108 要求，一并做 |

#108 第 2 项"验证不同依赖图能否构建出相同 SDK 字节"从工作项中移除：一方流水线已有锚点
方案，外部构建由本设计取代该问题的前提。

## 九、明确不做

- 不证明任意 Cargo 图可复现 SDK 字节（§一第 2 条）。
- 不发布 `rutis-sdk` 到 crates.io：可安装的源码形态会诱导插件声明依赖并从源码重建，产物
  必与发布 SDK 不同。`rutis-sdk` 的源码在仓库中可读，构建包另附 `GUIDE.md`。
- 不改变信任边界：dylib 插件仍是同进程可信方案，签名与 Team ID 限制按 macOS 设计稿
  §3.8；不受控代码走协议插件。
- 不动运行发布目录的内容与启动器校验（构建包是新增产物，不替换它）。
