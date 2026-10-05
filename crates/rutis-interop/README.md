# rutis-interop：在 rutis 应用中挂载 Cordis 插件

rutis 应用可以直接挂载已发布的 Cordis（Node）插件：插件在真实的 Cordis 中运行，源码不改；rutis 侧拿到的是构建时从插件类型声明生成的 Rust 类型，按 rutis 原生方式使用。rutis 内核和 Cordis 都不需要修改。

设计与边界见 [兼容层设计](../../docs/design-protocol-plugin-mount.md)，完整示例见 [`examples/dsh-baseline`](../../examples/dsh-baseline)。当前支持 Unix（Linux、macOS），需要 Node 26 或更高版本。

## 1. 准备 npm 项目

在应用旁边准备一个 npm 项目，安装要挂载的插件和运行时 [`@arcships/rutis-interop`](https://www.npmjs.com/package/@arcships/rutis-interop)（源码在仓库的 `interop/node`）。版本以这个项目的锁文件为准；运行时的协议版本须与 crate 一致（构建时检查）：

```json
{
  "private": true,
  "type": "module",
  "dependencies": {
    "@deepseek-ai/cordis": "4.0.4",
    "@deepseek-ai/dsh-credentials-local": "0.2.0-rc.1",
    "@arcships/rutis-interop": "0.3.0"
  }
}
```

```sh
npm --prefix cordis ci
```

构建不会自动安装 npm 依赖；缺少包时构建失败，并给出要执行的命令。在本仓库内开发时可改用 `"file:../path/to/rutis/interop/node"` 引用源码。

## 2. 在 Cargo.toml 中声明挂载

```toml
[dependencies]
rutis = "…"
# 与 npm 运行时 @arcships/rutis-interop 同版本发布。
rutis-interop = "0.3"
tokio = { version = "1", features = ["full"] }

[build-dependencies]
rutis-interop = "0.3"

[package.metadata.rutis-interop]
npm = "cordis"                   # 第 1 步的 npm 项目，相对 Cargo.toml
# runtime = "…"                  # 默认 <npm>/node_modules/@arcships/rutis-interop

# 一个挂载 = 一个生成的模块
[package.metadata.rutis-interop.mounts.credentials]
plugin = "@deepseek-ai/dsh-credentials-local"   # npm 包；或 path = "src/plugin.ts"
version = "0.2.0-rc.1"           # 可选：与已安装版本不一致时构建失败
events = ["credentials/record-updated"]         # 转发给 rutis 监听的 Cordis 事件
# emits = ["…"]                  # 由 rutis 发往 Cordis 监听的事件
# provide = ["…"]                # 由 rutis 应用提供给插件的服务

# 组合挂载：彼此依赖的插件装进同一个 Cordis Context
[package.metadata.rutis-interop.mounts.workspace]
group = [
    { name = "storage", plugin = "@deepseek-ai/dsh-storage" },
    { name = "storage_json", plugin = "@deepseek-ai/dsh-storage-json" },
    { name = "storage_domain", plugin = "@deepseek-ai/dsh-storage-domain" },
    { name = "sessions", plugin = "@deepseek-ai/dsh-session-persistence-jsonl" },
    { name = "workspace", plugin = "@deepseek-ai/dsh-workspace" },
]
```

`build.rs`：

```rust
fn main() {
    rutis_interop::build::from_manifest().expect("generate Cordis bindings");
}
```

代码中引入全部挂载模块：

```rust
rutis_interop::include_mounts!();   // 生成 credentials、workspace 等模块
```

生成的文件在 Cargo 构建目录里，不提交、不手工维护；插件或其类型变化时，Cargo 会自动重新生成。无法绑定的成员在构建时以 `warning` 列出位置和原因，其余成员照常生成。

## 3. 使用

```rust
let ctx = rutis::Ctx::root()?;
let view = ctx.plugin(credentials::Plugin::new(credentials::Config {
    dsh_home: Some("/tmp/home".into()),
    ..Default::default()
}));
(&view).await?;

// 服务是普通的 rutis 服务：依赖门控、换值、撤销都按 rutis 原生规则。
let store = ctx.require::<credentials::CredentialProvider>()?;
store.set(&credentials::CredentialRef::from("app/api-key"), "s3cret").await?;
```

| 能力 | 用法 |
| --- | --- |
| 方法 | 同步方法仍同步，返回 Promise 的方法是 `async fn`；返回 `Result<T, rutis_interop::Error>`，Cordis 的业务错误为 `Error::Remote` |
| 数据类型 | 品牌类型为 newtype（`CredentialRef::from("…")`），接口为结构体，字面量联合为枚举；可选（`x?: T`）为 `Option`，`None` 发送 `undefined`；必填可空（`T \| null`）为 `Option`，`None` 发送 `null`；可选且可空为 `Option<Option<T>>` |
| 活对象 | 带方法的对象（例如 `Workspace`）是代理：属性 getter 实时读取，方法调用原对象，传回时还原为原对象；活对象的联合为 `ObjectRef`，与数据混合的联合为枚举 |
| 回调 | 函数参数传 Rust 闭包；返回的函数（例如注销函数）为 `RemoteFunction` |
| 取消 / 超时 | 丢弃返回的 future 即取消，Cordis 方法收到的 `AbortSignal` 会中止：`tokio::time::timeout(d, store.read_record(&key)).await` |
| 事件 | `events` 中的事件生成 rutis 事件类型，用 `ctx.events().on(&ctx, &EventKey::<CredentialsRecordUpdated>::of(), listener)` 订阅；`emits` 中的事件由 rutis `emit` / `parallel` 发往 Cordis |
| 宿主服务 | `provide` 中的服务生成 trait（例如 `SystemPromptHost`），实现后用生成的 `provide_system_prompt(&ctx, host)` 注册；挂载会等它就绪 |

## 部署

清单驱动的挂载按 npm 项目的相对位置定位运行时和插件。二进制换到别的机器或目录时，把 npm 项目（含已解析的 `node_modules`）一起部署，并用 `RUTIS_INTEROP_ROOT` 指向它：

```sh
cp -RL cordis /opt/app/cordis        # -L：file: 依赖等符号链接展开为实际文件
RUTIS_INTEROP_ROOT=/opt/app/cordis /opt/app/my-app
```

未设置时使用构建时的位置，开发期无需配置。用 `path` 挂载的 TypeScript 源文件应放在 npm 项目内，部署时随项目一起复制；运行时用默认的 `node_modules/@arcships/rutis-interop`（不设 `runtime`）即可随项目移动。

## 4. Cordis 插件需要遵守的边界

跨进程后，少数由 JS 语言栈带来的行为无法保持，写成了 [需求 §5](../../docs/requirements-protocol-plugins.md) 的边界规则，主要是：

- 跨边界的 `emit` 只是通知，不保证 `emit` 返回时 rutis 侧已处理；需要等待时用 `parallel`。
- 事件顺序只在同一侧内保证；waterfall / 有返回值的事件不转发。
- 直接 `ctx.set` 换值，rutis 侧在下一次调用该服务后才切换到新对象。
- 同步方法不能在执行中等待需要 Node 事件循环推进的结果，这类等待返回 `SyncWaitCycle`。
- 宿主提供的服务在 Cordis 侧是代理对象，`instanceof` 判断不成立。
- 插件不得长期阻塞事件循环；兼容层不为调用加超时，需要时用异步方法配合 `tokio::time::timeout`（超时即取消）。
- 插件里未捕获的异常或未处理的 Promise 拒绝会结束整个 Node 进程（Node 的默认规则），同一挂载里的插件一起停止。此后挂载的服务全部撤销，依赖它们的 rutis 插件停止等待；调用返回的 `Error::Transport` 说明进程如何结束。需要恢复时由应用卸载并重新挂载。

## 5. 常见构建错误

| 错误 | 处理 |
| --- | --- |
| `the Cordis plugins are not installed: run npm --prefix … ci` | 安装 npm 项目的依赖 |
| `… is not installed: add it to …/package.json` | 把插件加入 npm 项目并安装 |
| `… 1.0.0 is installed, 2.0.0 is required` | 让 npm 项目与 `version` 一致 |
| `… speaks protocol N, this rutis-interop speaks M` | 安装与 crate 匹配的 `@arcships/rutis-interop` |
| `native plugin dependencies are unresolved: … (name)`（运行时） | 缺少的服务需要放进同一个 `group`，或在 `provide` 中由 rutis 提供 |

## 逐个装载（rows）

`Mount { anchor: Some(package_json), .. }` 不带插件时启动一个空的 Cordis Context，之后用 `Process::load_row` / `unload_row` 逐个装载、卸载插件，`describe_row` 读取插件声明的内容：schemastery `Config` 转成的 JSON Schema、`inject` 的服务名，以及包的 `package.json` 里 `rutis.provides` 声明的、要提供给 rutis 的服务和方法形状。`load_row_exporting` 装载时把这些服务投到 rutis（`row_projection`，键为 `host_key(name)`）；`lease_host` 按行向 Cordis 注册宿主服务，最后一个使用者释放后撤销。rutis-loader 的 `InteropResolver` 就是这样把 JavaScript 插件作为行来管理的。

要按 rutis 的生命周期管理这个 Context，挂载运行时插件。本机运行时用 [`rutis-transport-local`](../rutis-transport-local) 的 `LocalRuntime`：本机承载以继承 fd 拉起进程，link 以兼容协议（2）接入，`RuntimePlugin` 在这条会话上提供 `Runtime` 服务；清理时先撤销服务，再关闭进程。远程运行时用 `RuntimePlugin::remote(名字)`，会话来自到它的 link（见 rutis-bridge 的 `RuntimeAccessPlugin`）。两种运行时的行完全一样。（`RuntimePlugin::node` / `python` / `launcher` 自己拉起进程，是保留的兼容路径，已标为弃用。）

- 运行时本身不依赖任何服务。逐个装载的插件用到哪个宿主服务，就由那个插件去等它、在运行期间租用它（`Process::lease_host`），宿主服务撤销时只有用到它的插件停下。
- `.host(名字, 方法)` 只声明宿主服务的方法形状，给没有自己报出形状（`HostDispatch::methods`）的服务用；它不再让运行时等待这个服务。
- 进程意外结束时，它的 link 停止、会话撤销，运行时随之停下，依赖它的插件回到等待；`RuntimeHandle` 的状态是 `Down(原因)`，说明进程怎样结束。之后由应用调用 `LocalRuntime` 那个 fiber 的 `restart` 重新启动（拉起新进程）。
- 运行时有名字（`.named(名字)`，默认 `"node"`），服务键是 `Runtime::key(名字)`。所以同一个应用里可以同时有多个运行时，包括不同语言的。

```rust
root.provide_as::<dyn HostDispatch>(host_key("probe"), Arc::new(probe))?;
let runtime = LocalRuntime::node(node_package, anchor).host("probe", json!({ "record": "sync" }));
let handle = runtime.handle();   // 给 rutis-loader 的 InteropResolver
let view = root.plugin(runtime);
```

## 其他语言的运行时

每种语言是一个 Cargo feature，应用只编译它启用的语言：

| feature | 内容 | 默认 |
| --- | --- | --- |
| `node` | Node 运行时、构建期代码生成（`build`，静态挂载用）及其依赖 syn、quote、toml | 开 |
| `python` | Python 运行时 | 关 |

`LocalRuntime::node` / `python` 在 `rutis-transport-local` 里，用同名 feature 打开。

协议、进程管理、服务投影（`Process`、`Projection`、`RuntimePlugin` 本身、`Launcher`）不属于任何一种语言，总是可用。只用 Python 的应用写 `rutis-interop = { version = "0.3", default-features = false, features = ["python"] }`；不挂运行时插件，就不会启动任何进程。

一种语言一个运行时插件、一个进程。它们和 Node 运行时说同一套协议和行契约（`rows.*`、`hosts.*`、服务投影），所以 rutis-loader 用同样的方式管理它们的插件。

- **Python**：`LocalRuntime::python(sdk, project)`，名字为 `"py"`。`sdk` 是本仓库的 `interop/python`（Python 包 `rutis_runtime`），`project` 是插件模块所在的目录。用 `python3 -m rutis_runtime` 启动，需要 Python 3.12 或更高；`.interpreter(路径)` 换解释器（例如项目的 venv）。写法见 [interop/python/README.md](../../interop/python/README.md)。
- 其他启动方式：`Mount::launcher` 接受任意 `Launcher`（`program`、`args`、`env`、`cwd`），它的最后两个参数是通道和项目位置。通道默认是要回拨的 socket 路径；进程能接继承的 socket 时用 `.inherit_fd()` 声明，通道就是 `fd:3`（Node、Python 运行时都已支持）。设置 `RUTIS_INTEROP_TRACE` 时，运行时通道上的每条消息都会在 stderr 记一行（方向和长度，不含内容）。

Python 运行时只跑"叶子插件"：插件有 `apply(ctx, config)`，在里面用服务（`ctx.use`）、提供服务（`ctx.provide`），返回清理函数；依赖、启停顺序和重启都由 rutis 决定。它在 `mount` 时报告 `leaf` 特性，rutis-loader 据此让插件 `inject` 的每个名字都在 rutis 里门控。

会话不依赖具体通道：`rpc::Connection::open(channel, dispatch)` 可以建立在任意 [`rutis-channel`](../rutis-channel) 的 `Channel` 上（有序、可靠、保持消息边界）。本机运行时进程仍走 Unix socket、逐行 JSON，线格式不变；`Connection::connect(UnixStream, …)` 保留为它的简写。

同一个进程里的插件互相使用服务时直接拿到对象本身，不走进程间通信。跨进程的调用经 Rust 转发，同步调用链会按会话改写（`rpc::rebase`），回调能回到正在等待的线程。

**同步调用与可重入**：Node 运行时在同步等待期间只执行属于这条调用链的进来调用，其他调用延后。Python 运行时在同步等待期间也执行其他进来的调用：否则两个运行时同时同步调用对方的服务时，会互相等待对方先返回而卡死。所以 Python 插件的服务可能在它自己正处于一次同步调用之中时被调用，不要在调用 rutis 的服务时持有锁。两个 Node 运行时之间互相同步调用仍可能卡死，跨运行时的高频或可能交叉的调用请用异步方法。

