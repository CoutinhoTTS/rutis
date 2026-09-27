# #62 / #63 研究：类型化事件键、模式订阅与同步派发

日期：2026-09-27。本文保留开工前的研究结论；同日已按用户指令实施。实际接口与语义见 [0.4 迁移说明](migration-0.3-to-0.4.md)，其中模式回调采用按值传入的 `EventKey<E>`，模式命中计数默认开启，精确订阅不计数。

基线：rutis main `603f8220b049c6ad26e82f258fd72f214b8575fe`，本地与远程一致；Cordis 本机快照 `f8ea3cd50f1a5724e8e715995bcde131c9c12b2c`。已读取两个 issue 的最新正文，研究时均无评论。

## 结论

两项都有实际用途，建议推进。#62 应拆成「统一键」与「模式订阅」两个实现阶段；#63 依赖统一键，不必等模式订阅全部完成。无需重写四种异步派发的业务语义。

目前正文还不能直接作为实现契约。#62 缺少命中键的传递方式、完整的顺序与 once 定义、统一 emit 的错误签名和可执行的迁移方式；#63 缺少 bail 的监听器注册面、同步终点的借用约束、重入边界与在途准入的完整定义。#63 关于复用 #29 的说法需要修正：固定输入 waterfall 与当前逐步替换值的拦截链并不等价。

| 议题 | 建议 |
| --- | --- |
| 事件身份 | 保留 `(TypeId, 可选名字, 可选 InstanceId)`；同名不同类型继续互不串扰 |
| 默认类型键 | 显式 `EventKey::<E>::of()`，维持没有名字的原身份；不改成 `E::NAME` 或类型名字符串 |
| 模式 | 首版为按事件类型匹配的字符串前缀，仅匹配显式命名、没有实例号的键 |
| 模式回调 | 必须收到实际命中的类型化键 |
| 监听器顺序 | 推荐精确与模式共享注册 / prepend 顺序；若采用 issue 的精确优先方案，须明确 prepend 只在各组内生效 |
| 同步类型约束 | 首版把 `SyncEvent` 定义为支持同步派发的能力声明；同步入口严格要求普通函数监听器 |
| 同步终点 | 接受本次调用栈上的 `FnOnce`，不要求 `Send + 'static` |
| 生命周期 | 复用 admission、effect 和 fiber 在途计数；快照与计数必须在同一准入临界区完成 |
| #29 | 保留现有服务拦截语义；可复用内部生命周期工具，不在本批迁移到固定输入 waterfall |
| 性能 | 撤下未经测量的 ns / μs 数字，建立代表性基准后再决定索引与缓存 |

## 现有实现与 Cordis 对照

### 内核已有统一的存储键

[`key.rs`](../crates/rutis/src/key.rs) 的 `TypeKey` 已经包含类型、限定名和实例号；静态与动态名字按字符串内容等值。服务与事件共同使用这一内部键，但公开事件入口不接受任意 `TypeKey`。

[`bus.rs`](../crates/rutis/src/bus.rs) 当前有 21 个业务注册 / 派发入口，另有 `observe_dispatch`。问题主要是公开签名重复，内部 `add_hook`、`emit_keyed_inner` 等已经共享实现，不是存在三套完全独立的总线。

支持仍不完整：没有实例 waterfall 的公开入口，也没有实例 once / prepend 的完整注册面。#62 的验收因此既有 API 收敛，也有补齐行为，后者必须单独验证生命周期。

实例派发不能简单转发到现有非实例实现。当前 `parallel_instance` 通过独立 runner 保证等待 future 被丢弃后，已接纳的派发仍执行完并持有 flight；`serial_instance` 在借用 future 被丢弃时释放 flight。统一键时必须保留这种差别。

### 对原设计前提的修正基本成立

Cordis 的字符串名字通过 `Events` 的键绑定参数 / 返回类型，所以「名字参与身份必然失去类型安全」不成立；rutis 也可以用 `EventKey<E>` 绑定键与载荷类型。[Cordis events.ts](https://github.com/cordiverse/cordis/blob/f8ea3cd50f1a5724e8e715995bcde131c9c12b2c/packages/core/src/events.ts#L17-L32)

不过，自由构造的 `EventKey::<E>::named("拼错的名字")` 仍然会编译。这比 Cordis 有限的静态 `keyof Events` 名字集合弱一层。它保证「键的 E 与载荷 E 一致」，不自动保证「名字正确」或「每个名字只有一种类型」。静态业务事件应由接口 crate 导出键常量 / 构造函数。

Cordis 的 `bail` 在运行时没有 await：返回 Promise 的监听器会立即交回 Promise，并不会等待它的解析结果再决定是否继续。`internal/update` 的类型也是 `Awaitable<void>`，终点可返回异步重启任务；它具有同步的决策阶段，不能作为整个更新流程必然同步的证明。Rust 应明确定义自己的普通函数监听器契约，而不是机械翻译 `ReturnType`。[Cordis events.ts](https://github.com/cordiverse/cordis/blob/f8ea3cd50f1a5724e8e715995bcde131c9c12b2c/packages/core/src/events.ts#L104-L134)、[fiber.ts](https://github.com/cordiverse/cordis/blob/f8ea3cd50f1a5724e8e715995bcde131c9c12b2c/packages/core/src/fiber.ts#L478-L496)

同步决策点确实存在。当前 rutis 的投递观察和服务拦截已经证明：普通函数回调可以纳入 effect 与关闭等待；全面异步不是所有用途的必要约束。[现有观察与拦截设计](design-cordis-observation.md)

## #62：建议冻结的契约

### 键的形状与身份

以下是实现前的接口草图。现已按此接口实现，当前用法见 [迁移说明](migration-0.3-to-0.4.md)。公开入口接收 `&EventKey<E>`：

```rust
const DEFAULT: EventKey<RoomEvent> = EventKey::of();
const ROOM: EventKey<RoomEvent> = EventKey::named("room/main");

let room = EventKey::<RoomEvent>::dynamic(format!("room/{id}"));
let scoped = room.clone().instance(ctx.instance());

bus.on(ctx, &room, listener)?;
bus.emit(ctx, &room, Arc::new(event))?;
bus.serial(ctx, &room, &event).await?;
bus.waterfall(ctx, &scoped, &event, terminal).await?;
```

键字段私有；进入内部存储时由 E 生成 `TypeId` 并擦除为 `TypeKey`。如果提供从 `TypeKey` 恢复类型化键的入口，必须检查类型，不提供不经检查的转换。

保留默认键的 `name = None`。以诊断用的 `Event::NAME` 代替原默认键会把显式命名通道与原类型通道合并；以 `type_name::<E>()` 代替身份也没有必要。名字按原始字符串内容比较，大小写敏感，不进行路径规范化；静态与动态同名等值；实例号仍是身份的一部分。

仓库声明 MSRV 1.85，而 `TypeId::of` 的 const 稳定版本是 1.91。为了支持 `const EventKey::named`，公开键可只保存名字表示和类型标记，使用时再生成内部 `TypeId`，不能直接把当前 `TypeKey` 包进 const 构造器。类型标记建议保持 E 不变型，避免类型擦除与泛型变型产生额外约束问题。[Rust TypeId 文档](https://doc.rust-lang.org/std/any/struct.TypeId.html)

### 模式监听器必须拿到命中键

当前 `Listener<E>` 只收到 `Ctx` 与 `E`。若多个房间共用 `RoomEvent` 且载荷不重复保存房间名，按 `room/` 订阅后无法判断来自哪个房间。现有 `HostEvent` 在载荷内保存 name 是桥的具体选择，不应成为所有事件的隐含要求。

推荐增加类型化的 `PatternListener<E>` / waterfall 对应适配器，其回调多接收 `&EventKey<E>`；精确监听器仍可使用现有载荷形状。注册适配器最终进入同一派发快照。若决定所有监听器都接收投递元数据，应在统一键的破坏性迁移中一次完成。

前缀仅匹配同类型、显式命名、没有实例号的键。空前缀可定义为该类型的全部此类命名键；它不匹配 `of()` 或实例键。`room/` 是普通字符串前缀，不是 glob / 正则，也不是实例子树选择器。

模式不改变现有普通事件的 isolate 行为。实例键不会落入全局模式表；「订阅所有房间」只适用于这些房间原本采用非实例命名键的场景，不能同时声称它覆盖所有实例事件。

### 顺序、去重与 once

建议精确与模式监听器共享一个顺序：普通注册追加，prepend 插在全部已匹配监听器前。这样前缀中间件也能明确包裹某个精确 waterfall，且 prepend 保持直观含义。可用注册序号与 prepend 标志合并两份有序快照，不改变无模式时精确监听器的相对顺序。

issue 提议的「先精确、后模式」同样可实现，但会让 prepend 模式监听器仍排在所有精确监听器后。若选择它，必须把这一限制写入文档和测试，不能同时承诺跨组的 prepend。上述全局顺序仅指一次派发内的匹配顺序，不引入跨键的全局执行尾链。

去重必须按注册身份，不能按闭包地址或 `Arc` 回调指针。建议一个 `EventPattern::any_prefix([...])` 表示一次注册的多个前缀，共享一个 HookId；一次派发命中多个前缀时仍只选择一次。同一闭包分别执行两次 `on_pattern` 是两次独立注册，各自调用一次。

模式 once 是整个注册最多被一个派发快照认领一次，不是每个匹配名字各一次。两个名字并发派发时，认领与所有匹配索引的移除在同一个总线锁内完成。

现有 `claim_once` 在选取快照时就移除全部 once 条目；后续 serial 短路或 waterfall veto 可能使其中一些没有实际执行。它保证至多一次，不保证每个条目实际执行一次。首版建议保持这一语义，并补充「被前置监听器短路」的测试；若要改成实际调用时认领，需要单独设计，不作为模式订阅的隐性行为变化。

### 尾链与快照

匹配模式后，emit 仍按被派发的完整键进入 `dispatch_tail`。只有模式监听器、没有精确监听器时也必须建立该键的尾链。相同前缀下不同名字可并发调用同一个模式监听器，不承诺观察者收到所有名字的全序。

精确与模式条目的选择、去重和 once 认领要形成同一快照。调用任何监听器时释放总线锁；实例路径同时保留 admission 与 flight 的原子接纳。注册 / 卸载发生在快照之后时，遵守所选快照契约，不在每次回调前重新拼接链。

首版可以按类型分桶扫描前缀，成本是该类型前缀条目数量和总比较长度，不是固定的几十 ns。先测 0 / 1 / 8 / 64 / 1024 条模式的命中与不命中；测量证明必要后再增加 trie 或缓存。动态名字很多时，尾链完成与卸载后必须释放相应条目，不用「全部历史名字」缓存换取表面查找速度。

诊断显示精确键 / 模式、owner、注册身份、模式数量与所选监听器。匹配数、被选次数和实际调用数是不同指标，尤其 serial / waterfall 会短路。前置观察器只知道 DispatchAttempt，不能据此填报实际送达结果。总线也不能枚举尚未出现的所有动态名字。

### 错误签名与版本迁移

统一的 emit 建议返回 `Result<(), CordisError>`：实例关闭或越界的同步拒绝不能丢失；接纳后的监听器错误仍进入 ErrorSink。它表示已接纳，不表示监听器完成，也不等同于 #43 的背压接口。

Rust 不支持根据参数数量重载方法。旧 `on(ctx, listener)` 与新 `on(ctx, key, listener)` 不能同时以同一个 inherent 方法名存在。因此「所有旧接口都保留一个弃用期」不可直接成立。

建议在下一个 minor 版本统一改默认入口并迁移工作区调用者；名字不冲突的 `*_keyed` / `*_instance` 可作为弃用包装保留一个版本。若确实需要无破坏的准备阶段，先提供不同名称的显式键入口，再在 minor 版本改最终方法名。

迁移需覆盖 rutis-agent、HostEvent 桥、文档和示例；普通异步非实例派发的历史上下文行为也需要保留或明确申报变化，不能借 API 收敛顺便改变。rutis / 接口变化涉及已验证 dylib SDK 时，要发布新的 SDK 身份并重编对应产物。

## #63：建议冻结的契约

### 同步性与注册面

`SyncEvent: Event` 只是附加能力约束。它不会禁止把同一个 E 注册到现有异步 `on`，也不会使 `bus.waterfall` 自动变成同步函数。本研究建议首版接受这一能力模型：同步入口要求 E 实现 SyncEvent，并只接收同步监听器；不要宣称事件类型已经互斥地决定全部派发方式。

若业务目标是强制互斥，应采用关联派发种类或独立 Sync / Async 事件 trait，连同现有 Event 实现一起迁移。不能通过约定隐含这一保证。

需要补齐两套不同的同步回调形状：

```rust
trait SyncListener<E: SyncEvent>: Send + Sync + 'static {
    fn call(&self, ctx: &Ctx, event: &E)
        -> Result<Option<E::Value>, CordisError>;
}

trait SyncWaterfallListener<E: SyncEvent>: Send + Sync + 'static {
    fn call<'a>(&'a self, ctx: &'a Ctx, event: &'a E, next: SyncNext<'a, E>)
        -> Result<E::Value, CordisError>;
}
```

相应公开注册为 `on_sync` 与 `on_waterfall_sync`，派发为 `bail_sync` 与 `waterfall_sync`，均接收 `EventKey<E>`。只注册同步 waterfall 不能让 bail 自动找到 `Option<Value>` 监听器。once / prepend 的选项与快照规则应明确复用，不再按名字和实例增加同类方法。

同步回调不会装箱 future，也不会由总线 spawn；同一键的两个线程可同时执行回调，不新增每键同步执行锁。重入限制是调用栈约束，不是跨线程串行化。跨进程协议插件也不能自动成为此同步监听器；它们需要异步接口或显式业务改造。

### 续延和终点的借用

`SyncNext::call(self)` 消耗续延，不实现 Clone / Copy；下一层仍接收同一个借用的 E。这样重复调用和把续延保存为 'static 都可以在编译期被拒绝。

同步终点应接受 `FnOnce(&Ctx, &E) -> Result<E::Value, CordisError>`，允许借用本次调用栈，不添加现有异步 `Terminal` 的 `Send + 'static` 约束。实现可借用一个本地终点适配器，不需要装箱该闭包。

探针已经验证：终点可以捕获当前持有的 `MutexGuard` 和栈上可变计数器，监听器可包裹 / veto；这一结论只验证 Rust 接口形状，不表示生产总线已经支持它。

### 准入、关闭与重入

推荐执行顺序：

1. 检查调用上下文与键；新同步入口拒绝关闭、失活和过时代上下文，维持明确的错误优先级。
2. 为 `(当前总线身份, 完整事件键)` 建立线程局部 RAII 重入保护。
3. 运行已有投递尝试观察器；之后再次在 admission 下校验并取得业务快照。观察器中的注册可影响快照，观察器中的 shutdown 可使本次业务派发被拒绝。
4. 在同一 admission 临界区为 emitter、实例 owner、所选监听器 owner 建立 flight，随后释放所有框架锁。
5. 同步调用链或终点；返回 / 错误 / unwind 时通过 RAII 释放 flight 与重入保护。

重入保护必须覆盖观察器与终点，不能只在业务监听器前建立，否则观察器可先触发同键无限递归。身份包括总线，避免独立 root 中的同名同类型事件误相互阻断；同一个总线、同一个完整键在 bail 与 waterfall 间互相重入也应拒绝。不同键或实例可嵌套；不同线程的普通并发不被 TLS 拒绝。

无监听器时，waterfall 的终点仍是用户同步代码；若要求关闭等待本次同步派发，终点执行也要计入 emitter / 实例 owner 的 flight，不能直接绕过生命周期。准入还应排除已进入卸载状态的监听器，不能仅检查 owner Weak 是否可升级。

撤销过程先在同一 admission 边界移除注册，阻止新快照选中，再异步等待已接纳的调用退出。已有 owner 级 wait_events 可复用，但它可能同时等待该 owner 的其他在途调用，不应宣称是每个监听器独立排空。同步回调可以发起自己的 shutdown 后返回；不能阻塞等待包含自身 flight 的卸载完成。

框架锁外调用是必要条件，但不保证任意业务 Mutex 用法都不会死锁：回调再次取得调用者已持有的同一个非重入 Mutex 仍会阻塞。同键重入保护也不能解决不同键回调之间的业务锁环。文档中的持锁示例应只通过载荷与终点交换所需数据。

### 错误、panic 与提交位置

监听器返回的 CordisError 原样交回。同步用户调用边界捕获 panic，转为明确错误并报告 ErrorSink；sink 的 panic 不应覆盖返回错误。需要定义终点 panic 的同样行为，并避免同一 panic 穿过多个续延适配层时重复报告。

观察器仍遵循已有契约：panic 被报告，业务派发继续。DispatchMode 需要加入两个同步模式，公开枚举变更归入版本迁移。重入错误保护仍覆盖 sink 中的同键派发。

若用途是「算出最终候选值，再提交」，终点应返回候选值，实际写入放在整个 waterfall 返回且校验完成之后。若终点内部已经提交，外层监听器随后改写返回值或失败，总线不会自动撤销那次提交。

flight 保证覆盖本次派发调用栈；返回后的业务提交不自动纳入该保护。服务写入仍应保留原有 owner、generation 与绑定身份的提交检查。

### 不能直接收敛 #29

当前 [`intercept.rs`](../crates/rutis/src/intercept.rs) 的 `run` 按注册顺序把 replacement 交给下一个拦截器，是逐步变换：

```text
原值 1 → 第一项 +10 → 第二项 ×2 → 最终值 22
```

固定输入 waterfall 中，两个监听器依次包裹 `next()` 的结果时，返回方向相反：

```text
终点返回 1 → 内层 ×2 → 外层 +10 → 最终值 12
```

两者不仅顺序不同：当前后继拦截器看到前驱的替换值，固定输入 waterfall 的后继仍收到原来的 E。改变注册顺序无法一般性地保留载荷可见性、拒绝位置和副作用。

#29 还按完整服务键、有效 isolate scope、调用方祖先链选择钩子，并在提交前复查绑定身份；普通事件监听器并不具备全部这些筛选条件。泛化同步派发不能替代这些服务契约。

如果以后确实需要统一，可保留一个显式「把新值传给下游」的 transform / fold 原语，或只共享 flight、重入与注册清理工具。原文所说的 transform 辅助函数若只是包装固定输入 waterfall，也不能自动解决上述差异。

### 性能声明的边界

实现后的首批基准已完成，覆盖监听器数量、前缀数量、实例空链和新旧精确派发。以下保留原测量计划；短路、veto、观察器数量和注册卸载竞争的性能测量仍待补充。实际结果见 [性能样本](performance-event-dispatch-2026-09-27.md)。

「没有监听器时只有一次查找」与完整的准入 / 在途等待承诺不符：还可能有上下文检查、flight 计数、观察器和重入保护。没有 future / spawn 是结构事实；没有分配、锁成本只有几 ns 等需要实测。

先测无监听器、1 / N 监听器、bail 放行 / 短路、waterfall 包裹 / veto、0 / N 观察器、实例 / 非实例以及注册卸载竞争。若沿用当前 ErasedValue 适配方式，还会装箱返回值；若要移除这部分分配，可在测量后评估按 E 存储类型化 bucket。编译探针的无擦除链不是最终 bus 性能的证据。

## 实施分期与验收

建议先冻结上述键身份、模式顺序、once、错误签名和 SyncEvent 能力模型，再按以下范围提交实现：

| 阶段 | 内容 | 主要验收 |
| --- | --- | --- |
| #62-A | EventKey 与精确入口统一；补实例 waterfall / once / prepend；迁移工作区 | 身份等值、载荷错配编译失败、21 个旧入口的行为映射、实例关闭和取消语义 |
| #62-B | 前缀模式、命中键、顺序合并、注册组去重、模式 once、诊断 | 只有模式时同键仍保序，跨名可并发，重叠前缀只选一次，双线程一次性认领，veto / 短路 / prepend |
| #63 | 同步回调与续延、重入、panic、全部键形态的 admission / flight | Mutex 持锁示例，终点借用，重复 next / 逃逸编译失败，关闭 / 提前卸载 / 重载竞态 |

#63 可以在 #62-A 后推进，不要求 #62-B 完成。但公共选择与生命周期工具应一次定义，避免再生成 keyed / instance 同步 API。#43 的整个背压项目不是前置阻塞；本批至少先记录代表性基准，不承诺未经测量的结果。

新增验证应特别覆盖：观察器同键重入；终点同键重入；不同总线同键合法嵌套；bail / waterfall 互相重入；无监听器终点与 shutdown 竞争；panic 后 flight 归零；旧代上下文被拒；实例兄弟隔离；1000 轮注册、提前卸载和子树关闭无残留。关键竞态用 Barrier / Notify / oneshot 控制，不用 sleep 建立顺序。

迁移后重跑现有 Cordis parity、事件键、尾链、实例子树和观察回归，并核对桥事件与 agent 顺序。#29 服务拦截继续保留现有回归。

## 已执行的验证与限制

现有代码执行：

```sh
cargo test -p rutis --offline --test event_keys --test dispatch_chain_probe --test instance_subtrees --test dispatch_observation --test service_intercepts
```

共 56 项通过：事件键 11、尾链 1、实例子树 23、投递观察 8、服务拦截 13。这验证当前行为基线，不表示 #62 / #63 已实现。未运行全工作区测试，也未测量性能。

独立探针保存在 [probes/event-keys-sync-dispatch.rs](probes/event-keys-sync-dispatch.rs)，运行编译器为 rustc 1.98.1：

```sh
rustc --edition=2021 docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-probe
/tmp/rutis-events-probe

# 下列三个命令均应编译失败。
rustc --edition=2021 --cfg mismatched_payload docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-mismatch
rustc --edition=2021 --cfg double_next docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-double
rustc --edition=2021 --cfg escape_next docs/probes/event-keys-sync-dispatch.rs -o /tmp/rutis-events-escape
```

正常探针通过：默认 / 显式命名的身份区别、静态 / 动态名字等值、同名不同类型隔离、借用 MutexGuard、借用终点局部变量、终点调用一次与 veto、22 / 12 的值流差异。反例分别得到载荷类型错误 E0308、续延移动后复用 E0382、借用逃逸生命周期错误。

同一探针还确认两个限制：自由构造的同类型拼错名字能通过编译；实现 SyncEvent 后仍能传入仅要求 Event 的异步 API。它没有真正的 EventBus、InstanceId、admission、panic 隔离或并发认领，因此不用于证明这些机制已经落地，也不是性能基准。MSRV 1.85 未在本机安装，未进行该版本的编译验证。

研究仅新增本地文档与独立探针，没有改动运行时代码或远程 issue。
