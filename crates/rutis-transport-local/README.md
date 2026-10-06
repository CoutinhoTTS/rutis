# rutis-transport-local

本机承载：同一台机器上的 Unix socket 通道，以及由它拉起的进程。

`LocalPlugin` 提供 `Transport#local`（插件名 `rutis-bridge/local`）：

- 拨号 `unix:<路径>`（或直接写路径）连到 Unix socket，按换行分帧。
- 用 `LocalTransport::spawner(名字, Spawn)` 登记拉起配置后，拨号 `spawn:<名字>` 会启动进程并接入它：
  - `Spawn` 是程序、参数、环境、工作目录，以及通道怎样交给进程：`Handover::Inherit`（继承 fd 3，进程收到参数 `fd:3`）或 `Handover::DialBack`（私有目录里的 socket 路径，进程回拨）。
  - 通道拥有进程：通道结束的原因说明进程怎样结束（如 `the process exited with exit status: 7`）；关闭通道后给进程 2 秒自行退出，否则结束它。
  - 无法启动的程序（找不到、无权限）是 `Incompatible`。
- 卸载插件会关闭它打开的所有通道，也就结束它拉起的进程。
- `LocalTransport::trace(sink)` 记录之后打开的每条通道上的消息（方向和长度）。

它不认识进程里跑的是什么。本机语言运行时（Node、Python）由 [`rutis-runtime-local`](../rutis-runtime-local) 在它之上组合；语言怎样启动由 rutis-interop 的 `Launcher` 给出。

拉起进程只在 Unix 上可用；其他平台拨号 `spawn:` 返回 `Incompatible`。
