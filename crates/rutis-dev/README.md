# rutis-dev

运行中的 rutis 宿主的开发通道（设计见 [design-host-dev-mode](../../docs/design-host-dev-mode-2026-09-25.md)），建在 [rutis-loader](../rutis-loader) 上：本地 Unix socket，JSON lines，协议版本 1。

```rust
let channel = rutis_dev::DevChannel::start(root.clone(), loader.clone(), DevOptions::new("/tmp/host.sock")).await?;
```

请求是一行 JSON 对象：`cmd` 是命令，`req`（可选，任意 JSON）原样回到响应里用于关联；其余字段是命令参数，其中 `id` 指行 id。响应形如 `{"req": …, "ok": true, "result": …}` 或 `{"req": …, "ok": false, "error": "…"}`。

| 命令 | 参数 | 作用 |
| --- | --- | --- |
| `hello` | | 协议版本与宿主身份（`DevOptions::hello`） |
| `describe` | | fiber、服务绑定、事件积压、loader 行（含对应的 fiber） |
| `status` | | loader 行；经通道装载的标为 `dev` |
| `watch` | | 之后持续推送事件：fiber 状态、服务上下线、loader 变化 |
| `load` | `name`, `id?`, `config?`, `parent?` | 在通道的 overlay 层加一行 |
| `swap` | `id` | `Loader::reload`：全有或全无，失败时旧版本继续运行 |
| `unload-dev` | `id` | 卸掉经通道装载的一行 |

装载的行放在 overlay 层（`Loader::set_overlay`）：不持久化、不进用户配置、应用自己 reconcile 时保留。socket 以 `0600` 创建；路径上已有的非 socket 文件（普通文件、符号链接）一律报错不删；只在开发宿主里启动。改动类命令（`load`、`swap`、`unload-dev`）都交给审计钩子，默认打到 stderr。

命令行客户端：

```sh
cargo run -p rutis-dev -- /tmp/host.sock load '{"name": "dylib:greeter", "id": "g"}'
cargo run -p rutis-dev -- /tmp/host.sock swap '{"id": "g"}'
cargo run -p rutis-dev -- /tmp/host.sock watch
```

尚未实现（见设计稿 §十一）：总线 probe 与事件 payload、录制、`cargo xtask dev` 循环、dev 合成清单、dylib 保留上限的 dev 配置。
