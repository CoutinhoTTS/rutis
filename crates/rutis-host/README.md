# rutis-host

A [rutis](https://github.com/arcships/rutis) host that needs no Rust: run plugins written in TypeScript, JavaScript and Python from a `rutis.json`, develop them with live reload, and link machines.

```bash
rutis-host new greeter --lang node     # or --lang python: a plugin project
rutis-host dev                          # in a plugin project: run it, reload on change
rutis-host check [rutis.json]           # describe every row; fail on what cannot run
rutis-host run [rutis.json]             # run a host
```

```json
{
  "runtimes": { "node": { "project": "." }, "py": { "project": "plugins" } },
  "rows": [
    { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "py:llm_gateway" }
  ]
}
```

Install: `npx @arcships/rutis-host`, `uvx rutis-host`, `cargo install rutis-host`, or a binary from the GitHub release. Linux and macOS; on Windows, use WSL.

Guide (Chinese): [docs/guide/rutis-host.md](https://github.com/arcships/rutis/blob/main/docs/guide/rutis-host.md).
