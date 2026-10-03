# 把 Cordis 运行时做成 rutis 插件（2026-10-03）

依据：[需求](requirements-protocol-plugins.md)（rutis 是宿主）、[loader 设计](design-rutis-loader-2026-10-02.md) P6、PR #101（`InteropResolver`，尚未合并）、[决策记录](decision-multilang-2026-10-03.md)。

状态：已决定实施，在 PR #101 合并前完成。

## 1. 问题

P6 把 JS 插件做成了 loader 行，每行是一个 rutis 插件。但所有行共用的 Node 进程和 Cordis Context 不是插件，而是放在 `InteropResolver` 的 `OnceCell` 里：

| 现状 | 后果 |
| --- | --- |
| 第一次 `resolve` 时才启动进程，之后一直存活到 Resolver 被丢弃 | 进程的启停不受 rutis 生命周期管理，没有 fiber，诊断里看不到它 |
| 进程崩溃后，行继续持有失效的 `Arc<Process>` | 没有任何插件进入失败状态，各行在下一次调用时才拿到 `Transport` 错误 |
| 宿主服务由 `with_hosts(Vec<Host>)` 一次性传入，`Host` 里的 dispatch 已在构造时取好 | 宿主服务不参与依赖门控；撤销或替换后 JS 一侧不会跟随 |
| 行的工厂直接捕获 `Arc<Process>` | 行与运行时之间的依赖是隐式的，不在 inject 图里 |

静态挂载（build.rs 生成的挂载插件）没有这些问题，因为它自己就是一个插件：apply 时启动进程，在 effect 里清理，宿主服务写在 `injects` 里。本文让动态这条路也照这个做。

## 2. 方案

```text
root
 ├─ CordisRuntime（插件）      apply：Process::mount(anchor, hosts) → provide CordisRuntime 服务
 │    injects: 宿主服务        effect：process.dispose()
 └─ LoaderPlugin
      ├─ 行 "@x/a"（JsRow）     injects: CordisRuntime → apply：runtime.load_row(…)
      └─ 行 "@x/b"（JsRow）     injects: CordisRuntime
```

**`CordisRuntime` 插件**（放在 rutis-interop，仅 Unix，不依赖 loader）：

- **配置**：`node_package`、`anchor`、宿主服务列表（名字、方法清单、`TypeKey`）。
- **injects**：宿主服务的 `TypeKey`。宿主服务没就绪时，运行时停在 Pending；宿主服务撤销后，运行时按原生规则卸载并重新等待。这和静态挂载 §5 的语义一致：撤销后整个运行时重启，不做就地替换。
- **apply 依次执行**：
  1. 用 `ctx.require_as` 取宿主服务，构造 `Host`；
  2. 调 `Process::mount(.., Mount { anchor, hosts, .. })`；
  3. 先登记清理 effect（`process.dispose()`），再 `provide` `CordisRuntime` 服务（内含 `Arc<Process>`）。

  这样清理时会先撤服务、让各行卸载，最后才关进程。
- **崩溃**：apply 时起一个任务等待 `process.closed()`。进程不是由 dispose 结束的，就用 `provide` 返回的 `Disposer` 撤销服务。
  - 各行失去依赖，按原生规则回到 Pending；
  - 运行时插件本身保持 Active 但不再提供服务，与静态挂载 §6 的“崩溃后的服务”一致；
  - 是否重启由应用决定，例如调用该 fiber 的 `restart`。兼容层不自动重启。
  - 诊断里能看到运行时缺失服务，各行的 Pending 原因指向它。

**行（JsRow）**：

- `injects` 包含 `TypeKey::of::<CordisRuntime>()`；apply 时从 `ctx.require` 取进程，不再在工厂里捕获。
- volatile 更新的转发同样经由这个服务。
- 卸载顺序由依赖保证：各行先卸载，运行时后关闭。

**Resolver**：

- 名字解析（`resolve_entry`）本来就在 Rust 侧完成，不需要进程。
- 只有 schema（`row_schema`）要问 Node。为此 Resolver 持有运行时插件提供的一个句柄（`CordisRuntime::handle()`，内部是 watch 通道），`resolve` 等运行时就绪后再取 schema。
- 约束：运行时插件由应用在 reconcile 之前装上（它是基础设施，与 `LoaderPlugin` 同级），不能作为同一个 loader 里的一行；否则 resolve 等运行时、运行时等 reconcile，会互相等待。运行时 apply 失败时，句柄返回错误，转成 `LoaderError::Resolve`。

**应用侧用法**：

```rust
let runtime = root.plugin(CordisRuntime::new(CordisRuntimeConfig {
    node_package, anchor,
    hosts: vec![HostSpec::of::<dyn SystemPromptHost>("systemPrompt", methods)],
}));
let resolver = Chain::new(builtins).then(InteropResolver::new(runtime.handle()));
root.plugin(LoaderPlugin::new(resolver, options)).await?;
```

## 3. 不变的部分

- 静态挂载与 build.rs 代码生成不变。
- 协议、runner.mjs 和行相关的控制操作（`rows.*`）不变。改动只在 Rust 侧的归属关系。
- 多个运行时实例：每个 `CordisRuntime` 是一个独立的 Node 进程和 Context。想要隔离，就用不同的服务键（`TypeKey::keyed`）挂多个运行时；行要选择挂到哪个运行时，留待有需求时再做。
- 反方向（Cordis 宿主挂载 rutis 插件）继续冻结。

## 4. 验收

在 PR #101 的 `crates/rutis-loader/tests/interop_rows.rs` 基础上补充：

1. 运行时插件卸载时，各行先卸载，进程最后退出。
2. 杀掉 Node 进程后：运行时撤销服务；各行回到 Pending，诊断能指出缺的是运行时；调用运行时 fiber 的 `restart` 后各行恢复。
3. 宿主服务未提供时，运行时与各行都处于等待；提供后全部启动；撤销后全部停止。
4. 运行时 apply 失败时，`resolve` 返回 `LoaderError::Resolve`，不会卡住。
5. P6 原有测试全部通过。

## 5. 待决

- 崩溃后是只撤服务、保持 Active（与静态挂载一致，本文的选择），还是让运行时进入 Failed？后者需要内核提供“插件自报失败”的入口，与“内核不为兼容而修改”冲突，所以不选。
- 句柄放在 `CordisRuntime` 上还是作为服务暴露：取决于 Resolver 是否会改成由插件提供（另议）。
