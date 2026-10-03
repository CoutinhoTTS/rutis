# 一方插件的 dylib SDK 设计(2026-09-24)

> 对应 [#45](https://github.com/arcships/rutis/issues/45),跟踪 [#52](https://github.com/arcships/rutis/issues/52)。
> 状态:设计稿 + Linux 实现与自动化验证,使用说明见 [dylib-sdk-implementation](dylib-sdk-implementation.md)。**不改变默认形态**:一方插件默认静态链接,
> 宿主默认单二进制;本设计只为“不重新发布宿主就换一方插件代码”的少数场景提供可选路径。

## 一、目标与非目标

**目标**

1. 一方(可信)插件可以单独编译、单独发布,由宿主在运行时加载,并通过 rutis 现有的生命周期换代码:
   旧代卸载 → 消费者驱逐 → 新代装配 → 消费者重载。
2. 插件与宿主共享任意 Rust 类型(服务、事件、`Ctx`、tokio),不做序列化。
3. 不兼容的插件在加载前被拒绝,并给出可读原因;不依赖“dlopen 成功”来判断兼容。

**非目标**

- 三方插件、不可信代码、崩溃隔离、强制停止:走[协议插件](design-protocol-plugins-2026-09-25.md)(#46–#48)。
- 卸载旧代码(dlclose):初版不做,见 §九。
- 状态迁移(Erlang `code_change`):不做,换代 = 干净的卸载 + 重装配(与 [research-hot-reload](research-hot-reload-2026-08-17.md) 一致)。
- 不同 rustc 版本之间的兼容(abi_stable 路线):不做;SDK 变动过频时再单独评估。
- 不中断替换:换代存在卸载到装载的空窗,见 #51。

## 二、与既有决策的关系

- **单二进制分发**([design-dual-core](design-dual-core-2026-08-20.md)):保持为默认。dylib 能力是宿主的**可选构建变体**,
  只有该变体需要独立启动器,并随附 `librutis_sdk.so` 和 `libstd-*.so`(§十)。
- **生命周期热重载与代码热更正交**([research-hot-reload](research-hot-reload-2026-08-17.md) §四.3):本设计即该文档预留的
  “正交组合点”——dylib 负责拿到新代码,rutis 负责安全换件。
- **已删除的 hotplug 实验**(`14f8c53`,cdylib + C ABI + JSON 字符串):有序列化开销却没有隔离,`dyn Plugin` 过不了边界。
  本设计走相反方向:Rust ABI + 共享 SDK dylib,边界上直接传 `Box<dyn PluginFactory>`。
- **TypeId 跨 dylib 风险**(research-hot-reload §四.4):SDK 单份 dylib 使共享类型只有一个定义,TypeId 一致(原型已验证),
  注册表键无需换稳定 id。前提是共享类型必须在 SDK 内(§四)。

## 三、总体结构

```
                 ┌──────────────────────── 进程 ────────────────────────┐
                 │  host(dylib 变体)                                     │
                 │     └─ rutis-dylib(加载器,宿主侧)                     │
                 │                                                        │
 DT_NEEDED ───▶  │  librutis_sdk.so  = rutis + tokio + 接口 crate + 分配器  │ ◀── 只加载一份
                 │  libstd-<hash>.so                                      │
                 │                                                        │
 dlopen ───────▶ │  libplugin_a.so (v3)   libplugin_a.so (v4)  libplugin_b.so │
 (RTLD_LOCAL)    │    └ 只依赖 SDK;私有依赖静态链入各自 .so                 │
                 └────────────────────────────────────────────────────────┘
```

| crate | 形态 | 内容 | 变动是否换 SDK 版本 |
| --- | --- | --- | --- |
| `rutis-sdk` | `crate-type = ["dylib"]` | 重导出 rutis、tokio、跨边界的接口 crate;`#[global_allocator]`;SDK 身份常量;`export_plugin!` 宏 | 是 |
| `rutis-dylib` | rlib,只被宿主依赖 | 清单解析、校验、内容寻址缓存、dlopen、`DylibFactory`、模块登记表 | 否(宿主侧) |
| 插件 crate | `crate-type = ["dylib"]` | 只直接依赖 `rutis-sdk` 与自身私有依赖 | — |

加载器不放进 SDK:它只在宿主使用,放进 SDK 会让加载器的每次修改都迫使所有插件重编。

## 四、SDK:兼容单位

### 4.1 放什么

只放**跨边界必须共享同一份**的东西:

1. rustc 版本(隐含)与 `std`(动态 `libstd-*.so`);
2. `rutis`(全局状态如 `InstanceId` 计数器、注册表类型);
3. `tokio`(运行时上下文是线程局部变量,必须单份,否则插件里 `tokio::spawn` 找不到宿主运行时);
4. 接口 crate:插件之间、插件与宿主之间交换的服务类型、事件类型;
5. 全局分配器(§4.3);
6. 插件配置的公共载体:`serde_json::Value`(§7.3)。

**不放**:插件私有依赖。它们静态链入各自的插件 .so,类型不得出现在边界上(不得作为服务键、事件、跨插件传递的值)。

### 4.2 共享依赖的规则

- 插件 crate 若直接依赖 SDK 已包含的 crate(如 `tokio = "1"`),只要与 SDK 解析到同一版本,rustc 会链接 SDK 中的那一份
  (原型的 host2 已验证:rutis/tokio 的符号全部来自 `librutis_sdk.so`)。
- 若解析出 semver 不兼容的第二个版本,就会出现第二份拷贝,其类型与 SDK 中的同名类型 TypeId 不同。
  CI 用 `cargo tree -d` 检查:SDK 包含的 crate 在插件依赖树中不得出现重复版本。
- 插件**私有**定义的服务类型只在该插件内可见。同一插件的两个版本各自定义的同名私有类型 TypeId 不同,因此
  换代码时对外提供的服务键必须是 SDK 接口类型,否则消费者在换代后找不到服务。

### 4.3 分配器(必须)

原型实测:宿主二进制里定义的 `#[global_allocator]` **不作用于**插件 dylib——插件分配 1 MiB 时宿主的计数分配器没有看到。
若宿主用 mimalloc 而插件用 System,插件分配、宿主释放就是未定义行为。

规则:**`#[global_allocator]` 只能在 SDK 中定义**;宿主和插件都禁止定义。原型实测放进 SDK 后,插件的分配经过它。
只能用 System:动态链接 std 时,libstd 内部的分配不经过 SDK 的分配器(macOS 两级命名空间、Windows),Linux 上配 jemalloc 自 rustc 1.71 起也会崩溃(上游 rust-lang/rust#100781、#114518,未修复)。上游修复前不允许换 mimalloc/jemalloc,详见 [design-dylib-macos-windows](design-dylib-macos-windows-2026-10-03.md) §3.5、§八 R1。`export_plugin!` 可加一个编译期检查辅助,CI 另用
`nm` 检查插件 .so 不导出 `__rust_alloc` 的自有实现。

### 4.4 panic 策略

SDK、宿主、插件必须全部 `panic = "unwind"`(`abort` 与 `unwind` 混用无法链接或行为未定义)。panic 策略计入 SDK 身份。
原型实测:插件内 panic 可被宿主 `catch_unwind` 捕获;rutis 已在 apply/清理/监听器边界捕获 panic(#7),入口函数另由
`export_plugin!` 生成的包装捕获。

## 五、身份与兼容性校验

### 5.1 为什么不能依赖 dlopen

原型的两个反例:

| 实验 | 结果 |
| --- | --- |
| (a) SDK 内容变化但 crate 元数据哈希不变,插件按新 SDK 编译,加载进旧 SDK 的宿主 | `dlopen` **成功**;只有显式身份比对拦下 |
| (b) SDK 的 tokio features 不同(`+net`),插件按它编译,加载进原 SDK 的宿主 | `dlopen` **成功**,插件正常运行——这次侥幸,不能作为兼容证明 |

Rust 符号修饰中的 crate 哈希只能拦下一部分不匹配,无法覆盖“同名同哈希但布局已变”的情况。因此需要两层显式校验。

### 5.2 L1:声明身份(编译期常量 vs 运行期值)

SDK 的 `build.rs` 计算 `SDK_ID`:

```
sha256(
  sdk 版本号, rustc -vV 全文, 目标三元组,
  发布构建的 Cargo.lock 中 SDK 子依赖树(包名+版本+来源+checksum),
  启用的 features, profile 中影响 ABI 的项(panic、debug-assertions、overflow-checks;opt-level、debuginfo 不计),
  规范化后的 RUSTFLAGS(见下)
)
```

- `pub const SDK_ID: &str` 被插件和宿主在编译期**内联**进各自的产物;
- `#[inline(never)] pub fn loaded_sdk_id() -> &'static str` 在运行期返回**实际加载的** SDK 中的值。

两者不等即拒绝(宿主一侧见 §5.4,插件一侧见 §7.1)。原型实验 (a) 使用人工指定的身份被这一层拦下,
不代表以上输入能识别任意源码变化:同版本的 SDK 或 path 依赖源码改变、其他输入不变时,L1 可以相同。
L1 用于配置诊断;产物是否一致必须由 L2 确认,插件也必须内嵌 L2,不能只信任外部清单。
发布脚本以 `RUTIS_SDK_LOCKFILE` 显式指定实际解析的锁文件;Cargo 不向依赖 build script 提供调用方工作区的锁文件路径。
未指定时 SDK 仍可编译(例如已发布 crate 的普通下游构建),但 L1 不包含锁文件依赖树,不能用该产物冒充发布 SDK。

**输入必须与机器无关。** `SDK_ID` 会被编译进 SDK,任何机器相关的输入都会同时改变 L1 和 L2 的结果,破坏可复现构建。
例如两台机器分别用 `--remap-path-prefix=/build/a=/target` 和 `--remap-path-prefix=/build/b=/target`,原始 RUSTFLAGS
不同,`SDK_ID` 就不同;路径重映射只改写输出中的源路径,改不了已经算出的哈希。因此 RUSTFLAGS 按**白名单**规范化:

- 读取 `CARGO_ENCODED_RUSTFLAGS`,只保留影响代码生成或类型布局的项:`-C target-cpu`、`-C target-feature`、
  `-C panic`、`-C debug-assertions`、`-C overflow-checks`、`--cfg`、`-Z` 系列;排序后参与哈希。
- 明确忽略:`--remap-path-prefix`、`-L`、`-l`、`-C link-arg(s)`、`-C linker`、`-C debuginfo`、`-C opt-level`、
  `-C incremental`、`-C codegen-units` 等只影响路径、链接或优化的项,以及 `-D warnings`、`--cap-lints` 等 lint 控制项。
- 遇到不在两张表中的项,`build.rs` 直接报错,要求先归类,避免新参数悄悄进入或漏出身份。
- `build.rs` 为以上输入声明 `rerun-if-env-changed`。

### 5.3 L2:产物身份(二进制哈希)

评估文档要求“版本号必须对应一组不可变、经过验证的构建产物”。L2 落实这一点:

- 每个发布的 SDK 版本对应一个 `librutis_sdk.so` 的 sha256(`sdk_artifact`),记录在 SDK 发布清单中;
- **宿主**构建时把它所链接的 SDK 产物哈希编译进自己(§5.4);
- **插件**同样采用两阶段构建:先构建 SDK 并计算哈希,再通过 `RUTIS_SDK_ARTIFACT_SHA256` 编译插件,
  将哈希内嵌到引导 blob 和运行期 `PluginMeta`;该值属于插件,不回写 SDK,避免自引用哈希。
  插件构建完成后复核实际链接的 SDK 产物未变,打包工具从插件引导 blob 读取该值写入清单,
  并与 SDK 发布清单和实际 SDK 文件哈希交叉核对,任何不一致都失败;
- 独立启动器在执行宿主之前校验整套运行产物(§5.4);宿主启动后通过 `dladdr` 等反查实际加载的 SDK 文件,
  再与自身内嵌哈希核对。加载插件时,清单、插件内嵌 L2 和宿主已确认的值必须三者相同。

L2 要求**可复现构建**:插件在自己的 cargo 调用中会从源码重建 SDK,必须得到逐字节相同的产物。原型实测:

| 条件 | 两个 target 目录的 release SDK 哈希 |
| --- | --- |
| 不加路径重映射 | 不同 |
| `--remap-path-prefix=<target>=/target --remap-path-prefix=$HOME=/home` | **相同** |

这次原型的 `SDK_ID` 用的是固定的环境变量,没有包含 §5.2 的真实生成逻辑。跨机器可复现(不同源码路径、
CARGO_HOME、target 目录、用户名)尚未验证,见 §十一 V1;V1 必须用最终的 `build.rs` 生成 `SDK_ID`,并让各机器的
重映射参数不同,以确认规范化有效。路径重映射参数统一写进 `.cargo/config.toml` 与 CI,但**不计入** L1。

**L2 不可复现时的退路**:一方插件与 SDK 在同一 CI 流水线中产出,先构建 SDK,再在同一 target 目录中
分阶段构建宿主和插件、内嵌同一 SDK 哈希,最终复核 SDK 未重建变化;插件按 SDK 版本批量发布。
这正是评估文档中“SDK 升级时自动重编全部一方插件”的做法;代价是“单独发布”退化为“按 SDK 批次发布”。

2026-09-25 Linux 实现发现另一项 Cargo 约束:宿主和独立插件的**传递依赖 feature 图**不同会改变 SDK 二进制,
即使版本和源码相同且路径重映射一致。当前 `sdk.toml` 记录宿主构建锚点,插件打包时把宿主包纳入同一次 Cargo 构建,
以取得相同的 feature 图;宿主二进制无需重新发布。脱离该图独立构建的插件若 L2 不同会被拒绝。
这是上述“同一流水线”退路的实现,尚未证明任意独立 Cargo 图可产出相同 SDK。

### 5.4 宿主绑定与启动检查顺序

只比较“运行时 SDK”和“插件构建时 SDK”是不够的:宿主自己也按某个 SDK 的布局编译。部署时若误换了同名、符号可解析
但布局不同的 SDK,配套的新插件会通过校验,宿主却仍按旧布局访问共享类型,产生未定义行为。原型复现了这一场景:

| 场景 | 结果 |
| --- | --- |
| 宿主按 SDK A 编译;部署了 SDK B 与按 B 编译的插件;宿主不做自检 | 启动、加载、换代码、shutdown **全部通过** |
| 同上,宿主启动时比对自身内联的 `SDK_ID` 与 `loaded_sdk_id()` | 启动即拒绝 |

原型第二行只验证了进程内自检能发现不匹配,没有证明自检前不会执行 SDK 代码。SDK 是宿主的
`DT_NEEDED` 依赖,还提供全局分配器;Rust 运行时可能在进入 `main` 前调用它。
2026-09-25 审查的 Linux/rustc 1.98.1 最小复现实测:首次 C ABI 引导查询前,SDK 分配器已被调用 2 次。
因此把首次显式查询改为 C ABI 仍不能建立“校验前无 Rust ABI 跨界调用”的边界。

**dylib 变体必须通过独立启动器启动。** 启动器不链接 `rutis-sdk`,其 Rust 依赖(包括 std)静态链接,
校验阶段仅做文件 I/O 和哈希,不加载待验证的宿主或 SDK。构建绑定顺序如下:

1. 构建 SDK,计算 L2 哈希。
2. 以 `RUTIS_SDK_ARTIFACT_SHA256` 构建宿主,由 `env!` 内嵌哈希,同时内联 L1 `SDK_ID`。
   复用第一阶段 SDK 产物,构建结束后复核哈希未变。
3. 最后构建独立启动器,将宿主、SDK、动态 libstd 的文件名和预期哈希编译进启动器。
   不从可被误配的外部清单获取预期值。启动器变动不影响 SDK 身份或插件兼容性。

**启动检查顺序**:

1. 启动器校验同一发布目录中的宿主、SDK 和 libstd,任一不匹配即退出,不执行宿主。
2. 校验通过才执行宿主。发布目录必须可信且在校验到进程运行结束期间保持不可变;更新使用新的版本目录,
   禁止原地覆盖。启动器固定宿主绝对路径并控制动态库搜索环境,执行宿主时暂时清除 `LD_LIBRARY_PATH`、`LD_PRELOAD`、
   `LD_AUDIT` 等覆盖项;宿主在启动运行时线程前恢复调用者原有的 `LD_*`,避免子进程继承发布目录搜索路径。
   打包检查确保宿主和 SDK 的动态依赖实际解析到已验证的 SDK/libstd,
   不落到工作目录或其他安装版本。平台对应的加载路径约束必须分别验证(§十一)。
3. 宿主在创建 root 和加载插件前,经 C ABI `rutis_sdk_boot_id(buf, cap)` 核对 L1,
   并从实际加载模块反查 SDK 文件核对 L2。这是启动后的交叉检查,不是启动期 ABI 安全边界。
4. 每个插件的清单、引导 blob 和运行期元数据均须携带相同的 L1/L2,并等于宿主已确认的值;
   在 `dlopen` 前核对 blob,加载后交叉核对运行期元数据(§7.1)。

直接运行内部宿主二进制不具备第 1 步的保证,不属于支持的启动入口;进程内检查无法事后补救启动期的错误调用。
这一边界处理可信部署中的产物误配,不承诺抵御能在校验后改写安装文件或控制进程加载器的攻击者。
Linux 独立启动器与加载路径检查已实现并进入自动化测试;跨平台仍须分别验证。

## 六、插件产物与清单

每个插件发布为一个目录(或归档):

```
greeter-4.2.0-x86_64-unknown-linux-gnu/
  plugin.toml
  libgreeter.so
```

```toml
[plugin]
id = "greeter"                 # 稳定标识,用于版本保留与诊断
version = "4.2.0"
library = "libgreeter.so"
library_sha256 = "…"

[sdk]
version = "0.3.0"              # 人读
id = "…"                       # L1,与 .so 内联常量相同
artifact_sha256 = "…"          # L2

[interfaces]                   # 所需接口版本(接口 crate 名 → semver 要求),只用于给出可读的拒绝原因
"rutis-iface-llm" = "^1.3"

[build]
target = "x86_64-unknown-linux-gnu"
rustc = "rustc 1.98.1 (48a229cea 2026-09-01)"
lock_sha256 = "…"             # 插件 Cargo.lock 全文
```

清单与 .so 内嵌元数据由 `export_plugin!` 和打包工具共同保证一致;加载时双重核对:加载前静态解析引导元数据
(§7.1 第 2 步),加载后再调 `rutis_plugin_meta()` 交叉核对(§7.1 第 5 步)。

## 七、加载流程

### 7.1 三阶段加载:校验 → 加载 → 构造(全部在 `PluginFactory::build` 之外)

`PluginFactory::build` 在 `update` 的 dry-run 与实际装载时各调用一次,约定为纯构造(`crates/rutis/src/plugin.rs:32`),
不能在其中加载库。加载是宿主的显式操作:`unsafe fn Loader::load(dir) -> Result<Arc<Module>, LoadError>`。

1. **读清单并校验**(不触碰 .so 的代码):目标三元组;`sdk.id`、`sdk.artifact_sha256` 等于宿主在 §5.4 启动检查中确认过的值;接口版本;同 id 已保留版本数(§九)。
2. **静态解析 .so 引导元数据**(只读文件 I/O,不加载、不执行任何代码):按 ELF 节名定位 `export_plugin!` 嵌入的引导 blob
   (§7.2),核对其中 `SDK_ID` 和 `SDK_ARTIFACT_SHA256` 均等于清单及宿主的值、插件 id 等于清单值;
   定位不到或核对失败即拒绝。清单是打包工具写的
   **声明**,这一步核对的是**二进制自身携带的值**——`dlopen` 在返回前就会执行 ELF 初始化代码
   (`.init`/`.init_array`),没有这一步,一个 `library_sha256` 与文件一致、但 `sdk.id` 错标为当前 SDK 的包,
   会在运行期核对(第 5 步)之前执行初始化代码,违背 §一 目标 3 的“加载前拒绝”。
3. **复制到内容寻址缓存**:`<cache>/<library_sha256>/lib<name>.so`,复制后重新计算哈希并比对。所有平台都这样做:
   Windows 避免文件锁,Linux 避免原地覆盖已映射的 .so 导致 SIGBUS;同一哈希已有有效缓存时复用,损坏时经临时文件原子替换。
4. **加载库**:`dlopen(path, RTLD_NOW | RTLD_LOCAL)`。`RTLD_NOW` 让未解析符号在此刻失败而不是在调用时崩溃;
   `RTLD_LOCAL` 使同一插件的多个版本可以共存(原型实测 v1/v2 同名 crate 同时加载,互不串线)。此时第 2 步已
   确认二进制自述身份兼容,初始化代码的执行不再违背“加载前拒绝”。
5. **核对运行期元数据**:调用 `rutis_plugin_meta()`,比对 `sdk_id`(L1)、`sdk_artifact_sha256`(L2)、插件 id 与版本和
   清单及引导 blob 中的对应字段一致,SDK 的 L1/L2 还须等于宿主已确认的值——加载后的交叉核对。
6. **构造工厂**:调用 `rutis_plugin_entry()` 得到 `Box<dyn PluginFactory<ConfigValue>>`,并在此时读取一次 `name()`、`injects()`,连同插件 id 一起记入 `Module`(供 §8.2 的不变量检查使用,之后不再回调)。

任一步失败返回 `LoadError`,带插件 id、版本、失败步骤和原因;第 4 步之后失败的库不卸载(§九),计入诊断。

### 7.2 插件侧导出与引导接口

```rust
rutis_sdk::export_plugin! {
    id: "greeter",
    factory: GreeterFactory,        // impl PluginFactory<ConfigValue>
}
```

宏展开为:

- `#[no_mangle] pub fn rutis_plugin_meta() -> rutis_sdk::PluginMeta`(Rust ABI;包含 `SDK_ID`、插件内嵌的 `SDK_ARTIFACT_SHA256`、id、`CARGO_PKG_VERSION`);
- `#[no_mangle] pub fn rutis_plugin_entry() -> Result<Box<dyn PluginFactory<ConfigValue>>, CordisError>`,内部用 `catch_unwind` 包住构造并把 panic 转为错误;
- 编译期断言:`panic = "unwind"`;在插件调用点通过 `env!` 读取 `RUTIS_SDK_ARTIFACT_SHA256`,
  缺失或不是合法 SHA-256 时构建失败;引导 blob 和 `PluginMeta` 使用同一插件常量;
- 一段**引导 blob**(纯数据,无初始化代码),供宿主在 `dlopen` 之前静态解析(§7.1 第 2 步):
  `#[used] #[link_section = ".note.rutis.meta"] static RUTIS_BOOT_META: [u8; N]`,内容为带格式版本的 magic、
  `SDK_ID`、插件内嵌的 `SDK_ARTIFACT_SHA256`、插件 id 与版本,各带长度前缀,余下填零。初版 Linux 加载器按 ELF 节名定位;
  Rust 产物的 `.rustc` 元数据会复制 magic 和 blob,全文件字节搜索无法唯一定位。macOS/Windows 需分别实现并验证节定位;定位不到按不兼容拒绝。

SDK 侧另导出一个 **C ABI 引导函数**,供宿主启动后交叉检查 L1(§5.4 启动顺序第 3 步):

- `#[no_mangle] pub extern "C" fn rutis_sdk_boot_id(buf: *mut u8, cap: usize) -> usize`:把 `SDK_ID` 的 UTF-8
  字节拷入 `buf`,返回长度;`cap` 不足时返回所需长度且不写入。函数体只有拷贝与长度比较,不触碰任何 SDK 类型。

其余导出(`rutis_plugin_meta()`、`rutis_plugin_entry()`、`loaded_sdk_id()`)都是 Rust ABI(不是 `extern "C"`):
宿主与插件保证同一 rustc、同一 SDK,传 trait object 是安全的——这正是 L1/L2 要保证的前提,也因此它们的首次
调用必须发生在对应校验通过之后。C ABI 查询不能阻止 Rust 启动期调用 SDK 分配器;
宿主的启动期边界由独立启动器保证,插件的边界由 `dlopen` 前的 L1/L2 blob 检查保证。

### 7.3 配置类型

`PluginFactory<C>` 的 `C` 必须是宿主与插件都认识的类型。统一用 `ConfigValue = serde_json::Value`:

- 宿主从配置文件读取后原样交给插件,插件在 `validate_config` 中反序列化并校验;
- 插件配置结构变化不影响 SDK,不需要换 SDK 版本。

需要强类型配置的插件族,可以把配置类型放进接口 crate(即进入 SDK),代价是配置变动即 SDK 变动。

## 八、接入 rutis 生命周期

### 8.1 换代码 = `update`

rutis 的 `FiberView::update(config)` 要求 config 类型不变、工厂不变(`crates/rutis/src/fiber.rs:1279`)。
让 config 携带模块,即可复用 update 的全部语义(dry-run、预取消、重启、消费者驱逐与重载):

```rust
pub struct DylibConfig { module: Arc<Module>, value: ConfigValue }   // 字段私有,经 DylibConfig::new 构造

/// spawn 时从首个模块取定,终身不变(对应 rutis 的静态依赖声明,D32f)。
struct DylibFactory { id: String, name: String, injects: Vec<TypeKey> }

impl DylibFactory {
    /// 模块必须与 spawn 时的插件 id、name、injects 完全一致。纯比较,无副作用。
    fn check_module(&self, m: &Module) -> Result<(), CordisError> { /* 不一致 → CordisError::Validation */ }
}

impl PluginFactory<DylibConfig> for DylibFactory {
    fn validate_config(&self, c: &DylibConfig) -> Result<(), CordisError> {
        self.check_module(&c.module)?;
        c.module.factory.validate_config(&c.value)
    }
    fn build(&self, c: &DylibConfig) -> Result<Box<dyn Plugin>, CordisError> {
        self.check_module(&c.module)?;       // 纯构造:库早已加载,检查也是纯比较
        c.module.factory.build(&c.value)
    }
    // name / injects 返回 spawn 时取定的值
}
```

宿主侧 API:

```rust
let view = loader.spawn(&ctx, &module_v3, config)?;     // = ctx.plugin_with(DylibFactory, DylibConfig::new(..))
loader.swap(&view, &module_v4, config).await?;          // 便捷封装:提前给出可读错误,再调用 view.update(..)
```

原型已验证整条链路:consumer 依赖插件提供的 `Greeting`,`update` 把 v1 换成 v2 后,consumer 被自动驱逐并以新服务重载
(输出 `["hello v1 …", "hello v2 …"]`),插件内 `tokio::spawn` 正常。

### 8.2 不变量放在工厂里,而不是 `swap` 里

rutis 的依赖声明是静态的(D32f):`injects` 在 spawn 时注册一次,`update` 不会改变依赖注册表。如果换上去的模块声明了
不同的依赖,它会在旧的依赖门控下运行。

`FiberView::update` 是公开 API,调用方可以绕过 `loader.swap` 直接 `view.update(DylibConfig::new(other, ..))`。
因此插件 id、`name()`、`injects()` 的一致性检查必须放在 `DylibFactory` 内部,而不是只放在 `swap` 里:

- `update` 的 dry-run 依次调用 `validate_config` 和 `build`(`crates/rutis/src/fiber.rs:1299-1300`),两处都会拒绝;
- rutis 首次装载不调用 `validate_config`,但每次装载都调用 `build`,所以 `build` 中的检查覆盖所有路径;
- 检查所需的 id、name、injects 在加载时已记入 `Module`(§7.1 第 6 步),比较是纯操作,符合 `build` 的纯构造约定。

不一致时返回 `CordisError::Validation`,提示“依赖声明或插件身份变化需要 dispose 后重新 spawn”。`swap` 只是提前给出
同样的错误。

### 8.3 旧代残留

换代后旧版本的代码仍在内存中,旧代的后台任务若不观察取消信号,会继续运行、继续访问能力。这与 #12/#41 的问题相同,
dylib 场景只会更明显(旧代码甚至来自另一个版本)。**#41(按装载代号拒绝迟到注册)是本设计上线的前置条件。**

## 九、版本保留:初版不卸载

- `Module` 持有库句柄,**永不调用 dlclose**,即使最后一个 `Arc<Module>` 被释放。理由:外部持有的 `Arc`、trait object
  的 vtable、`Drop` 实现、已 spawn 的任务、线程局部析构器都可能仍引用旧库的代码,无法证明已全部释放。
- 登记表记录每个插件 id 已加载的版本、各自仍被多少 fiber 使用(`Arc::strong_count`)、加载时间与映射大小,进入诊断。
- **保留上限**:每个插件 id 最多保留 N 个版本(默认 4,可配置)。超过时 `load` 返回 `LoadError::RetentionExceeded`,
  提示需要重启进程回收。上限计的是已加载过的版本,不因旧版本不再使用而减少。
- 将来若要支持卸载,需要:`ctx.spawn` 登记插件的所有任务、证明任务已结束、外部引用归零,并在各平台验证 dlclose 语义。另行立项。

## 十、构建与发布

### 10.1 宿主的两个构建变体

| 变体 | 链接 | 产物 | 能否加载插件 |
| --- | --- | --- | --- |
| 默认 | 全部静态 | 单二进制 | 否 |
| `--features dylib-plugins` | 内部宿主的 rutis/tokio/std 动态 | 独立启动器 + 内部宿主二进制 + `librutis_sdk.so` + `libstd-<hash>.so` | 是 |

采用 Bevy `dynamic_linking` 的写法:宿主依赖 `rutis` 不变,另加可选依赖 `rutis-sdk`,启用 feature 时 `use rutis_sdk as _;`。
原型 host2 实测:同一份源码,默认构建不依赖任何 Rust 动态库;启用 feature 后 rutis 符号全部解析到 `librutis_sdk.so`。
宿主代码中的 `use rutis::…` 无需改动。

发布布局:四个文件放在同一不可变版本目录,公开入口是独立启动器。Linux 下内部宿主及 SDK 配置
`-C link-args=-Wl,-rpath,$ORIGIN`,并验证直接、间接动态依赖均解析到该目录的 SDK/libstd(§5.4)。
`libstd-<hash>.so` 取自对应工具链的 `<sysroot>/lib/rustlib/<target>/lib/`。

### 10.2 CI 与发布

1. 仓库固定 `rust-toolchain.toml`;构建脚本和 CI 按各自源码、target 与 Cargo home 路径设置重映射参数。
2. SDK 发布:先构建 `rutis-sdk` 并计算产物哈希,再以 `RUTIS_SDK_ARTIFACT_SHA256` 构建宿主 dylib 变体(§5.4 两阶段构建),
   复核 SDK 文件哈希未变;随后绑定整套产物哈希构建独立启动器。产出 SDK 发布清单(`id`、`artifact_sha256`、包含的 crate 与版本)。
3. 插件发布:检出 SDK 对应的 tag,与 SDK 共用锁文件执行 §5.3 两阶段构建,将 SDK 产物哈希内嵌进插件。
   第二阶段须复用第一阶段的 SDK;打包时核对实际 SDK 哈希、插件 blob 中的 L2 与 SDK 发布清单一致,
   再从 blob 生成插件清单,任一不一致即失败。CI 必须测试误配外部清单不能覆盖插件内嵌 L2。
4. SDK 升级时,CI 以新 SDK 重编全部一方插件并批量发布。SDK 升级节奏放慢(例如按季度);只加不改的变化也会产生新 SDK 版本。
5. 接口 crate 启用 `cargo-semver-checks`(#44)。

## 十一、原型验证记录

原型位于会话临时目录,未提交;环境为 Linux x86_64、rustc 1.98.1、rutis 0.3.0(`7d7402d`)。

本仓库现有的 Linux 实现另由 `tools/test-dylib.sh`、`tools/test-dylib-launcher.sh`、
`tools/test-dylib-repro.sh` 验证:发布包启动、两个插件版本换代和消费者重载、L1/L2 错误在 ELF 初始化前拒绝、
宿主/SDK/libstd 任一哈希错误在执行宿主前拒绝、两套不同源码与 target 路径下 SDK 字节一致,以及未归类的 RUSTFLAGS 被拒绝。
CI 进一步使用两个独立 runner 对比 SDK 哈希;其结果以对应 PR 的 CI 为准。

| 验证项 | 结果 |
| --- | --- |
| 宿主依赖 SDK dylib 后自动动态链接 `libstd` | 是,需要随附 `libstd-<hash>.so` |
| 宿主、SDK、插件三方看到的共享类型 TypeId | 一致 |
| 插件内 `tokio::spawn` 使用宿主运行时 | 正常 |
| rutis 依赖门控、`update` 换代码、消费者自动重载 | 正常 |
| 插件分配、宿主释放;插件类型的 Drop 在宿主调用 | 正常(Drop 执行 1 次) |
| 宿主 `#[global_allocator]` 覆盖插件 | **否**;放在 SDK 中则覆盖 |
| 插件 panic 被宿主 `catch_unwind` 捕获 | 是 |
| 同一插件 crate 的两个版本同时加载(`RTLD_LOCAL`) | 正常,互不串线 |
| SDK 内容变化、元数据哈希不变 | `dlopen` 成功,仅 L1 拦下 |
| SDK 依赖 features 变化 | `dlopen` 成功,运行未出错(不代表兼容) |
| SDK 可复现构建(同机不同 target 目录) | 加路径重映射后逐字节一致 |
| 宿主可选动态链接(Bevy 写法) | 可行 |
| 旧宿主(SDK A)+ 新 SDK(B)+ 按 B 编译的插件,宿主不自检 | 全部通过(即 §5.4 的漏洞) |
| 同上,宿主启动时比对内联 `SDK_ID` 与 `loaded_sdk_id()` | 启动即拒绝 |

**未验证(实施前必须完成)**

- V1 跨机器可复现构建(不同源码路径、CARGO_HOME、target 目录、用户名,且各机器的重映射参数不同),
  必须使用 §5.2 的真实 `SDK_ID` 生成逻辑;不成立则采用 §5.3 的退路。
- V2 macOS:`install_name` / `@rpath`、`RTLD_LOCAL` 下同名 crate 多版本共存、两级命名空间的影响。
- V3 Windows:Rust `dylib` 的导出符号数量上限(DLL 导出表 65535 项,大型 dylib 可能超限)、加载锁、`std` DLL 的分发。
  若不可行,Windows 只提供静态变体。**2026-10-03 已验证可行**,见下文“Windows 可行性验证”;实现见 #118。
- V4 release 配置(LTO、`codegen-units`、`opt-level`)下的同样验证;SDK 不得启用跨 crate 的 LTO。
- V5 跨库 trait object 的长期运行:旧版本插件留下的任务在新版本运行期间继续执行、析构发生在旧库代码中。
- V6 线程局部变量与 `tracing` 等带全局分发器的 crate(若进入 SDK)在跨库下的行为。
- V7 引导接口与引导 blob(原型未验证):启动后 C ABI 查询与 L1 一致;blob 包含插件构建时 L1/L2,
  与运行期元数据一致;magic 字节搜索在 macOS/Windows 产物(含 debuginfo、去符号、strip 变体)上的定位可靠性。
- V8 独立启动器(本轮新增,未验证):不依赖 SDK/动态 libstd;误配 SDK 或 libstd 时不执行宿主,
  不触发其启动期分配器/初始化代码;加载路径固定到已验证的产物,覆盖不同工作目录、环境覆盖及多个安装版本。
  各平台未满足此约束前只提供静态变体。

### Windows 可行性验证(2026-10-03)

对应 [#103](https://github.com/arcships/rutis/issues/103),验证方案和不可行的判定见
[design-dylib-macos-windows](design-dylib-macos-windows-2026-10-03.md) §四(W1–W8)。

**环境**:GitHub Actions `windows-2025` runner(Windows Server 2025),rustc 1.98.1(48a229cea 2026-09-01,LLVM 22.1.8),
目标 `x86_64-pc-windows-msvc`,Visual Studio 18 Enterprise(MSVC 14.51.36231,`link.exe`),Windows SDK 10.0.26100.0。
实验代码在 `docs/probes/windows-dylib/`(重跑方法和 workflow 见其中 README),由 `probe/windows-dylib` 分支上的临时 workflow 运行,
共两轮:[run 37124981363](https://github.com/arcships/rutis/actions/runs/37124981363)、
[run 37125920008](https://github.com/arcships/rutis/actions/runs/37125920008)(第二轮加了 opt-level 变体、真实 SDK 的 W8、
持有句柄和可复现检查;两轮相同项的数字一致)。

**实验对象**:W1 用真实的 `crates/rutis-sdk`。`rutis-cli --features dylib-plugins` 在 Windows 上编译失败
(`rutis_dylib::Loader` 只在 Linux 上存在),但 Cargo 在失败前已经按它的依赖图链接出 `rutis_sdk.dll`;
另用 `w1-anchor`(同一依赖图去掉 `rutis-dylib`)构建一次,两者导出数相同。W2、W3、W5–W8 用一个最小模型
(`runtime/`:`sdk` 是 dylib 并重导出 tokio,插件和宿主都动态链接它,宿主用 `LoadLibraryExW` 按完整路径加载插件);
W8 另用真实 SDK + greeter 夹具副本跑了一遍(`w8-real/`)。

| # | 方法 | 实测结果 | 结论 |
| --- | --- | --- | --- |
| W1 | `rutis_sdk.dll` 按宿主方式构建,用 `object` 读导出目录,`dumpbin /exports` 复核;另用 SDK 依赖集合的副本加常见依赖重测 | 真实 SDK:release **1597** 项(占上限 2.4%),dev **14545** 项(22.2%);无 LNK1189 或其他链接错误。dev 下导出以泛型单态化为主(core 4945、tokio 3467、alloc 1593、std 1110)。增长(release / dev):副本基线 1568 / 14259;加 serde derive 1585 / 14350;加 regex、tracing、tracing-subscriber、chrono、uuid 2949 / 26773;加 reqwest(rustls)4188 / 34882;全部加上 5425 / **46683**(71.2%)。dev 下只给 `rutis-sdk` 包设 opt-level 2 没有效果(14543);给所有依赖设 opt-level 2(`[profile.dev.package."*"]`)降到 2609,全部依赖的副本降到 6457 | **可行**。发布用的 release 构建余量约 6.4 万项。dev 构建在依赖大幅增加时会接近上限,需要时让 SDK 的全部依赖以 opt-level ≥ 2 构建(只设 SDK 包本身不够,与 macOS 设计稿 §八 R7 的建议不同) |
| W2 | 同一插件 crate 用 `GREETER_VERSION=1/2` 在两个 target 目录构建,复制到 `<cache>/<hashA>/greeter.dll`、`<cache>/<hashB>/greeter.dll`,`LoadLibraryExW` 按完整路径先后加载 | 两个不同的模块句柄,各自返回 v1、v2,插件私有类型的 vtable 互不串线;同一路径再次加载返回同一句柄。v2 链接的是另一 target 目录的 `sdk.dll.lib`,运行时用的是宿主已加载的 `sdk.dll`,照常工作 | **可行**。同名不冲突;反过来说明加载器只按 DLL 名和符号名匹配 SDK,SDK 身份只能靠 L1/L2 校验 |
| W3 | 发布目录放宿主、`sdk.dll`、`std-*.dll`;插件以 `LOAD_LIBRARY_SEARCH_APPLICATION_DIR \| LOAD_LIBRARY_SEARCH_SYSTEM32` 加载。分别在工作目录、`PATH` 中的目录、`host.exe.local` 目录(以及 `host.exe.local` 空文件 + 工作目录)、插件所在的缓存目录放一份同名的“恶意” SDK(同符号、`mark()` 返回 evil)和一个无效的同名 std 文件 | 五种放置方式下,宿主静态导入和插件依赖都解析到发布目录的 SDK/std,插件复用已加载的 `sdk.dll`。对照组:发布目录**缺少** `sdk.dll` 时,不放恶意文件则启动失败(`STATUS_DLL_NOT_FOUND`),在 `PATH` 中放了就加载恶意 SDK 并正常运行 | **可行**。发布目录中的文件齐全时,没有找到能绕过它的路径;但文件缺失时会继续按 `PATH` 搜索,所以启动器必须在启动宿主前确认 SDK、std 存在且哈希正确,并持有它们直到宿主退出(W7) |
| W4 | `dumpbin /dependents` 和 `object` 读导入表 | rustup 的 `std-44a584f44bc3dd65.dll` 只依赖 `KERNEL32`、`ntdll`、`USERENV`、`WS2_32`、`bcryptprimitives`、`api-ms-win-core-synch-l1-2-0`,**不依赖** VC 运行时。`rutis_sdk.dll`、插件和宿主依赖 `VCRUNTIME140.dll` 和 UCRT(`api-ms-win-crt-*`)。std DLL 在工具链中有两份相同文件(`bin\` 和 `lib\rustlib\x86_64-pc-windows-msvc\lib\`) | 不影响可行性。VC++ 运行库按已决定的做法由用户安装;依赖来自 rustc 默认动态链接 CRT 的宿主、SDK 和插件,不是来自 std(macOS 设计稿 §八 R10 的说法需更正)。UCRT 是 Windows 10 起的系统组件 |
| W5 | 插件带 `ctor` 静态初始化(`.CRT$XCU`,在 DllMain 中执行,持有加载锁:分配内存、加锁、访问 SDK 和插件的 `thread_local!`、读环境变量)和带析构的 `thread_local!`;在有 4 个工作线程、blocking 线程不断创建和退出的 tokio 运行时中加载;另有一个初始化中启动线程并等待的反例 | 加载 0.8–1.1 ms 完成,初始化执行,无死锁;64 个任务和 16 个短线程访问后,线程退出时析构全部执行(运行时关闭后 inits=dtors=29/30)。反例:初始化里等待新线程 5 秒超时(新线程要等 DllMain 返回才能运行;若用 `join` 会永久卡住) | **可行**。只需在插件作者指南中加一条:静态初始化中不得等待其他线程(`join`、阻塞地等待 channel 或锁);`rutis_plugin_entry` 在 `LoadLibrary` 返回后调用,不受此限 |
| W6 | release 插件中 `#[used] #[link_section = ".rutism"] static [u8; 512]`(不被任何代码引用),`object` 按节名查找 | `.rutism` 恰好一个,512 字节,内容正确。现有 `export_plugin!` 用的 `.note.rutis.meta` 在 PE 中被截断为 `.note.ru`(真实 greeter 夹具同样如此)。同样的 512 字节在文件中出现两次:另一份在 `.rustc` 元数据节中 | **可行**。PE 上节名用 `.rutism`;必须按节名定位,不能按 magic 字节搜索整个文件(对应 V7) |
| W7 | 加载缓存中的 DLL 后尝试覆盖、复制覆盖、删除、改名、删除目录;再复制到新路径加载;另以只允许读取的共享模式(`FILE_SHARE_READ`)先打开文件再加载 | 覆盖、复制覆盖被拒(错误 32),删除和删除目录被拒(错误 5);**改名成功**,模块路径随之变化。新路径正常加载为另一个模块。先以 `FILE_SHARE_READ` 打开再加载:加载成功,改名和删除都被拒(错误 32) | **可行**。内容寻址缓存只新建、不覆盖的设计成立。已加载的 DLL 可以被改名,所以加载器校验哈希时应持有只读共享的句柄直到 `LoadLibrary` 返回;启动器对发布目录文件同样这样做 |
| W8 | 最小模型:TypeId、双向 downcast、插件内 `Handle::try_current` 和 `tokio::spawn`、SDK 中 `thread_local!`(const 与惰性初始化两种)在宿主和插件中是否同一份、SDK 静态变量、`catch_unwind`、Drop 次数;v1、v2 各跑一次。真实 SDK:`rutis_plugin_meta`、`rutis_plugin_entry`,在 `Ctx::root()` 上运行夹具插件 | 全部通过。TypeId 相同;downcast 双向成功;运行时外调用 `try_current` 返回失败,运行时内成功,4 个任务都在宿主的 `host-worker` 线程上执行;两个线程上宿主、插件(内联访问)、SDK 函数看到的 TLS 地址和值一致;静态变量共享;panic 被捕获,payload 完整,插件之后仍可调用;Drop 执行 1 次。真实 SDK:`sdk_id` 与宿主 `SDK_ID` 一致,插件 `provide` 的 `String` 和 `Snapshot` 在根 Ctx 上可读,dispose 后 Drop 标记为一次 | **可行** |

**附带发现**

- **SDK 字节可复现**:不加参数时,两个 target 目录构建的 `sdk.dll` 哈希不同;只加 `--remap-path-prefix` 仍不同;
  再加 `-C link-arg=/Brepro`(去掉链接器写入的时间戳)后相同(release 默认无调试信息,`/PDBALTPATH` 不影响结果)。
  只验证了同机不同目录,跨机器(V1)仍待验证。
- **`rutis_sdk.dll` 大小**:release 1.9 MB(导入库 0.95 MB),dev 7.5 MB(导入库 15 MB)。导入库不进发布目录。

**结论:可行。** W1–W8 都没有触发不可行的判定。实现见 [#118](https://github.com/arcships/rutis/issues/118),
范围除照搬 Linux/macOS 的部分外,必须包括:

1. **启动器不能 `exec`**:创建子进程并等待,转发退出码,自身忽略 Ctrl+C,用 Job Object 保证启动器退出时子进程一起结束;
   依赖宿主 PID 的工具要知道 PID 与 Linux/macOS 不同。
2. **启动器持有发布目录文件**:校验宿主、SDK、std 的存在和哈希后,以只允许读取的共享模式持有句柄直到子进程退出(W3 对照组、W7)。
3. **加载器**:`LoadLibraryExW` 按完整路径加载,标志用 `LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32`;
   校验哈希时持有只读共享句柄直到加载完成;插件目录中不得有 SDK 或 std 副本。
4. **引导 blob**:`export_plugin!` 在 PE 上用 `.rutism` 节;读取按节名定位。
5. **可复现构建**:构建脚本加 `-C link-arg=/Brepro` 和路径重映射,CI 比对 SDK 哈希。
6. **文件名与布局**:库文件名没有 `lib` 前缀(`rutis_sdk.dll`、`std-<hash>.dll`、`<plugin>.dll`),`.dll.lib` 不进发布目录。
7. **dev 构建的导出余量**:在文档中说明;需要时给 SDK 的全部依赖设 opt-level ≥ 2,并在 CI 中统计 release 构建的导出数,
   超过一个阈值(例如 30000)时报警。
8. **文档**:VC++ 运行库由用户安装;插件静态初始化中不得等待其他线程。

## 十二、实施步骤

1. **前置**:#41(旧代迟到注册)合入。
2. `rutis-sdk` crate:重导出、分配器、`SDK_ID`(`build.rs`,含 RUSTFLAGS 白名单规范化)、C ABI 引导函数、`export_plugin!`(含引导 blob)、`ConfigValue`。
3. `rutis-dylib` crate:宿主启动后交叉检查(§5.4,显式身份查询走 C ABI 接口)、清单、引导元数据静态解析(§7.1 第 2 步)、内容寻址缓存、三阶段加载、`DylibFactory`(含模块不变量检查)、`spawn`/`swap`、模块登记表与诊断、保留上限。
4. 宿主 `dylib-plugins` 构建变体、独立启动器及不可变发布布局(含动态依赖路径检查)。
5. 打包工具(`cargo xtask pack-plugin`):SDK/插件两阶段构建、插件内嵌 L2、从 blob 生成清单、复核 SDK 哈希、`cargo tree -d` 检查。
6. CI:SDK 发布流水线、插件批量重编、V1–V4、V7–V8 的自动化验证。
7. 文档:插件作者指南(禁止事项:自定义分配器、`panic = "abort"`、边界上使用私有类型、依赖 SDK crate 的其他版本)。

## 十三、验收标准(对应 #45)

- [x] 宿主 + 一个 dylib 插件:跨库 TypeId、downcast、trait object、异步调用、析构均有自动化测试。
- [ ] L1/L2 任一不匹配时加载被拒绝,原因可读;测试覆盖 §5.1 的两个反例。
- [ ] 宿主启动边界(§5.4):独立启动器在执行宿主前核对宿主/SDK/libstd;“旧宿主 + 新 SDK + 新插件”
  在宿主运行时启动前被拒绝。用 SDK 分配器/初始化计数验证拒绝路径没有执行 SDK 代码。
- [ ] 加载路径:可信不可变版本目录中,不同工作目录、环境覆盖、多版本安装都不会使宿主解析到未验证的 SDK/libstd;
  宿主启动后交叉检查实际加载的 SDK 身份。直接启动内部宿主不计为通过此验收。
- [ ] 引导元数据:清单 `library_sha256` 与文件一致、但 `sdk.id` 与 .so 内嵌值(引导 blob)不符的包,在 `dlopen`
  之前被拒绝,不执行任何初始化代码;引导 blob 与 `rutis_plugin_meta()` 不一致在第 5 步被拒绝。
- [ ] 插件 L2:SDK A/B 的 L1 相同但产物/布局不同,按 B 编译的插件配上标为 A 的清单(插件文件哈希正确),
  仍因内嵌 L2 不匹配在 `dlopen` 前被拒绝,初始化代码不执行;正常两阶段构建产物可加载。
- [x] `SDK_ID` 规范化:仅重映射参数不同的两次构建得到相同的 `SDK_ID` 与 SDK 产物哈希;未归类的 RUSTFLAGS 项使构建失败。
- [x] `swap` 加载新版本后,旧实例按 rutis 生命周期卸载,消费者切换到新版本服务。
- [x] 插件 id、name 或 injects 不同的模块:经 `swap` 和直接 `view.update(..)` 两条路径都被拒绝,fiber 保持原模块运行。
- [x] 保留上限生效,诊断中可见各版本使用情况与内存占用。
- [x] 默认构建仍为单二进制,不依赖任何 Rust 动态库。
- [ ] V1–V4、V7–V8 有结论并记录在本文档。
