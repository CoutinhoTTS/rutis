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
    "@arcships/rutis-interop": "0.1.0"
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
rutis-interop = "0.1"
tokio = { version = "1", features = ["full"] }

[build-dependencies]
rutis-interop = "0.1"

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

`Mount { anchor: Some(package_json), .. }` 不带插件时启动一个空的 Cordis Context，之后用 `Process::load_row` / `unload_row` 逐个装载、卸载插件，`row_schema` 读取插件 schemastery `Config` 转成的 JSON Schema。rutis-loader 的 `InteropResolver` 就是这样把 JavaScript 插件作为行来管理的。
