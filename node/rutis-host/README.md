# @arcships/rutis-host

[rutis-host](https://github.com/arcships/rutis/blob/main/docs/guide/rutis-host.md) on npm: a rutis host that needs no Rust. It brings the binary for your platform and the Node runtime (`@arcships/rutis-runtime`).

```bash
npx @arcships/rutis-host new greeter --lang node
cd greeter && npm install
npx rutis-host dev
```

Linux and macOS, x64 and arm64; on Windows, use WSL.
