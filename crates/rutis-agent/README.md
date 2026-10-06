# rutis-agent

A minimal agent framework built on the five pillars of [rutis](https://crates.io/crates/rutis): one aimux
[`LanguageModel`](https://crates.io/crates/aimux-core) service + one `ToolRegistry`
plugin + one driver plugin implementing the `Agent` interface + one in-memory session.

- Streaming `followup` only triggers turns and reports back terminal states; in-progress deltas are broadcast via `agent/*` events
- Waterfall middleware at key loop points: `agent/pre-step` (rewrite/reject messages) +
  a three-stage tool pipeline (pre-execute gating / execute / post-execute result decision)
- Minimal mode ships with `bash` + `replace_text` tools (a coding agent that can edit files and
  run commands, semantically aligned with deepseek-harness)
- A ratatui TUI plugin subscribes to events for rendering

## Usage

```toml
[dependencies]
rutis-agent = "0.1.0"
```

For the command-line form see [rutis-cli](https://crates.io/crates/rutis-cli). Design and acceptance
documents live in the [repository docs](https://github.com/eric8810/rutis/tree/main/docs).

## License

MIT (inherited from [Cordis](https://github.com/shigma/cordis) © Shigma).
