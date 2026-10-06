# rutis Guide

Choose a guide based on what you want to do:

| I want to… | Read |
| --- | --- |
| Write a plugin in TypeScript / JavaScript | [Write a TypeScript plugin](typescript-plugin.en.md) |
| Write a plugin in Python | [Write a Python plugin](python-plugin.en.md) |
| Look up the plugin API and value passing rules | [Plugin API](plugin-api.en.md) |
| Run plugins without writing Rust | [rutis-host and rutis.json](rutis-host.en.md) |
| Connect machines and run plugins remotely | [Connect nodes](nodes.en.md) |
| Load plugins in my Rust application | [Embed in a Rust application](rust-host.en.md) |
| Connect an existing Cordis application to rutis, or mount Cordis plugins in Rust | [Cordis](cordis.en.md) |

## Packages

| User | Rust (crates.io) | Node (npm) | Python (PyPI) |
| --- | --- | --- | --- |
| Plugin author | `rutis-sdk` (dylib plugins) | `@arcships/rutis` | `rutis` |
| Host (runs plugins) | `rutis`, `rutis-loader`, `rutis-bridge` | `@arcships/rutis-runtime` | `rutis` |
| Host without Rust | `rutis-host` | `@arcships/rutis-host` | `rutis-host` |

Except for the `rutis` core, these packages are released together with matching version numbers (a release train).

## Requirements

- Linux or macOS. Local plugin execution depends on Unix; use WSL on Windows.
- Node 24 or later for TypeScript / JavaScript plugins.
- Python 3.12 or later for Python plugins.
- Rust 1.85 or later, only when embedding rutis in Rust.
