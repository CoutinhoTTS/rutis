# dylib 插件加载：macOS 实现与 Windows 可行性验证(2026-10-03)

> 对应 [#102](https://github.com/arcships/rutis/issues/102)(macOS)、[#103](https://github.com/arcships/rutis/issues/103)(Windows),
> 上游设计 [design-dylib-sdk](design-dylib-sdk-2026-09-24.md)(下称“SDK 设计稿”)§十一 V1、V2、V3、V7、V8。
> 状态:设计稿 + macOS 预实验 + 一轮评审(§七)+ 外部调研(§八)。不改变默认形态:各平台默认仍是静态链接的单二进制。

## 一、范围与前提

| 平台 | 本文给出 | 结论条件 |
| --- | --- | --- |
| macOS arm64 | 实现方案,验收与 Linux 相同 | 预实验(§二)已排除主要不可行项 |
| macOS x86_64 | 不在本轮 | arm64 通过后单独开 CI 任务 |
| Windows x64 (msvc) | 验证方案和中止条件 | 验证记录写入 SDK 设计稿 §十一 后再决定是否实现 |
| windows-gnu、其他 unix | 不支持 | 继续只提供静态链接 |

**前提一:P3–P6 先进 main。** #98(P3 `DylibResolver`)到 #101 合入的是各自的堆叠基分支,不是 main:
`origin/main` 停在 #97,`DylibResolver` 只在 `feat/loader-catalog-expr` 上,P6 的完整代码在 `feat/loader-volatile` 上。
#102 的验收包括 `loader_host` 示例,[#104](https://github.com/arcships/rutis/pull/104) 已于 2026-10-03 把同一棵树合进 main,这一前提已满足。

**前提二:修正现有的 SDK 可复现测试。** `tools/test-dylib-repro.sh` 和 CI 的 `sdk-repro` 用 `cargo build -p rutis-sdk` 构建。
SDK 作为 primary package 时,Cargo 不传 `-C prefer-dynamic`,产出的是**静态链接 std** 的变体;实际发布的 SDK 是作为
`rutis-cli --features dylib-plugins` 的依赖构建的,动态链接 libstd,字节不同(评审在 macOS 上复现:前者 `otool -L` 无 libstd,哈希也不同)。
所以现有测试证明的是另一个产物可复现。改为按宿主锚点构建(与 `build-dylib-bundle.sh` 相同)再比较 SDK 哈希。这是 Linux 上已经存在的问题,单独修。

## 二、macOS 预实验(2026-10-03)

环境:macOS 26 arm64,rustc 1.98.1,ld-1267,MacOSX SDK 26.5。原型是一个最小三件套:`sdk`(dylib,带一个全局计数器)、
`greeter`(dylib,依赖 sdk,带 `#[used] #[link_section]` 引导数据)、`host`(依赖 sdk,`dlopen` 插件)。
v1、v2 是同一个 crate 名,分别在两个 target 目录中构建。原型在会话临时目录,未提交。E13–E16 由评审补做。

| # | 实验 | 结果 | 对方案的影响 |
| --- | --- | --- | --- |
| E1 | `#[used] #[link_section = "__DATA,__rutis_meta"]` | release dylib 中保留,`otool -s __DATA __rutis_meta` 读到完整内容 | Mach-O 节名可用(§3.1) |
| E2 | rustc 给 dylib 写的 install name | **构建目录的绝对路径**(`…/target/release/deps/libsdk.dylib`),不是 `@rpath/…`;libstd 是 `@rpath/libstd-<hash>.dylib` | 宿主和插件记下的 SDK 依赖是绝对路径 |
| E3 | 按 E2 的默认值,v1、v2 两个插件同时加载 | 都能加载,但 v2 从**它自己的** target 目录加载了第二份 SDK:两个插件各调一次计数器,宿主读到 1 而不是 2 | SDK 分裂:TypeId、tokio 运行时都不再是单份。`dlopen` 和 L1/L2 都拦不住(§3.3) |
| E4 | SDK `build.rs` 输出 `cargo::rustc-link-arg=-Wl,-install_name,@rpath/libsdk.dylib` | 只作用于 SDK 包,对 `crate-type = ["dylib"]` 生效;宿主、插件记录的依赖变为 `@rpath/libsdk.dylib` | 链接参数按 crate 注入的机制可用(§3.3) |
| E5 | E4 之后,删除两个 target 目录,只留发布目录,同时加载 v1、v2 | 两版各自输出自己的版本号,私有类型互不串线;计数器为 2,SDK 单份。评审补测两个插件 install name 都是 `@rpath/libgreeter.dylib` 的情况,同样分别加载 | V2 的主要问题成立:两级命名空间 + `RTLD_LOCAL` 下同名 crate 两版共存 |
| E6 | SDK 作为依赖构建,不同 target 目录,路径重映射后的 sha256 | 逐字节相同(含 install name 和链接器的 ad-hoc 签名);评审用五个 target 目录复核 | 同机可复现;跨机器仍待 CI(§3.6) |
| E7 | 宿主环境中 `DYLD_LIBRARY_PATH=<恶意目录>`,目录中是一个带 `__mod_init_func` 初始化函数的合法 SDK | 初始化函数**在 main 之前执行**;`DYLD_INSERT_LIBRARIES` 同样生效 | 调用者的 DYLD_* 能到达未开 hardened runtime 的进程(§3.4) |
| E8 | 恶意目录中放一个无效文件 | dyld 跳过它,继续按 `@rpath` 加载正确的 SDK | 只有合法的 Mach-O 才构成威胁 |
| E9 | 宿主加 hardened runtime(`codesign -o runtime`) | DYLD_* 被忽略并从环境中删除;但 library validation 拒绝 ad-hoc 签名的 SDK(Team ID 不同) | 宿主的 hardened runtime 要和签名方案一起决定(§3.4) |
| E10 | hardened runtime + `com.apple.security.cs.disable-library-validation` | DYLD_* 仍被忽略,SDK 与插件正常加载 | 宿主侧纵深防御选项 |
| E11 | 插件带 `com.apple.quarantine` 属性后 `dlopen` | 调用**一直不返回**(Gatekeeper 评估,无界面时卡住);`cp` 会把该属性复制到目标文件 | `dlopen` 前检查实际要打开的文件(§3.2) |
| E12 | 插件导出表中的 weak 定义 | 原型插件中没有 Rust 符号的 weak 定义;工具链自带的 libstd 有 `___isOSVersionAtLeast` 等 compiler-rt 的 weak 定义 | weak 检查只针对 Rust 修饰名(§3.5) |
| E13 | 启动器(未开 hardened runtime)环境中有 `DYLD_INSERT_LIBRARIES` | 插入库的初始化函数**在启动器 main 之前**执行;启动器用 `-o runtime` 重签后不再执行,main 中也看不到任何 DYLD_* | 注入启动器本身属于“控制进程加载器”,在 SDK 设计稿 §5.4 声明的防护范围之外;启动器只需保证 DYLD_* 不传给宿主(§3.4) |
| E14 | SDK 中放一个计数的 `#[global_allocator]` | 宿主、插件的分配经过它;**libstd 内部的分配不经过**(`current_dir()`、`read_to_string()` 计数不变)。`nm -m` 显示 libstd 自己导出 `___rust_alloc`,宿主和插件绑定 `(from libsdk)` | 两级命名空间下分配器被拆成两份;SDK 分配器只能是 `System`(§3.5、§八 R1) |
| E15 | 对 linker-signed 插件执行 `install_name_tool -id`、`strip -x` | `codesign -v` 仍通过,宿主正常加载;Apple 工具会自动重签 linker-signed 的 ad-hoc 签名 | 事后修改不会破坏签名,但会改变字节;仍然在链接时设定(§3.4) |
| E16 | `DYLD_X=… /usr/bin/env prog`、`/usr/bin/env DYLD_X=… prog`、`/bin/sh -c` | 第一种和第三种被删掉;第二种生效 | SIP 只在执行受保护的系统二进制时清除;E7 的情形真实存在 |

## 三、macOS 方案

按 #102 的五项展开;与 issue 原文不同的地方单独标出。

### 3.1 加载前读取身份信息

**新 crate `rutis-dylib-meta`**:只依赖 `object`(features `read_core`、`elf`、`macho`、`pe`、`std`),不依赖 `rutis`、`rutis-sdk`。
提供 `read_boot(bytes, target) -> Result<BootMeta, String>` 和 `check_deps(bytes, policy) -> Result<(), String>`(§3.3)。
`rutis-dylib` 和 `xtask pack-plugin` 共用它。

单独成 crate 的原因:xtask 要检查任意目标的产物,而依赖 `rutis-dylib` 就会链接 SDK dylib 和动态 libstd,xtask 不应如此。
SDK 不能反过来依赖这个 crate(会改变 SDK 的依赖树和身份),所以 `BOOT_MAGIC`、`BOOT_SIZE` 在两边各写一份,
`rutis-dylib` 中加测试断言两边一致。

**节名按目标格式选择**,在 `export_plugin!` 中用 `cfg_attr`:

| 格式 | 节 | 说明 |
| --- | --- | --- |
| ELF | `.note.rutis.meta` | 不变 |
| Mach-O | `__DATA,__rutis_meta` | 节名 12 字节,上限 16;E1 已验证 |
| PE | `.rutism` | 镜像中的节名最多 8 字节,`.note.rutis.meta` 会被截断,不能沿用(§四 W6) |

**读取规则**(比现在的 ELF 解析更严格):

1. 格式与目标一致。ELF:ELF64 小端、`ET_DYN`、`e_machine` 与目标一致。Mach-O:64 位、`MH_DYLIB`、CPU 类型一致,
   且 `LC_BUILD_VERSION.platform` 为 macOS(iOS 模拟器的 arm64 产物 CPU 类型相同)。PE:PE32+ DLL、`Machine` 一致。
   架构不符在这里给出可读错误,而不是让 `dlopen` 报“incompatible architecture”。
2. **拒绝 fat(universal)Mach-O。** 按 target 构建,只发布单架构。
3. 按名字匹配到的节必须恰好一个,大小等于 `BOOT_SIZE`,以 `BOOT_MAGIC` 开头。
4. 只读节内容,不做重定位;引导 blob 是纯数据,格式不变。

Linux 的手写 `elf_section` 删除,改用同一套代码,现有 Linux 测试(含 bad-boot)原样通过。

### 3.2 加载器

`rutis-dylib` 的 `linux` 模块改名为 `unix`,启用条件写成 `cfg(any(target_os = "linux", target_os = "macos"))`。
不用 `cfg(unix)`:FreeBSD 等没有验证过。也不用 build.rs 输出的自定义 cfg,因为它不会传给 `rutis-cli`、示例等下游 crate。
`DylibResolver`、`rutis-cli` 的 `dylib-plugins` feature、示例都改用这个条件。
(现在 `linux.rs` 开头的 `compile_error!` 实际不会触发,因为整个模块已被 `lib.rs` 的 cfg 排除;改名时一并删除。)

`dlopen`/`dlsym`/`dladdr` 的用法不变。macOS 上的差异:

- **库文件名**不再写死:打包工具按目标三元组推出前后缀(`lib*.so`、`lib*.dylib`、`*.dll`)。加载器本来就读清单中的 `library`。
- **quarantine(E11)**:检查两处,任一处带 `com.apple.quarantine` 就拒绝。错误中说明原因和解除方法(`xattr -d com.apple.quarantine`),
  由用户决定;加载器不自动清除,那等于替用户绕过 Gatekeeper。
  1. **源文件**:每次 `load` 都在读取源文件、写入或复用缓存**之前**检查,不论缓存是否命中。缓存的写入路径是“读字节 → 写临时文件 → rename”,
     不会带上扩展属性;如果只查缓存,第一次加载一个下载来的插件时,quarantine 在写入缓存时就丢了,检查形同虚设。
     复制进缓存不能被当作用户已经同意。
  2. **缓存文件**:`dlopen` 前检查实际要打开的缓存文件。`ensure_cached` 会复用哈希一致的已有缓存条目(`linux.rs:555`),
     用户用 `cp` 或 Finder 放进去的同哈希文件可能带着该属性。
- **覆盖已映射的文件**:原地改写已加载的 dylib 会因代码签名页校验失败导致进程被杀。现有缓存“临时文件 + 原子 rename、
  不原地覆盖”的规则已覆盖这一点,在注释中写明。
- **宿主自检** `loaded_sdk_path()` 仍用 `dladdr`;评审确认 macOS 返回展开后的绝对路径。

### 3.3 install name、库搜索路径与依赖检查

这部分是 issue 原文漏掉的关键点。E2/E3 说明:**rustc 默认的 install name 是构建目录绝对路径,插件可能在 L1/L2 都通过的情况下加载第二份 SDK。**
L1/L2 检查“插件按哪个 SDK 编译”,不检查“dyld 实际从哪里加载 SDK”;两份 SDK 字节相同时两项都通过。
Linux 没有这个问题:rustc 生成的 `DT_NEEDED` 只是文件名,glibc 按名字复用已加载的 SDK。

**链接参数按产物注入,不再写进 RUSTFLAGS。** 现在 `-rpath,$ORIGIN` 通过 RUSTFLAGS 作用于 SDK、宿主和插件全部产物
(`tools/pack-dylib-plugin.py:155`、`tools/test-dylib.sh:10`、`tools/build-dylib-bundle.sh:13`、`ci.yml` 的 sdk-repro)。
插件构建必须重建出逐字节相同的 SDK,因此不能只给插件加减 RUSTFLAGS;同理,用 RUSTFLAGS 给插件设 install name 会落到 SDK 上。改为各 crate 的 `build.rs`:

| 产物 | 注入位置 | Linux | macOS |
| --- | --- | --- | --- |
| SDK | `rutis-sdk/build.rs`,`rustc-link-arg` | `-rpath,$ORIGIN` | `-install_name,@rpath/librutis_sdk.dylib`、`-rpath,@loader_path` |
| 宿主 | `rutis-cli/build.rs`,`CARGO_FEATURE_DYLIB_PLUGINS` 下 `rustc-link-arg-bins` | `-rpath,$ORIGIN` | `-rpath,@loader_path` |
| 示例宿主 | `rutis-dylib/build.rs`,`rustc-link-arg-examples` | 同上 | 同上(测试脚本不再靠 `LD_LIBRARY_PATH`/`DYLD_LIBRARY_PATH` 运行示例) |
| 插件 | 不注入 | 无 RUNPATH | 无 LC_RPATH;install name 不强制,按路径 `dlopen` 不受影响 |

这会改变 Linux SDK 的字节,和 SDK 升级放在同一个 PR(§3.6)。

**插件不带 rpath。** 插件被 `dlopen` 时,SDK 和 libstd 已由宿主加载,dyld 按 install name 复用(评审用 `DYLD_PRINT_SEARCHING` 看到
`already-loaded-by-rpath`)。评审还确认:不带 LC_RPATH 的插件,缺失的 `@rpath` 依赖只会到宿主的 `@loader_path`(发布目录)查找,
不会到插件所在的缓存目录查找。

**加载前依赖检查(新增,SDK 设计稿 §7.1 第 2 步的一部分)。** 插件的依赖分三类,遇到不属于任何一类的一律拒绝。

| 类别 | 允许什么 | 理由 |
| --- | --- | --- |
| Rust 共享部分 | `librutis_sdk`;**精确等于**启动器绑定的 `libstd-<hash>` | 必须复用宿主已加载的那一份 |
| 原生库(如 `libssl`、`libz`、系统框架) | 允许 | 插件可以动态链接系统里或用户安装的原生库 |
| 其他 Rust dylib | 不允许 | 会带进第二份 std 或 SDK |

原生库的写法要求(防止从插件缓存目录或其他不受控的位置找库):

- Mach-O:
  - 检查所有 dylib 类加载命令(`LC_LOAD_DYLIB`、`LC_LOAD_WEAK_DYLIB`、`LC_REEXPORT_DYLIB`、`LC_LOAD_UPWARD_DYLIB`、`LC_LAZY_LOAD_DYLIB`)。
  - SDK 和 libstd 必须写成 `@rpath/librutis_sdk.dylib`、`@rpath/libstd-<hash>.dylib`;其他 `@rpath/…` 依赖拒绝(它们会被解析到发布目录,而发布目录里没有这些库)。
  - 原生库必须是绝对路径,例如 `/usr/lib/libz.1.dylib`、`/System/Library/Frameworks/…`、`/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`。
  - 依赖路径中出现 `@executable_path`、`@loader_path`、相对路径或 `..` 即拒绝。
  - 插件不得有 `LC_RPATH`;不认识的带 `LC_REQ_DYLD` 位的命令拒绝。
  - 必须是 `MH_TWOLEVEL`,拒绝 `MH_FORCE_FLAT` 和 flat lookup 绑定:插件的 C 依赖若以 `-undefined dynamic_lookup` 构建,v2 的符号可能绑到 v1。
  - 宿主和 SDK(打包时检查)另外拒绝 `LC_DYLD_ENVIRONMENT`。
- ELF:
  - `DT_NEEDED` 不得含 `/`;SDK 必须是 `librutis_sdk.so`,libstd 必须精确等于绑定的文件名;其他名字视为原生库,由系统动态链接器按标准路径查找。
  - 插件不得有 `DT_RUNPATH`/`DT_RPATH`;拒绝 `DT_AUXILIARY`、`DT_FILTER`、`DT_AUDIT`、`DT_DEPAUDIT`。
- 名字形如 `libstd-*`、`librutis_sdk*` 但与绑定值不同的依赖,按“其他 Rust dylib”拒绝。间接依赖里的 Rust dylib 无法在加载前完全识别,写进插件作者指南。

原生库**不在启动器的校验范围内**:它们由部署插件的人负责,与插件本身一样被视为可信。为了便于排查和审计:

- 打包工具把插件的原生库依赖写进清单 `[plugin] native_deps`;加载器核对二进制中的依赖与清单一致,不一致即拒绝。
- 原生库缺失时,`dlopen`(`RTLD_NOW`)直接失败,错误信息中带上缺失的库名。
- Linux 上动态链接器按 SONAME 复用已加载的库:两个插件版本依赖同一 SONAME 的不同实现时,后加载的会用到先加载的那份。不兼容的原生库版本必须有不同的 SONAME(系统库通常如此),写进插件作者指南。
- 宿主若开了 hardened runtime(§3.4),原生库同样要满足签名要求。

打包时用同一套检查核对宿主和 SDK,替代 `build-dylib-bundle.sh` 里的 `ldd`;CI 另用 `otool -L`/`otool -l` 输出供人工对照。

### 3.4 启动器与代码签名

**修正 issue 原文。** issue 写“SIP 会清除 DYLD_* 环境变量,因此不能依赖环境变量”。后半句对:发布目录靠 `@rpath`/`@loader_path` 指定,
不靠 `DYLD_LIBRARY_PATH`。前半句不对:SIP 只在执行受保护的系统二进制时清除(E16)。调用者环境中的 DYLD_* 能到达宿主,
E7 中一个放错的 SDK 在宿主 main 之前就运行了。

**防护范围与 SDK 设计稿 §5.4 相同:防部署和环境配置出错,不防能控制进程加载器的攻击者。** 在 macOS 上,要防的典型情况是
开发机环境里留着 `DYLD_LIBRARY_PATH` 或 `DYLD_INSERT_LIBRARIES`,指向另一份 SDK 构建。这些变量对启动器本身没有影响(启动器不链接 SDK),
有影响的是被启动的宿主。所以启动器的做法与 Linux 清除 `LD_*` 相同:

1. 启动器校验宿主、SDK、libstd 的哈希,与 Linux 相同。
2. 把所有 `DYLD_*` 变量改名为 `RUTIS_ORIG_DYLD_*`,从宿主的环境中删除;不设置任何 `DYLD_*`。
3. `exec` 宿主。宿主在启动运行时线程前恢复 `RUTIS_ORIG_*`(`rutis-cli` 现有的 `LD_*` 恢复逻辑推广到 `DYLD_`),
   宿主启动的子进程看到的环境与调用者一致。评审确认:宿主运行中再设置 `DYLD_*` 不影响之后的 `dlopen`。
4. 启动器不需要特殊签名,用链接器自动加的 ad-hoc 签名即可;宿主的发布方可以按自己的方案重新签名。

**不防的情况,写进文档:** 有人在调用者环境中设置 `DYLD_INSERT_LIBRARIES`,把代码注入启动器本身(E13)。注入的代码在启动器的 main 之前运行,
启动器无法阻止。这属于控制进程加载器,在防护范围之外。需要防这一点的发布方,可以给启动器加 hardened runtime 签名
(`codesign -o runtime`,不带任何 entitlement;启动器只依赖系统库,不受 library validation 影响):E13 中这样签名后注入不再生效。
rutis 不默认这样做。

**宿主是否开 hardened runtime,由宿主的发布方决定,rutis 不做规定。** rutis 负责两件事:加载器在开与不开两种情况下都能工作;
文档写清楚两种情况的差别。

| | 不开 | 开 |
| --- | --- | --- |
| 绕过启动器直接运行宿主时,DYLD_* | 生效(E7) | 被忽略(E9) |
| 签名要求 | 无 | SDK、libstd、插件及其原生库要与宿主用同一个 Team ID 签名;或者宿主带 `com.apple.security.cs.disable-library-validation`(E10) |
| 公证 | 不能公证 | 公证要求开 |

不论开不开,受支持的入口都是启动器(SDK 设计稿 §5.4)。宿主开了 hardened runtime 时,dyld 会删除它收到的 DYLD_*,
步骤 3 的恢复仍然有效,因为恢复是宿主进程内设置环境变量,与 dyld 无关。rutis 自带的 `rutis-cli` 不开。CI 中加一个开了 hardened runtime 并带
`disable-library-validation` 的宿主变体,确认加载器在这种配置下能完成换代测试。

**链接参数在链接时设定,不做事后修改。** `install_name_tool`、`strip` 不会破坏 linker-signed 的 ad-hoc 签名(E15,Apple 工具会自动重签),
但会改变字节、引入第二套产物,破坏 L2 与可复现构建。所以 install name、rpath 都由 `build.rs` 注入(§3.3)。
发布方用自己的证书重新签名(例如 Developer ID)属于发布流程的最后一步,不影响 SDK 身份:L2 绑定的是 SDK 和插件构建出来时的字节,
重签会改变字节,所以重签只能用于宿主和启动器,SDK 和插件按构建产物原样发布(需要重签 SDK 和插件的发布方,应在 L2 计算之前完成签名)。
CI 加一步 `codesign --verify` 检查发布目录中的每个文件。

### 3.5 V2:两级命名空间的影响

E5 回答了 SDK 设计稿 V2 的主要问题:`RTLD_LOCAL` + 两级命名空间下,同名 crate 两个版本共存,互不串线,SDK 单份。另有三项:

- **全局分配器被拆成两份(E14)。** SDK 设计稿 §4.3 要求分配器只在 SDK 中定义,Linux 上依靠 ELF 符号覆盖让 libstd 的分配也走它。
  macOS 两级命名空间下,libstd 内部引用的 `__rust_alloc` 绑定的是它自己导出的默认实现,不走 SDK 的分配器。现在 SDK 用 `System`,
  和 libstd 的默认实现相同,所以没有问题;一旦换成 mimalloc,libstd 内部分配、宿主或插件释放的内存就会配错分配器,是未定义行为。
  **规则:SDK 的分配器只能是 `System`,所有平台都如此。** 这是上游已知、尚未修复的问题(§八 R1):Linux 上 prefer-dynamic 配 jemalloc
  自 1.71 起同样会崩溃。SDK 中加编译期断言(`#[global_allocator]` 的类型必须是 `std::alloc::System`),测试中保留 E14 的计数实验作为回归。
  SDK 设计稿 §4.3 中“若要换 mimalloc/jemalloc,属于 SDK 变更”改为“上游修复前不允许”。
- **weak 定义合并。** dyld 在已加载镜像间合并 weak 定义,插件 v2 的 weak 符号可能绑定到 v1 的实现。
  打包检查:插件导出表中不得有 **Rust 修饰名**(`_ZN`、`_R` 前缀)的 weak 定义;compiler-rt 的 `___isOSVersionAtLeast`、
  `___isPlatformVersionAtLeast` 等 helper 列入白名单(E12,libstd 自己就带)。出现不在白名单中的 weak 定义时打包失败并列出符号。
- **两版本同 install name。** E5 已验证分别加载,`test-dylib.sh` 的换代测试持续覆盖;不需要给每个版本生成不同的 install name。

### 3.6 SDK 身份、可复现构建与工具链

- **SDK 升级。** 节名(§3.1)、`build.rs` 注入的链接参数(§3.3)都改变 SDK,Linux SDK 哈希会变,Linux 插件随之重编,符合 SDK 设计稿 §10.2 第 4 条。
  P3–P6 已经改过 `export_plugin!`(新增 `rutis_plugin_config_schema`),`rutis-sdk` 仍是 0.4.0。合成一次 minor 升级,
  并同步 `rutis-dylib`、`rutis-cli` 中写死的 `version = "0.4.0"`。
- **macOS 构建信息只做诊断,不进 L1。** 产物字节还受 deployment target(`minos`)、MacOSX SDK 版本、链接器版本影响,三者都记录在产物的
  `LC_BUILD_VERSION` 中(例:`minos 11.0 / sdk 26.5 / ld 1267.0`)。打包工具从产物读出这三项写进 `sdk.toml` 和插件清单的 `[build]`,
  L2 不一致时用于给出原因。不放进 `SDK_ID`:SDK 设计稿 §5.2 规定 L1 只放影响 ABI 的输入;而且 `build.rs` 里运行 `ld -v`
  不一定是 rustc 实际调用的链接器。发布脚本显式固定 `MACOSX_DEPLOYMENT_TARGET`(建议 13.0),CI 用 `xcode-select` 固定 Xcode。
- **跨机器可复现(V1 的 macOS 部分)。** E6 只是同机。`sdk-repro` 矩阵加两个 macos-15 runner,不同源码、target 与 Cargo home 路径,
  按宿主锚点构建(前提二)后比较 SDK 哈希。Xcode 不同导致哈希不同属于预期,由上一条的诊断信息说明。
- **debuginfo。** macOS 的 debug map(OSO stab)记录 `.o` 文件的绝对路径,`--remap-path-prefix` 管不到。现在不出问题是因为 Cargo 默认的
  release 配置是 `debug = 0`、`strip = "debuginfo"`,而仓库没有显式写 `[profile.release]`。显式写上这两项,
  并在可复现测试中断言 SDK 不含 OSO 条目,防止有人打开 line-tables 后悄悄失去可复现性。

### 3.7 打包与测试

**`pack-dylib-plugin.py` 改写为 Rust,放进 `rutis-xtask`**,用 `rutis-dylib-meta`,打包和加载读同一份逻辑。`cargo xtask pack-plugin` 的参数不变。

**脚本可移植性。** `tools/test-dylib*.sh`、`build-dylib-bundle.sh` 中的 GNU 用法:`sed -i`(BSD 需要 `-i ''`)、`find -printf`、`ldd`、
写死的 `.so`、`sha256sum`(macOS 26 自带,早期版本没有)。新增 `tools/lib/dylib-common.sh`,提供 `sha256_of`、`dylib_name`、`std_dylib`
等函数,各脚本改用它;依赖检查交给 xtask。macOS 系统 `/bin/bash` 是 3.2,`set -u` 下展开空数组会报错,脚本要避开这种写法。

**macOS 额外测试**(在 Linux 原有测试之外):

| 测试 | 预期 |
| --- | --- |
| 经启动器启动,调用者环境中有 `DYLD_LIBRARY_PATH`,指向一个带初始化函数(写标记文件)的合法 SDK | 宿主加载的是发布目录中的 SDK,标记文件不存在;宿主的子进程能看到原来的 `DYLD_LIBRARY_PATH` |
| 经启动器启动,调用者环境中有 `DYLD_INSERT_LIBRARIES`,插入库的初始化函数记录所在进程 | 插入库可能在启动器中运行(不防),但不在宿主中运行 |
| 插件依赖写成绝对路径的 SDK(复现 E3) | `dlopen` 前被拒绝,原因为依赖不符 |
| 插件带 LC_RPATH、flat lookup、`@loader_path` 依赖 | 打包失败;手工打包的在加载前被拒绝 |
| 插件动态链接一个系统原生库(如 `/usr/lib/libz.1.dylib`) | 正常加载;清单 `[plugin] native_deps` 中有该库 |
| 二进制的原生库依赖与清单 `[plugin] native_deps` 不一致 | 加载前被拒绝 |
| 宿主开 hardened runtime + `disable-library-validation` | 换代测试通过 |
| 源文件带 quarantine,缓存为空 | 读取和写入缓存之前被拒绝,缓存中不出现该文件 |
| 源文件带 quarantine,缓存中已有同哈希的干净条目 | 被拒绝(源文件检查不因缓存命中而跳过) |
| 源文件干净,缓存中同哈希条目带 quarantine | `dlopen` 前被拒绝,不卡住 |
| fat 插件、x86_64 插件、iOS 模拟器插件 | 加载前被拒绝,原因为格式、架构或平台 |
| SDK 计数分配器(E14) | 宿主与插件的分配经过 SDK;断言 SDK 分配器类型为 `System` |
| 发布目录 `codesign --verify` | 全部通过 |

**CI。** 新增 `dylib-macos` 任务(macos-15,arm64),运行与 `dylib-linux` 相同的三个脚本和 `loader_host` 示例;
`sdk-repro` 矩阵加入两个 macos-15 runner。`static-platforms` 中的 macOS 条目保留,继续检查默认静态构建。

### 3.8 其他开发者签名的插件

SDK 设计稿面向一方插件。插件也可以来自其他团队或公司,用他们自己的 Apple 开发者账号签名。这在技术上可行,条件如下。

**签名。** 是否能加载,取决于宿主的签名方式:

| 宿主 | 其他开发者签名的插件 |
| --- | --- |
| 不开 hardened runtime | 能加载;ad-hoc 签名的也能加载 |
| 开 hardened runtime,带 `disable-library-validation` | 能加载;宿主仍然可以公证 |
| 开 hardened runtime,不带该权限 | 不能加载:系统只允许与宿主同一 Team ID 签名的库 |

从网上下载的插件带 quarantine 属性,必须由插件作者公证,或由用户手动去掉该属性(§3.2)。

**可选:按 Team ID 限制插件来源。** 宿主可以配置一个允许的 Team ID 列表。配置后,加载器在 `dlopen` 之前,对缓存中实际要打开的文件
用 Security 框架(`SecStaticCodeCreateWithPath` + `SecStaticCodeCheckValidity`)校验签名有效,且签名证书的 Team ID 在列表中;
不满足即拒绝,ad-hoc 签名的插件也被拒绝。未配置时不检查,行为与现在相同。这一检查只在 macOS 上提供,与 L1/L2 互相独立:
L1/L2 确认插件与 SDK 兼容,Team ID 确认插件是谁签的。插件的原生库(§3.3)不在检查范围内。

**构建条件是更大的障碍。** 插件必须链接与宿主完全相同的 SDK 产物:同一个 rustc、同一份锁文件、同一套 feature。
现在的做法是插件打包时把宿主的包一起纳入同一次 Cargo 构建(SDK 设计稿 §5.3),外部开发者因此需要拿到宿主的构建锚点和锁文件,
并且每次 SDK 升级都要重新编译。要让外部开发者方便地构建插件,需要发布一份“SDK 构建包”(锁文件 + 构建锚点 + 构建参数),
并证明不同的 Cargo 依赖图能构建出相同的 SDK 字节(SDK 设计稿 §5.3 记录的未证明项)。见 [#108](https://github.com/arcships/rutis/issues/108)。

**信任。** dylib 插件和宿主在同一个进程中运行,没有隔离,插件的错误会直接导致宿主崩溃,也能访问宿主的全部内存。
签名只能说明插件是谁做的,不能说明它安全。不受控的代码应走协议插件(SDK 设计稿 §一 非目标)。

### 3.9 验收映射

| #102 验收项 | 对应 |
| --- | --- |
| 两个版本换代,消费者重载 | `test-dylib.sh`(§3.7) |
| 身份不符在初始化代码前被拒绝 | §3.1 + bad-boot 测试;另加依赖不符(§3.3) |
| 宿主、SDK、libstd 被改动时拒绝启动 | `test-dylib-launcher.sh`;另加 DYLD_* 测试(§3.4) |
| SDK 字节可复现 | 修正后的 `test-dylib-repro.sh`(前提二)+ CI 跨 runner 比对(§3.6) |
| `DylibResolver` 的 `loader_host` 示例 | 依赖前提一 |

## 四、Windows 可行性验证方案

按顺序做,前一项不可行就停,把结论写进 SDK 设计稿 §十一。实验放 `docs/probes/windows-dylib/`,
在 `probe/windows-dylib` 分支上用 push 触发的 CI 任务在 windows-2025 上运行
(`workflow_dispatch` 只能触发默认分支上已有的 workflow,不适合临时任务)。只考虑 `x86_64-pc-windows-msvc`。
W1 不依赖本文其他改动,可以立即开始。

| # | 验证项 | 方法 | 不可行的判定 |
| --- | --- | --- | --- |
| W1 | **导出符号数量** | 用当前 `rutis-sdk` 按宿主锚点构建 `rutis_sdk.dll`,用 `object` 读 PE 导出目录计数;同时看链接是否报 LNK1189(导入库对象数超限)。release(opt-level 3)和 dev(opt-level 0)各测一次:dylib 会导出泛型单态化,而 share-generics 在 opt-level 0/1 默认开启,dev 构建的导出数会大得多(§八 R7)。再加 `tokio/full`、`serde` derive 等常见依赖各构建一次,估算增长速度 | 当前已超限,或余量不够一次常规依赖升级。工具链固定为 stable,不能用 `-Z` 参数;release 下 share-generics 本来就关闭,也没有降低导出数的余地 |
| W2 | **同名 DLL 多版本** | `LoadLibraryExW` 以完整路径加载 `<cache>/<hashA>/greeter.dll` 和 `<cache>/<hashB>/greeter.dll` | 第二次返回第一次的模块,且无法通过路径或缓存文件名规避 |
| W3 | **依赖解析** | 宿主对 SDK/std 的静态导入按标准搜索顺序(应用目录优先)解析;插件以 `LOAD_LIBRARY_SEARCH_APPLICATION_DIR \| LOAD_LIBRARY_SEARCH_SYSTEM32` 加载(不用 `DLL_LOAD_DIR`,它会搜索插件所在的缓存目录,与 §3.3 拒绝 `@loader_path` 同理),其 SDK 依赖应复用已加载模块。分别在工作目录、`PATH`、`.local` 重定向目录中放同名 DLL 测试 | 存在无法关闭的路径,使宿主或插件解析到发布目录外的 SDK/std。注意 `SetDefaultDllDirectories` 只影响之后的 `LoadLibrary`,管不到宿主自身的静态导入 |
| W4 | **std DLL 与 VC 运行时** | `std-*.dll` 放进发布目录并由启动器校验。预编译的 `std-*.dll` 本身动态依赖 `vcruntime140.dll` 等 VC 运行时,无论宿主是否 `+crt-static`,运行时都需要它 | 不算不可行。VC++ 运行库由用户自行安装,不随包分发,启动器也不检查,只在文档中说明 |
| W5 | **加载锁** | 插件初始化(`rutis_plugin_entry`)在 `LoadLibrary` 返回后调用,不在加载锁内。要验证的是:Rust std 的 TLS 回调、插件私有依赖中的 `.CRT$XCU` 静态初始化(如 `ctor`、`inventory`)在加载锁下运行时会不会死锁 | 常见依赖在加载锁下死锁且无法用规则禁止 |
| W6 | **引导 blob** | `#[link_section = ".rutism"]` + `#[used]` 在 MSVC 链接器 `/OPT:REF` 下是否保留;`object` 能否定位 | 无法保留且没有替代(例如导出一个数据符号,从导出表定位) |
| W7 | **文件占用** | 已加载 DLL 无法覆盖或删除;确认内容寻址缓存只新建、不覆盖,损坏条目被占用时给出可读错误 | 不预期不可行 |
| W8 | **跨 DLL 运行期行为** | 把 greeter 夹具及 TypeId、downcast、`tokio::spawn` 使用宿主运行时、`thread_local!` 单份、`catch_unwind`、Drop 的测试在 windows-2025 上原样运行。MSVC 不支持跨 DLL 以 dllimport 导入 TLS 变量;rustc 自 1.70 起对 dylib 的跨 crate TLS 访问改走 shim 函数(§八 R8),理论上可行,但没有找到 Rust dylib + tokio 在 Windows 上的实测报告 | 任何一项失败 |

**issue 原文没有提到、但实现时必须处理的差异**(写进验证记录,作为实现 issue 的范围):

- **启动器不能 `exec`。** Windows 没有替换当前进程的调用,启动器只能创建子进程并等待:转发退出码,忽略自身的 Ctrl+C(子进程在同一控制台会收到),
  用 Job Object 保证启动器退出时子进程一起结束。进程 ID 与 Linux/macOS 不同,依赖宿主 PID 的工具要知道这一点。
- **Windows 可以比 unix 做得更严。** 启动器校验后以不允许写入、删除的共享模式持有宿主、SDK、std 的文件句柄,直到子进程退出,
  在运行期间真正保证发布目录不可变,而不只是约定。
- **库文件名没有 `lib` 前缀**,导入库(`.dll.lib`)不进发布目录。

## 五、分期

| PR | 内容 | 依赖 |
| --- | --- | --- |
| 0a | [#104](https://github.com/arcships/rutis/pull/104):P3–P6 合进 main(已合入) | — |
| 0b | 修正 SDK 可复现测试:按宿主锚点构建(前提二) | — |
| A1 | `rutis-dylib-meta`(`object`);Linux 改用它读引导 blob;打包工具改写为 Rust;脚本公共函数。Linux 行为与 SDK 字节都不变 | 0a |
| A2 | 链接参数改由 `build.rs` 按产物注入;依赖检查(§3.3);`export_plugin!` 节名按格式选择;显式 `[profile.release]`;SDK minor 升级 | A1、0b |
| B | macOS:`unix` 模块与平台 cfg、SDK install name、启动器清除与恢复 DYLD_*、quarantine、可选的 Team ID 检查、分配器断言、构建诊断、`dylib-macos` CI 与 sdk-repro | A2 |
| C | Windows 验证 W1–W8 与 SDK 设计稿 §十一 记录。W1–W5、W7 不依赖其他 PR,立即开始;W6、W8 用 A2 之后的节名和夹具 | W6/W8 依赖 A2 |

A1 是纯重构;A2 改变 SDK 字节,集中做一次升级;B 和 C 互不依赖。

实现进度(2026-10-03):0b [#113](https://github.com/arcships/rutis/pull/113)、A1 [#114](https://github.com/arcships/rutis/pull/114)、
A2 [#115](https://github.com/arcships/rutis/pull/115)、B [#116](https://github.com/arcships/rutis/pull/116)、
C [#119](https://github.com/arcships/rutis/pull/119)(Windows 可行,实现见 [#118](https://github.com/arcships/rutis/issues/118))已开 PR。
与本文的差别:清单字段实现为 `[plugin]` 下的 `native_deps` 数组;Team ID 检查的接口是 `Loader::require_team_ids`;
示例宿主在 macOS 上带 run path(指向 target 目录与工具链 libstd),以便测试开了 hardened runtime 的宿主;
示例插件的初始化标记在 macOS 上放进 `__DATA,__mod_init_func`。

## 六、已决定事项(2026-10-03)

1. **hardened runtime**:宿主和启动器是否开,都由发布方决定,rutis 不做规定;加载器两种情况都支持,`rutis-cli` 不开(§3.4)。
   启动器只负责不把 DYLD_* 传给宿主;注入启动器本身不在防护范围内。
2. **插件依赖原生库**:允许。限制在依赖的写法上(§3.3)。
3. **Windows 的 VC++ 运行库**:用户自行安装,rutis 不分发、不检查(W4)。

## 七、评审记录(2026-10-03)

独立评审在同一台 macOS arm64 上补做了 E13–E16 等实验。未发现否定整体方向的问题;以下意见已全部并入正文。

| 级别 | 意见 | 处理 |
| --- | --- | --- |
| P1 | 启动器自身会被 `DYLD_INSERT_LIBRARIES` 注入,在其 main 之前执行,原测试预期无法达成 | 事实成立。按 SDK 设计稿 §5.4 的防护范围,注入启动器本身属于“控制进程加载器”,定为不防;启动器只保证 DYLD_* 不传给宿主,并像 Linux 一样由宿主恢复;测试预期改为“不在宿主中运行”(§3.4)。起初的处理是要求启动器开 hardened runtime,后经讨论撤回 |
| P1 | macOS 上 libstd 内部分配不经过 SDK 分配器 | SDK 分配器限定为 `System`,加断言和回归测试(§3.5、E14) |
| P1 | rpath 写在 RUSTFLAGS 中作用于所有产物,“插件无 rpath”与现有构建冲突;按产物注入的机制没写 | 链接参数改由各 crate `build.rs` 注入;放进改变 SDK 字节的 A2(§3.3、§五) |
| P1 | 现有可复现测试构建的是静态 std 的 SDK 变体,不是发布产物 | 列为前提二和 PR 0b(§一) |
| P1 | Windows 验证缺少跨 DLL 运行期行为(TLS、tokio 上下文) | 新增 W8 |
| P2 | `install_name_tool`/`strip` 会破坏签名的说法错误 | 改正,理由改为可复现与单一产物(§3.4、E15) |
| P2 | quarantine 检查应针对实际打开的缓存文件;缓存写入本来就不继承扩展属性 | 改正(§3.2) |
| P2 | 依赖检查不完整,应为白名单、未知项拒绝 | 补全 Mach-O/ELF 规则,libstd 精确匹配(§3.3);原生库后来按 §六 第 2 条改为允许 |
| P2 | 一律禁止 weak 定义会误伤 compiler-rt helper | 只禁止 Rust 修饰名,helper 白名单(§3.5) |
| P2 | L1 的 macOS 输入选错 | 改为从 `LC_BUILD_VERSION` 读出、只做诊断(§3.6) |
| P2 | `rutis-dylib-meta` 单独成 crate 的理由不成立 | 理由改为 xtask 不链接 SDK;补充常量一致性测试(§3.1) |
| P2 | build.rs 的自定义 cfg 不传给下游 | 改用 `cfg(any(linux, macos))`(§3.2) |
| P2 | Windows 部分的事实错误(crt-static、`-Z` 参数、`DLL_LOAD_DIR`、`workflow_dispatch`) | 逐条改正(§四) |
| P2 | PR 顺序:W1 可立即开始;PR A 太大 | 拆为 A1/A2,C 不等 A(§五) |
| P2 | E7 中经 `/usr/bin/env` 的说法需要区分 | 新增 E16 |
| P3 | Linux 并无 E3 类漏洞;补 `e_machine`/平台检查;SDK 版本合并升级;显式 `[profile.release]`;示例宿主的 rpath;bash 3.2 | 均已并入 |

## 八、外部调研(2026-10-03)

针对本文发现的约束查了上游 issue、Apple/Microsoft 文档和同类项目。结论:没有一项有比本文更好的现成解法;
分配器的限制需要扩大到所有平台;其余几项的做法得到印证,并补充了几处检查。

| # | 约束 | 调研结论 | 对本文的影响 |
| --- | --- | --- | --- |
| R1 | 分配器被拆分(E14) | 上游已知且未修复:[rust-lang/rust#100781](https://github.com/rust-lang/rust/issues/100781)(`global_allocator` 与 `-C prefer-dynamic` 不兼容;Mach-O 两级命名空间和 Windows 上 libstd 用 System);[#114518](https://github.com/rust-lang/rust/issues/114518)(自 1.71 起 prefer-dynamic + jemalloc 在 macOS **和 Linux** 上段错误)。用弱符号或函数指针替代 shim 的提议([#134522](https://github.com/rust-lang/rust/pull/134522))未合并。Bevy 用户在 mimalloc + `dynamic_linking` 下遇到同样的崩溃 | 分配器限定为 `System`,推广到所有平台(§3.5) |
| R1a | 让 SDK 静态包含 std,进程中只有一份分配器 shim | 调研建议的方案。**本机验证不可行**:Cargo 对作为依赖的 dylib 强制传 `-C prefer-dynamic`;绕过 Cargo 直接用 rustc 构建出静态包含 std 的 SDK 后,宿主和插件都无法链接它(`cannot satisfy dependencies so 'core' only shows up once`) | 排除 |
| R1b | `-flat_namespace`、`__DATA,__interpose` | 都只影响跨镜像的导入;libstd 调用自己导出的 `__rust_alloc` 很可能是镜像内调用,改不了。flat namespace 还会引入全局符号冲突,破坏多版本共存 | 排除 |
| R2 | install name 默认是绝对路径(E2) | 上游没有改默认值([#28640](https://github.com/rust-lang/rust/issues/28640) 仍开放)。`-C rpath` 会顺带设 `@rpath/<文件名>`,但同时给所有产物写入按构建目录计算的 LC_RPATH。Cargo 的 `rustc-link-arg-*` 没有 dylib 变体,只能用作用于整个包的 `rustc-link-arg` | 保持 §3.3 的 `build.rs` 方案 |
| R3 | DYLD_* 注入(E7、E13) | Apple DTS 确认 hardened runtime 进程忽略并清除 DYLD_*,例外是 `allow-dyld-environment-variables` 和 `get-task-allow` 两个 entitlement。没有找到 ad-hoc + runtime 的专门说明 | 只作为发布方可选的加固手段写进 §3.4;选用时启动器不得带 `get-task-allow`、`allow-dyld-environment-variables` |
| R4 | quarantine(E11) | Apple 文档:10.15 起,带 quarantine 的插件只有经过公证才能加载,否则需要用户在系统设置中批准(无界面时表现为卡住)。单个 dylib 无法 staple 公证票据。音频插件宿主普遍让用户执行 `xattr -d` 或发布已公证的插件 | 保持“加载前拒绝 + 给出 `xattr -d` 命令”(§3.2) |
| R5 | 同 install name、不同路径(E5) | Apple 说明 dyld 先按路径定位文件,再按文件查已加载表;以完整路径 `dlopen` 时两份文件是两个镜像。风险在插件的 `@rpath/…` 依赖:dyld 会先复用已加载的同名镜像 | §3.3 已把插件依赖限定为已由宿主加载的 SDK 和 libstd,这一复用正是需要的行为 |
| R6 | 永不 dlclose | macOS 上用过 TLS 的镜像(Rust 的 `print!` 就会用),dyld 本来就忽略 `dlclose`;abi_stable 也明确不支持卸载 | 印证 SDK 设计稿 §九 |
| R7 | Windows 导出上限(W1) | 问题真实存在:Bevy [#1110](https://github.com/bevyengine/bevy/issues/1110)(2020 至今未关)、[#14930](https://github.com/bevyengine/bevy/issues/14930)。原因之一是 dylib 仍导出泛型单态化,share-generics 在 opt-level 0/1 默认开启。Bevy 要求 Windows 上动态链接时依赖开 `opt-level=3`。stable 上没有其他手段:`-Zshare-generics=n` 和 `#[export_visibility]`([#151425](https://github.com/rust-lang/rust/issues/151425))都是 unstable | W1 实测([#119](https://github.com/arcships/rutis/pull/119)):release 1597 项、dev 14545 项。只给 `rutis-sdk` 包设 opt-level 2 没有作用(dev 仍为 14543),要给全部依赖设才降到 2609。所以如需压低 dev 构建的导出数,应写 `[profile.dev.package."*"] opt-level = 2` |
| R8 | Windows 跨 DLL TLS(W8) | rustc 1.70 起([#108089](https://github.com/rust-lang/rust/pull/108089))msvc 目标对 dylib 的跨 crate TLS 访问改走 shim 函数;1.98 把 TLS 析构改为 FLS 实现。tokio 只在 SDK 中有一份时,上下文应是单份 | W8 风险下调,但仍需实测 |
| R9 | 同名 DLL 两个版本(W2) | Microsoft 文档:传完整路径时只在该路径查找;依赖 DLL 按模块名解析,并优先复用已加载的同名模块 | W2 预期可行;插件目录中不得再放 SDK 副本 |
| R10 | VC 运行时(W4) | 调研称 rustup 预编译的 std 依赖 vcruntime140.dll。**W4 实测推翻**:`std-*.dll` 不依赖 VC 运行时;依赖它的是 SDK、宿主和插件(`VCRUNTIME140.dll` 与 UCRT) | 不开 `crt-static`;VC++ 运行库由用户安装(§六 第 3 条) |

## 九、PR 评审记录(#105,2026-10-03)

| 级别 | 意见 | 处理 |
| --- | --- | --- |
| P2 | quarantine 只检查复制后的缓存文件可被绕过:缓存按字节写入,不保留扩展属性,下载插件第一次加载时属性丢失 | 源文件在读取、写入或复用缓存之前检查,不论缓存是否命中;缓存文件在 `dlopen` 前再检查;增加三种验收(§3.2、§3.7) |

