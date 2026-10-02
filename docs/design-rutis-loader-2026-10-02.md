# rutis-loader：插件管理层（设计稿）

状态：P1、P2、P3 已实现（rutis-loader、rutis-dsh 的 `profile` 模块、rutis-dylib 的 `DylibResolver`），P4 起见各自 PR。日期：2026-10-02。
对照对象：dsh vendored 的 `@deepseek-ai/cordis-plugin-loader` 1.0.5（`src/config/{entry,tree,group}.ts`、`src/index.ts`）、`cordis-plugin-include` 1.0.9、`dsh-app-boot`、`dsh-config-editor`。

## 一、要解决什么

rutis 内核能管**一个**插件：装、卸、重启、改配置（`Ctx::plugin_with` + `FiberView`）。
但"装哪些插件由数据决定"的项目（用户自己挑插件、UI 里管理插件、按 profile 组合插件）要管**一堆**插件：

- 按名字装插件，不用在代码里写死类型；
- 每个插件有稳定 id，能随时增、删、改配置、启用/禁用、分组；
- 几个来源的配置能分层叠加（默认、扩展包、用户、临时覆盖），改动能持久化，下次启动照着恢复；
- 能列出"现在装了什么、各自什么状态"，给 UI / CLI / dev 通道用。

cordis 里这是 `cordis-plugin-loader` + `cordis-plugin-include` 干的事，rutis 目前没有对应物。本文设计一个独立 crate `rutis-loader` 补上。

插件组合写死在代码里的项目直接用 `ctx.plugin`，不需要 loader。

不在本文范围：包安装、bundle、profile（dsh-plugin-manager 那一层），属于应用。

## 二、结论先说

1. **新 crate，内核零改动**。只用现有公开 API：`plugin_with`（工厂装载）、`FiberView::update`（dry-run 改配置）、`watch`、`dispose`。
2. **不需要配置文件，需要"期望状态"**。loader 的输入是有序的几层数据（每层是一个 patch 列表），合成出"应该运行什么"，loader 负责让运行态向它收敛（reconcile）。数据从哪来、存到哪去是应用的事：文件、数据库、代码里写死都行（§八）。
3. **命令式 API 是"改可编辑层 + reconcile"的快捷方式**。改动只写进被指定为可编辑的那一层，再通过持久化钩子交给应用保存。被上层覆盖的修改直接拒绝，reconcile 失败就回滚（§八）。
4. **配置统一是 JSON**（`serde_json::Value`），与 rutis-sdk 的 `ConfigValue` 一致。
5. **"按名字找插件"抽成 `Resolver` trait**：内置表、dylib、interop 各一个实现。
6. **分组 = 一个插件**，子插件挂在它的 ctx 下。禁用分组，内核的级联卸载自动带走子插件。
7. **先校验，后提交**：dry-run 不过的改动不进可编辑层、不持久化、不重启，旧版本继续跑。
8. **配置 schema 第一阶段就做**，挂在解析结果上（§九）。
9. **配置里的 isolate / inject 要支持**，靠"服务名 → `TypeKey`"目录（§十）。
10. **dsh 独有的东西都放在 rutis-dsh**：profile 分层规则、YAML 读写、`!!js` 求值器、文件锁、文件监视、嵌套 include（§十二）。
11. **插件卸载自己是已知缺口**，不是刻意不做（§十三）。

## 三、概念

```
应用提供的层（有序）                     loader
┌────────────────────┐
│ layer "defaults"   │──┐
│ layer "user" ✎     │──┼─ apply_patches ─→ 期望树 ─ reconcile ─→ 运行态（fiber 树）
│ layer "overlay"    │──┘                                  ▲
└────────────────────┘                                     │
          ▲  persist(user)                                  │
          └──────────── 命令式 API 改 ✎ 层 ─────────────────┘
```

期望树长这样：

```
根分组
 ├─ Entry "llm"      name = "@rutis/dsh-aimux"    config = {...}
 ├─ Entry "tools"    name = "rutis-tools"         disabled = true
 └─ Entry "agents"   group = true
     ├─ Entry "a1"   name = "dylib:agent-x"
     └─ Entry "a2"   ...
```

```rust
/// 期望树里的一行（与 cordis EntryOptions 同形；不含 intercept，见 §十四）。
#[derive(Serialize, Deserialize, Clone)]
pub struct EntryOptions {
    pub id: String,            // 整棵期望树内唯一
    pub name: String,          // 交给 Resolver 的模块名，同时是行的身份（§十八-1）
    #[serde(default)]
    pub config: Value,         // 分组时是子 EntryOptions 数组（同 cordis）
    #[serde(default)]
    pub group: bool,
    #[serde(default)]
    pub disabled: Value,       // bool，或表达式节点（§十一）
    #[serde(default)]
    pub inject: Option<Vec<String>>,                  // P2，见 §十
    #[serde(default)]
    pub isolate: Option<BTreeMap<String, Isolate>>,   // P2，见 §十
}

/// `true` = 本 entry 私有作用域；字符串 = 同名 label 共享（cordis LocalRealm / GlobalRealm）。
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Isolate { Private(bool), Shared(String) }

/// 一条 patch（与 cordis-plugin-include 的 PatchOptions 同形）。
#[derive(Serialize, Deserialize, Clone)]
pub struct Patch {
    pub id: Option<String>,
    pub insert: Option<Vec<EntryOptions>>,
    pub name: Option<String>,
    #[serde(flatten)]
    pub overrides: Map<String, Value>,   // config / disabled / group / inject / isolate …
}

pub struct Layer {
    pub name: String,
    pub patches: Vec<Patch>,
}
```

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
| `InteropResolver` | 其余的 npm 包名 | 后续阶段，见 §十五 |
| `Chain` | — | 按上面的顺序依次尝试（§十八-1） |

为什么返回工厂而不是插件实例：改配置要走 `FiberView::update`，而它只对工厂装载的 fiber 生效。

## 五、装载方式：配置里带上"解析结果"

每个 entry 用同一个 loader 内部工厂装载，配置类型是：

```rust
struct EntryConfig {
    resolved: Arc<Resolved>,   // 这一代用哪个模块
    value: Value,              // 求值后的用户配置
}
```

这是照搬 rutis-dylib `DylibConfig { module, value }` 的做法。好处是**换模块版本也能走 `update`**：

- 只改 `config` → `update(EntryConfig { 同一个 resolved, 新 value })`；
- 换 `name` 或重新解析（dylib 升级）→ `update(EntryConfig { 新 resolved, value })`。

两种情况都继承 `update` 的全部保证：dry-run 不过就不动，PluginId 不变，下游依赖照常驱逐重载。

**例外**：依赖声明（`injects`）在 spawn 时就固定了（内核 D32f）。新模块的 `injects` 和旧的不一样时，只能 dispose 旧 fiber 再 spawn 新的，PluginId 也会变。模块工厂的名字（插件身份）变了同样重建，与 rutis-dylib `swap` 拒绝换身份一致。loader 自动判断走哪条路。

## 六、分组

分组 entry 装载一个内部插件 `GroupPlugin`，它的 apply：

1. 向 loader 登记"这个分组当前的 ctx"；
2. 对每个未禁用的子 entry，在这个 ctx 下 `plugin_with` 装载，并把 `FiberView` 回填给 loader；
3. 返回的清理函数注销登记。

于是：

- 禁用或删除分组 → 分组 fiber 卸载 → 内核级联卸载所有子插件（D28 所有权），loader 只需把子 entry 的 view 清空；
- 分组重启 → apply 重跑，子插件按当前期望树重建；
- 移动 entry 到别的分组 = 在旧分组下 dispose，在新分组下 spawn（PluginId 会变，与 cordis 一致）。

**锁规则**（防死锁）：loader 的状态放在同步 `Mutex` 里，绝不跨 `await` 持有；所有写操作（reconcile、命令式 API）串行化在一把 async 操作锁上。`GroupPlugin::apply` 只碰状态锁，**不拿**操作锁。否则"操作等分组装载完 → 分组装载等操作锁"就成环了。

## 七、对外 API（控制面）

`LoaderPlugin::new(resolver, options)` 挂到 root。它提供 `Arc<Loader>` 服务，其它插件（如管理 UI）可以依赖它；宿主也能在挂载前直接拿到句柄。

```rust
pub struct LoaderOptions {
    pub persist: Arc<dyn Persist>,            // 默认 NoPersist
    pub expressions: Option<Arc<dyn Expressions>>, // §十一
}

impl Loader {
    // 期望状态（§八）
    async fn reconcile(&self, layers: Vec<Layer>, editable: Option<&str>) -> Result<ReconcileReport, LoaderError>;
    fn layers(&self) -> Vec<Layer>;

    // 查
    fn entries(&self) -> Vec<EntryInfo>;                 // 按树顺序
    fn get(&self, id: &str) -> Option<EntryInfo>;
    fn locate(&self, plugin: PluginId) -> Option<String>;// 某个 fiber 属于哪个 entry（沿 diagnostics 的 parent 往上找）
    async fn schema_of(&self, name: &str) -> Result<Option<Value>, LoaderError>; // 只解析不装载，供"新建前先填表单"
    fn evaluated(&self, id: &str) -> Option<Result<Value, LoaderError>>; // 当前生效的求值后配置，只读（§十一之二）

    // 改（= 改可编辑层 + reconcile + persist；等到稳定才返回）
    // 校验或 reconcile 失败：回滚，可编辑层和存储不变（apply 失败的运行态见 §八）；
    // 只有持久化失败（PersistFailed）时，修改已在运行态和可编辑层生效，留在待保存队列
    async fn create(&self, opts: NewEntry, parent: Option<&str>, position: Option<usize>) -> Result<(String, Option<FiberView>), LoaderError>; // 等到稳定才返回，§八
    async fn update(&self, id: &str, config: Value) -> Result<(), LoaderError>;
    async fn rename_module(&self, id: &str, name: &str) -> Result<(), LoaderError>; // 换 name，见 §五
    async fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), LoaderError>;
    async fn move_to(&self, id: &str, parent: Option<&str>, position: Option<usize>) -> Result<(), LoaderError>;
    async fn remove(&self, id: &str) -> Result<(), LoaderError>;

    // 运行态操作（不改期望状态，不持久化）
    async fn reload(&self, id: &str) -> Result<(), LoaderError>; // 重新 resolve（dylib 升级）
    async fn restart(&self, id: &str) -> Result<(), LoaderError>;

    // 持久化（§八 待保存队列）
    async fn flush(&self) -> Result<(), LoaderError>;  // 重试保存队列里还没存下的修改
    fn pending(&self) -> Vec<Edit>;

    // 等
    async fn settled(&self);   // 所有 resolve 和 fiber 转换都落地（对应 cordis tree.await()）
}

pub struct EntryInfo {
    pub options: EntryOptions,     // 期望树里的原始值（表达式未求值）
    pub parent: Option<String>,
    pub origin: Origin,            // 哪一层插入的；被哪些层覆盖过
    pub status: EntryStatus,       // Disabled / Resolving / Unresolved(err) / Running(Snapshot)
    pub plugin: Option<PluginId>,
    pub view: Option<FiberView>,
    pub schema: Option<Value>,
    pub meta: Value,
}
```

变更通知：每次 reconcile 或命令式操作完成后，在 bus 上发 `LoaderChanged { ids, kind }`。fiber 自身的状态变化仍看内核的 `FiberStatusChanged`，用 `EntryInfo::plugin` 对上号。

### 失败语义

| 情况 | 行为 |
|---|---|
| resolve 失败 | entry 留在期望树里，状态 `Unresolved(err)`，无 fiber。`reload` 可重试。（cordis 只打日志） |
| 首次装载 apply 失败 | 与内核一致：fiber `Failed`，entry 保留 |
| 命令式修改 dry-run 失败 | 返回 Err，可编辑层不变，不持久化，旧配置继续跑 |
| dry-run 通过、新实例 apply 失败 | 回滚：恢复旧可编辑层并 reconcile，旧配置重新装载；返回 `ApplyFailed`；不持久化（§八） |
| 回滚时旧配置也装不起来 | 返回 `RollbackFailed { apply, rollback }`；可编辑层和存储都是旧内容，该行状态 `Failed`，与存储一致 |
| 存储里的版本已被别人改过 | 按最新内容重做本次操作，有限次重试后仍冲突则返回 `Conflict`（§八） |
| 持久化失败（非冲突） | 返回 `PersistFailed`；运行态和内存里的可编辑层已改，这次修改留在**待保存队列**里，下次保存或 `flush()` 时整队写出（§八） |
| 冲突重放时，队列里较早的修改已不适用 | 从队列移除，发 `PendingEditDropped { edit, error }` 事件；本次修改照常继续（§八） |
| 宿主关闭 | 所有操作返回 `Closed`，不再持久化 |

## 八、期望状态、可编辑层与持久化

### reconcile

`reconcile(layers, editable)`：

1. 用 `apply_patches`（§十一之一）把各层按顺序合成期望树，同时收集警告（patch 找不到目标等）；
2. 和当前运行态按 id 对比：新增 → spawn；消失 → dispose；只改 `config` → update；改 `name` → §五；改 `isolate` / `inject` / 父分组 → 重建；`disabled` 变化 → dispose 或 spawn；
3. 等树稳定，返回 `ReconcileReport { warnings, new_failures }`。**稳定**指没有 fiber 处在转换中（Loading / Unloading）：`Active`、`Pending`（依赖未就绪）、`Failed`、`Unresolved` 都算稳定，所以依赖没到位的插件不会让调用方一直等下去。`new_failures` 只算**这次新出现**的失败行，原本就坏着的行不算（与 dsh 的 `reconcileProfilePatches` 一致）。

外部数据变了（比如 dsh 的文件被改），应用就带着新的层再调一次 `reconcile`。这一步只负责应用和报告，**不回滚**：外部改动的来源是应用，回不回滚由应用决定。

`editable` 指定哪一层是可编辑层，可以没有（此时命令式修改返回 `NoEditableLayer`）。

### 命令式修改

所有命令式修改（包括 `create`）都是同一个流程，**等到稳定才返回**：

1. 检查归属和覆盖（见下文）；
2. 算出可编辑层的新内容；
3. dry-run（resolve + `validate_config` + `build` + 实例 `validate`），不过就返回 Err；
4. 用新层 reconcile；**出现新失败行 → 恢复旧层、再 reconcile 一次，返回 `ApplyFailed`**；
5. 把本次 `Edit` 加进待保存队列，调 `Persist::save` 把整个队列写出（见"持久化钩子""待保存队列""多写者"）。

`create` 也不例外：dry-run 通过但 apply 失败 → 回滚（把刚插入的行从可编辑层去掉、再 reconcile），返回 `ApplyFailed`，不持久化；依赖没就绪 → 新行停在 `Pending`，算成功，正常持久化并返回 `(id, view)`。

先 reconcile 后持久化，所以存下来的永远是**通过校验并完成本轮协调**的期望状态，不需要回滚存储。注意这不等于"都已启动"：依赖未就绪的行停在 `Pending` 也会被存下来。

**apply 失败的细节**：内核 `FiberView::update` 不是原子替换：它先存新配置、卸载旧实例，再装新实例。dry-run 只能挡住校验和构造阶段的错误，挡不住 apply 阶段的。所以：

- 新实例 apply 失败时，旧实例已经卸载了。第 4 步的回滚是**再走一次 update**，把旧配置装回去，不是"什么都没发生"；
- 回滚期间，这个插件和依赖它的插件会短暂不可用（被驱逐后重新装载）。这是内核 update 的固有代价，loader 不额外承诺无缝切换；
- 回滚后旧配置也装不起来（比如它依赖的外部资源刚好坏了）：返回 `RollbackFailed { apply, rollback }`，两个错误都带上。此时可编辑层和存储都是旧内容，该行状态为 `Failed`。也就是说，配置和存储一致，只是运行态坏着，下次 reconcile 或 restart 可以重试；
- 调用方不能把 `update` 的所有错误都当成"什么都没变"：只有 dry-run 阶段的错误（`Validation` 一类）才保证运行态没被动过。

**归属规则**：patch 只能插入行和覆盖字段，不能删行或挪行，所以：

| 操作 | 行是可编辑层插入的 | 行是下层插入的 |
|---|---|---|
| update（config） | 直接改那条 insert | 写覆盖 patch `{ id, config }` |
| set_disabled | 直接改那条 insert | 写覆盖 patch `{ id, disabled }` |
| 改 isolate / inject | 直接改那条 insert | 写覆盖 patch `{ id, isolate }` / `{ id, inject }` |
| rename_module | 直接改那条 insert 的 `name` | 拒绝，`NotOwned`：patch 里的 `name` 只用来**核对**，不会覆盖（cordis 语义） |
| remove | 删掉那条 insert（它下面的子行一并消失） | 拒绝，`NotOwned`：patch 没有删除操作，删了重新合成也会回来。提示改用 set_disabled |
| move_to | 挪那条 insert | 拒绝，`NotOwned` |
| create（父是根或可编辑层拥有的分组） | 插进对应位置，`position` 生效 | — |
| create（父是下层拥有的分组） | — | 写 `{ insert: [...], id: <分组 id> }`，只能**追加到末尾**；传了 `position` 返回 `Unsupported` |

同一行的多次覆盖合并成一条 patch，后写的字段覆盖先写的，不会越堆越多。

**覆盖规则**（与 dsh-config-editor 一致）：可编辑层**上面**的某层覆盖了同一行的同一字段时，拒绝修改，返回 `OverriddenByLayer { layer }`。否则用户改了也不生效，还以为改成功了。

### 持久化钩子

```rust
pub trait Persist: Send + Sync + 'static {
    /// 读存储里这一层的最新内容和版本号（冲突后重做时用）。
    fn load<'a>(&'a self, layer: &'a str) -> BoxFuture<'a, Result<(Vec<Patch>, Version), LoaderError>>;
    /// 只有存储当前版本等于 `expected` 时才写入，返回新版本；否则返回 `Conflict`。
    /// `edits` 是待保存队列（自 `expected` 那个版本以来、按顺序的全部修改），
    /// 实现可以用它做局部修改（比如保留文件注释）；`patches` 是整层的最终内容。
    /// 契约：把 `edits` 依次作用在版本 `expected` 的内容上，结果必须等于 `patches`。
    fn save<'a>(&'a self, layer: &'a str, expected: &'a Version, edits: &'a [Edit], patches: &'a [Patch])
        -> BoxFuture<'a, Result<Version, PersistError>>;
}

/// 一次命令式操作：Create / Update / SetDisabled / Rename / Move / Remove，带上参数。
pub enum Edit { /* … */ }
```

- loader 保证同一进程内 `save` 调用串行、按提交顺序；
- 实现做局部修改时，要自己核对"局部修改的结果 == `patches`"，不一致就退回整层重写（可能丢掉注释），并打警告。宁可丢格式，也不能丢修改；
- 自带 `NoPersist`（什么都不存，版本号恒定）；
- 存成文件、写数据库还是发到远端，由应用实现。

### 待保存队列

loader 在内存里维护一个**待保存队列**：自上次成功保存以来、已经生效（reconcile 通过）但还没存下的 `Edit`，按顺序排列。

- 每次命令式修改成功后加进队尾，然后调 `save(expected = 上次成功保存的版本, edits = 整个队列, patches = 当前可编辑层)`；
- `save` 成功 → 记下新版本，**清空队列**；
- `save` 非冲突失败 → 队列原样保留，本次返回 `PersistFailed`；下一次修改或 `flush()` 会把整队一起写出；
- 队列只在内存里，进程退出就没了。所以 `PersistFailed` 必须报告给调用方（UI 应提示"未保存"），应用也可以定期调 `flush()` 重试。

### 多写者

同一份存储可能有好几个进程在改（dsh 的 CLI 和 Web 就会同时开着）。只给写入加锁不够：两个进程都从同一个版本出发，各改一行，就算写入完全串行，后写的也会用自己的旧快照覆盖先写的修改。

所以用**版本号比较（CAS）**：

1. `save` 发现存储版本不是自己期望的 → 返回 `Conflict`；
2. loader 用 `Persist::load` 读最新内容和版本，作为新的起点；
3. 在最新内容上**按顺序重放整个待保存队列**（操作是语义化的，比如"把 X 的 config 改成 Y"，所以可以重放）。每一项都重新走命令式修改的第 1–4 步（归属、覆盖检查，dry-run，reconcile，失败回滚）：
   - 较早的项重放失败（比如别人删了那一行）→ 从队列移除，发 `PendingEditDropped { edit, error }` 事件，继续重放后面的；
   - 本次的项重放失败 → 从队列移除，本次返回它的错误；
4. 用新版本作为 `expected`，再 `save` 剩下的整个队列；
5. 有限次（默认 3 次）后仍冲突 → 返回 `Conflict`，队列保留，下次修改或 `flush()` 再试。

例子：本进程改 A，保存失败（队列：A）→ 另一进程提交了 C → 本进程改 B，保存冲突 → 读到含 C 的最新内容，依次重放 A、B → 一次写出 → 存储里 A、B、C 都在，队列清空。

版本号也让应用的文件监视分得清"别人改的"和"我自己刚写的"：版本等于自己刚写的版本时，不用再 reconcile。

### 三种典型用法

| 场景 | 怎么用 |
|---|---|
| 纯 API，不持久化 | 一层空的可编辑层 + `NoPersist`。重启后状态丢失 |
| 纯 API，要持久化 | 启动时从自己的存储读出可编辑层，`reconcile([user], Some("user"))`；之后命令式修改经 `Persist` 存回去 |
| 分层（dsh） | 应用拼出 bundle、用户、home、命令行几层，指定用户层可编辑；文件变化时重新 reconcile（§十二） |

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

**写法**（与 cordis 一致，下面用 YAML 只是为了好读）：

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

## 十一、patch 语义与表达式

### 十一之一、`apply_patches`（P1）

纯函数，与 cordis `applyEntryPatches` 逐条一致。离线工具也能复用，保证"dump 出来的"和"实际启动的"一致：

- `{ id, ...字段 }`：按 id 找到行，**整字段替换**（`config` 整个换掉，不做合并）；`name` 写了但不匹配 → 跳过并警告；
- `{ insert: [...], id? }`：插入新行；带 id 时插进那个分组（目标不是分组 → 警告并跳过）；
- 找不到目标 → 警告并跳过，不报错；
- id 索引只在开头建一次，之后只给 `insert` 进来的行补索引。所以如果某个 patch 用整字段替换了一个分组的 `config`（换进新的子行），后面的 patch **看不到**这些新子行。这是现有行为的一个怪癖，要复刻，不要"修好"；
- 输入不被修改（先深拷贝），所以去掉一层后重新合成能干净回退。

### 十一之二、表达式（P2）

cordis 的配置可以写 `!!js` 表达式，读进来是 `{ "__jsExpr": "<源码>" }` 这样的节点。loader 沿用这个 JSON 约定，不关心它原来是什么文件格式。

- **`__jsExpr` 是保留键**：恰好只有这一个键、值是字符串的对象，一律当作表达式节点。普通配置里不能出现这种形状的对象（cordis 同样如此）。普通字符串永远是字面值，不会被当成表达式；
- 表达式可以出现在 `disabled` 和 `config` 的任意深度（所以 `disabled` 的类型是 `Value`，不是 `bool`）；
- **两份配置要分清**：
  - **原始配置**：存在层和期望树里，带表达式节点。`EntryInfo::options` 返回它，可编辑层和存储也只存它；
  - **求值后的配置**：每次装载或 update 前现算，只交给插件（`EntryConfig::value`），不回写、不持久化；
- 编辑时，调用方传入的是**原始配置**。UI 想显示"实际生效的值"，另外调 `Loader::evaluated(id)` 查看，不能拿它当 update 的输入，否则表达式会被固化成常量；
- 注释、文件里的位置这些"来源信息"不进 loader 的数据模型，由持久化实现自己保留（§十二）。

loader 只定义钩子，不带实现：

```rust
pub trait Expressions: Send + Sync + 'static {
    /// 把一个表达式节点求值成 JSON 值。
    fn evaluate(&self, expr: &str, scope: &ExprScope<'_>) -> Result<Value, LoaderError>;
}
```

实现时把求值器拿到的 `&Ctx` 换成了受限的 `ExprScope`：只能 `has(name)`（服务名目录里任意名字）和 `read(name)`（仅登记为可读、可序列化的服务），以结构保证 §十八-6，求值器拿不到完整的 ctx。`disabled` 用 loader 的根 ctx 求值；`config` 用该行自己的 ctx（父分组 + 本行 isolate）求值，每次 reconcile 重新求值，值变了就原地 update。

- **求值时机**（与 cordis 一致）：`disabled` 由 loader 在决定是否装载时求值；`config` 里的表达式在每次装载或 update 前求值，结果交给插件；期望树和可编辑层里永远保留原始表达式。分组自身的 `config`（子行列表）不求值，子行由子行自己求值（cordis 的"树载体保持字面"规则）。
- 没有装钩子时，遇到表达式节点，该 entry 进 `Unresolved("no expression evaluator")`。
- JS 子集求值器属于 dsh，放在 rutis-dsh（§十二）。

## 十二、rutis-dsh 侧（dsh 独有）

`crates/rutis-dsh` 已经是"rutis 当宿主、经 rutis-interop 跑 dsh"的 crate。下面这些都是 dsh 的概念，放在那里，不进 rutis-loader。

### 分层规则

来源：dsh-app-boot 的 `readProfilePatches` / `loadProfileDirectory` / `applyEntryPatches`、dsh 的 `profile-boot`。

基础文件 `<profile>/cordis.yml` 内容就是 `[]`（文件头注释写着"改 cordis.patch.yml，别改这里"）。**所有行都来自 patch**。rutis-dsh 按下表读出各层，交给 `Loader::reconcile`，用户层为可编辑层：

| 顺序 | 层 | 来源 | 文件不存在 / 解析失败 |
|---|---|---|---|
| 1 | bundle 层 | profile 的 `package.json` 里 `dsh.profile.bundles` 的顺序；每个 bundle 包的 `dsh.bundle.patch`（一个文件或文件列表，按列表顺序） | 整个 bundle **跳过**并记下原因（包找不到、没声明 `dsh.bundle`、版本不兼容） |
| 2 | 用户层（**可编辑层**） | `<profile>/cordis.patch.yml` | 不存在 = 空层；解析失败 = 启动报错 |
| 3 | home 层 | `~/.dsh/cordis.patch.yml` | 同上 |
| 4 | 命令行层 | 每个 `--patch <file>`，按参数顺序 | 不存在或解析失败都报错（用户点名要的文件） |
| 5 | 遥测开关 | `DSH_TELEMETRY_DISABLED` 非空，并且合成结果里有 `session-telemetry-otel` 这一行 → 追加 `{ id: session-telemetry-otel, disabled: true }` | — |

每个 patch 文件必须是 YAML 顶层数组，每一项都是映射，否则整个文件报错。`insert` 里的行，`name` 是相对路径（`./`、`../`）或绝对路径时，改写成相对**该 patch 文件**的 `file://` URL。

### 文件读写

- YAML 读写；`!!js` 标签与 `{ "__jsExpr" }` 节点互转，原样读、原样写回；
- 实现 `Persist`，只写用户层文件：
  - 版本号 = 文件内容哈希；
  - `save` 在 profile 的**跨进程文件锁**（dsh 用的是 `withFileLock`）里做"读当前文件 → 比较版本 → 写"，不一致就返回 `Conflict`。锁只保证这一步原子，防止丢失修改靠的是版本比较（§八 多写者）；
  - 按 patch 粒度重写：没有变化的 patch 原样保留源文本（包括注释和格式），变化了的 patch 重新生成（它自己的注释随之丢失）；写之前解析一遍结果，与 `patches` 比对，不一致就整层重新生成。dsh-config-editor 用 `yaml` 库在节点粒度上修改，粒度比这里细，见 §十九。
  - 临时文件 + rename 原子替换。

### 热重载

监视各层文件（对应 `dsh-hmr`），变化后重新读层、调 `reconcile`；`new_failures` 非空就报告，不回滚（与 dsh 现状一致）。文件读不了或解析失败时，打警告、保持当前运行态，绝不因为改坏配置把进程带崩。

### 嵌套 include

用户配置里可能还有嵌套的 include 行（指向另一个文件的子树）。rutis-dsh 把它展开成一个分组：子行来自那个文件再加上它自己的 patches，子行 id 加前缀 `<include id>:`。嵌套 include 里的行**不可通过 API 编辑**：它们属于另一个文件。这与 dsh-config-editor 一致，它只编辑根 include 下的行。

### JS 子集求值器（实现 `Expressions`）

**不能不求值**。dsh 的基础配置大量用它，不求值的话 dsh-base、dsh-web-app、dsh-headless 这些行全都起不来。实际用法（从 dsh 各包里搜出来的）：

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

语法照抄 JS，这样现有配置文件不用改：

- 支持：字面量、成员访问、函数调用、`??`、`||`、`&&`、`!`、`===`、`!==`、三元表达式；
- 只能访问作用域里的名字：
  - `process.env`、`process.platform`（取 Node 的写法，比如 `win32` / `darwin` / `linux`）、`process.cwd()`、`Number`、`String`；
  - dsh 的宿主函数，比如 `dshHomePath`；
  - `ctx.<服务名>.<字段>`、`ctx.get('<服务名>')`：范围见 §十八-6。
- 超出子集（比如 `getBuiltinModule`）→ 该 entry 进 `Unresolved("unsupported expression: ...")`，不会悄悄算错。上表最后一行需要改写成宿主函数（比如注册一个 `pathJoin`）。

以后经 interop 逐个管理的 JS 插件（P6），配置表达式可以选择原样交给 node 侧求值，那边是完整的 JS 环境；但 `disabled` 始终由 loader 求值。

### 迁移路径

现在 `dsh/launcher.ts` 在 node 里启动 dsh profile，分层由 node 里的 dsh-app-boot 完成。有了 rutis-loader 以后，分层改由 rutis-dsh 在 Rust 侧完成，`launcher.ts` 逐步缩小到只负责挂载 JS 插件。两套分层实现并存期间，用 `--dump-config` 对拍保证一致。

## 十三、插件卸载自己（P5，已知缺口）

**cordis 的行为**：插件调 `ctx.fiber.dispose()` 把自己关掉，loader 把它的 entry 标成 `disabled` 并写回配置。以下几种卸载**不**算"自己卸载"：loader 自己发起的、父分组或整棵树正在卸载的、热更新替换的。

**rutis 现状**：插件拿不到自己的 `FiberView`，所以做不到。这是功能缺失。dsh 插件源码里暂时没搜到这种用法，优先级低。

**补法**：

1. 内核加 `Ctx::dispose_self()`（或者 `Ctx::fiber_view()`），小改动；
2. loader 在 `watch()` 里看到 entry 的 fiber 进入 `Disposed`，并且不是 loader 自己发起的、父分组也没有在卸载时，就当作一次 `set_disabled(true)`：写进可编辑层并持久化。

## 十四、和 cordis 的差异汇总

| cordis 有 | rutis-loader | 说明 |
|---|---|---|
| 配置文件（Loader 根树 + include） | 不需要文件，改为分层期望状态 + 持久化钩子 | §八；dsh 的文件在 rutis-dsh |
| 配置 schema | P1 | §九 |
| 配置里的 `inject` / `isolate` | P2 | 需要服务名目录，§十 |
| 配置里的 `intercept` | 不做 | 和 rutis 的 `ServiceIntercept` 不是一回事，dsh 未用 |
| patch 分层 | P1（`apply_patches`） | §十一之一 |
| `!!js` 表达式 | loader 提供钩子；JS 子集求值器在 rutis-dsh | §十一之二、§十二；超出子集的报错，不静默 |
| volatile 字段（改了不重启） | P5 | 需要 schema 标记约定，以及插件侧接收不重启的更新 |
| 插件卸载自己 → 标记 disabled | P5 | 已知缺口，§十三 |
| 先写文件后校验 | 先校验、先 reconcile，最后持久化 | 存下来的永远是通过校验并完成本轮协调的期望状态 |
| 修改写回 | 写进可编辑层，上层覆盖则拒绝，失败回滚 | §八，与 dsh-config-editor 一致 |

## 十五、和现有东西的关系

- **dev 通道**（design-host-dev-mode）：它的 `load` / `swap` / `status` 就是 loader 的 `create` / `reload` / `entries` 加一层 socket，以后直接建在 loader 上。
- **rutis-dylib**：变成 `DylibResolver` 的实现细节。它现有的 `spawn` / `swap` 保留，给不用 loader 的项目用。
- **rutis-interop**：
  - 现在每个 cordis 挂载都要在 build 期生成 Rust 绑定，属于"静态插件"。生成的绑定可以注册进 `Builtins`，但生成的 `Config` 目前只 derive 了 `Serialize`，要让生成器加上 `Deserialize`，还要加 `JsonSchema`（或者直接透传 node 侧的 schema）。
  - 运行时才知道名字的 JS 插件，可以由 `InteropResolver` 用 `Process::mount` 挂上去，但 Rust 侧只能用无类型的 `call`；同时要负责 schema 导出和 isolate 转发。放在 P6。
- **rutis-dsh**：dsh 独有的部分都放在这里，见 §十二。
- **TypedPlugin**：没有影响，`Typed<P>` 就是普通 `Plugin`，能注册进 `Builtins`。

## 十六、分阶段

1. **P1 loader 本体**：数据模型（EntryOptions / Patch / Layer）、`apply_patches`、`reconcile`、可编辑层与命令式 API、`Persist` + `NoPersist`、`Builtins` + `Chain`、分组、配置 schema（schemars）、`LoaderChanged`。内核不动。
2. **P2 对齐 dsh 的配置能力**：
   - rutis-loader：服务名目录 + 配置里的 isolate / inject；表达式钩子；
   - rutis-dsh：dsh 分层、YAML 读写与 `Persist` 实现、文件锁、热重载、嵌套 include、JS 子集求值器；
   - 内核小补，各自独立 PR：`impl Plugin for Box<dyn Plugin>`、按 `PluginId` 取 `FiberView`、服务绑定变化事件；
   - interop 生成的 `Config` 加 `Deserialize`。
3. **P3 `DylibResolver`**（Linux）+ SDK 元数据加 schema；macOS dylib 单独立项。（已实现：`rutis-dylib` 的 `loader` feature；名字 `dylib:<目录>`。服务名没有放进 SDK：服务名目录要登记带类型的探针，dylib 插件给不了，需要把 rutis-loader 编进 SDK 的 ABI；改为由宿主用共享的接口 crate 登记。）
4. **P4 dev 通道**建在 loader 上。
5. **P5 volatile 字段；插件卸载自己**（内核 `dispose_self` + loader 识别）。
6. **P6 `InteropResolver`**：逐个管理 JS 插件，含 schema 导出、isolate 转发。

## 十七、要写的测试

**P1（rutis-loader）**

- create → 运行；update 合法配置 → 重载、PluginId 不变；update 非法配置 → Err、旧配置继续跑、可编辑层没变、`Persist::save` 没被调用；
- set_disabled(true) → 卸载、可编辑层多了 `disabled: true`；再 false → 重新装载；
- 归属：下层插入的行 remove / move_to → `NotOwned`；可编辑层插入的行可以删、可以挪；
- 覆盖：上层覆盖了某行的 `config` → 对该行 update 返回 `OverriddenByLayer`；
- 回滚：修改导致新失败行 → 可编辑层恢复、运行态回到旧配置、返回 Err；
- reconcile：增、删、改 config、改 name、改父分组各走对应路径；已经坏着的行不算新失败；
- 没有可编辑层时命令式修改 → `NoEditableLayer`；
- 持久化：`save` 按提交顺序、串行调用；`save` 失败 → `PersistFailed`、修改留在队列；下次修改或 `flush()` 整队写出后队列清空；
- 待保存队列 + 冲突：修改 A 保存失败 → 另一写者提交 C → 修改 B 触发冲突 → 最终存储里 A、B、C 都在、队列清空；
- 重放丢弃：队列里的 A 在重放时已不适用（目标行被另一写者删掉）→ A 被移除并发 `PendingEditDropped`，B 正常保存；
- `save` 契约：模拟一个只做局部修改的存储，局部结果与 `patches` 不一致 → 退回整层重写并告警；
- create 等待：dry-run 通过、apply 失败 → `ApplyFailed`，新行不在可编辑层、未持久化；依赖未就绪 → 返回成功、行为 `Pending`、已持久化；
- 重启一致性：对每种命令式操作（create、update、set_disabled、改 isolate/inject、rename、move、remove），用存下来的层在新 loader 里 reconcile，得到的期望树与修改后的完全一致；
- 归属：下层的行 rename → `NotOwned`；在下层分组里 create 带 `position` → `Unsupported`、不带则追加到末尾；
- 多写者：两个 loader 共用一个模拟存储，从同一版本出发各改不同的行 → 两处修改都保留；改同一行 → 后者在最新内容上重做；持续冲突 → `Conflict`；
- apply 失败：dry-run 通过、新实例 apply 失败 → 旧配置被重新装载、可编辑层与存储未变、返回 `ApplyFailed`；回滚也失败 → `RollbackFailed` 带两个错误、该行 `Failed`、可编辑层与存储仍是旧内容；
- `apply_patches`：覆盖、insert、insert 后再 patch、目标不存在时警告并跳过、name 不匹配时跳过、去掉一层后能回退；怪癖复刻：整字段替换分组 `config` 后，后续 patch 看不到新子行；
- 分组禁用 → 子插件全部卸载、子 entry 保留；分组启用 → 子插件按顺序回来；
- 换 name 且 injects 相同 → 原地 update；injects 不同 → 重建、PluginId 变；
- resolve 失败 → `Unresolved`，reload 成功后转为运行；
- `schema_of` 返回 schemars 生成的 schema；未提供 schema 的插件返回 None 且照常运行；
- 依赖链：A 提供服务、B 依赖 A；update A → B 被驱逐并重载（继承内核语义的回归测试）；
- 并发：同一 entry 上 update 和 remove 同时发起，结果确定，不死锁；分组 apply 期间发起操作，不死锁；
- 宿主 shutdown 期间的操作返回 `Closed`。

**P2（rutis-loader）**

- isolate `true`：两个 entry 各自提供同名服务，互不可见；isolate 同一个字符串 label：共享；
- inject 额外门控：依赖未就绪时 Pending，就绪后启动；
- 改 isolate / inject → 重建、PluginId 变；服务名未登记 → `Unresolved` 并列出名字；
- 表达式：`disabled` 表达式控制装载；`config` 表达式每次装载前求值；可编辑层里保留原文；没装钩子 → `Unresolved`；只有单键 `__jsExpr` 对象被当作表达式，同名字段混在别的键里时当普通对象；`evaluated(id)` 返回求值结果且不影响原始配置。

**P2（rutis-dsh）**

- 对拍：同一个 profile，rutis-dsh 拼出的层经 `apply_patches` 得到的结果，与 dsh `--dump-config` 输出逐行一致。用例至少包括 dsh-base + 一个模式 bundle + 本仓库的 `aimux.patch.yml` + 用户层 + `--patch`；
- `insert` 里的相对路径 name 按 patch 文件所在目录解析；
- 修改只落在用户层文件、注释保留；上层（home、`--patch`）覆盖时拒绝；
- 双进程：两个进程同时改同一 profile 的不同行 → 两处修改都在文件里；
- 改坏文件后热重载 → 保持当前运行态、打警告；
- 嵌套 include：子 id 带前缀；其中的行不可编辑；
- JS 子集：上表每一类都有用例；超出子集 → `Unresolved`。

## 十八、已定事项

1. **名字不加前缀，名字本身就是身份。** `Builtins` 可以用任意名字注册，包括 npm 包名（比如用 Rust 重写的插件直接注册成 `@deepseek-ai/dsh-llm`）。解析顺序：先查 Builtins 精确匹配，再按 `dylib:` 之类的显式前缀分发，最后（P6）把 npm 包名交给 interop。理由：dsh 的配置和 patch 都按 name 定位行（patch 会校验 name 是否匹配），迁移时名字必须保持不变；用 Rust 替换某个 JS 插件，应该只换实现，不改配置。
2. **`create` 和其它命令式修改一样，等到稳定才返回。** 没有特例：校验失败 → 什么都不改；apply 失败 → 回滚、`ApplyFailed`、不持久化；依赖未就绪 → 停在 `Pending` 也算稳定，返回成功并持久化。（早先"create 不等启动"的写法与回滚语义矛盾，已删除。）`reconcile` 的行为不同：它把外部给的层原样应用，期望树里起不来的行保留，状态是 `Unresolved` / `Failed`，不删用户的配置，也不回滚。
3. **写入**：loader 保证进程内 `Persist::save` 串行、按顺序；多进程之间靠版本号比较 + 重做操作防止丢失修改（§八 多写者）。文件锁、原子替换是文件的事，在 rutis-dsh（§十二）。
4. **被覆盖字段的修改**：照 dsh-config-editor 的做法，写进可编辑层；上层覆盖则拒绝；失败则回滚，详见 §八。
5. **`!!js` 要支持**：rutis-loader 提供表达式钩子，JS 子集求值器放在 rutis-dsh，详见 §十一之二、§十二。原先"一直不求值"的想法不可行。
6. **表达式里的 `ctx` 只开放宿主登记过的服务。** `ctx.<服务>.<字段>` 只能读宿主专门登记为"表达式可读"的服务（比如 `webStartup`、`headlessStartup` 这类启动参数服务），读其它服务直接报错。`ctx.get('<名字>')` 只判断服务在不在，不读内容，所以对服务名目录里所有登记过的名字都开放。这样配置读不到插件内部数据，出问题也好查。
7. **rutis-loader 不读写任何文件。** 输入是有序的层（数据），输出是持久化钩子。插件组合写死在代码里的项目不用 loader；要动态管理的项目自己决定数据存哪；dsh 的文件规则在 rutis-dsh。

## 十九、仍待确认

- 用户层的注释按 patch 粒度保留（§十二）。如果需要节点粒度（被修改的 patch 内部的注释也保留），要么找一个保留注释的 Rust YAML 编辑库，要么自己做基于位置的节点替换。
