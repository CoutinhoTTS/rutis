# rutis 0.5 → 0.6：可扩展的公开类型

0.6.0 把会继续扩展的公开类型标为 `#[non_exhaustive]`（[#44](https://github.com/arcships/rutis/issues/44)）。之后给这些结构体加字段、给这些枚举加变体不再是破坏性变更。给已有的带字段变体（例如 `CordisError::StaleGeneration { expected, current }`）加字段仍是破坏性变更，由发布前的兼容检查拦截（见文末）。服务、事件、插件等接口不变。

## 构造 `EventOptions`

`EventOptions` 不能再用结构体字面量构造，改用 `Default` 加设置方法：

| 0.5 | 0.6 |
|---|---|
| `EventOptions { prepend: true, ..Default::default() }` | `EventOptions::default().prepend(true)` |
| `EventOptions { once: true, ..Default::default() }` | `EventOptions::default().once(true)` |
| `EventOptions { prepend: true, once: true }` | `EventOptions::default().prepend(true).once(true)` |

字段仍可读取和赋值（`options.once = true`）。

## 匹配错误和状态

以下枚举的 `match` 需要一个兜底分支：

- 错误：`CordisError`、`ServiceReadFailure`、`ServiceWriteFailure`、`DisposeWaitError`
- 诊断与观察：`DependencyStatus`、`DispatchMode`

```rust
match error {
    CordisError::Closed => retry_later(),
    other => return Err(other),
}
```

`FiberState`、`EffectPhase`、`Effect` 不变，仍可穷尽匹配；`Snapshot` 不变，仍可用字面量构造（例如插件把它作为服务提供，或测试中构造假数据）。

## 解构诊断与记录

以下结构体由 rutis 生成、供读取，不能再在 crate 外用字面量构造；解构时加 `..`：

- `ServiceReadError`、`ServiceWriteError`
- `RuntimeDiagnostics`、`PluginDiagnostics`、`DependencyDiagnostics`、`ResolvedDependency`、`ServiceAccess`、`BindingDiagnostics`、`EffectMeta`
- `DispatchAttempt`、`FiberStatusChanged`

```rust
let PluginDiagnostics { name, state, .. } = plugin;
```

字段访问（`snapshot.state`）不受影响。

## 兼容检查

发布 rutis 前，CI 用 [cargo-semver-checks](https://github.com/obi1kenobi/cargo-semver-checks) 对照 crates.io 上的最新版本：补丁版本含破坏性变更时发布失败。PR 中同样运行该检查并列出破坏性变更，提示发布时需要提升 minor 版本，但不阻止合并。
