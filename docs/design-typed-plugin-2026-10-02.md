# TypedPlugin：类型化依赖（草案）

状态：草案，第一版已实现（#50）。日期：2026-10-02。

## 目标

插件只用一个类型声明依赖，框架据此门控，并把取到的值作为 `apply` 的参数传入。
声明与取用来自同一类型，不一致时编译失败。

现有写法里，`injects()` 是运行时的 `TypeKey` 列表，`ctx.get` / `ctx.require`
另写一遍类型；两者对不上（声明 A 取 B、取了没声明）只在运行时暴露。

## 与 Cordis、与现有 `Plugin` 的关系

- **行为不变**：`Typed<P>` 把 `TypedPlugin` 包成普通 `Plugin`。`injects()` 由依赖类型
  生成，门控、驱逐、重载、卸载期不可见等规则与其它插件完全一样，内核不改。
- **写法是 Rust 独有的**：Cordis 的 `inject` 是字符串列表、`ctx.xxx` 靠模块扩充取类型，
  两者同样不挂钩。rutis 对齐的是 Cordis 的行为，接口本就是 Rust 风格（`EventKey`、
  `TypeKey`），这一层属于同类差异。
- **可混用**：`Plugin` trait 不动；typed 与非 typed 插件互相提供、互相依赖；
  rutis-interop 挂载的 Cordis 插件不受影响。

## 第一版接口

```rust
pub trait Deps: Sized + Send + 'static {
    fn injects(keys: &mut Vec<TypeKey>);
    fn resolve(ctx: &Ctx) -> Result<Self, CordisError>;
}

pub trait TypedPlugin: Send + Sync + 'static {
    type Deps: Deps;
    fn name(&self) -> &str;
    fn gates(&self) -> Vec<TypeKey> { Vec::new() }       // 只门控、不传参
    fn validate(&self) -> Result<(), CordisError> { Ok(()) }
    fn apply<'a>(&'a self, ctx: &'a Ctx, deps: Self::Deps)
        -> BoxFuture<'a, Result<Effect, CordisError>>;
}

ctx.plugin(Typed::new(MyPlugin));
```

`Deps` 的实现：

| 写法 | 门控 | 读取 |
|---|---|---|
| `Arc<T>` | 是 | `ctx.require::<T>()`（门控已开，正常必有；失败即装载失败） |
| `Option<Arc<T>>` | 否 | `ctx.get::<T>()`；出现或消失不触发重载，与现在"不声明、apply 里 get"一致 |
| `()`、至多 8 元组 | 各成员之和 | 依次读取，首个错误返回 |

`gates()` 用于"等它到位再启动，但不需要它的值"（如就绪标记），与 `Deps` 的键合并去重。

## 宣称边界

`apply` 仍拿到完整的 `Ctx`。只能说"经 `Deps` 取得的依赖在编译期对齐"，
不能说"依赖错误都会编译失败"。

## 尚未覆盖（后续版本）

issue 要求一并设计，第一版先不做，等形状定下来再补：

- **限定名（named）与多实例（instance 键）**：键不是类型，需要一个携带键的类型，
  例如 `Named<T, K>`（`K` 为提供 `const`/函数键的标记类型），`injects` 用 `TypeKey::keyed`。
- **isolate**：读取按作用域解析，`require` 已处理；需确认 typed 路径下的声明位置与
  `require_as` 的祖先规则一致，并补测试。
- **check 门控**：`check` 由提供方注册，消费方门控已自动包含，预计无需新接口，需补测试。
- **配置热更新**：`PluginFactory` 构造的插件能否是 typed（`build` 返回 `Typed<P>` 即可），需补测试。
- **诊断**：可选依赖经 `get` 读取，在诊断里记为"未声明的访问"；是否标注为 optional 待定。

## 待讨论

1. 包装方式：`Typed::new(p)` 显式包装，还是 `ctx.plugin_typed(p)`。前者不增加 `Ctx` 方法，
   当前采用。不能用 blanket `impl<P: TypedPlugin> Plugin for P`：`injects()` 要返回借用切片，
   需要实例里存一份。
2. 必需依赖读取失败时（门控通过到 `apply` 之间被驱逐）当前返回错误、fiber 进 Failed；
   该窗口内驱逐本身也会触发卸载重载，是否应改为回到 Pending。
