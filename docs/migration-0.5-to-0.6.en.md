# rutis 0.5 to 0.6: Extensible Public Types

Version 0.6.0 marks public types expected to grow with `#[non_exhaustive]` ([#44](https://github.com/arcships/rutis/issues/44)). Adding fields to these structs or variants to these enums is no longer a breaking change. Adding a field to an existing data-carrying variant (for example, `CordisError::StaleGeneration { expected, current }`) remains breaking and is caught by the compatibility check before release (see below). Service, event, and plugin interfaces are unchanged.

## Construct `EventOptions`

`EventOptions` can no longer be created with a struct literal. Start with `Default` and use the setters:

| 0.5 | 0.6 |
|---|---|
| `EventOptions { prepend: true, ..Default::default() }` | `EventOptions::default().prepend(true)` |
| `EventOptions { once: true, ..Default::default() }` | `EventOptions::default().once(true)` |
| `EventOptions { prepend: true, once: true }` | `EventOptions::default().prepend(true).once(true)` |

Fields can still be read and assigned (`options.once = true`).

## Match errors and statuses

Matches on the following enums need a fallback arm:

- Errors: `CordisError`, `ServiceReadFailure`, `ServiceWriteFailure`, `DisposeWaitError`
- Diagnostics and observation: `DependencyStatus`, `DispatchMode`

```rust
match error {
    CordisError::Closed => retry_later(),
    other => return Err(other),
}
```

`FiberState`, `EffectPhase`, and `Effect` are unchanged and remain exhaustively matchable. `Snapshot` is unchanged and can still be constructed with a literal, for example as a service provided by a plugin or as test data.

## Destructure diagnostics and records

rutis generates the following read-only structs. They can no longer be constructed with literals outside the crate; add `..` when destructuring:

- `ServiceReadError`, `ServiceWriteError`
- `RuntimeDiagnostics`, `PluginDiagnostics`, `DependencyDiagnostics`, `ResolvedDependency`, `ServiceAccess`, `BindingDiagnostics`, `EffectMeta`
- `DispatchAttempt`, `FiberStatusChanged`

```rust
let PluginDiagnostics { name, state, .. } = plugin;
```

Field access such as `snapshot.state` is unchanged.

## Addition

- `RuntimeDiagnostics::event_backlogs`: for each event key, the number of received `emit` calls still pending and the wait time of the oldest one (`EventBacklog`), to help find slow listeners ([#43](https://github.com/arcships/rutis/issues/43)).

## Compatibility checks

Before releasing rutis, CI runs [cargo-semver-checks](https://github.com/obi1kenobi/cargo-semver-checks) against the latest version on crates.io. A release fails if a patch version contains a breaking change. Pull requests run the same check and list breaking changes, indicating that a minor version bump will be needed for release, but the check does not block merging.
