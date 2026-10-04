# 多语言插件 M1：在 Node 运行时上补齐运行时契约（实施设计稿）

状态：设计稿，未实现。日期：2026-10-04。
依据：[多语言插件：每种语言一个运行时插件](design-multilang-runtimes-2026-10-03.md)（#121，下称"总体稿"）、[Cordis 运行时插件化](design-cordis-runtime-plugin-2026-10-03.md)（#109）、[rutis-loader 设计](design-rutis-loader-2026-10-02.md)。
基准：`main` `4da1f7a`。

## 一、这份稿子做什么

总体稿定了模型：一种语言一个运行时插件、一个进程；其他语言的插件是 loader 的行，只做叶子；范式只在 rutis 里有一份。它把第一步定为 M1：先在现有的 Node 运行时上把运行时契约补齐，再做 Python（M2）。

本文把 M1 落到代码上：每一项改哪个文件、新增哪些类型和控制操作、怎么测，以及总体稿里几处读代码后需要修正或说清的地方（§二）。不改总体稿的结论。

M1 做完后应当成立的事（即总体稿 §十一 的验收）：

1. JS 行提供的服务，Rust 行可以按名字 `inject` 并调用；
2. JS 插件自己声明的依赖参与 rutis 的门控；
3. 先解析一个依赖 `llm` 的行、后启动运行时，这一行等到 `llm` 出现才启动，`llm` 撤销后停下；
4. 现有测试全部通过。

M1 不做：Python 运行时（M2）、两个运行时同时冷启动的验收（M1/M2 交界，需要第二个运行时才能测，但 M1 的会话层改动要为它留好位置，§七）。

## 二、读代码后对总体稿的修正

| 总体稿的说法 | 现状 | 本文的做法 |
| --- | --- | --- |
| 行提供的服务注册成 `TypeKey::keyed_dynamic::<RemoteService>(name)`（§五） | 宿主服务已经有一个按名字的键：`host_key(name)`，类型是 `dyn HostDispatch`；Node 调 `host:<name>` 时就是查它 | **不新增 `RemoteService`**。所有跨语言共享的服务都用 `host_key(name)` 注册为 `dyn HostDispatch`，不管提供者是 Rust 还是某个运行时的行。于是"其他语言调宿主服务"和"调另一种语言的服务"是同一条路径（§四） |
| 运行时插件在两段之间用 `entries()` + `reload(id)` 刷新行（§六） | 运行时插件在 rutis-interop 里，rutis-interop 不依赖 rutis-loader，拿不到 `Loader` | 第二段放到 rutis-loader 里：新增一个 `RuntimeRowsPlugin`，依赖 `CordisRuntime` 和 `Loader`，刷新后提供 `CordisRuntimeRows`（§六）。运行时插件本身只提供第一段 |
| 插件声明的依赖由 rutis 门控（§六） | dsh 一类的 Cordis 插件之间大量依赖只在 Cordis 里提供的服务（没有投到 rutis） | 只有 rutis 认识的名字才由 rutis 门控；其余仍交给 Cordis 自己门控，和现在一样（§五） |
| 转发时给 `path` 里其他会话的调用号加前缀（§五） | Rust 自己发出的调用号是 `rust:N`，每个会话各自计数，也会出现在 `path` 里 | `node:` 和 `rust:` 两种条目都要按会话区分。改写只在跨会话转发的那一处做（§七） |
| 不升协议版本 | 新增控制操作 `hosts.provide / hosts.withdraw`，旧的 runner 会回 `unknown control method` | 帧格式不变，`PROTOCOL` 保持 2；`rutis-interop`（crate 和 npm 包同版本发布）升到 0.3.0，`mount` 的回复里带上 `features`，Rust 侧缺特性时给出明确错误（§八） |

## 三、改动一览

```text
rutis-interop（Rust）
  rpc.rs          跨会话的引用转交（中继对象）；会话标签；path 改写
  process.rs      宿主服务表改为动态、计数；hosts.provide / withdraw；rows.load 带 services
  runtime.rs      不再注入宿主服务；只提供 CordisRuntime；HostDispatch 带方法形状
  rows.rs（新）   行服务投影：Cordis 槽位 → host_key(name) 的 dyn HostDispatch
rutis-interop（npm runner）
  runner.mjs      rows.load 的 services 参数；rows.schema 报依赖和服务；hosts.provide / withdraw
rutis-loader
  interop.rs      行的依赖 = CordisRuntimeRows + 已知名字的 host_key；apply 时 provide 宿主服务、投影行服务
  interop/rows.rs（新）RuntimeRowsPlugin：第二段放行
  catalog.rs      register_shared(name)：按名字共享的服务
```

## 四、一个键：`host_key(name)`

M1 之后，rutis 里"按名字、跨语言共享的服务"只有一种表示：

```rust
TypeKey::keyed_dynamic::<dyn HostDispatch>(name)   // 即 host_key(name)
```

提供者可以是：

- Rust 应用或 Rust 插件：和现在一样 `provide_as::<dyn HostDispatch>(host_key(name), ..)`；
- 某个运行时里的行：rutis-loader 替它注册一个转发到该运行时的 `HostDispatch`（§五.3）。

使用者可以是：

- Rust 插件：在 catalog 里登记过这个名字（`register_shared`，见下）后，配置里写 `inject: [weather]`，apply 时 `require_as::<dyn HostDispatch>(host_key("weather"))`；
- 运行时里的行：调用 `host:weather`，Rust 侧按名字查到当前提供者，就地调用或转发到另一个会话。

### 4.1 方法形状跟着服务走

Node 侧给宿主服务建代理时，要知道每个方法是同步还是异步。现在这份信息写在 `CordisRuntimePlugin::host(name, methods)` 上。宿主服务改成按行注册后，形状要跟着服务本身走：

```rust
pub trait HostDispatch: Send + Sync + 'static {
    fn invoke(&self, method: &str, args: RpcValue) -> Reply;
    /// { method: "sync" | "async" }。None：由运行时的 `host` 声明补上。
    fn methods(&self) -> Option<Value> { None }
}
```

- 默认实现返回 `None`，现有实现不用改；
- `CordisRuntimePlugin::host(name, methods)` 保留，但**只登记形状，不再让运行时依赖这个服务**。这是行为变化，写进 0.3.0 的迁移说明；
- 行服务的 `HostDispatch`（§五.3）总是带形状。

### 4.2 catalog：`register_shared`

catalog 现在是应用启动时一次建好的静态表，Rust 行的 `inject` 名字要先在这里登记。新增一个便捷方法：

```rust
catalog.register_shared("weather");
// 等价于 register_keyed::<dyn HostDispatch>("weather", host_key("weather"))
```

登记过的名字就是"rutis 认识的共享服务名"。§五.2 用它决定哪些依赖由 rutis 门控。这样做的代价是应用要列出跨语言共享的服务名；好处是规则是静态的，不依赖解析顺序（见 §十 的备选）。

## 五、Node 运行时上的行

### 5.1 `rows.schema` 报出依赖和服务

现在 `rows.schema(entry)` 只返回配置的 JSON Schema。改为返回：

```json
{
  "config": { "...": "JSON Schema，或 null" },
  "inject": { "llm": { "required": true }, "cache": { "required": false } },
  "provides": { "weather": { "today": "async", "unit": "sync" } }
}
```

- `inject`：读插件模块的 `inject` 导出，数组和对象两种写法都规范成对象；
- `provides`：Cordis 插件在运行时才 `ctx.provide`，方法形状没有地方可读。M1 从插件所在包的 `package.json` 读一个字段：

  ```json
  { "rutis": { "provides": { "weather": { "today": "async", "unit": "sync" } } } }
  ```

  这个字段可以手写，也可以由现有生成器读 `.d.ts` 写出（`generate.mjs` 加一个只输出形状的模式）。总体稿 §九 在这件事上的两种做法里倾向这一种，本文定下来。没有这个字段的插件，它提供的服务只在 Cordis 里可见，不投到 rutis，和现在一样。
- 旧 runner 返回的是裸 Schema。Rust 侧凭 `features`（§八）区分，不猜格式。

### 5.2 行的依赖

`InteropResolver` 拿到 schema 后，行的依赖是：

| 依赖 | 来源 | 门控方 |
| --- | --- | --- |
| `CordisRuntimeRows`（替代现在的 `CordisRuntime`） | 固定 | rutis |
| `host_key(n)`，`n` 是 `inject` 里 `required` 且在 catalog 里登记为共享的名字 | `rows.schema` | rutis |
| 其余 `inject` 名字 | `rows.schema` | Cordis，和现在一样：插件在 Cordis 里按原生规则等待 |
| 行配置里的 `inject` | loader 行 | Cordis（`foreign_scope`，现状不变） |

可选依赖（`required: false`）不进 rutis 的依赖：apply 时存在就提供给 Cordis，之后它出现或撤销不会重启这一行。和 rutis 原生插件对可选服务的处理一致。

`Resolved::factory` 的 `injects` 是在解析时定下的，所以依赖声明变了就要重新解析这一行。这正是第二段放行要做的事（§六）。

### 5.3 把行提供的服务投到 rutis

runner 里已经有一套"导出槽位"的机制（`slots`、`exporter`、`service` 通知），目前只用于构建期生成的挂载。M1 让行也用它：

1. `rows.load(key, entry, config, isolate, inject, services)`：新增最后一个参数，是这一行在 `provides` 里声明的名字和形状。runner 为每个名字建一个导出槽位，exporter fiber 跟着这一行的 fiber 一起卸载；
2. 槽位变化照旧经 `service(name, handle, version)` 通知报给 Rust；
3. Rust 侧新增 `rows.rs`：`Process` 把每个名字的通知转给登记了它的那一行；
4. 那一行（`JsRow`，在 rutis-loader 里）收到有 handle 的通知时，在**自己的** `Ctx` 上 `provide_as::<dyn HostDispatch>(host_key(name), RowService { process, handle, methods })`；收到 `None` 时撤销；
5. `RowService::invoke(method, args)` 就是 `process.invoke(handle, method, args)`。

服务注册在行的 fiber 上，所以行卸载时 rutis 自动撤销它，依赖它的行和 Rust 插件按原生规则停下。

两个行声明同一个名字时，Cordis 里只有一个生效，rutis 里会有两个提供者。M1 不处理这种冲突，在诊断里报出来（两行的 `provides` 有交集）。

### 5.4 宿主服务改为按行提供

现在宿主服务是运行时启动时一次注册进 Node（`Mount::hosts` → `args.provided`），运行时依赖它们。M1 改为：

- `CordisRuntimePlugin` 的 `injects` 变为空，`Mount::hosts` 为空。运行时只依赖它启动本身需要的东西；
- 新增运行时级控制操作：
  - `hosts.provide(name, methods)`：runner 用 `ctx.provide(name, hostProxy(name, methods))` 注册代理，保存返回的撤销函数；
  - `hosts.withdraw(name)`：调用保存的撤销函数。
- `Process` 里的宿主服务表从启动时固定的 `HashMap` 改为 `Mutex<HashMap<String, Entry>>`，`Entry` 记录 `Arc<dyn HostDispatch>` 和引用计数：
  - 第一个需要它的行调用时，写入表并发 `hosts.provide`；
  - 最后一个行释放时，发 `hosts.withdraw` 并移出表；
  - `host:<name>` 调用查这张表，查不到就报"没有这个宿主服务"，和现在一样。
- `JsRow::apply` 在 `rows.load` 之前，对 §5.2 里由 rutis 门控的每个名字：
  1. `require_as::<dyn HostDispatch>(host_key(n))`；
  2. 如果这个服务就是本运行时某一行投出来的（`RowService` 的 `process` 与本行的相同），跳过：Cordis 里已经有原生的提供者，再注册代理会和它冲突；
  3. 否则向 `Process` 登记一次引用（必要时触发 `hosts.provide`），并把释放登记为这一行的清理。

  清理顺序是先 `rows.unload`，再释放宿主服务引用，所以插件不会在卸载前看到服务消失。
- 提供者被替换时，依赖它的行按 rutis 原生规则重启：先释放（计数可能归零，发 `withdraw`），再重新登记（发 `provide`，用新的 dispatch）。所以不需要"就地替换代理"。

这样运行时不再等任何宿主服务。总体稿 §四 担心的"运行时之间互相等"在 M1 里就不会出现：所有等待都落在行上。

## 六、两段放行

### 6.1 为什么要第二段

行在运行时启动前就可能被解析：`InteropResolver` 这时返回"没有 schema、只依赖运行时"的结果（#109 的行为），也不缓存。如果运行时一出现行就启动，它的 `inject` 还没进依赖声明，可能在 `llm` 未就绪时就执行插件。

### 6.2 `RuntimeRowsPlugin`

放在 rutis-loader 的 `interop` 模块里（只有它同时看得到 `Loader` 和 `CordisRuntime`）：

```text
CordisRuntimePlugin   提供 CordisRuntime                     （第一段）
RuntimeRowsPlugin     依赖 CordisRuntime + Loader
                      刷新行的解析结果后，提供 CordisRuntimeRows （第二段）
行（JsRow）           依赖 CordisRuntimeRows + 共享服务
```

`RuntimeRowsPlugin::apply`：

1. 从 `InteropResolver` 取出需要刷新的行名：上次解析时没有 schema 的（`meta.schema` 写着 unavailable），以及 `package.json` 的 `version` 与上次解析时不同的。版本记在 `meta.version` 里；
2. 让 resolver 丢掉这些名字的缓存；
3. 在一个任务里对这些行逐个调用 `Loader::reload(id)`，全部完成后 `provide(CordisRuntimeRows)`。任务登记为这个插件的清理：插件卸载或重启时取消它，没提供的服务也就不会提供。

为什么放在任务里，而不是直接在 `apply` 里等：`reload` 要拿 loader 的操作锁，而运行时可能正是在一次 reconcile 中途重启的。`apply` 里直接等可能和那次 reconcile 互相等。放进任务后，`apply` 立即返回；行在 `CordisRuntimeRows` 出现之前都在等待，`reload` 不会让它们启动，依赖声明变了重建 fiber 也不会。

刷新失败（某一行解析失败）时，这一行按 loader 现有规则报失败，其余行照常放行，`CordisRuntimeRows` 照常提供。

### 6.3 应用侧用法

```rust
let mut catalog = ServiceCatalog::new();
catalog.register_shared("llm").register_shared("weather");

root.provide_as::<dyn HostDispatch>(host_key("llm"), Arc::new(llm))?;
let runtime = CordisRuntimePlugin::new(node_package, anchor);
let resolver = Arc::new(InteropResolver::new(runtime.handle()));
root.plugin(runtime);
let options = LoaderOptions { catalog, ..LoaderOptions::default() };
root.plugin(LoaderPlugin::new(Chain::new().with_shared(resolver.clone()), options)).await?;
root.plugin(RuntimeRowsPlugin::new(resolver));
```

`RuntimeRowsPlugin` 要和 loader 共用同一个 `InteropResolver`（它的缓存），所以 `Chain` 新增 `with_shared(Arc<dyn Resolver>)`；`Chain` 内部本来就存 `Arc<dyn Resolver>`。

## 七、会话层：跨会话转发

这是 M1 里唯一较大的会话层改动，也是 M2 两个运行时互相调用的前提。M1 只有一个运行时，但 Rust 插件持有 JS 回调再交给同一个会话，不涉及跨会话；所以 M1 用测试里起的第二个 Node 运行时来覆盖它（§九）。

### 7.1 引用转交：中继对象

现在 `encode` 遇到另一个会话的远程引用时直接报错（`rpc.rs`：`cross-session reference forwarding is not implemented`）。改为：

- 编码另一个会话的 `Remote(import)` 时，在本会话导出一个**本地中继对象**，种类与 `import.kind` 相同：
  - Function：调用时转调 `import`；
  - Future：等待时转等 `import`；
  - Object：方法调用和属性读取都转给 `import`。
- 中继对象持有 `Arc<Import>`。对端释放中继（计数归零）时，中继被丢掉，`Import` 随之丢掉，向原会话发 `release`。引用计数沿着中继链自然传递，不需要新的帧。
- 同一个 `import` 多次转交到同一个会话，复用同一个中继（按 `(会话, import 指针)` 记在本会话的导出表里），对端看到的是同一个引用，身份比较仍然成立。
- 转回原会话时不经过中继：编码时如果 `import` 属于目标会话，照现在的做法以 `home: true` 发回。中继的中继同理：解开到最内层，再判断。

### 7.2 调用链改写

每个 `Connection` 新增一个进程内唯一的会话标签（例如 `s3`）。改写只在中继和 `RowService` 这类"跨会话转发"的代码里做，由一个函数完成：

```text
rebase(path, from, to):
  不带标签的条目      → 加上 from 的标签（"s1/node:3"、"s1/rust:7"）
  带 to 的标签的条目  → 去掉标签，还原成 to 会话里的原始调用号
  带其他标签的条目    → 原样保留
```

- 转发前把当前调用链（`current_path()`）从源会话改写到目标会话，再以它作为发出调用的 `path`；
- 带标签的条目永远对不上任何会话里的原始调用号，所以不会被误认成本会话的调用；
- 回到原会话时，原会话自己的条目被还原，它能认出"这个反向调用属于我正在同步等待的那个调用"，在等待它的线程上执行。

举例：Python 同步调用 `host:weather`（Python 会话里的 `node:3`），Rust 转发给 Node；Node 在执行中同步回调 Python 传进来的函数（Node 会话里的 `node:9`）。回调经中继转回 Python 时，`path` 是 `["node:3", "s2/rust:5", "s2/node:9"]`：Python 认出 `node:3` 是自己正在等的调用，在主线程上嵌套执行回调，不会死锁。

`rpc.rs` 里依赖调用号格式的地方要一起检查：`related.sort_by_key(... strip_prefix("node:") ...)` 只对本会话收到的调用排序，条目都不带标签，不受影响；`receive` 里对 `id` 的校验只看本帧的 `id`，不看 `path`，也不受影响。

## 八、版本与兼容

- 帧格式不变，`PROTOCOL` 仍是 2。
- `rutis-interop` crate 和 npm 包一起升到 0.3.0（它们同版本发布）。
- runner 在 `mount` 的回复里加 `features: ["rows.v2", "hosts"]`。Rust 侧在行模式下检查：缺 `rows.v2` 就报"Node 运行时版本过旧，需要 @arcships/rutis-interop ≥ 0.3.0"，不去猜 `rows.schema` 的返回格式。
- 构建期生成的静态挂载不受影响：它不用 `rows.*`，仍可以在 `Mount::hosts` 里一次注册宿主服务。`Mount::hosts` 保留，只是 `CordisRuntimePlugin` 不再用它。
- 行为变化（写进迁移说明）：
  - `CordisRuntimePlugin::host` 不再让运行时等待宿主服务；宿主服务的等待落在用到它的行上；
  - JS 行要用一个 rutis 服务，需要在 catalog 里 `register_shared` 这个名字，插件的 `inject` 里也要写它。只在行配置里写 `inject` 的，仍然只由 Cordis 门控；
  - 应用要挂上 `RuntimeRowsPlugin`，否则行会一直等待 `CordisRuntimeRows`。诊断里的等待原因会指向它。

## 九、测试

都加在现有测试旁边，测试用的 JS 插件放在 `crates/rutis-loader/tests/fixtures`。

| 测试 | 位置 | 验证 |
| --- | --- | --- |
| 行服务投到 rutis | `rutis-loader/tests/interop_rows.rs` | JS 行在 `package.json` 声明 `provides.weather`；Rust 行 `inject: [weather]` 后能同步、异步调用；JS 行卸载后 Rust 行停下 |
| 插件声明的依赖参与门控 | 同上 | 插件 `inject = ['llm']`、`llm` 登记为共享：宿主不提供 `llm` 时行停在 Pending，提供后启动，撤销后停下，再提供后恢复 |
| 先解析、后启动运行时 | 同上 | 运行时还没启动时 reconcile；运行时启动后，行经第二段刷新拿到 `inject`，在 `llm` 出现前不执行插件 |
| 未登记的名字仍由 Cordis 门控 | 同上 | 两个 JS 行，一个提供 `foo`（未登记），另一个 `inject foo`：两行都启动，rutis 不为 `foo` 门控 |
| 同运行时不注册重复代理 | 同上 | `weather` 由 JS 行 A 提供并登记为共享，JS 行 B `inject weather`：B 拿到的是 A 的原生对象，`host:weather` 没被调用 |
| 宿主服务计数 | `rutis-interop/tests/host_services.rs` | 两行用同一个宿主服务：第一行卸载后 Node 里仍有代理，第二行卸载后撤销 |
| 引用转交 | `rutis-interop/src/rpc/tests.rs` | 会话 A 的函数、Future、对象经 Rust 转交给会话 B；B 调用、等待、读属性都到达 A；B 释放后 A 收到 `release`；同一个引用转交两次，B 看到的身份相同 |
| 调用链改写 | `rutis-interop/tests/rpc_callbacks.rs` | 起两个 Node 运行时，两边同号的调用（都是 `node:1`）同时在同步等待；A 经 Rust 同步调 B，B 同步回调 A 传来的函数：回调在 A 正在等待的线程上执行，不死锁，也不会被 B 误认 |
| 旧 runner | `rutis-interop/tests/error_shape.rs` | 回复里没有 `features` 时，行模式给出版本过旧的错误 |

本地 Node 是 22.x，几个已知测试在 main 上也失败（`node_sync_wait…` 等）。以 CI 的 Node 26 为准。

## 十、拆成几个 PR

按依赖顺序，每个 PR 都直接以 `main` 为目标（不做堆叠分支）：

| PR | 内容 | 依赖 |
| --- | --- | --- |
| M1a | 会话层：会话标签、`rebase`、中继对象，及其单元测试和双运行时测试（§七） | 无 |
| M1b | runner 与 `Process`：`rows.schema` 新格式、`rows.load` 的 `services`、`hosts.provide / withdraw`、动态宿主服务表、`features`；`HostDispatch::methods`；版本 0.3.0 | 无，可与 M1a 并行 |
| M1c | rutis-loader：`register_shared`、行的依赖、行服务投影、按行提供宿主服务、`RuntimeRowsPlugin`；`CordisRuntimePlugin` 去掉宿主服务依赖；迁移说明 | M1b |

M1a 不在 M1c 的路径上：M1 只有一个运行时，跨会话转发要到 M2 才真正用到。先做它是因为它风险最大，越早有测试越好。

## 十一、待定

- **共享名字的来源**：本文用 catalog 显式登记（§4.2）。备选是"任何运行时的行在 `provides` 里声明过的名字都算共享"。它省掉登记，但一个行是否由 rutis 门控会取决于提供者行有没有先被解析，结果和配置顺序有关，所以不选。如果显式登记在实际使用中太繁琐，再考虑让 loader 在一次 reconcile 里先解析完所有行、再定依赖。
- **可选依赖**：M1 不为它重启行（§5.2）。如果有插件需要"可选服务出现后重新 apply"，再加。
- **同名服务冲突**：M1 只报诊断（§5.3），不决定谁生效。
- **调用号前缀**：沿用 `node:`，与总体稿 §九 相同。
