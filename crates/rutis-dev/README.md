# rutis-dev

A development channel for a running rutis host (design: [design-host-dev-mode](../../docs/design-host-dev-mode-2026-09-25.md)), built on [rutis-loader](../rutis-loader): local Unix socket, JSON lines, protocol version 1.

```rust
let channel = rutis_dev::DevChannel::start(root.clone(), loader.clone(), DevOptions::new("/tmp/host.sock")).await?;
```

A request is a single-line JSON object: `cmd` is the command; `req` (optional, any JSON) is echoed back in the response for correlation; the remaining fields are command arguments, where `id` refers to a row id. Responses look like `{"req": …, "ok": true, "result": …}` or `{"req": …, "ok": false, "error": "…"}`.

| Command | Arguments | Effect |
| --- | --- | --- |
| `hello` | | Protocol version and host identity (`DevOptions::hello`) |
| `describe` | | Fibers, service bindings, event backlogs, loader rows (with their fibers) |
| `status` | | Loader rows; rows loaded through the channel are marked `dev` |
| `watch` | | Continuously pushes events thereafter: fiber states, services coming up/down, loader changes |
| `load` | `name`, `id?`, `config?`, `parent?` | Adds a row in the channel's overlay layer |
| `swap` | `id` | `Loader::reload`: all-or-nothing; on failure the old version keeps running |
| `unload-dev` | `id` | Unloads a row loaded through the channel |

Loaded rows live in the overlay layer (`Loader::set_overlay`): not persisted, not written into user config, and preserved when the application reconciles on its own. The socket is created with mode `0600`; a non-socket file already at the path (regular file, symlink) is always an error and never deleted; start the channel only in development hosts. Mutating commands (`load`, `swap`, `unload-dev`) all go through an audit hook, logged to stderr by default.

Command-line client:

```sh
cargo run -p rutis-dev -- /tmp/host.sock load '{"name": "dylib:greeter", "id": "g"}'
cargo run -p rutis-dev -- /tmp/host.sock swap '{"id": "g"}'
cargo run -p rutis-dev -- /tmp/host.sock watch
```

Not yet implemented (see design doc §11): bus probes and event payloads, recording, the `cargo xtask dev` loop, dev-composed manifests, and dev configuration for dylib retention limits.
