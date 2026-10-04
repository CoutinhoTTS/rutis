# SDK 构建包：外部构建 dylib 插件不重建 SDK（2026-10-04）

对应 [#108](https://github.com/arcships/rutis/issues/108)。前提与背景见
[dylib SDK 设计](design-dylib-sdk-2026-09-24.md) §5.2/§5.3 和
[macOS/Windows 设计](design-dylib-macos-windows-2026-10-03.md) §3.8。
2026-10-04 评审（见 §十）后修订：构建包补入依赖闭包 rlib、注入机制改
`-L dependency=`、共存规则重写、验收语义更新。

## 一、结论先说

1. **外部构建走预编译 SDK，不走"不同依赖图复现相同字节"。** 构建包携带发布流水线编译
   好的 SDK 产物；插件构建时不重建 SDK，只链接发布的那一份。feature 图问题因此消失：
   SDK 字节根本不来自插件的构建。
2. **放弃跨图复现的原因**：Cargo 单次调用的 feature 统一跨全图，插件私有依赖只要碰到
   `tokio`、`serde_json` 等 SDK 树内的 crate，统一结果就变，SDK 字节随之改变（SDK 设计稿
   §5.3 的 2026-09-25 发现）。补救方案"SDK 饱和启用全部 feature"不可靠：互斥 feature、
   feature 引入新依赖导致闭包漂移、任何一次依赖升级都要重新审计全树。
3. **一方流水线维持现状。** 锚点构建（`sdk.toml` 的 `[build] anchor_package/anchor_features`）
   继续服务仓库内插件与发布流水线；`--prebuilt-library`（只打包不构建）不变。
4. **预编译不等于"只带 dylib"。** `--extern rutis_sdk=<dylib>` 让 rustc 从 dylib 读 SDK 自身
   元数据，但该元数据声明了对 `rutis`、`tokio`、`tokio-util`、`serde_json`、`std` 等的
   依赖；插件代码必然触及这些类型（`ConfigValue` 就是 `serde_json::Value`），rustc 必须在
   搜索路径找到匹配的 rlib 元数据，否则报 `E0463`。因此构建包必须同时携带 **SDK 依赖闭包
   的 rlib**，注入用 `-L dependency=`（评审实验证实，见 §十）。闭包 rlib 与 dylib 出自同
   一次构建，crate disambiguator 一致，符号哈希天然匹配。
5. **插件私有图与 SDK 闭包的重叠不是"天然安全"。** 实测：插件直接使用独立解析的同名
   crate（如 `serde_json`）在**编译期**报 `colliding StableCrateId values`；间接引入且
   feature 恰与 SDK 副本全同时 disambiguator 相同，链接行为未定义。W2 证明的是两个插件
   版本共存，不覆盖"SDK 闭包 vs 插件私有图"。规则由此收敛：**共享 crate 只能来自
   `rutis_sdk::` 的再导出**——直接依赖在打包前置检查中拒绝；间接重叠在编译期暴露或由
   检查拒绝，打包器转发成可读诊断（§四、§五）。

## 二、现状与缺口

| 现状 | 缺口 |
| --- | --- |
| `pack-plugin` 默认模式要求锚点包在同一 Cargo 图中，即需要宿主源码 | 外部开发者没有宿主源码，此路不通 |
| `--prebuilt-library` 只做校验，插件二进制须在别处（发布流水线）构建 | 外部开发者没有构建环境 |
| `build-dylib-bundle.sh` 只复制运行文件（脚本第 53 行起的 `cp` 列表） | 是**运行**包：没有 SDK 依赖闭包的 rlib、Windows 导入库、工具链固定与注入配置 |
| `sdk.toml` 有 `[sdk]` version/id/artifact_sha256/target/rustc/packages 与 `[build]` anchor | 缺 std 字段、规范化 RUSTFLAGS、重映射规则、deployment target；`[build]` 是 shell 追加的，`--sdk-info` 生成不了全部字段（build.rs 的规范化结果未写入 `identity.rs`，重映射命令事后无法还原） |
| `check_shared_duplicates` 用 `cargo tree -d` 只查**同名不同版本** | 插件直接依赖 `tokio` 等共享 crate（同版本）不被拦截；与 SDK 闭包的间接重叠无诊断 |

## 三、SDK 构建包（sdk-bundle）

每个 SDK 版本、每个平台一份，由发布流水线产出。**不含宿主与启动器**，与运行发布目录
分开分发；两者通过 SDK `version + L2` 关联（同一份字节相同的 SDK 产物）：

```
sdk-bundle-<target>-<sdk-version>/
  bundle.toml        包级清单（见下）
  sdk.toml           SDK 身份与构建诊断
  lib/               librutis_sdk.so / .dylib / rutis_sdk.dll
                     Windows：rutis_sdk.dll.lib 与 .dll 同目录同名
  deps/              SDK 依赖闭包的编译期产物（E0 实测见 §六：rlib 与
                     proc-macro 的 .so 都要——rustc 递归加载闭包 crate 的
                     完整 rmeta 依赖链，闭包 crate 依赖的 proc-macro 也在内；
                     std 元数据来自固定工具链 sysroot，不进包）
  Cargo.lock         SDK 构建的锁文件（诊断与复核 packages 用）
  rust-toolchain.toml 固定 1.98.1
  cargo-config.toml  .cargo/config.toml 注入模板
  GUIDE.md           插件作者指南
```

**闭包收集与收缩（E0 定案）**：收集器按 `sdk.toml` 的 `packages` 包名，从锚点构建的
`target/release/deps` 拷贝每个包的**全部同名变体**（`.rlib` 与 proc-macro 的 `.so`；
多变体共存时 rustc 按 crate disambiguator 自动挑选，不冲突），随后用**收缩轮**去重：
逐个变体移走重试构建，能通过即删除。发布产物只留被选中的变体（E0 实测收缩后
41 个文件、60.1 MB，其中 4 个是 proc-macro 的 .so）。收缩轮在发布流水线内执行，
约两三百次增量编译，分钟级。


**`bundle.toml`（包级清单）**：`format_version`（构建包格式自身版本）、关联的 SDK
`version/L1/L2`、**每个文件（含闭包 rlib、导入库、模板）的 sha256**。完整性靠这份清单
而非 `version + L2`：依赖闭包、导入库、配置模板都可能在 SDK 字节不变时变化，仅校验 SDK
哈希发现不了缺件与错配。升级与撤回就是发布新的包目录（新的清单哈希），旧的停止分发。

**`sdk.toml` 字段采集**（当前 `--sdk-info` 只有 `identity.rs` 内的编译期常量，新增字段要
写清谁在什么时刻写入，不承诺单靠现有 `--sdk-info` 生成全部字段）：

| 字段 | 采集方与时机 |
| --- | --- |
| `[sdk]` version/id/artifact_sha256/target/rustc/packages | `--sdk-info`（`identity.rs`，现状已有） |
| `std_file` / `std_sha256` | 发布脚本（复制 std 时记录）；宿主 `--sdk-info` 另从 SDK 二进制读 `std_reference` 交叉核对 |
| 规范化 RUSTFLAGS | `build.rs` 编译期写入 `identity.rs`（扩展现有白名单规范化逻辑） |
| 路径重映射规则、`MACOSX_DEPLOYMENT_TARGET`、`/Brepro` | 发布步骤按实际构建参数记录（事后不可还原） |
| Xcode/链接器版本 | 发布步骤从产物 `LC_BUILD_VERSION` 读 |
| `[build]` anchor_package/anchor_features | 发布步骤（仅锚点模式消费） |

**`cargo-config.toml` 模板**内容：`--extern rutis_sdk=<lib/SDK>`、`-L dependency=<deps/>`、
`-L native=<lib/>`，macOS 另含 `MACOSX_DEPLOYMENT_TARGET=13.0`。模板头部注明：环境变量
`RUSTFLAGS` 会**整体覆盖** config 的 rustflags（不是追加），任何已存在的 RUSTFLAGS 会让
注入静默失效并报误导性的 `E0463`；打包器对此显式检测（§四第 3 步），GUIDE 建议构建时清空。

**体积**：闭包 rlib 为几十至百 MB 量级，是正确性的代价；E0 确定最小集合后再评估压缩，
不先做裁剪。

## 四、插件侧构建（`pack-plugin --bundle`）

插件 `Cargo.toml **不声明** `rutis-sdk` 依赖，`use rutis_sdk::...` 由注入的 `--extern`
解析。模式与默认锚点模式、`--prebuilt-library` 互斥；**直接依赖黑名单只对本模式启用**
（锚点模式必须声明 rutis-sdk 依赖，见夹具与实现文档用法，黑名单对锚点模式会拒掉全部
现有流程）。

1. 读 `bundle.toml` 与 `sdk.toml`：校验包内**所有**文件哈希与清单一致；`target` 与本机
   三元组一致；`--sdk-info` 记录的 `rustc` 版本与实际执行的 `rustc -vV` 完整版本一致
   （不符直接失败——不同 rustc 读元数据会先报 `E0514`，早于任何打包检查）。
2. **打包前置检查**（解析 manifest 与依赖图，不依赖 rustc 错误码）：插件**直接依赖**含
   `rutis-sdk` 或 `rutis`、`tokio`、`tokio-util`、`serde_json` 任一项 → 失败，指出包名与
   引入路径。"声明了但代码未引用"与"经传递引入 rutis-sdk 源码"同样拒绝。若靠注入后的
   rustc 报错兜底，实测会先撞上 SDK `build.rs` 的 RUSTFLAGS 白名单 panic（未归类的
   `--extern`），信息不可读，所以必须在构建前拒绝。
3. 构建插件：打包器**显式接管环境**——调用者已设置 `RUSTFLAGS` 或 `CARGO_ENCODED_RUSTFLAGS`
   时直接失败并说明原因；随后注入 `--extern rutis_sdk=<包内 lib/SDK>`、
   `-L dependency=<包内 deps/>`、`-L native=<包内 lib/>`（Windows 上 `--extern` 指向
   `rutis_sdk.dll`，同目录同名 `.dll.lib` 供链接，E0/E5 确认）；`RUTIS_SDK_ARTIFACT_SHA256`
   设为 `sdk.toml` 的 L2；工作目录与 `--manifest-path` 由打包器显式设定（不假设读取插件
   工作区的 config）。
4. 复用现有检查：自定义分配器、weak Rust 导出、`native_deps`、引导节 L1/L2 与
   `sdk.toml` 交叉核对。std 一致性改为：从**包内 SDK 二进制**读 `std_reference`
   （`rutis_dylib_meta::std_reference`，现状机制），比对插件产物的动态 std 引用；
   `sdk.toml` 的 `std_file/std_sha256` 字段做交叉核对。
5. **链接产物按平台核验**（不只比引导节）：Linux `DT_NEEDED` 只含 SDK soname、动态
   libstd、`native_deps`；macOS 依赖为 `@rpath/librutis_sdk.dylib` 且无 `LC_RPATH`、无
   flat lookup；Windows 导入表与延迟导入表只含 `rutis_sdk.dll`、std、允许的
   `native_deps`（小写文件名）。
6. **间接重叠诊断**：与 SDK 闭包重复的间接依赖若触发编译期错误（如 `colliding
   StableCrateId`），打包器把 rustc 输出转发为可读信息："crate X 与 SDK 闭包重复，改用
   `rutis_sdk::` 的再导出或移除该依赖"。
7. 从引导节生成 `plugin.toml`（含 `native_deps`），输出插件目录。

开发体验：`cargo-config.toml` 模板放进插件工作区后，`cargo check`/`cargo build` 可用
（前提是环境无 `RUSTFLAGS` 干扰）。rust-analyzer 的 flycheck 走 `cargo check` 可用，但
`--extern` 注入的 crate 不在 RA 的 crate graph 里，`rutis_sdk::` 的补全与跳转可能
unresolved——如实测量并记录（E8c），不作为验收承诺。

## 五、身份为什么成立

- **L1**：`SDK_ID` 是编译期 const，内联自发布 dylib 的元数据，自动等于发布值；链接了别的
  SDK 文件会被第 1/4 步的哈希核对拦下。
- **L2 与符号**：插件对 SDK 的全部引用（含泛型实例、vtable）用发布 dylib 元数据中的
  disambiguator；闭包 rlib 与 dylib 出自同一次构建，依赖闭包内每个 crate 的
  disambiguator 与 SDK 内一致，`-L dependency=` 下 rustc 找到的就是发布时用的那批
  元数据，符号必然解析（E0 实测确认）。
- **闭包是身份面**：闭包 rlib 的元数据决定插件的类型解析与符号引用；篡改闭包可以在
  L1/L2 全部通过的情况下产出 ABI 错乱的插件。因此闭包全部进 `bundle.toml` 哈希清单，
  逐文件校验（§四第 1 步），篡改面由 E6 覆盖。
- **libstd**：插件用固定工具链，std 元数据来自工具链 sysroot，与 SDK dylib 构建时的
  std 同源；插件产出的 `libstd-<hash>` 引用与运行发布目录的 std 文件一致，加载器现有
  检查兜底。E0 确认 rustc 接受 sysroot 元数据满足 SDK dylib 的 std 依赖。
- **共存规则（重写）**：删除"元数据哈希不同则天然不串线"的通则。实测事实：
  - 插件直接用独立解析的同名 crate → **编译期** `colliding StableCrateId`，不可链接；
  - 间接引入且 feature 与 SDK 副本全同 → disambiguator 相同，两份副本符号名相同，链接
    绑定顺序未定义，必须拒绝或给出警告（E2 记录行为）；
  - 间接引入但 feature 不同 → 符号不同，`RTLD_LOCAL` 下互不绑定（W2 只证插件互存，此
    场景由 E2 补证）。
  收敛规则：共享类型只经 `rutis_sdk::` 再导出；直接依赖前置拒绝，间接重叠靠编译期冲突
  或打包检查暴露。GUIDE 写明：间接静态链接的同名 crate 其**运行时上下文不与宿主共享**
  （如 `tokio::spawn` 找不到宿主运行时，除非用 SDK 再导出的 tokio）。
- **#108 验收语义变化**：见 §八。

## 六、验证（E 系列）

判定：E0 或 E1 失败，把结论记回本稿；若 E1 失败则外部构建退回"仅发布流水线
`--prebuilt-library`"，#108 以文档化限制关闭。

| # | 内容 | 判定 |
| --- | --- | --- |
| E0 | **最小闭包与注入形式（三平台）**：以发布 SDK 实测 rustc 递归要求的 rlib 集合（按元数据依赖图）；`.rmeta` 是否够（build 模式预期不够）；sysroot std 元数据可用性；Windows `--extern` 指向 `.dll` + 同目录 `.dll.lib` 的链接；闭包体积数据 | 每项结论记入本稿；闭包集合成为收集器规格 |

**E0 记录（2026-10-04，Linux x64，SDK 0.5.0 / L2 `a1064b9e…`）**：

- 注入形式成立：`--extern rutis_sdk=<发布 dylib>` + `-L dependency=<deps/>`，插件
  Cargo.toml 不声明 rutis-sdk 依赖，独立工作区编译链接通过；产物 `DT_NEEDED` 只含
  `librutis_sdk.so`、`libstd-<hash>.so` 与系统库，无 run path，引导节 L1/L2 与
  `sdk.toml` 一致。
- **闭包按 rmeta 依赖链完整递归，proc-macro 的 .so 必须随包**：只带 rlib 时 E0463
  （`futures_macro`、`tokio_macros`、`thiserror_impl` 等被 rustc 打开）。缺任何一项的
  报错都只有误导性的 `can't find crate for rutis_sdk`，无 note；闭包完整性必须由打包器
  按 `bundle.toml` 清单自查（对应 E9 的可读错误）。
- 同名多变体共存不冲突：rustc 按 disambiguator 挑选。收缩后最小集 **41 个产物
  （37 rlib + 4 个 proc-macro .so）、60.1 MB**（见 §三收集与收缩）。
- sysroot std 元数据满足 SDK dylib 的 std 依赖（固定工具链即可，std 不进包）。
- rustc 版本先于一切：1.97.1 工具链下最先报 `E0514`，先于任何 E0463。
- E1 等价链路通过：收缩集构建的插件被发布宿主（`rutis-cli --plugin`）加载，L1/L2/boot
  校验通过，初始化与 apply（含 `rutis_sdk::tokio::spawn` 在宿主运行时）执行。
- 待平台补测：macOS（E4）、Windows（E5，`.dll.lib` 与 `--extern` 指向形式）。

| E1 | Linux：独立工作区（无宿主源码、不在仓库内），构建包 + `--bundle` 构建 greeter 等价插件；发布宿主加载，v1→v2 换代、消费者重载；§四第 5 步链接产物核验 | 与 `test-dylib.sh` 同口径 |
| E2 | 私有依赖矩阵：SDK 树外 crate（`base64`）→ 正常；直接依赖 `serde_json` → 前置拒绝；间接引入同版本不同 feature → 行为记录；间接引入 feature 全同 → 预期拒绝/警告，行为记录 | 每格结论记入 §五 |
| E3 | 前置检查：声明 `rutis-sdk`（含声明未引用）、传递引入 SDK 源码、直接依赖共享 crate → 构建前失败，信息含包名与引入路径 | 拒绝发生在调用 rustc 前 |
| E4 | macOS：E1 等价（arm64、`@rpath`、quarantine、无 run path、两级命名空间） | 同 E1 |
| E5 | Windows：E1 等价（导入库来自构建包；导入表核验；GUIDE 的静态初始化禁则带最小失败测试） | 同 E1 |
| E6 | 篡改：SDK dylib 改一字节、闭包 rlib 改一字节、`bundle.toml` 缺件/哈希不符、用锚点模式自建 SDK 冒充 → 全部拒绝，错误可读 | 拒绝在构建/`dlopen` 前 |
| E7 | 工具链：非 1.98.1 构建 → 第 1 步 rustc 版本核对失败，信息提示用包内 `rust-toolchain.toml`；不出现 E0514 | 版本核对先于一切 |
| E8 | 开发体验拆分：E8a 干净环境（无 RUSTFLAGS）`cargo check`/`build` 通过；E8b 环境带 `RUSTFLAGS` → 打包器立即失败并说明覆盖规则；E8c rust-analyzer 行为实测记录（不承诺补全） | E8a/E8b 可验收，E8c 仅记录 |
| E9 | 闭包缺失/损坏（删一个 rlib）→ 可读错误，指向缺失的 crate（不是误导性 E0463 指向 rutis_sdk） | 错误可读 |

CI：`dylib-linux`、`dylib-macos` 增加 bundle 作业（产出构建包 → 独立临时工作区构建夹具
插件 → `loader_host` 加载换代）；Windows 走 `dylib-windows` 现有触发条件。

## 七、实施步骤

1. **E0 实验**：闭包集合与注入形式定案，结论（含体积）记入本稿 §三/§五。
2. `build.rs` 把规范化 RUSTFLAGS 写入 `identity.rs`；`--sdk-info` 输出 std 交叉核对；
   按 §三字段表实现发布步骤的采集。
3. 构建包产出：闭包收集器（按元数据依赖图递归，排除 proc-macro 宿主产物）+
   `bundle.toml` 清单，三平台。
4. `pack-plugin --bundle`：前置检查（§四第 2 步）、环境接管、注入、rustc 版本核对、
   链接产物核验（§四第 5 步）、间接重叠诊断（§四第 6 步）。
5. E1–E3、E9 通过后进 CI；E4–E8 随平台作业。
6. 文档：`dylib-sdk-implementation.md` 增"外部构建"一节；#108 验收第 2 条措辞按 §八
   更新；GUIDE 含 Windows 静态初始化禁则、TLS 上下文说明、RUSTFLAGS 警告、
   deployment target。

## 八、验收映射（#108）

| #108 验收 | 对应 |
| --- | --- |
| 无宿主源码的独立工作区，只用构建包构建带私有依赖的插件，L2 与发布 SDK 一致，被发布宿主加载并完成换代 | E1、E2（非重叠私有依赖）、E4；Windows E5 超出要求一并做 |
| "私有依赖改变 SDK feature 时打包失败并指出哪个依赖哪个 feature" | **语义更新并请求改写**：预编译模式下私有依赖改不了 SDK 的 feature；对应拒绝为"与 SDK 闭包重复 → 打包失败并指出哪个依赖"——直接依赖前置拒绝（E3），间接重叠由编译期冲突转发或打包检查拒绝（E2）。feature 差异不再可观测：要么符号不同（允许共存），要么符号相同（拒绝），两种结果都要可读 |
| Linux 和 macOS 都通过 | E1、E4 |

#108 第 2 项"验证不同依赖图能否构建出相同 SDK 字节"从工作项移除：一方流水线已有锚点
方案，外部构建由本设计取代该问题的前提。

## 九、明确不做

- 不证明任意 Cargo 图可复现 SDK 字节（§一第 2 条）。
- 不发布 `rutis-sdk` 到 crates.io：可安装的源码形态会诱导插件声明依赖并从源码重建。
- 不承诺完整的 rust-analyzer 体验：`--extern` 注入的 crate 不在 RA crate graph 中，
  补全/跳转按 E8c 实测记录。
- 不改变信任边界：dylib 插件仍是同进程可信方案，签名与 Team ID 按 macOS 设计稿 §3.8；
  不受控代码走协议插件。
- 不动运行发布目录的内容与启动器校验；构建包是独立新增产物，靠 `bundle.toml` 与
  `version + L2` 关联。
- 不先做构建包瘦身：闭包完整性优先，E0 之后按最小集合自然收敛，再评估压缩。

## 十、评审记录（2026-10-04）

GLM 5.3 与 GPT-6-sol（并行独立评审，两者都做了最小复现实验，未改动仓库），结论均为
request-changes。v2 修订采纳的要点：

| 发现（来源） | 修订 |
| --- | --- |
| 构建包只有 dylib 时 `--extern` 注入报 `E0463`（两家实测：找不到 `rutis` 等闭包 crate 的元数据；`.rmeta` 在 build 模式不够；`-L native=` 不搜 Rust crate） | §三 `deps/` 闭包 rlib；§四第 3 步 `-L dependency=`；E0、E9 |
| 插件直接用独立解析的 `serde_json` 报 `colliding StableCrateId`（gpt 实测）；feature 全同时 disambiguator 相同、链接行为未定义（glm 分析） | §一第 5 条、§五共存规则重写；E2 矩阵 |
| `.cargo/config.toml` 的 rustflags 被环境 `RUSTFLAGS` 整体覆盖（两家实测），静默失效报误导性 E0463 | §三模板警告、§四第 3 步环境接管、E8b |
| "声明 rutis-sdk 必报 E0464"不准：实测先撞 SDK `build.rs:163` 的 RUSTFLAGS 白名单 panic（gpt） | §四第 2 步前置检查取代错误码依赖；E3 |
| 直接依赖黑名单"补进锚点模式"自相矛盾：锚点模式必须声明 rutis-sdk（glm） | §四：黑名单仅 `--bundle` 模式 |
| rustc 版本不符会先报 E0514，早于 std 检查；固定工具链不保证实际使用（gpt） | §四第 1 步构建前版本核对；E7 |
| 两阶段"只换第一阶段"过简；bundle 模式需核验链接产物（导入表、install name、run path、DT_NEEDED）（gpt） | §四第 5 步；E1/E4/E5 判定 |
| `version + L2` 不标识整个构建包；缺件/错配不可见（两家） | §三 `bundle.toml` 文件级清单；E6 |
| `--sdk-info` 生成不了全部 `[build]` 字段：规范化 RUSTFLAGS 未进 `identity.rs`、重映射事后不可还原（gpt） | §三字段采集表；§七第 2 步 |
| §四引用的"sdk.toml 记录的 std 文件"字段不存在（glm） | §三 `std_file/std_sha256` + §四第 4 步从 SDK 二进制读 |
| macOS deployment target 未进注入模板（glm）；Windows `--extern` 目标形式未写清（glm） | §三模板、§四第 3 步 |
| Windows GUIDE 漏"静态初始化不得等待线程"（gpt，W5）；间接 tokio 的 TLS 上下文不共享应写明（glm） | §五 GUIDE 条目、§七第 6 步；E5 |
| rust-analyzer"补全正常"是过度承诺（glm） | §四开发体验、E8c、§九 |
