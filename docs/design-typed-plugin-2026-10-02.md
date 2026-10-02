# TypedPlugin：类型化依赖（草案）

状态：草案，第一版已实现（#50）。挂载方式与依赖丢失处理已定（见文末）。日期：2026-10-02。

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
| `Arc<T>` | 是 | `ctx.require::<T>()`；门控之后被撤掉则回到 Pending（见文末） |
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

## 已定

1. **包装方式**：`ctx.plugin(Typed::new(p))` 显式包装，不给 `Ctx` 加方法。不用 blanket
   `impl<P: TypedPlugin> Plugin for P`：`injects()` 返回借用切片，需要实例里存一份。
2. **门控通过后、取用前依赖消失**：回到 Pending，不进 Failed。
   - typed 读取遇到 `Unavailable` 时返回 `CordisError::InjectUnsatisfied`（该变体此前未被使用）。
   - 内核规则：apply 返回 `InjectUnsatisfied` 且此刻确有依赖缺失 → 这一代回滚（清理照常
     LIFO 排干，清理错误进 ErrorSink）并回到 Pending；依赖回来时照常装载。
   - 依赖其实齐全时不认这个理由，按普通失败进 Failed，避免装载循环。
   - 非 typed 插件也可以主动返回这个错误得到同样效果。
   - 与 Cordis 的差异：Cordis 在此窗口内 apply 抛错会进 FAILED（依赖回来时重试）；
     rutis 只对这个明确的错误变体放宽，其它错误仍是粘性 Failed。
