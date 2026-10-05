# rutis-runtime-local

本机语言运行时：本机承载拉起的进程、到它的 link、在这条会话上运行插件的运行时，组合成一个插件 `LocalRuntime`。

本机运行时的会话和远程运行时一样，来自 link（`RuntimeSession#<名字>`），区别只是进程由这里拉起：

- [`rutis-transport-local`](../rutis-transport-local) 以继承 fd（`fd:3`）拉起进程，旧的运行时包则用回拨 socket；通道拥有进程。
- link 处于本机模式（`LinkConfig::local_runtime`）：说兼容协议（2），不校验契约，不发 `link.offers`，会话结束后不重连。
- `RuntimeAccessPlugin` 和 `RuntimePlugin::session` 在这条会话上提供 `Runtime#<名字>`，loader 的行照常运行。

进程结束时 link 停止，运行时随之停下，`RuntimeHandle` 的状态是 `Down(原因)`，原因说明进程怎样结束。是否重启由应用决定：对这个插件的 fiber 调用 `restart`，会拉起新进程。启动期间可以 dispose 或 restart；启动失败时插件失败，不挡住行解析。

```rust
let runtime = LocalRuntime::node(node_package, anchor).host("probe", json!({ "record": "sync" }));
let handle = runtime.handle();   // 给 rutis-loader 的 InteropResolver
root.plugin(runtime);
```

| feature | 内容 | 默认 |
| --- | --- | --- |
| `node` | `LocalRuntime::node` | 开 |
| `python` | `LocalRuntime::python`（`.interpreter(路径)` 换解释器） | 关 |

`LocalRuntime::launcher(名字, Launcher, anchor)` 用任意 `rutis_interop::Launcher` 启动；它收到的最后两个参数是通道和 anchor。设置 `RUTIS_INTEROP_TRACE` 时，运行时通道上的每条消息在 stderr 记一行（方向和长度，不含内容）。

语言怎样启动（程序、参数、通道交接）由 rutis-interop 的 `Launcher` 给出，进程怎样拉起由 rutis-transport-local 负责，这个 crate 只做组合。
