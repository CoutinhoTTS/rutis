# Contributing to rutis

[中文](CONTRIBUTING.md)

Thank you for spending time on rutis. A bug report, a sentence in the docs that did not make sense, a patch: all of it helps.

## Questions and feedback

- **Bugs**: [open an issue](https://github.com/arcships/rutis/issues/new/choose) with the version, platform, a minimal reproduction and the behavior you expected. A failing test is the best reproduction.
- **Feature ideas**: open an issue describing the problem you want to solve first. For changes where the design is open, agreeing on the direction before writing code saves time on both sides.
- **Security issues**: please do not open a public issue; report them privately as described in the [security policy](SECURITY.md).

## Repository layout

| Directory | Contents |
| --- | --- |
| `crates/rutis` | The core: plugins, fibers, services, events |
| `crates/rutis-loader` | The data-driven plugin control plane |
| `crates/rutis-bridge` | Across processes, languages and machines: channels, sessions, language runtimes, nodes |
| `crates/rutis-host` | A host that needs no Rust |
| `crates/rutis-sdk`, `crates/rutis-dylib*` | The dylib plugin toolchain |
| `node/` | npm packages: the plugin SDK `@arcships/rutis`, the runtime, the host |
| `python/rutis` | The PyPI package `rutis`: Python plugin SDK and runtime |
| `docs/` | Guides, design records, migration guides |

## Setting up

- Rust: `rust-toolchain.toml` pins the toolchain, and `rustup` installs it automatically.
- Node 24+ and Python 3.12+, when working on the language runtimes or the host.
- Linux or macOS. On Windows, use WSL.

## Running the tests

```bash
cargo test --workspace                                  # the core and most crates
cargo test -p rutis-bridge --all-features               # channels, sessions, runtimes, nodes
cargo test -p rutis-loader --features node,python,peer  # every kind of loader row
npm --prefix node/rutis test
npm --prefix node/rutis-runtime ci && npm --prefix node/rutis-runtime test
(cd python/rutis && python3 -m unittest discover -s tests)
```

When you change one part, running the related tests is enough; CI runs the full set.

## Sending a change

1. Branch from `main`.
2. Keep it focused: one pull request, one concern.
3. Behavior changes come with tests; for a bug fix, start with a test that reproduces it.
4. Run `cargo fmt` and the relevant tests before pushing.
5. Write commit messages in the [Conventional Commits](https://www.conventionalcommits.org/) style, such as `fix(loader): …`, `feat(bridge): …`, `docs: …`.
6. Update the docs along with public APIs or user-visible behavior; for breaking changes, describe the migration in the pull request.

### Documentation

The docs are written in Chinese first, with English versions in matching `.en.md` files. When you change one, please update the other if you can. If you only write one of the two languages, that is fine: say so in the pull request and we will fill in the other.

## Releases

Maintainers cut releases; the process is in [docs/release.en.md](docs/release.en.md).

## License

Contributions to this repository are released under the [MIT](LICENSE) license.
