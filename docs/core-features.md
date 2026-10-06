# 内核能力一览

[English](core-features.en.md) · [开发手册](development-handbook.md) · [API 文档](https://docs.rs/rutis)

`rutis` 内核在插件、fiber、服务和事件之外提供的能力，以及它们的使用边界。入门请先看 [README](../README.md) 的快速开始和[应用设计指南](development-guide.md)。

## 配置热更新

运行中改配置,复用状态机的恰好一次清理,受影响的消费者自动跟随:

```rust
struct MyFactory;
impl PluginFactory<MyConfig> for MyFactory {
    fn build(&self, cfg: &MyConfig) -> Result<Box<dyn Plugin>, CordisError> { /* ... */ }
}

let view = ctx.plugin_with(MyFactory, cfg_v1);
view.update(cfg_v2).await?;   // dry-run 不过则现状不动;通过则卸载重载
```

## 动态事件名

运行时才知道名字的事件(宿主事件、脚本注册),类型化事件 + 动态限定名,四分发与生命周期清理免费继承:

```rust
let key = rutis::EventKey::<RoomEvent>::dynamic(name);
ctx.events().on(&ctx, &key, listener)?;
ctx.events().emit(&ctx, &key, Arc::new(event))?;
```

## 模式与同步事件

`EventPattern::prefix("room/")` 可以订阅一组动态名字，监听器收到实际命中的键。需要当场决定时，事件实现 `SyncEvent`，使用 `bail_sync` / `waterfall_sync`；同步终点可借用调用方的局部变量或 MutexGuard。见 [0.3 → 0.5 迁移说明](migration-0.3-to-0.5.md)。

## 投递前观察

`ctx.events().observe_dispatch(&ctx, observer)` 在选择业务监听器前同步调用观察器，零监听器时也会调用。观察器只看注册 fiber 子树内的投递，随该 fiber 清理；`DispatchAttempt` 包含完整事件键、分发模式、发射方和借用的事件。它记录投递尝试，不提供拒绝投递的返回值。

## 清理树

`ctx.effect_named("cache watcher", || effect)` 为清理项命名；`fiber.effects()` 返回当前 fiber 的只读清理树。复合 `Effect::Many` 保留真实嵌套，正在清理的记录显示为 `Draining`，完成后从树中移除。插件装载、服务注册和事件监听会自动生成标签。

## 服务读写拦截

`ctx.intercept_require_as::<T>(key, hook)` 只作用于通过声明、实例和就绪检查的严格读取；显式 `get_as` 仍直接定位服务。`ctx.intercept_set_as::<T>(key, hook)` 可处理可写服务的更新。`provide_mut_as` 返回绑定代次限定的 `ServiceWriter<T>`，由提供者使用 `writer.set(&ctx, Arc::new(value))` 更新；旧 `Arc<T>` 快照保持旧值，写入不自动重载消费者。拦截器可替换同类型值或拒绝操作，因此只应注册可信代码。

## 使用边界

`require/require_as` 是严格读取，对应 Cordis 普通插件访问服务时的声明检查；它沿 fiber 祖先链核对 `injects()`，并区分未声明、未就绪、实例越界和上下文失活，错误保留调用位置。同一次读取同时越界且上下文失活时，先报实例越界；登记和实例派发的错误优先级单独定义。`get/get_as` 对应 Cordis 显式 `ctx.get()` 定位器：返回 `Option`，不强制依赖声明。provider 未 Active 或读取方正在卸载时通常不可见，provider 子树在清理期间仍可读取自己提供的服务。实例键另有子树可见性检查。监听器由注册时传给 `on` 的 `Ctx` 持有，回调参数 `Ctx` 来自发送方；回调要给注册插件登记资源时，应捕获注册方的 `Ctx`。可编译示例见 [listener_ctx_ownership.rs](../crates/rutis/examples/listener_ctx_ownership.rs)。

## 依赖诊断

`ctx.diagnostics()` 列出存活 fiber 的身份、状态、声明与已绑定依赖，以及服务绑定。`injects[].status` 可区分缺失、实例越界、provider 未就绪、摘除中、check 拒绝或 panic；`TypeKey::describe()` 给出类型名、限定名和实例号。读取只使用已登记的元数据与最近一次门控检查结果，不调用插件或 check，也不触发生命周期转换。它逐个读取 fiber 和绑定，**不是全树原子快照**：并发生命周期变化时，同一结果中的状态、依赖和绑定可能来自不同时刻。check 状态可能停留在上次门控结果；调用 `refresh()` 后应等待相关 fiber 收敛，再重新读取诊断。Cordis 原生的投递观察、清理树和服务拦截差异见 [设计草案](design-cordis-observation.md)。

## 错误与 panic

`apply` 的同步及异步 panic 会变成插件错误；`check()` panic 视为依赖未就绪；`waterfall` 回调 panic 向调用方传播。`settle` 仅是该 fiber 的 FIFO 栅栏，Pending 也可能是稳定结果。root 的 `dispose()` 后仍可重启，`shutdown()` 是最终关闭；丢弃等待 future 不会停止已启动的清理。`update(config)` 重新装配插件，不替换进程中的代码。提前 `Disposer::dispose()` 的失败立即返回给调用方，不自动通知 ErrorSink；`Ctx::take_cleanup_errors()` 可取走并释放这些历史错误。未取走的错误在终态卸载时进入结果，在重载时交给 ErrorSink。

## 与 Cordis 的关系

rutis 是 [Cordis](https://github.com/shigma/cordis) 范式的 Rust 惯用实现,不是翻译:96 个原版 spec 逐条审阅,58 个语言无关不变量全部自动化对拍;其余差异全部显式声明(决策表 + 不移植清单 + 对照审计)。已知的刻意强化:跨 effect 清理严格串行 LIFO(cordis 并发)、emit 同键保序显式重建。
