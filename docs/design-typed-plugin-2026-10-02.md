# TypedPlugin：类型化依赖（草案）

状态：已实现（#94，关闭 #50），第三版。日期：2026-10-02。

第二版处理了 review 的四项意见：
- 运行期键（限定名、实例）有了接口；
- 工厂从同一依赖描述生成声明；
- 支持 trait object 服务；
- 依赖丢失的判定改为以本代失败的严格读取为证据（第三版，修正了第二版“绑定身份”的两个漏洞）。

## 目标

插件只写一份依赖描述，框架据此门控，并把取到的值作为 `apply` 的参数传入。
声明与取用来自同一描述，不一致时编译失败。

现有写法里，`injects()` 是运行时的 `TypeKey` 列表，`ctx.get` / `ctx.require`
另写一遍类型；两者对不上（声明 A 取 B、取了没声明）只在运行时暴露。

## 与 Cordis、与现有 `Plugin` 的关系

- **行为不变**：`Typed<P>` / `TypedFactory<F, C>` 把 typed 插件和工厂包成普通的
  `Plugin` / `PluginFactory`。门控、驱逐、重载、卸载期不可见等规则与其它插件完全一样。
  内核只多了一条规则（见“依赖在门控后丢失”）。
- **写法是 Rust 独有的**：Cordis 的 `inject` 是字符串列表、`ctx.xxx` 靠模块扩充取类型，
  两者同样不挂钩。rutis 对齐的是 Cordis 的行为，接口本就是 Rust 风格（`EventKey`、
  `TypeKey`），这一层属于同类差异。
- **可混用**：`Plugin` trait 不动；typed 与非 typed 插件互相提供、互相依赖；
  rutis-interop 挂载的 Cordis 插件不受影响。

## 依赖描述：类型 + 运行期键

```rust
pub trait Deps: Sized + Send + 'static {
    type Keys: Clone + Send + Sync + 'static;            // 运行期部分
    fn injects(keys: &Self::Keys, out: &mut Vec<TypeKey>);
    fn resolve(keys: &Self::Keys, ctx: &Ctx) -> Result<Self, CordisError>;
}
```

类型决定“要什么、拿到什么”，`Keys` 携带挂载时才知道的部分（名字、实例）。
门控声明和读取都由同一对 `(Deps, Keys)` 生成，因此不会分叉。

| 写法 | Keys | 门控 | 传入 |
|---|---|---|---|
| `Arc<T>` | `()` | `TypeKey::of::<T>()` | 服务 |
| `Option<Arc<T>>` | `()` | 否 | 装载时可见则有 |
| `Keyed<T>` | `DepKey<T>` | 该键 | 服务 |
| `Option<Keyed<T>>` | `DepKey<T>` | 否 | 装载时可见则有 |
| `Gate<T>` | `()` | `TypeKey::of::<T>()` | 无（只门控） |
| `KeyedGate<T>` | `DepKey<T>` | 该键 | 无（只门控） |
| `()`、至多 8 元组 | 成员 Keys 的元组 | 全部 | 各自 |

- **`T` 可以是 unsized**（`Arc<dyn LanguageModel>`）：经 `require_as` / `get_as` 读取。
- **`DepKey<T>` 是带值类型的键**：构造函数 `of` / `named` / `dynamic` / `.instance(id)`，
  以及 `From<Key<T>>`，都写明 `T`，键不会指向别的类型的服务。`DepKey` 的默认值是 `of()`。
- **只门控的依赖也是描述的一部分**（`Gate` / `KeyedGate`），第一版的 `gates()` 方法删除：
  插件和工厂不再各有一份声明。
- **可选依赖是装载时快照**：不门控，出现或消失不触发重载，与现在“不声明、apply 里 get”一致。
  在诊断里记为未声明访问。

## 挂载

```rust
ctx.plugin(Typed::new(plugin));                     // Keys: Default（全部由类型决定）
ctx.plugin(Typed::with_keys(plugin, keys));         // 挂载时选定名字、实例

ctx.plugin_with(TypedFactory::new(factory), config);
ctx.plugin_with(TypedFactory::with_keys(factory, keys), config);
```

`TypedPluginFactory<C>` 的 `build` 返回 typed 插件。`TypedFactory` 的 `injects()`
由 `Plugin::Deps` 和 keys 生成，每次构造的插件都用同一组 keys 读取，所以配置热更新
不会改变声明，与 `PluginFactory` 的“声明静态”规则一致。

## 覆盖情况（#50 设计要求）

| 要求 | 方式 | 测试 |
|---|---|---|
| 限定名 | `Keyed<T>` + `DepKey::named` / `dynamic` / `Key<T>` | `named_keys_are_chosen_when_mounting` |
| 同类型多实例 | `DepKey::of().instance(id)`，挂载时传入 | `instance_keys_resolve_inside_their_instance_only` |
| isolate | 读取按作用域解析，与非 typed 相同 | `isolated_scopes_pass_their_own_service` |
| 可选依赖 | `Option<Arc<T>>` / `Option<Keyed<T>>` | `an_optional_dependency_is_passed_when_present` |
| check 门控 | 提供方注册，消费方门控自动包含 | `a_failing_check_keeps_the_plugin_pending` |
| 工厂 | `TypedFactory` | `a_typed_factory_declares_from_the_same_description` |
| trait object | `T: ?Sized` | `trait_object_services_are_dependencies` |

## 依赖在门控后丢失

门控通过后、`apply` 读取前，必需依赖失效：插件回到 Pending，而不是 Failed。

- typed 读取遇到 `Unavailable` 时，返回 `CordisError::InjectUnsatisfied`。
- **内核规则**：`apply` 返回 `InjectUnsatisfied` 时，内核查看这一代的访问记录（每次装载开始时
  清空）。如果其中确有一次对声明依赖的严格读取（`require` / `require_as`）因 `Unavailable`
  而失败，这一代就按驱逐处理：清理照常 LIFO 排干，清理错误进 ErrorSink，然后回到 Pending。
  依赖若已恢复，立即投递重查，重新装载。
- **为什么以失败的读取为证据**：前两版分别看“此刻是否缺失”和“绑定是否还是门控时那个”，
  都有漏洞：
  - 依赖可能在错误被处理前就已恢复；
  - 同一提供者可能在同一代里撤掉再提供；
  - 快照取得晚于门控时，会错过中间的撤销；
  - 同一绑定的 `check` 也可能从通过变为拒绝。

  失败的严格读取是内核自己记下的事实，直接说明“门控之后依赖确实失效过”，与时序和快照无关。
  失效原因可以是被摘除、提供者失活或 `check` 拒绝。
- **防循环**：这一代没有这样的读取时，不认这个理由，按普通失败进 Failed。
- 这一代的 `InjectUnsatisfied` 本身不进 ErrorSink（与驱逐一样静默）。落在这个窗口里的
  `restart()` 返回 Ok，fiber 回到 Pending 或重新装载。
- **非 typed 插件**：用 `require` / `require_as` 读取，再返回这个错误，可以得到同样的效果；
  用 `get` 读取不算证据。rutis-agent 的 driver 和 tui 都用 `get_as`，仍按普通失败处理，
  行为与改动前一致。
- **与 Cordis 的差异**：Cordis 在此窗口内 `apply` 抛错会进 FAILED（依赖回来时重试）；
  rutis 只对这个明确的错误变体、且有失败读取为证时放宽，其它错误仍是粘性 Failed。

## 宣称边界

`apply` 仍拿到完整的 `Ctx`。只能说“经 `Deps` 取得的依赖在编译期对齐”，
不能说“依赖错误都会编译失败”。

## 已定

1. **包装方式**：`Typed::new` / `Typed::with_keys` 显式包装，不给 `Ctx` 加方法。不用 blanket
   `impl<P: TypedPlugin> Plugin for P`：`injects()` 返回借用切片，需要实例里存一份；
   运行期键也需要存放的地方。
2. **依赖在门控后丢失**：回到 Pending（见上）。

## 待定

- 可选依赖在诊断里是否单独标注为 optional（目前记为未声明访问）。
- 是否需要 `Option<D>` 的通用实现（例如可选的元组）；目前只有单个服务的可选形式。
