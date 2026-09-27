# rutis 0.3 → 0.4：事件键、模式订阅与同步调用

0.4.0 是本分支准备的下一个版本，尚未发布。它统一事件接口，增加模式订阅与同步决策点；服务的 `TypeKey`、依赖声明、现有服务拦截保持原接口。

## 把事件身份作为参数

默认、命名和实例通道使用同一组方法：

```rust
let default = EventKey::<Ping>::of();
let named = EventKey::<Ping>::dynamic(format!("room/{id}"));
let private = named.clone().instance(ctx.instance());

bus.on(&ctx, &default, listener)?;
bus.on(&ctx, &named, listener)?;
bus.on(&ctx, &private, listener)?;
bus.emit(&ctx, &named, Arc::new(event))?;
```

| 0.3 | 0.4 |
| --- | --- |
| `on(ctx, listener)` | `on(ctx, &EventKey::of(), listener)` |
| `on_keyed(ctx, name, listener)` | `on(ctx, &EventKey::dynamic(name), listener)` |
| `on_instance(ctx, id, listener)` | `on(ctx, &EventKey::of().instance(id), listener)` |
| `on_opt(ctx, listener, opts)` | `on_opt(ctx, &key, listener, opts)` |
| `once(ctx, listener)` | `once(ctx, &key, listener)` |
| `emit(ctx, event)` | `emit(ctx, &key, event)?` |
| `parallel(ctx, event).await` | `parallel(ctx, &key, event).await` |
| `serial(ctx, event).await` | `serial(ctx, &key, event).await` |
| `waterfall(ctx, event, terminal).await` | `waterfall(ctx, &key, event, terminal).await` |

Rust 不能按参数数量重载同名方法，因此原默认方法改为接收事件键。原 `*_keyed` / `*_instance` 方法保留为弃用包装。实例 waterfall、once 和 prepend 现在也通过同一键接口使用。

`emit` 返回是否接收了本次调用；实例越界 / 关闭会返回错误。返回成功不表示监听器已经执行完；监听器执行失败仍交给 ErrorSink。非实例派发保留旧上下文的历史行为；`ctx` 必须属于这个总线。库内状态通知、agent 通知和宿主事件转发无法入队时，也把错误交给 ErrorSink。

`EventKey::named` 可用于 const 静态键且不分配。静态 / 动态同名等值，默认键不等于任何命名键。身份仍包含类型和实例号；名字自由构造不会检查拼写，业务接口应导出键常量或构造函数。

`EventOptions` 新增 `once` 字段。旧代码直接构造该结构体时，需要补 `once: false` 或 `..Default::default()`。例如，保留原 prepend 设置用 `EventOptions { prepend: true, ..Default::default() }`；只调用一次用 `EventOptions { once: true, ..Default::default() }`。`on_waterfall_opt` 同样支持 once。once 在派发取得快照时被认领，前面的监听器可能中止后续处理，使它没有实际执行；语义是至多一次。

## 一次订阅一组动态名字

```rust
bus.on_pattern(&ctx, EventPattern::<RoomEvent>::prefix("room/"), listener)?;
bus.on_pattern(&ctx, EventPattern::any_prefix(["room/", "room/special"]), listener)?;
```

`PatternListener<E>` 接收实际命中的 `EventKey<E>`、借用的载荷与发送方 Ctx。键按值传入，动态名复用 Arc；监听器可以保存这个键，在回调返回后继续使用。

模式仅匹配同事件类型的显式命名、非实例通道；空前缀匹配全部此类通道。实例隔离不被模式绕过。一个注册的重叠前缀只选择一次；同一回调独立注册两次仍是两份订阅。

精确与模式监听器共享注册顺序，prepend 放到所有匹配监听器之前。`on_pattern_opt` / `on_waterfall_pattern_opt` 支持 prepend 和 once。模式 once 在所有命中名字之间共享一次认领，多个线程不能各自认领。

emit 的尾链仍按实际完整键建立：同名保序，跨名可并发调用同一个模式监听器，不提供跨名字的总顺序。模式卸载先移除订阅，再等待已接纳的回调；关闭后不再接纳新模式回调。非实例精确异步监听器仍保留原快照 / 卸载行为。

模式回调可以启动自己的卸载后返回，不能在回调内 await 等待包含自身的卸载完成；只需调用一次时用 `once` 选项。

`bus.subscriptions()` 返回当前精确 / 模式注册、owner、回调种类和前缀组。模式的 selected / invoked 计数区分取得快照和实际调用；精确注册的计数为 None，以免常规派发增加原子统计成本。不保存全部历史动态名字，也不记录业务载荷。

## 需要当场拿到结果时使用同步调用

事件类型额外实现 `SyncEvent`，注册普通函数监听器：

```rust
impl SyncEvent for BeforeSave {}
bus.on_sync(&ctx, &key, listener)?;
let decision = bus.bail_sync(&ctx, &key, &event)?;

bus.on_waterfall_sync(&ctx, &key, middleware)?;
let candidate = bus.waterfall_sync(&ctx, &key, &event, |_, event| Ok(event.candidate))?;
// 检查 candidate，再执行实际写入。
```

bail 按注册 / prepend 顺序返回第一个 Some；waterfall 的 `SyncNext::call(self)` 包裹下游结果，不替换输入，未调用 next 即拦截。续延不能重复调用或逃逸为 'static。异步注册与同步注册是不同的表，不会互相调用；SyncEvent 是附加能力，不禁止该类型使用异步 API。

同步终点是本地 `FnOnce`，可以借用栈变量或 MutexGuard，不要求 Send / 'static。监听器仍要求 Send + Sync + 'static。总线在用户回调期间不持框架锁，但回调不得再次取得调用者已经持有的同一个业务 Mutex。

可运行例子：[sync_decision.rs](../crates/rutis/examples/sync_decision.rs)。调用方持锁读旧值，在终点生成候选值，监听器改写返回值，调用方校验后仍在同一个临界区内提交：`cargo run -p rutis --example sync_decision`。

普通错误原样返回。监听器 / 终点 panic 被捕获，报告一次 ErrorSink，并通过 `CordisError::SyncEventPanicked` 返回；ErrorSink 自己的 panic 也被隔离。投递尝试观察器沿用原契约，panic 报告后继续；`DispatchMode` 枚举新增 `BailSync` / `WaterfallSync` 两个值。

若业务穷举匹配 `DispatchMode` 或 `CordisError`，需补齐新增分支。同步派发是 Rust 本地调用，不增加桥协议的远程派发模式。

同一线程中、同一总线和完整键的同步重入返回 `ReentrantEvent`，覆盖观察器、监听器、终点和 sink；不同键 / 实例 / 总线可嵌套。跨线程并发不会由此变成全局串行执行。

同步普通 / 模式注册都支持 EventOptions。模式对应 `on_sync_pattern` 和 `on_waterfall_sync_pattern`，回调也接收实际命中键。

## 生命周期与已有拦截

同步快照与在途登记在同一 admission 临界区完成；关闭、普通卸载、重启和装载失败回滚都等待已接纳调用退出，包括没有监听器的同步终点。旧代 / 失活 / 关闭的同步上下文被拒绝。回调可以发起自己的 shutdown 后返回，不能阻塞等待包含自身在途计数的卸载完成。

等待沿用 owner 级计数，可能包括同一 owner 的其他回调。保护覆盖同步派发调用栈，不包括返回后的业务提交；服务更新继续复查 provider、generation 与绑定身份。

现有服务拦截按注册顺序把改写后的值交给下一项。事件 waterfall 中，下游仍收到原始输入，返回值按回调嵌套顺序向上返回。因此服务拦截保留原实现。

## dylib SDK 与基准

rutis 的破坏性 API 版本准备为 0.4.0；共享 SDK 版本准备为 0.2.0。SDK 身份随版本与锁内依赖变化，宿主和插件必须一起重编，不能把新产物当成旧 SDK 的替代文件。现有预执行和 pre-dlopen 校验保持原路径。

```sh
cargo bench -p rutis --bench events
```

基准报告同步空链 / 1 / 8 / 64 监听器、实例空链，以及异步精确派发与 0 / 1 / 8 / 64 / 1024 个前缀。同步空链没有 future / spawn，仍需检查上下文、阻止重入并记录正在执行的调用。基准结果是本机样本，不作为跨机器的固定延迟保证。

本机结果、测试方法与新旧精确派发对比见 [性能样本](performance-event-dispatch-2026-09-27.md)。
