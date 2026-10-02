# rutis-loader：插件管理层（设计稿）

状态：设计稿，未实现。日期：2026-10-02。
对照对象：dsh vendored 的 `@deepseek-ai/cordis-plugin-loader` 1.0.5（`src/config/{entry,tree,group}.ts`、`src/index.ts`）。

## 一、要解决什么

rutis 内核能管**一个**插件：装、卸、重启、改配置（`Ctx::plugin_with` + `FiberView`）。
但使用 rutis 的项目要的是管**一堆**插件：

- 按名字装插件，不用在代码里写死类型；
- 每个插件有稳定 id，能随时增、删、改配置、启用/禁用、分组；
- 改完存回配置文件，下次启动照着恢复；
- 能列出"现在装了什么、各自什么状态"，给 UI / CLI / dev 通道用。

cordis 里这是 `cordis-plugin-loader` 干的事，rutis 目前没有对应物。本文设计一个独立 crate `rutis-loader` 补上。

不在本文范围：包安装、bundle、profile（dsh-plugin-manager 那一层），它们建在 loader 之上，属于应用。

## 二、结论先说

1. **新 crate，内核零改动**。第一版只用现有公开 API：`plugin_with`（工厂装载）、`FiberView::update`（dry-run 改配置）、`watch`、`dispose`。
2. **配置统一是 JSON**（`serde_json::Value`），与 rutis-sdk 的 `ConfigValue` 一致。
3. **"按名字找插件"抽成 `Resolver` trait**：内置表、dylib、interop 各一个实现。
4. **分组 = 一个插件**，它的子插件挂在它的 ctx 下。禁用分组，内核的级联卸载自动带走子插件，loader 不用自己递归。
5. **先校验后落盘**：配置改动 dry-run 失败时，不存、不重启、旧版本继续跑。（cordis 是先写文件再校验，这里刻意更严。）
6. **配置 schema 第一阶段就做**，挂在解析结果上，不改内核（§九）。设置页的配置表单靠它。
7. **配置里的 isolate / inject 要支持**，做法是加一张"服务名 → `TypeKey`"目录（§十）。dsh 的 agent preset 就是靠 isolate 隔离挂载的。
8. **include + patch 分层要支持**（§十一）。dsh 的配置分层（默认层、bundle 层、用户层、`--patch`）整个建在它上面。
9. **插件卸载自己是已知缺口**，不是刻意不做（§十二）。

## 三、概念

```
Loader（服务，挂在 root 下的一个插件）
 └─ 根分组
     ├─ Entry "llm"      name = "@rutis/dsh-aimux"    config = {...}
     ├─ Entry "tools"    name = "rutis-tools"         disabled = true
     └─ Entry "agents"   group = true
         ├─ Entry "a1"   name = "dylib:agent-x"
         └─ Entry "a2"   ...
```

```rust
/// 存进配置文件的一行（与 cordis EntryOptions 同形；不含 intercept，见 §十三）。
#[derive(Serialize, Deserialize, Clone)]
pub struct EntryOptions {
    pub id: String,            // 同一个配置文件内唯一；省略时 create 自动生成
    pub name: String,          // 交给 Resolver 的模块名，同时是行的身份（§十七-1）
    #[serde(default)]
    pub config: Value,         // 分组时是子 EntryOptions 数组（同 cordis）
    #[serde(default)]
    pub group: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub inject: Option<Vec<String>>,                  // P2，见 §十
    #[serde(default)]
    pub isolate: Option<BTreeMap<String, Isolate>>,   // P2，见 §十
}

/// `true` = 本 entry 私有作用域；字符串 = 同名 label 共享（cordis LocalRealm / GlobalRealm）。
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Isolate { Private(bool), Shared(String) }
```

id 规则与 cordis 相同：同一个配置文件内唯一；include 进来的子树加前缀 `<include 的 id>:`（§十一）。P1 没有 include，所以 id 实际上全局唯一。

## 四、Resolver：名字 → 插件工厂

```rust
pub trait Resolver: Send + Sync + 'static {
    /// 解析模块名。可以慢（读文件、dlopen），所以是 async。
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoadError>>;
}

pub struct Resolved {
    pub factory: Arc<dyn PluginFactory<Value>>,
    /// 配置的 JSON Schema（§九）。拿不到时为 None，loader 照常工作，只是没法生成表单。
    pub schema: Option<Value>,
    /// 诊断用：版本、来源路径、哈希等。
    pub meta: Value,
}
```

内置实现：

| 实现 | 匹配哪些名字 | 说明 |
|---|---|---|
| `Builtins` | 注册过的任意名字（精确匹配，优先） | 编译进宿主的插件表。`register::<C: DeserializeOwned + JsonSchema>(name, factory)` 自动把 JSON 反序列化成 `C`（失败转 `CordisError::Validation`），并用 schemars 生成 schema。`Typed<P>`、普通 `Plugin` 都能注册 |
| `DylibResolver` | `dylib:` 前缀 | 包一层 rutis-dylib 的 `Loader::load`。**只有 Linux**（rutis-dylib 现状），macOS 要等 dylib 支持 |
| `InteropResolver` | 其余的 npm 包名 | 后续阶段，见 §十四 |
| `Chain` | — | 按上面的顺序依次尝试（§十七-1） |

为什么返回工厂而不是插件实例：改配置要走 `FiberView::update`，而它只对工厂装载的 fiber 生效。

## 五、装载方式：配置里带上"解析结果"

每个 entry 用同一个 loader 内部工厂装载，配置类型是：

```rust
struct EntryConfig {
    resolved: Arc<Resolved>,   // 这一代用哪个模块
    value: Value,              // 用户配置
}
```

这是照搬 rutis-dylib `DylibConfig { module, value }` 的做法。好处是**换模块版本也能走 `update`**：

- 只改 `config` → `update(EntryConfig { 同一个 resolved, 新 value })`；
- 换 `name` 或重新解析（dylib 升级）→ `update(EntryConfig { 新 resolved, value })`。

两种情况都继承 `update` 的全部保证：dry-run 不过就不动，PluginId 不变，下游依赖照常驱逐重载。

**例外**：依赖声明（`injects`）在 spawn 时就固定了（内核 D32f）。新模块的 `injects` 和旧的不一样时，没法原地 update，只能 dispose 旧 fiber 再 spawn 新的，PluginId 也会变。loader 自动判断走哪条路。

## 六、分组

分组 entry 装载一个内部插件 `GroupPlugin`，它的 apply：

1. 向 loader 登记"这个分组当前的 ctx"；
2. 对每个未禁用的子 entry，在这个 ctx 下 `plugin_with` 装载，并把 `FiberView` 回填给 loader；
3. 返回的清理函数注销登记。

于是：

- 禁用或删除分组 → 分组 fiber 卸载 → 内核级联卸载所有子插件（D28 所有权），loader 只需把子 entry 的 view 清空；
- 分组重启 → apply 重跑，子插件按当前列表重建；
- 移动 entry 到别的分组 = 在旧分组下 dispose，在新分组下 spawn（PluginId 会变，与 cordis 一致）。

**锁规则**（防死锁）：loader 的状态放在同步 `Mutex` 里，绝不跨 `await` 持有；所有写操作串行化在一把 async 操作锁上。`GroupPlugin::apply` 只碰状态锁，**不拿**操作锁。否则"操作等分组装载完 → 分组装载等操作锁"就成环了。

## 七、对外 API（控制面）

`LoaderPlugin::new(resolver, store)` 挂到 root。它提供 `Arc<Loader>` 服务，其它插件（如管理 UI）可以依赖它；宿主也能在挂载前直接拿到句柄。

```rust
impl Loader {
    // 查
    fn entries(&self) -> Vec<EntryInfo>;                 // 按树顺序
    fn get(&self, id: &str) -> Option<EntryInfo>;
    fn locate(&self, plugin: PluginId) -> Option<String>;// 某个 fiber 属于哪个 entry（沿 diagnostics 的 parent 往上找）
    async fn schema_of(&self, name: &str) -> Result<Option<Value>, LoaderError>; // 只解析不装载，供"新建前先填表单"

    // 改（都会落盘；dry-run 失败返回 Err，什么都不变）
    async fn create(&self, opts: NewEntry, parent: Option<&str>, position: Option<usize>) -> Result<String, LoaderError>;
    async fn update(&self, id: &str, config: Value) -> Result<(), LoaderError>;
    async fn rename_module(&self, id: &str, name: &str) -> Result<(), LoaderError>; // 换 name，见 §五
    async fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), LoaderError>;
    async fn move_to(&self, id: &str, parent: Option<&str>, position: Option<usize>) -> Result<(), LoaderError>;
    async fn remove(&self, id: &str) -> Result<(), LoaderError>;
    async fn reload(&self, id: &str) -> Result<(), LoaderError>; // 重新 resolve（dylib 升级）
    async fn restart(&self, id: &str) -> Result<(), LoaderError>;

    // 等
    async fn settled(&self);   // 所有 resolve 和 fiber 转换都落地（对应 cordis tree.await()）
}

pub struct EntryInfo {
    pub options: EntryOptions,
    pub parent: Option<String>,
    pub status: EntryStatus,   // Disabled / Resolving / Unresolved(err) / Running(Snapshot)
    pub plugin: Option<PluginId>,
    pub view: Option<FiberView>,
    pub schema: Option<Value>, // Resolved::schema
    pub meta: Value,           // Resolved::meta
}
```

变更通知：每次操作完成后在 bus 上发 `LoaderChanged { id, kind }`（Created / Updated / Removed / Moved / Disabled / Enabled）。fiber 自身的状态变化仍看内核的 `FiberStatusChanged`，用 `EntryInfo::plugin` 对上号。

### 失败语义

| 情况 | 行为 |
|---|---|
| resolve 失败 | entry 保留，状态 `Unresolved(err)`，无 fiber。`reload` 可重试。（cordis 只打日志） |
| 首次装载 apply 失败 | 与内核一致：fiber `Failed`，entry 保留 |
| `update` dry-run 失败 | 返回 Err，**不落盘**，旧配置继续跑 |
| 落盘失败 | 返回 Err；内存和运行态已改。下一次成功写入时一并写出 |
| 宿主关闭 | 所有操作返回 `Closed`，不再落盘 |

## 八、持久化

```rust
pub trait Store: Send + Sync + 'static {
    fn load(&self) -> Result<Vec<EntryOptions>, LoaderError>;
    fn save(&self, entries: &[EntryOptions]) -> Result<(), LoaderError>;
}
```

自带两个实现：`MemoryStore`（不落盘，测试用）和 `JsonFileStore`（写临时文件后 rename，原子替换）。YAML 需要时再加。

启动流程：`load()` → 按顺序 create（不触发 save）→ `settled()`。

## 九、配置 schema（P1）

**为什么 P1 就要**：dsh 的 Models / 设置页按 schema 生成配置表单；volatile 字段（改了不重启）也要靠 schema 标记。

**放在哪**：放在 `Resolved::schema` 上，由 Resolver 提供，**不改内核**。`PluginFactory` 不加方法，不用 loader 的项目不受影响。

| 来源 | 怎么拿 | 阶段 |
|---|---|---|
| Builtins | `C: JsonSchema` 时用 schemars 自动生成；注册时也可以手写 schema 传入 | P1 |
| dylib | rutis-sdk 的 `PluginMeta` 加一个可选的 schema 字段。这会改 SDK 的 ABI，要升 SDK 版本 | P3 |
| interop（JS 插件） | node 侧把 schemastery 转成 JSON Schema 传过来。dsh 已经有 `--dump-config-schema`，转换现成 | P6 |

对外：`EntryInfo::schema`，以及 `Loader::schema_of(name)`（新建之前先拿表单）。

loader 自己**不**拿 schema 做校验，校验仍然以插件的 `validate_config` 为准。schema 只用于展示和比对。

## 十、配置里的 isolate / inject：服务名目录（P2）

**问题**：cordis 配置里写的是字符串，比如 `isolate: { llm: true }`、`inject: [llm]`；rutis 的服务按 `TypeKey`（类型）区分。中间缺一张"服务名 → `TypeKey`"的表。

**服务名目录 `ServiceCatalog`**：名字到 `TypeKey` 的映射，由几方登记：

- Builtins 注册时声明：`builtins.service::<dyn Llm>("llm")`；
- dylib：rutis-sdk 元数据里声明服务名（跟 §九 的 schema 同一次 SDK 改动）；
- interop：生成绑定时已经知道服务名（`Bindings::provide("systemPrompt")`），顺带登记。

**配置写法**（与 cordis 一致）：

```yaml
- id: agent-a
  name: rutis-agent
  isolate:
    llm: true          # 本 entry 私有的作用域（label = "entry:<id>"）
    tools: shared-x    # 同名 label 的 entry 共享一个作用域
  inject: [llm]        # 额外的门控依赖，llm 就绪才启动
```

**实现**：

- isolate：spawn 前在父 ctx 上依次调 `ctx.isolate(key, label)`，再在得到的 ctx 上 `plugin_with`。这正好就是内核现有的 isolate 语义（同 label 合并）。
- inject：把对应的 `TypeKey` 追加到这个 entry 工厂的 `injects` 里。
- 改 isolate 或 inject → 重建 fiber（PluginId 变）。ctx 和 injects 都是 spawn 时定下来的。cordis 改这两项也会重挂。
- 名字查不到 → entry 进 `Unresolved`，错误里列出不认识的服务名。

**老插件代码怎么办**：

- **Rust 插件**：不受影响。代码里的 `injects()` / `Deps` / `ctx.isolate` 照常工作；配置里的 isolate / inject 是叠加在外面的。
- **JS 插件（经 interop）**：它们跑在 node 里真正的 cordis 中，那里的 isolate / inject 照常生效。在 P6 之前，像 `dsh-agent-preset-registry` 这样靠 isolate 的子树，整体放在一个 interop 挂载里，由 node 侧的 cordis loader 管。等 rutis-loader 要逐个管理 JS 插件时，再由 `InteropResolver` 把 isolate 转发给 node 侧（P6）。

intercept 不做：rutis 的 `ServiceIntercept` 是拦截服务读写，跟 cordis "按服务合并配置"的 intercept 不是一回事。dsh 插件源码里也没搜到配置层用它。

## 十一、include 与 patch 分层（P2）

**为什么要**：dsh 的 profile 是分层的：默认配置，加上 bundle 层（如本仓库的 `crates/rutis-dsh/dsh/aimux/aimux.patch.yml`），再加用户层、`--patch` 叠加层。每一层都是一个 patch 列表，由 `cordis-plugin-include` 应用。rutis 要接管 dsh 的插件管理，这个功能绕不过去。

**include 是一个内置的分组插件**：`name: "include"`，配置为 `{ path, initial?, patches? }`：

- 子 entry 列表 = 读文件，再按顺序应用 patches；
- 子 entry 的 id 加前缀 `<include id>:`；
- 每个 include 有自己的 `Store`（文件），根 loader 也是一个 Store。

**patch 语义**（与 cordis `applyEntryPatches` 一致，做成纯函数，离线工具也能复用，保证"dump 出来的"和"实际启动的"一致）：

- `{ id, ...字段 }`：覆盖该行的字段；`name` 写了但不匹配 → 跳过并警告；
- `{ insert: [...], id? }`：插入新行（带 id 时插进那个分组）；后面的 patch 可以改前面插入的行；
- 找不到目标 → 警告并跳过，不报错；
- 不修改原始数据：patch 改了或删了以后重新应用，可以干净地回退。

**文件格式**：dsh 用 YAML，P2 加 YAML Store。`!!js` 表达式见 §十一之二。

**热重读**：`Loader::refresh(include_id)` 重新读文件。文件读不了或解析失败时，打警告、保留上一份好的树（与 cordis 一致），绝不因为改坏配置把进程带崩。监听文件变化是宿主的事，不放在 loader 里。

**dsh 的分层规则**（来源：dsh-app-boot 的 `readProfilePatches` / `loadProfileDirectory` / `applyEntryPatches`、dsh 的 `profile-boot`）：

基础文件 `<profile>/cordis.yml` 内容就是 `[]`（文件头注释写着"改 cordis.patch.yml，别改这里"）。**所有行都来自 patch**。下面这些层按顺序**拼成一个 patch 列表**，作为根 include（id `include`）的 `patches` 一次性应用：

| 顺序 | 层 | 来源 | 文件不存在 / 解析失败 |
|---|---|---|---|
| 1 | bundle 层 | profile 的 `package.json` 里 `dsh.profile.bundles` 的顺序；每个 bundle 包的 `dsh.bundle.patch`（一个文件或文件列表，按列表顺序） | 整个 bundle **跳过**并记下原因（包找不到、没声明 `dsh.bundle`、版本不兼容） |
| 2 | 用户层（**编辑层**） | `<profile>/cordis.patch.yml` | 不存在 = 空层；解析失败 = 启动报错 |
| 3 | home 层 | `~/.dsh/cordis.patch.yml` | 同上 |
| 4 | 命令行层 | 每个 `--patch <file>`，按参数顺序 | 不存在或解析失败都报错（用户点名要的文件） |
| 5 | 遥测开关 | `DSH_TELEMETRY_DISABLED` 非空，并且组合结果里有 `session-telemetry-otel` 这一行 → 追加 `{ id: session-telemetry-otel, disabled: true }` | — |

**patch 应用细节**（必须逐条复刻）：

- 每个 patch 文件必须是 YAML 顶层数组，每一项都是映射，否则整个文件报错；单个 patch 找不到目标只警告。
- `insert` 里的行，`name` 是相对路径（`./`、`../`）或绝对路径时，改写成相对**该 patch 文件**的 `file://` URL。
- 覆盖 patch 是**整字段替换**：`config` 整个换掉，不做合并。dsh-base 的注释专门强调了这一点。
- id 索引只在开头建一次，之后只给 `insert` 进来的行补索引。所以如果某个 patch 用整字段替换了一个分组的 `config`（换进新的子行），后面的 patch **看不到**这些新子行。这是现有行为的一个怪癖，对拍时要复刻，不要"修好"。
- 输入不被修改（先深拷贝），所以去掉一层后重新组合能干净回退。

**热重载与回滚**：

- `dsh-hmr` 监视这些文件，变化后重新组合，调用 `reconcileProfilePatches`：用新 patch 列表 update 根 include，等待树稳定；如果**新出现**了起不来的行，就报错（已经坏着的行不算）。
- dsh-config-editor 写用户层时持有 profile 的文件锁，用保留注释的方式改 YAML（`yaml` 库的 `parseDocument`）；协调报错就把文件写回原样并再协调一次。

**写回规则**（照 dsh-config-editor 现在的做法）：

- 通过 loader API 做的修改，一律写成编辑层（用户层）里的 `{ id, config }` patch。下面的层（基础文件、bundle 层）永远不改写，所以 patch 也不会被烤进基础文件。
- 编辑层**上面**的层（home、命令行）覆盖了同一个 entry 的 `config` 时，拒绝修改，返回 `OverriddenByLayer { layer }`。
- 写文件后重新协调；协调失败就回滚编辑层文件，再按旧文件协调一次。
- 改 YAML 时保留用户的注释和格式。
- 没有分层的简单场景（单个 `JsonFileStore`、没有 include）：直接改这个文件里的行。

**在 rutis-loader 里怎么落地**：上面的分层**归 rutis-dsh 独享**，不进 rutis-loader。profile、bundle 包、home 目录、遥测环境变量、哪一层可编辑、profile 文件锁、文件监视，都是 dsh 的概念。

| 放哪 | 内容 |
|---|---|
| rutis-loader（通用） | include + 有序 patch 列表 + 纯函数 `apply_patches`（cordis-plugin-include 语义，包括怪癖）；"编辑层"的写回 / 拒绝 / 回滚**机制**（由调用方指定哪一层）；`reconcile(patches)` 并报告新出现的失败行 |
| rutis-dsh | 按 dsh 规则找文件、拼 patch 列表；指定用户层为编辑层；profile 文件锁；监视文件并调用 `reconcile` |

## 十一之二、`!!js` 表达式（P2）

**不能不求值**。dsh 的基础配置大量用它，不求值的话 dsh-base、dsh-web-app、dsh-headless 这些行全都起不来。实际用法分几类（从 dsh 各包里搜出来的）：

| 类别 | 例子 |
|---|---|
| 环境变量 + 默认值 | `process.env.DSH_PERMISSION_MODE ?? 'workspace-write'`、`process.env.X \|\| 'Y'` |
| 类型转换 | `Number(process.env.DSH_CONTEXT_WINDOW ?? 1000000)` |
| 平台判断（多用在 `disabled`） | `process.platform === 'win32'` |
| 进程信息 | `process.cwd()` |
| 宿主函数 | `dshHomePath('sessions')` |
| 读启动参数服务 | `ctx.webStartup.port ?? 3080`、`ctx.headlessStartup.task` |
| 判断服务是否存在（多用在 `disabled`） | `!ctx.get('profileContext')` |
| 其它 | `process.getBuiltinModule('node:path').join(...)`（仅 dsh-web-app 一处） |

**放在哪**：rutis-loader 只定义钩子，不带实现：

```rust
pub trait Expressions: Send + Sync + 'static {
    /// 把一个表达式节点求值成 JSON 值。调用时机见下文"求值时机"。
    fn evaluate(&self, expr: &str, ctx: &Ctx) -> Result<Value, LoaderError>;
}
```

没有装钩子时，遇到表达式节点，该 entry 进 `Unresolved("no expression evaluator")`。YAML 里 `!!js` 标签的读写（原样保留）在 rutis-loader 的 YAML Store 里，因为它属于 cordis 配置文件格式本身。

**JS 子集求值器归 rutis-dsh**，因为它能访问的东西全是 dsh 的环境。语法照抄 JS，这样现有配置文件不用改：

- 支持：字面量、成员访问、函数调用、`??`、`||`、`&&`、`!`、`===`、`!==`、三元表达式；
- 只能访问作用域里的名字：
  - `process.env`、`process.platform`（取 Node 的写法，比如 `win32` / `darwin` / `linux`）、`process.cwd()`、`Number`、`String`；
  - dsh 的宿主函数，比如 `dshHomePath`；
  - `ctx.<服务名>.<字段>`、`ctx.get('<服务名>')`：通过服务名目录（§十）读取宿主登记为"可在表达式里读"的服务值；`ctx.get` 在服务不存在时返回 null。
- 超出子集（比如 `getBuiltinModule`）→ 该 entry 进 `Unresolved("unsupported expression: ...")`，不会悄悄算错。上表最后一行需要改写成宿主函数（比如注册一个 `pathJoin`）。

**求值时机**（与 cordis 一致）：`disabled` 由 loader 在决定是否装载时求值；`config` 里的表达式在每次装载或 update 前求值，结果交给插件；写回时保留原始表达式文本。分组和 include 自身的配置不求值，它们的子行由子行自己求值（cordis 的"树载体保持字面"规则）。

**以后的 JS 插件**（P6）：经 interop 管理的 JS 插件，配置表达式可以选择原样交给 node 侧求值，那边是完整的 JS 环境。但 `disabled` 始终由 loader 求值。

## 十二、插件卸载自己（P5，已知缺口）

**cordis 的行为**：插件调 `ctx.fiber.dispose()` 把自己关掉，loader 把它的 entry 标成 `disabled` 并写回配置。以下几种卸载**不**算"自己卸载"：loader 自己发起的、父分组或整棵树正在卸载的、热更新替换的。

**rutis 现状**：插件拿不到自己的 `FiberView`，所以做不到。这是功能缺失。dsh 插件源码里暂时没搜到这种用法，优先级低。

**补法**：

1. 内核加 `Ctx::dispose_self()`（或者 `Ctx::fiber_view()`），小改动；
2. loader 在 `watch()` 里看到 entry 的 fiber 进入 `Disposed`，并且同时满足下面两点时，就标记 `disabled` 并落盘：不是 loader 自己发起的（loader 记录自己的意图）；父分组没有在卸载。

## 十三、和 cordis 的差异汇总

| cordis 有 | rutis-loader | 说明 |
|---|---|---|
| 配置 schema | P1 | §九 |
| 配置里的 `inject` / `isolate` | P2 | 需要服务名目录，§十 |
| 配置里的 `intercept` | 不做 | 和 rutis 的 `ServiceIntercept` 不是一回事，dsh 未用 |
| `include` + patch 分层 | P2 | §十一 |
| `!!js` 表达式 | loader 提供钩子；JS 子集求值器在 rutis-dsh | §十一之二；超出子集的报错，不静默 |
| volatile 字段（改了不重启） | P5 | 需要 schema 标记约定，以及插件侧接收不重启的更新 |
| 插件卸载自己 → 标记 disabled | P5 | 已知缺口，§十二 |
| 先写文件后校验 | 改成先校验后写 | 失败不留脏配置 |
| 修改写回 | 写进编辑层 patch，上层覆盖则拒绝，失败回滚 | §十一 写回规则，与 dsh-config-editor 一致 |

## 十四、和现有东西的关系

- **dev 通道**（design-host-dev-mode）：它的 `load` / `swap` / `status` 就是 loader 的 `create` / `reload` / `entries` 加一层 socket，以后直接建在 loader 上。
- **rutis-dylib**：变成 `DylibResolver` 的实现细节。它现有的 `spawn` / `swap` 保留，给不用 loader 的项目用。
- **rutis-interop**：
  - 现在每个 cordis 挂载都要在 build 期生成 Rust 绑定，属于"静态插件"。生成的绑定可以注册进 `Builtins`，但生成的 `Config` 目前只 derive 了 `Serialize`，要让生成器加上 `Deserialize`，还要加 `JsonSchema`（或者直接透传 node 侧的 schema）。
  - 运行时才知道名字的 JS 插件，可以由 `InteropResolver` 用 `Process::mount` 挂上去，但 Rust 侧只能用无类型的 `call`；同时要负责 schema 导出和 isolate 转发。放在 P6。
- **TypedPlugin**：没有影响，`Typed<P>` 就是普通 `Plugin`，能注册进 `Builtins`。

## 十五、分阶段

1. **P1 loader 本体**：EntryOptions、Loader 服务、`Builtins` + `Chain`、分组、配置 schema（Builtins / schemars）、`MemoryStore` / `JsonFileStore`、`LoaderChanged`。内核不动。
2. **P2 对齐 dsh 的配置能力**：
   - 服务名目录 + 配置里的 isolate / inject；
   - include + patch + 编辑层写回机制 + YAML Store（`!!js` 原样读写）+ 表达式钩子；
   - rutis-dsh（与 rutis-loader 分开）：dsh 分层、JS 子集求值器；
   - 内核小补，各自独立 PR：`impl Plugin for Box<dyn Plugin>`、按 `PluginId` 取 `FiberView`、服务绑定变化事件；
   - interop 生成的 `Config` 加 `Deserialize`。
3. **P3 `DylibResolver`**（Linux）+ SDK 元数据加 schema 和服务名；macOS dylib 单独立项。
4. **P4 dev 通道**建在 loader 上。
5. **P5 volatile 字段；插件卸载自己**（内核 `dispose_self` + loader 识别）。
6. **P6 `InteropResolver`**：逐个管理 JS 插件，含 schema 导出、isolate 转发。

## 十六、要写的测试

**P1**

- create → 运行；update 合法配置 → 重载、PluginId 不变；update 非法配置 → Err、旧配置继续跑、文件没变；
- set_disabled(true) → 卸载、文件里 `disabled: true`；再 false → 重新装载；
- 分组禁用 → 子插件全部卸载、子 entry 保留；分组启用 → 子插件按顺序回来；
- move_to 跨分组；remove 分组 → 子 entry 一并删除；
- 换 name 且 injects 相同 → 原地 update；injects 不同 → 重建、PluginId 变；
- resolve 失败 → `Unresolved`，reload 成功后转为运行；
- `schema_of` 返回 schemars 生成的 schema；未提供 schema 的插件返回 None 且照常运行；
- 依赖链：A 提供服务、B 依赖 A；update A → B 被驱逐并重载（继承内核语义的回归测试）；
- 并发：同一 entry 上 update 和 remove 同时发起，结果确定，不死锁；分组 apply 期间发起操作，不死锁；
- 重启宿主：从 `JsonFileStore` 恢复出同一棵树；
- 宿主 shutdown 期间的操作返回 `Closed`。

**P2**

- isolate `true`：两个 entry 各自提供同名服务，互不可见；isolate 同一个字符串 label：共享；
- inject 额外门控：依赖未就绪时 Pending，就绪后启动；
- 改 isolate / inject → 重建、PluginId 变；服务名未登记 → `Unresolved` 并列出名字；
- patch：覆盖、insert、insert 后再 patch、目标不存在时警告并跳过、name 不匹配时跳过；patch 删掉后重新应用能回退；
- include 子 id 带前缀，`resolve("inc:child")` 能找到；
- 改坏 include 文件后 refresh → 保留旧树、打警告；
- 写回：修改落在编辑层、下层文件不变；上层覆盖时 → `OverriddenByLayer`；协调失败 → 编辑层文件回滚、运行态回到旧配置；
- `!!js`：上表每一类都有用例；`disabled` 表达式控制装载；写回保留原文；超出子集 → `Unresolved`；
- 对拍：同一个 profile，宿主拼出的 patch 列表经 `apply_patches` 得到的结果，与 dsh `--dump-config` 输出逐行一致。用例至少包括 dsh-base + 一个模式 bundle + 本仓库的 `aimux.patch.yml` + 用户层 + `--patch`；
- 怪癖复刻：整字段替换分组 `config` 后，后续 patch 看不到新子行；
- `insert` 里的相对路径 name 按 patch 文件所在目录解析；
- reconcile：引入新的失败行 → 报错；原本就坏的行不算。

## 十七、已定事项（原待定问题）

0. **"接 dsh 的一侧"就是 `crates/rutis-dsh`。** 它已经是"rutis 当宿主、经 rutis-interop 跑 dsh"的 crate，现有内容（挂载 dsh、aimux 适配）保留，dsh 分层和 JS 子集求值器加进去。迁移路径：现在 `dsh/launcher.ts` 在 node 里启动 dsh profile，分层由 node 里的 dsh-app-boot 完成；有了 rutis-loader 以后，分层改由 rutis-dsh 在 Rust 侧拼出 patch 列表交给 rutis-loader，`launcher.ts` 逐步缩小到只负责挂载 JS 插件。两套分层实现并存期间，用 `--dump-config` 对拍保证一致。

1. **名字不加前缀，名字本身就是身份。** `Builtins` 可以用任意名字注册，包括 npm 包名（比如用 Rust 重写的插件直接注册成 `@deepseek-ai/dsh-llm`）。解析顺序：先查 Builtins 精确匹配，再按 `dylib:` 之类的显式前缀分发，最后（P6）把 npm 包名交给 interop。理由：dsh 的配置和 patch 都按 name 定位行（patch 会校验 name 是否匹配），迁移时名字必须保持不变；用 Rust 替换某个 JS 插件，应该只换实现，不改配置。
2. **`create` 先校验，再落盘，不等启动。** 解析失败或配置 dry-run 失败 → 返回 Err，什么都不写（与 update 同一规则）；通过后落盘、spawn，返回 `(id, FiberView)`。apply 是异步的，不在这里等；调用方要等就 `.await` view 或 `settled()`。启动时从文件恢复的行例外：失败的行保留在树里，状态是 `Unresolved` / `Failed`，不删用户配置。
3. **不按时间防抖，但写入串行、合并。** 每次操作完成都要求写一次；正在写的时候再来的请求，合并成"写最新快照"一次（cordis include 写队列的做法）。临时文件 + rename 保证原子；再加**跨进程文件锁**，因为 CLI 和 Web 可能同时改同一个 profile，dsh 用的是 `withFileLock`。
4. **被覆盖字段的修改**：照 dsh-config-editor 的做法，写进编辑层 patch；上层覆盖则拒绝；协调失败则回滚，详见 §十一。
5. **`!!js` 要支持**：rutis-loader 提供表达式钩子，JS 子集求值器放在 rutis-dsh，详见 §十一之二。原先"一直不求值"的想法不可行。
6. **表达式里的 `ctx` 只开放宿主登记过的服务。** `ctx.<服务>.<字段>` 只能读宿主专门登记为"表达式可读"的服务（比如 `webStartup`、`headlessStartup` 这类启动参数服务），读其它服务直接报错。`ctx.get('<名字>')` 只判断服务在不在，不读内容，所以对服务名目录里所有登记过的名字都开放。这样配置文件读不到插件内部数据，出问题也好查。
