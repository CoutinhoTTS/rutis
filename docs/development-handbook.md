# Rutis 开发手册

本手册介绍插件的实现、服务调用、资源管理和运行控制。应用结构的设计方法见 [应用开发指南](development-guide.md)。

## 快速开始

在 Rust 项目中添加依赖：

```toml
[dependencies]
rutis = "0.5.0"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
```

下面用一个提供方和一个消费者演示服务注册与依赖声明。示例中，Backend 提供服务，Indexer 声明并使用这个服务。

```rust
use rutis::{BoxFuture, CordisError, Ctx, Effect, Plugin, TypeKey};

struct Backend(String);
struct BackendPlugin(String);

impl Plugin for BackendPlugin {
    fn name(&self) -> &str { "backend" }

    fn apply<'a>(&'a self, ctx: &'a Ctx)
        -> BoxFuture<'a, Result<Effect, CordisError>>
    {
        Box::pin(async move {
            ctx.provide(Backend(self.0.clone()))?;
            Ok(Effect::Done)
        })
    }
}

struct IndexerPlugin {
    dependencies: Vec<TypeKey>,
}

impl Plugin for IndexerPlugin {
    fn name(&self) -> &str { "indexer" }

    fn injects(&self) -> &[TypeKey] { &self.dependencies }

    fn apply<'a>(&'a self, ctx: &'a Ctx)
        -> BoxFuture<'a, Result<Effect, CordisError>>
    {
        Box::pin(async move {
            let backend = ctx.get::<Backend>()
                .ok_or_else(|| CordisError::ServiceNotFound("Backend".into()))?;
            println!("Indexer connected to {}", backend.0);
            Ok(Effect::Done)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Ctx::root()?;
    let started = async {
        let backend = root.plugin(BackendPlugin("local".into()));
        (&backend).await?;

        let indexer = root.plugin(IndexerPlugin {
            dependencies: vec![TypeKey::of::<Backend>()],
        });
        (&indexer).await
    }.await;

    let closed = root.shutdown().await;
    started?;
    closed?;
    Ok(())
}
```

运行后输出 `Indexer connected to local`。这个例子先完成后端初始化，再加载 Indexer。实际应用也可以先注册消费者：它会在 `Pending` 状态等待依赖就绪。

这里的 `(&view).await` 等待该插件处理完此前排队的操作。判断一个运行中的服务是否可用，应观察提供它的插件是否进入 `Active`，具体见 [启动与关闭](#启动与关闭)。

仓库中的 [完整索引示例](../crates/rutis/examples/development_workflow.rs) 在此基础上加入了有界队列、后台任务和配置更新：

```bash
cargo run -p rutis --example development_workflow
```

## 实现插件

### 初始化

`apply` 按以下顺序组织：

1. 读取本次加载需要的服务。
2. 创建资源，并登记对应的清理函数。
3. 提供服务、注册监听器或创建子插件。
4. 返回 `Effect`，完成初始化。

插件进入 `Active` 后，其服务即可供外部消费者使用。持续运行的工作放在后台任务中，由清理函数负责结束。

`name()` 用于日志和诊断；`injects()` 声明服务依赖；`validate()` 校验插件持有的配置。插件实现需要满足 `Send + Sync + 'static`。

### 选择创建方式

| 创建方式 | 适用情况 | 重新加载时的行为 |
|---|---|---|
| `ctx.plugin(plugin)` | 配置由插件对象持有 | 在同一个对象上再次执行 `apply` |
| `ctx.plugin_with(factory, config)` | 运行期间需要更新配置 | 根据当前配置创建新的插件对象 |
| `ctx.plugin_from(build, config)` | 使用无依赖的简单工厂 | 调用闭包创建新的插件对象 |

普通插件把依赖声明写在 `Plugin::injects()` 中；工厂插件写在 `PluginFactory::injects()` 中。声明在注册时确定，并在该 fiber 的整个生命周期内保持固定。

创建资源的代码放在 `apply` 中。这样，每次重新加载都会取得当前依赖，创建所需资源，并登记新的清理函数。

## 服务键与作用域

### 定义服务键

注册、依赖声明和读取使用同一个键：

| 服务形式 | 注册 | 依赖键 | 读取 |
|---|---|---|---|
| 具体类型 | `provide(value)` | `TypeKey::of::<T>()` | `get::<T>()` |
| trait 接口 | `provide_as::<dyn T>(key, value)` | `key.clone()` | `get_as::<dyn T>(key)` |
| 带名称的服务 | `provide_as::<T>(key, value)` | `key.clone()` | `get_as::<T>(key)` |

其中，`provide_as` 接收 `Arc<T>`。trait 服务的键使用相同的 trait 类型；例如，`TypeKey::of::<dyn Store>()` 对应 `Arc<dyn Store>`。

同类型的多个服务通过限定名区分：

```rust
use rutis::TypeKey;

struct Backend;

fn primary_backend() -> TypeKey {
    TypeKey::keyed::<Backend>("primary")
}
```

把这类键函数放在公共接口模块，供提供方和使用方共同调用。运行时生成的名称使用 `TypeKey::keyed_dynamic`。

### 声明依赖

在 `injects()` 中列出插件运行必需的服务。框架会等待这些服务存在、提供方进入 `Active`，以及可选的健康检查通过，然后执行 `apply`。

`get/get_as` 负责查找当前可见的服务，返回 `Option<Arc<T>>`。自动重载关系由 `injects()` 建立，因此每个必需服务都应有对应的依赖声明。

服务需要额外健康条件时，可以通过 `provide_as_with_check` 注册同步谓词。条件改变后调用 `ctx.refresh()`，框架会重新检查依赖，并据此调整消费者状态。

### 选择作用域

普通类型键用于 root 内共享的服务。同一个键和作用域对应一个有效服务绑定；重复提供会返回 `ServiceExists`。

`isolate` 为指定服务键选择作用域。下面的片段为同一个键建立两个独立的查找范围：

```rust,ignore
let scope_a = root.isolate(primary_backend(), "a");
let scope_b = root.isolate(primary_backend(), "b");
```

在对应上下文下注册提供方和消费者，即可分别使用各自的绑定。同一个 root 内，同键同 label 共享作用域。isolate 作用于指定服务键，资源生命周期沿用原上下文；实例级资源管理见 [实例子树](#实例子树)。

## 管理资源

插件作者负责业务资源的完整生命周期，通过 `apply` 实现初始化，通过 `Effect` 登记清理。Rutis 按生命周期规则调用这些实现。职责说明见 [职责划分](development-guide.md#职责划分)。

### 登记清理

通过 `ctx.provide` 注册的服务、通过当前 `ctx` 注册的监听器，以及 `ctx.plugin` 创建的子插件，都由框架登记清理。外部订阅、连接和任务通过 `ctx.effect` 登记释放过程。

以下片段放在插件的 `apply` 中，演示异步清理的写法：

```rust,ignore
ctx.effect(move || {
    Effect::AsyncDisposer(Box::new(move || {
        Box::pin(async move {
            connection.close().await?;
            Ok(())
        })
    }))
})?;
```

`effect` 的工厂闭包立即执行，返回的清理函数在卸载时执行。资源取得后应及时登记清理，同时处理登记失败时的释放。`apply` 中途失败时，框架会执行已经登记的清理函数。

清理句柄 `Disposer` 可用于提前释放：调用 `disposer.dispose().await`，等待并处理结果。保留或丢弃句柄，都允许所属插件在卸载时负责最终清理。

### 管理后台任务

后台任务需要同时安排取消和等待退出。下面的片段展示了基本结构；任务中的等待可替换为业务循环：

```rust,ignore
let token = ctx.cancellation_token();
let stop = token.clone();
let runtime = ctx.handle().clone();

ctx.effect(move || {
    let task = runtime.spawn(async move {
        token.cancelled().await;
    });

    Effect::AsyncDisposer(Box::new(move || {
        Box::pin(async move {
            stop.cancel();
            task.await
                .map_err(|e| CordisError::PluginFailed(Box::new(e)))?;
            Ok(())
        })
    }))
})?;
```

每次 `apply` 捕获一次当前代的 token，并将它传给这一代的任务。任务收到取消信号后，按约定完成或取消当前操作，然后退出。清理函数通过 `JoinHandle` 等待实际退出。

业务循环通常用 `tokio::select!` 同时等待输入和取消。队列中的请求可以选择完成、取消或保存，具体策略由服务接口约定。完整实现见 [IndexerPlugin 示例](../crates/rutis/examples/development_workflow.rs)。

同步计算、阻塞操作和子进程也需要各自的停止方式，例如分段检查取消、关闭 I/O 或结束进程并等待退出。

### 安排清理顺序

同一插件内的清理按登记顺序逆序执行。后登记的资源先释放，`apply` 返回的 `Effect` 最后登记。

当底层连接需要保留到消费者退出时，采用以下顺序：

```text
初始化：创建连接并登记关闭 → 提供服务 → 创建依赖它的子插件
清理：  关闭子插件 → 摘除服务并等待消费者退出 → 关闭连接
```

清理中需要使用的对象，在 `apply` 中取得并捕获。一个清理函数失败后，框架会记录错误并继续执行其余清理。

服务从注册表移除后，调用方持有的 `Arc` 仍然有效。服务实现应根据自身运行状态处理后续调用。例如，示例中的旧 Indexer 会在队列关闭后返回错误。

## 使用事件

### 事件总线

事件总线管理监听器的注册、事件的分发和处理函数的调用。发送方提供事件数据，监听器实现具体处理逻辑，分发 API 决定执行顺序、完成条件和结果返回方式。

这种机制通常用于通知、请求分发或多个插件共同参与的处理过程。事件类型定义数据和结果，应用据此约定具体含义。

### 定义事件和监听器

定义事件类型时，实现 `Event`，指定用于诊断的名称和处理结果类型。处理逻辑通过 `Listener` 实现。下面以携带文档名称的 `DocumentIndexed` 为例：

```rust
use std::sync::Arc;
use rutis::{BoxFuture, CordisError, Ctx, Event, EventKey, Listener};

struct DocumentIndexed(String);

impl Event for DocumentIndexed {
    const NAME: &'static str = "app::DocumentIndexed";
    type Value = ();
}

struct Progress;

impl Listener<DocumentIndexed> for Progress {
    fn call<'a>(&'a self, _sender: &'a Ctx, event: &'a DocumentIndexed)
        -> BoxFuture<'a, Result<Option<()>, CordisError>>
    {
        Box::pin(async move {
            println!("indexed: {}", event.0);
            Ok(None)
        })
    }
}

fn index(ctx: &Ctx, path: String) -> Result<(), CordisError> {
    let key = EventKey::<DocumentIndexed>::of();
    ctx.events().on(ctx, &key, Progress)?;
    ctx.events().emit(ctx, &key, Arc::new(DocumentIndexed(path)))?;
    Ok(())
}
```

在这个例子中，`EventKey::<DocumentIndexed>::of()` 是这类事件的通道，`on` 在该通道上注册监听器，`emit` 向该通道提交事件；两者都返回 `Result`，失败时（例如上下文已经关闭）不会注册或投递。

事件按类型和通道匹配，`NAME` 用于诊断。同一事件类型可以用 `EventKey::named("…")`（运行时生成的名称用 `EventKey::dynamic`）划分多个通道。监听器属于注册时传入的 `Ctx`；回调收到的 `Ctx` 来自发送方。回调需要创建属于监听插件的资源时，应捕获注册方的 `Ctx`。可运行代码见 [上下文归属示例](../crates/rutis/examples/listener_ctx_ownership.rs)。

### 选择分发方式

| API | 执行方式 | 返回与错误处理 |
|---|---|---|
| `emit` | 在后台依次调用监听器，同键事件依次分发 | 立即返回；错误交给 ErrorSink |
| `parallel` | 并发调用监听器，等待全部完成 | 成功返回 `()`，失败汇总错误 |
| `serial` | 按注册顺序调用，遇到 `Some(value)` 或错误时结束 | 返回首个值；全部返回 `None` 时结果为 `None` |
| `waterfall` | 监听器通过 `next` 调用后续处理，并可处理其返回结果 | 返回整条调用链的结果 |

普通监听器的返回类型为 `Result<Option<E::Value>, CordisError>`。例如，事件定义 `type Value = String`，serial 监听器就用 `Ok(Some(title))` 返回标题，用 `Ok(None)` 交给下一位处理者。

emit、parallel 和 serial 使用 `on` 注册的普通监听器；waterfall 使用 `on_waterfall` 注册监听器。waterfall 监听器调用 `next.call().await` 执行后续流程，并可以处理返回结果；直接返回则结束当前流程。调用方通过 `terminal` 提供链末的处理函数。事件参数在整条链中保持相同，结果通过返回值传递。

emit 和 parallel 忽略监听器返回的 `Some(value)`；serial 用它决定何时结束调用。选择分发方式时，应明确是否等待完成、是否需要返回值，以及处理函数之间的顺序要求。

同键 `emit` 的后续分发会等待前一次完成。高频数据流可以使用有界队列，结合批处理或合并控制积压；需要重放的数据另行持久化。处理错误通过 `Result` 表达，waterfall 中的 panic 由调用方处理。

## 实例子树

实例子树把一个插件及其后代组织成共同管理的范围。实例服务键限制服务的访问范围，实例事件通道限制监听器的注册和事件发送范围，`FiberView::shutdown()` 负责关闭整棵子树。

这种组织方式可以用于管理多个生命周期独立的运行实例。下面以工作区为例演示服务与事件的关联方式。

### 示例：共享实例状态

工作区父插件用自己的实例 ID 创建服务键，再把键传给子插件。以下片段放在 `WorkspacePlugin::apply` 中，业务类型由应用定义：

```rust,ignore
let workspace = ctx.instance();
let state_key = TypeKey::instance::<WorkspaceState>(workspace);

ctx.provide_as::<WorkspaceState>(state_key.clone(), Arc::new(state))?;
ctx.plugin(IndexerPlugin::new(state_key));
Ok(Effect::Done)
```

Indexer 将传入的键用于 `injects` 和 `get_as`。该服务的访问范围是工作区及其子插件；外部管理功能通过工作区显式提供的业务接口调用。

父插件提供状态、创建子插件后即完成初始化。宿主随后等待 Indexer 就绪，这样子插件可以顺利取得已经 Active 的父插件服务。

### 示例：实例内的事件分发

工作区 ID 也作为参数传给 Progress 和 Indexer。它们使用自己的上下文，以及同一个工作区 ID：

```rust,ignore
let key = EventKey::<DocumentIndexed>::of().instance(workspace);

// 在 Progress 子插件中注册。
progress_ctx.events().on(progress_ctx, &key, Progress)?;

// 在 Indexer 子插件中发送。
indexer_ctx.events().emit(indexer_ctx, &key, Arc::new(DocumentIndexed(path)))?;
```

实例通道允许对应工作区及其子插件注册和发送事件。关闭相关子树时，框架等待已经接纳的实例事件处理完成，再完成资源清理。回调中产生的额外后台任务，按前面的任务管理方式登记。

普通事件用于 root 内共享通知，命名通道（`EventKey::named`）用名称区分通道，实例事件用于子树内通知。三类通道分别匹配；`.instance(id)` 也可以加在命名通道上。isolate 的服务作用域独立于事件通道。当前实例事件支持 emit、parallel 和 serial。旧的 `on_instance` / `emit_instance` / `on_keyed` / `emit_keyed` 自 0.5.0 起弃用，新代码统一使用 `EventKey`。

关闭操作由宿主协调。实例回调若需要关闭工作区，可向宿主发送请求并返回，由宿主等待关闭完成。这样，关闭过程可以正常等待当前回调结束。

## 更新配置

需要热更新的插件使用 `PluginFactory`。下面的工厂复用快速开始中的 `BackendPlugin`，并在构造时检查配置：

```rust,ignore
use rutis::PluginFactory;

struct BackendFactory;

impl PluginFactory<String> for BackendFactory {
    fn build(&self, label: &String) -> Result<Box<dyn Plugin>, CordisError> {
        if label.trim().is_empty() {
            return Err(CordisError::Validation {
                issues: vec!["backend label is required".into()],
            });
        }
        Ok(Box::new(BackendPlugin(label.clone())))
    }
}
```

宿主通过工厂注册插件，之后提交新配置：

```rust,ignore
let backend = root.plugin_with(BackendFactory, String::from("local"));
backend.update(String::from("remote")).await?;
```

一次更新分为两步：

1. **预检查。** 执行 `validate_config`、`build` 和插件的 `validate`。失败时返回错误，现有配置和实例保持原状。
2. **重新加载。** 保存新配置，清理旧实例，依赖就绪后再次构造和加载插件。消费者随服务变化重新加载。

`build` 负责纯粹的对象构造，网络连接、任务启动等操作放在 `apply` 中。首次加载也需要的校验写在 `build` 或插件的 `validate` 中；`validate_config` 用于更新预检查。

正式加载仍可能遇到网络或资源错误，宿主应保留恢复策略，例如重试或重新提交上次有效配置。更新后观察实际业务入口的状态，再恢复请求处理。需要改用另一组依赖时，由宿主选择并注册相应插件。

`current_config::<C>()` 返回保存的配置快照，可用于展示和诊断。配置更新作用于当前进程内的插件实例；代码升级由应用发布流程负责。

## 启动与关闭

### 观察运行状态

| 状态 | 含义 |
|---|---|
| `Pending` | 等待依赖服务就绪 |
| `Loading` | 正在初始化 |
| `Active` | 初始化成功，服务可用 |
| `Failed` | 初始化失败，错误保存在状态快照中 |
| `Unloading` | 正在清理资源 |
| `Disposed` | 当前插件已卸载 |

用 `view.state()` 读取快照，用 `view.watch()` 等待变化。启动等待应处理 Active、Failed、Disposed 和超时，完整函数见 [示例中的 wait_active](../crates/rutis/examples/development_workflow.rs)。

`(&view).await` 表示该插件此前排队的操作已处理完毕。等待结束时，插件仍可能因缺少依赖而处于 `Pending`；业务入口应在所需插件进入 `Active` 后开放。后台任务的健康状况由服务状态和错误报告反映。

### 选择关闭范围

关闭一个插件及其子树，使用该插件的 `FiberView::shutdown()`；关闭整个应用，使用 `Ctx::shutdown()`。任何子上下文上的 `Ctx::shutdown()` 也会关闭整个 root。

`view.restart()` 清理并重新加载当前插件。`view.dispose()` 卸载插件，普通插件到此结束；需要再次使用时注册新实例。root 的普通 dispose 支持后续 restart，最终退出应用使用 shutdown。

### 设置等待时间

`dispose_with_timeout` 和 root 的 `shutdown_with_timeout` 提供有限等待。超时后，关闭过程继续运行，调用方可以记录状态并再次等待同一结果。

对子树关闭设置等待时间，可以使用 Tokio timeout 包装 `view.shutdown()`：

```rust
use std::sync::Arc;
use std::time::Duration;
use rutis::{CordisError, FiberView};

async fn try_shutdown(view: &FiberView, limit: Duration)
    -> Result<bool, Arc<CordisError>>
{
    match tokio::time::timeout(limit, view.shutdown()).await {
        Ok(result) => {
            result?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}
```

返回 `true` 表示关闭完成；`false` 表示本次等待超时。宿主保留该 view，稍后再次调用 `view.shutdown().await` 取得结果。关闭操作一旦发起，就独立于等待方继续执行。

实例事件的在途处理由框架跟踪；普通和 named 事件中已经开始的回调可以继续运行。应用需要等待的其他工作，通过任务句柄和清理函数明确管理。

## 诊断与验证

### 查看问题

`root.diagnostics()` 提供插件状态、依赖和服务绑定的快照。可以从以下信息开始排查：

| 现象 | 检查内容 |
|---|---|
| 插件保持 Pending | 缺失的键、服务作用域、提供方状态、健康检查结果 |
| 服务读取返回 None | 注册与读取的类型、限定名、实例 ID，以及提供方是否 Active |
| 更新后仍使用旧服务 | 依赖声明是否完整，使用方是否取得新一代对象 |
| 插件进入 Failed | `view.state().error` 中的具体原因 |
| 关闭持续等待 | 初始化是否结束，任务是否响应取消，清理和实例回调是否完成 |

`diagnostics()` 读取已有状态。使用健康谓词的服务在条件变化后调用 `refresh()`，再观察检查结果。

### 处理错误

生命周期操作和等待型事件的错误通过返回值处理。emit 错误以及重载期间的清理错误进入 ErrorSink；默认输出到 stderr，宿主可用 `Ctx::root_with_sink` 接入自己的日志。

提前释放 effect 时，先处理 `Disposer::dispose()` 的结果。长寿命插件还可定期调用 `take_cleanup_errors()`，取走并处理已完成的历史清理错误。后台任务的错误由任务所有者观察和报告。

### 验证关键过程

为插件验证以下场景：

- 依赖稍后出现时，插件从 Pending 进入 Active，并正常提供服务。
- 依赖更新后，旧任务退出，新实例使用新服务，长期状态按设计保留。
- 初始化中途失败时，已创建的资源得到清理。
- 配置预检查失败时，原实例继续工作；正式加载失败时，可以执行恢复策略。
- 关闭一个实例子树后，其他独立子树继续运行。
- 在任务执行期间关闭，能取得约定的请求结果和最终清理结果。

测试中使用状态、oneshot、Notify 或 Barrier 协调时序，并为等待设置超时。仓库内可以运行：

```bash
cargo test -p rutis
cargo run -p rutis --example development_workflow
```

进一步的行为细节见 [生命周期契约测试](../crates/rutis/tests/contract.rs)、[配置更新测试](../crates/rutis/tests/config_update.rs)、[实例子树测试](../crates/rutis/tests/instance_subtrees.rs)和[关闭与等待时间说明](core-shutdown-and-disposal-deadline.md)。

---

适用于 Rutis 0.3.0。行为依据仓库提交 `015d48c`。标注为片段的代码在相应上下文中使用；完整程序见快速开始和配套示例。
