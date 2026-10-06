# rutis-loader: Plugin Management Layer (Design)

Status: P1–P6 implemented (`rutis-loader`, `rutis-dsh` `profile` module, `rutis-dylib` `DylibResolver`, `rutis-dev`, and `rutis-loader` `InteropResolver`). Date: 2026-10-02.
Compared with dsh-vendored `@deepseek-ai/cordis-plugin-loader` 1.0.5 (`src/config/{entry,tree,group}.ts`, `src/index.ts`), `cordis-plugin-include` 1.0.9, `dsh-app-boot`, and `dsh-config-editor`.

## 1. Problem

The rutis core manages **one** plugin: load, unload, restart, and update config (`Ctx::plugin_with` + `FiberView`). Projects where “which plugins are loaded” is data-driven (users select plugins, UI manages plugins, or profiles assemble plugins) need to manage **many** plugins:

- Load by name instead of hard-coding types in code.
- Give every plugin a stable ID; add/remove/update config/enable/disable/group at any time.
- Layer config from multiple sources (defaults, bundles, user, temporary overlay), persist changes, and restore them at next startup.
- List currently loaded plugins and their states for UI, CLI, and dev channels.

Cordis uses `cordis-plugin-loader` + `cordis-plugin-include` for this; rutis has no equivalent. This design adds standalone crate `rutis-loader`.

Projects whose plugin composition is hard-coded should use `ctx.plugin` directly; they do not need loader.

Out of scope: package installation, bundles, and profiles (the dsh-plugin-manager layer); those belong to the application.

## 2. Summary decisions

1. **New crate, no core changes.** Use only public APIs already present: `plugin_with` (factory load), `FiberView::update` (config dry-run), `watch`, and `dispose`.
2. **No config-file requirement; use desired state.** Loader input is ordered data layers (each a list of patches); combine them into “what should be running,” then reconcile actual runtime toward it. The application decides source and storage: files, database, or hard-coded data (§8).
3. **Imperative API means “edit editable layer + reconcile.”** Write only to the chosen editable layer and pass persistence hook to application. Reject edits overridden by upper layers; roll back if reconcile fails (§8).
4. **Configuration is JSON** (`serde_json::Value`), matching rutis-sdk `ConfigValue`.
5. **Name lookup is a `Resolver` trait:** built-in table, dylib, and interop each implement it.
6. **A group is one plugin** with child plugins beneath its ctx. Disabling group uses core cascading unload to remove children.
7. **Validate before commit.** Failed dry-run changes neither editable layer nor persistence and does not restart plugin; old version keeps running.
8. **Config schema starts in P1**, attached to resolved result (§9).
9. **Config `isolate` / `inject` are supported** through service-name → `TypeKey` catalog (§10).
10. **dsh-specific behavior stays in rutis-dsh:** profile layering, YAML I/O, `!!js` evaluator, file locks, file watching, nested include (§12).
11. **Self-unload is a known gap**, not intentionally omitted (§13).

## 3. Concepts

```text
Application-supplied layers (ordered)           loader
┌────────────────────┐
│ layer "defaults"   │──┐
│ layer "user" ✎     │──┼─ apply_patches ─→ desired tree ─ reconcile ─→ runtime (fiber tree)
│ layer "overlay"    │──┘                                              ▲
└────────────────────┘                                                 │
          ▲ persist(user)                                               │
          └──────────── imperative API edits ✎ layer ──────────────────┘
```

## 4. Resolver: name → plugin factory

```rust
pub trait Resolver: Send + Sync + 'static {
    /// Resolve module name. May be slow (file I/O, dlopen), so async.
    fn resolve<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Arc<Resolved>, LoadError>>;
}

pub struct Resolved {
    pub factory: Arc<dyn PluginFactory<Value>>,
    /// Config JSON Schema (§9). None if unavailable; loader still works without a form.
    pub schema: Option<Value>,
    /// Diagnostic metadata: version, source path, hash, etc.
    pub meta: Value,
}
```

Built-in implementations:

| Implementation | Names matched | Notes |
| --- | --- | --- |
| `Builtins` | Any registered name (exact match, highest priority) | Plugin table compiled into host. `register::<C: DeserializeOwned + JsonSchema>(name, factory)` automatically deserializes JSON into C (errors become `CordisError::Validation`) and generates schema with schemars. Can register `Typed<P>` or regular `Plugin`. |
| `DylibResolver` | `dylib:` prefix | Wraps rutis-dylib `Loader::load`. **Linux only** at current state; macOS waits for dylib support. |
| `InteropResolver` | Other npm package names | Later stage, see §15. |
| `Chain` | — | Tries resolvers in above order (§18-1). |

Why return factory rather than plugin instance: config changes go through `FiberView::update`, which works only for fibers loaded from a factory.

## 5. Loading: carry resolution result in config

Every entry is loaded through the same internal loader factory, with config type:

```rust
struct EntryConfig {
    resolved: Arc<Resolved>,   // module used by this generation
    value: Value,              // evaluated user config
}
```

This follows rutis-dylib `DylibConfig { module, value }`. It lets module-version changes use `update`:

- Only `config` changes → `update(EntryConfig { same resolved, new value })`.
- `name` changes or re-resolution (dylib upgrade) → `update(EntryConfig { new resolved, value })`.

Both inherit all `update` guarantees: failed dry-run does not change anything, PluginId remains stable, downstream dependents are evicted/reloaded normally.

**Exception:** dependency declaration (`injects`) is fixed at spawn (core D32f). If new module's `injects` differ, dispose old fiber and spawn new one; PluginId changes. A changed module-factory name (plugin identity) also requires reconstruction, as rutis-dylib `swap` rejects identity changes. Loader chooses the path automatically.

## 6. Groups

A group entry loads internal plugin `GroupPlugin`. Its `apply`:

1. Registers this group's current ctx with loader.
2. Loads each enabled child entry under that ctx with `plugin_with` and stores its `FiberView` in loader.
3. Returns cleanup that unregisters the group.

Consequences:

- Disabling/removing group unloads group fiber; core cascades to all children (D28 ownership). Loader only clears child views.
- Group restart reruns `apply`, rebuilding children from current desired tree.
- Moving entry to another group disposes under old group and spawns under new one (PluginId changes, like Cordis).

**Locking rules** (avoid deadlocks): loader state uses synchronous `Mutex`, never held across `await`; all writes (reconcile and imperative APIs) serialize on one async operation lock. `GroupPlugin::apply` touches only state lock, **never** operation lock. Otherwise “operation waits for group load → group load waits for operation lock” cycles.

## 7. Public API (control plane)

Install `LoaderPlugin::new(resolver, options)` on root. It provides `Arc<Loader>` for other plugins (e.g. management UI); host can also obtain handle before installing it.

```rust
pub struct LoaderOptions {
    pub persist: Arc<dyn Persist>,            // defaults to NoPersist
    pub expressions: Option<Arc<dyn Expressions>>, // §11
}

impl Loader {
    // Desired state (§8)
    async fn reconcile(&self, layers: Vec<Layer>, editable: Option<&str>) -> Result<ReconcileReport, LoaderError>;
    fn layers(&self) -> Vec<Layer>;

    // Query
    fn entries(&self) -> Vec<EntryInfo>;                 // tree order
    fn get(&self, id: &str) -> Option<EntryInfo>;
    fn locate(&self, plugin: PluginId) -> Option<String>; // find owning entry via diagnostics parent chain
    async fn schema_of(&self, name: &str) -> Result<Option<Value>, LoaderError>; // resolve only, no load; form before create
    fn evaluated(&self, id: &str) -> Option<Result<Value, LoaderError>>; // read-only evaluated config in effect (§11.2)

    // Edit (= editable layer + reconcile + persist; returns only when settled)
    // Validation/reconcile failure rolls back; editable layer and storage stay unchanged (runtime on apply failure: §8).
    // Only persistence failure (PersistFailed) leaves successful runtime/editable-layer update in pending-save queue.
    async fn create(&self, opts: NewEntry, parent: Option<&str>, position: Option<usize>) -> Result<(String, Option<FiberView>), LoaderError>; // waits for settle, §8
    async fn update(&self, id: &str, config: Value) -> Result<(), LoaderError>;
    async fn rename_module(&self, id: &str, name: &str) -> Result<(), LoaderError>; // change name, see §5
    async fn set_disabled(&self, id: &str, disabled: bool) -> Result<(), LoaderError>;
    async fn move_to(&self, id: &str, parent: Option<&str>, position: Option<usize>) -> Result<(), LoaderError>;
    async fn remove(&self, id: &str) -> Result<(), LoaderError>;

    // Runtime-only operations (do not alter desired state or persist)
    async fn reload(&self, id: &str) -> Result<(), LoaderError>; // resolve again, e.g. dylib upgrade
    async fn restart(&self, id: &str) -> Result<(), LoaderError>;

    // Persistence (§8 pending-save queue)
    async fn flush(&self) -> Result<(), LoaderError>;  // retry saving queued changes
    fn pending(&self) -> Vec<Edit>;

    // Wait
    async fn settled(&self);   // all resolutions and fiber transitions have landed, like Cordis tree.await()
}

pub struct EntryInfo {
    pub options: EntryOptions,     // original desired-tree values (expressions unevaluated)
    pub parent: Option<String>,
    pub origin: Origin,            // inserting layer and layers that override it
    pub status: EntryStatus,       // Disabled / Resolving / Unresolved(err) / Running(Snapshot)
    pub plugin: Option<PluginId>,
    pub view: Option<FiberView>,
    pub schema: Option<Value>,
    pub meta: Value,
}
```

After each reconcile or imperative operation, publish `LoaderChanged { ids, kind }` on bus. Fiber status changes remain in core `FiberStatusChanged`; correlate through `EntryInfo::plugin`.

### Failure semantics

| Situation | Behavior |
| --- | --- |
| Resolve failure | Keep entry in desired tree as `Unresolved(err)`, with no fiber. `reload` retries. (Cordis only logs.) |
| First-load apply failure | Same as core: fiber `Failed`, entry retained |
| Imperative-edit dry-run failure | Return Err; editable layer unchanged, no persistence, old config keeps running |
| Dry-run succeeds, new instance apply fails | Roll back old editable layer and reconcile to reload old config; return `ApplyFailed`; do not persist (§8) |
| Old config also fails during rollback | Return `RollbackFailed { apply, rollback }`; editable layer/storage remain old, row is `Failed`, matching storage |
| Stored version changed by another writer | Reapply this operation against latest content; after bounded retries return `Conflict` (§8) |
| Earlier queued edit no longer applies during conflict replay | Remove from queue; publish `PendingEditDropped { edit, error }`; continue replaying later edits (§8) |
| Non-conflict persistence failure | Return `PersistFailed`; runtime and in-memory editable layer already updated; keep edit in **pending-save queue** and write whole queue on next save/`flush()` (§8) |
| Host shutdown | All operations return `Closed`; do not persist further |

## 8. Desired state, editable layer, and persistence

### Reconcile

`reconcile(layers, editable)`:

1. Combine layers in order with `apply_patches` (§11.1), collecting warnings (e.g. patch target not found).
2. Compare desired tree with current runtime by ID: new → spawn; removed → dispose; only `config` changed → update; `name` changed → §5; `isolate` / `inject` / parent group changed → rebuild; `disabled` change → dispose or spawn.
3. Wait until tree settles and return `ReconcileReport { warnings, new_failures }`. **Settled** means no fiber is transitioning (Loading / Unloading); Active, Pending (dependency unavailable), Failed, and Unresolved all count as settled, so a missing dependency does not make caller wait indefinitely. `new_failures` contains only rows that newly failed in this reconcile; rows already failing are excluded (as in dsh `reconcileProfilePatches`).

When external data changes (e.g. dsh config file), application calls `reconcile` again with new layers. This only applies and reports; it **does not roll back**. Application owns external change and decides whether to undo it.

`editable` names the editable layer and may be absent (imperative edits then return `NoEditableLayer`).

### Imperative edits

Every imperative edit, including `create`, follows one process and **returns only when settled**:

1. Check ownership and overlays (below).
2. Compute new editable-layer content.
3. Dry-run (`resolve` + `validate_config` + `build` + instance `validate`); return Err on failure.
4. Reconcile with new layer; **if new failure appears, restore old layer, reconcile again, and return `ApplyFailed`**.
5. Add this `Edit` to pending-save queue and call `Persist::save` for entire queue (see persistence hook, queue, and multiple writers).

`create` is no exception: if dry-run passes but apply fails, roll back (remove inserted row from editable layer and reconcile), return `ApplyFailed`, and do not persist. If dependency is not ready, new row in `Pending` counts as success; persist and return `(id, view)`.

Reconcile happens before persistence, so storage contains desired state that passed validation and completed this reconciliation round; storage rollback is unnecessary. This does not mean every plugin is running: a row Pending on dependency can still be persisted.

**Apply failure details:** core `FiberView::update` is not atomic: it saves new config, unloads old instance, then loads new. Dry-run only catches validation/construction errors, not `apply` errors.

- If new instance apply fails, old instance is already unloaded. Step 4's rollback is a second `update` that reloads old config, not “nothing happened.”
- During rollback, plugin and its dependents may briefly be unavailable while evicted and reloaded. This is inherent in core update; loader promises no seamless swap.
- If old config also cannot reload (e.g. external resource broke), return `RollbackFailed { apply, rollback }` with both errors. Editable layer and storage stay old; row is `Failed`. Config/storage match, but runtime is broken; later reconcile or restart may retry.
- Callers must not treat every `update` error as “nothing changed.” Only dry-run errors (e.g. `Validation`) guarantee runtime was untouched.

**Ownership rules:** patches can insert rows and override fields, but cannot remove or move rows:

| Operation | Row inserted by editable layer | Row inserted by lower layer |
| --- | --- | --- |
| update (`config`) | Edit that insert | Add override patch `{ id, config }` |
| set_disabled | Edit that insert | Add override patch `{ id, disabled }` |
| Change isolate / inject | Edit that insert | Add `{ id, isolate }` / `{ id, inject }` override patch |
| rename_module | Edit insert's `name` | Reject `NotOwned`: patch `name` only **checks** identity; it does not override (Cordis semantics) |
| remove | Remove insert (its child rows disappear too) | Reject `NotOwned`: no delete operation in patch, so row would return on recombine. Suggest `set_disabled`. |
| move_to | Move insert | Reject `NotOwned` |
| create (root or group owned by editable layer) | Insert at requested position | — |
| create (group owned by lower layer) | — | Add `{ insert: [...], id: <group id> }`; append only. If `position` supplied, return `Unsupported`. |

Merge multiple overrides for one row into one patch; later field values override earlier values rather than accumulating patches.

**Overlay rule** (matching dsh-config-editor): if a layer **above** editable layer overrides same field on same row, reject edit with `OverriddenByLayer { layer }`. Otherwise user would think edit succeeded when it has no effect.

### Persistence hook

```rust
pub trait Persist: Send + Sync + 'static {
    /// Read latest stored content/version for this layer, used to retry after conflict.
    fn load<'a>(&'a self, layer: &'a str) -> BoxFuture<'a, Result<(Vec<Patch>, Version), LoaderError>>;
    /// Write only if current storage version equals `expected`; return new version, else Conflict.
    /// `edits` is entire pending queue since expected version, in order, so implementation can patch locally
    /// (e.g. preserve file comments); `patches` is final layer content.
    /// Contract: applying edits in order to content at `expected` must produce `patches`.
    fn save<'a>(&'a self, layer: &'a str, expected: &'a Version, edits: &'a [Edit], patches: &'a [Patch])
        -> BoxFuture<'a, Result<Version, PersistError>>;
}

/// One imperative operation: Create / Update / SetDisabled / Rename / Move / Remove, including arguments.
pub enum Edit { /* … */ }
```

- Loader serializes `save` calls within one process and preserves submission order.
- If implementation edits storage locally, verify “local-edit result == `patches`”; on mismatch fall back to rewriting entire layer (may lose comments) and warn. Losing formatting is preferable to losing changes.
- Built-in `NoPersist` saves nothing and has constant version.
- Application chooses files, database, or remote storage.

### Pending-save queue

Loader keeps an in-memory **pending-save queue**: edits already applied by successful reconcile but not yet stored since last successful save, in order.

- After successful imperative edit, append to queue and call `save(expected = last successful version, edits = whole queue, patches = current editable layer)`.
- On save success, record new version and **clear queue**.
- On non-conflict save failure, preserve queue as-is and return `PersistFailed`; next edit or `flush()` writes all of it together.
- Queue is memory-only and lost if process exits. `PersistFailed` must reach caller (UI should show “unsaved”); application may periodically call `flush()`.

### Multiple writers

Several processes may modify same storage (e.g. dsh CLI and web UI). A write lock alone is insufficient: both processes could start at same version and change different rows; serialized writes still let later stale snapshot erase earlier edit.

Use **version comparison (CAS)**:

1. If storage version differs from expected, `save` returns `Conflict`.
2. Loader calls `Persist::load` for latest content and version as new base.
3. **Replay entire pending queue in order** onto latest data (operations are semantic, such as “set X config to Y”). Re-run steps 1–4 of imperative process for each edit (ownership, overlays, dry-run, reconcile, rollback):
   - If an older queued edit no longer applies (e.g. another writer deleted its row), remove it and emit `PendingEditDropped { edit, error }`; continue.
   - If current operation no longer applies, remove it and return its error.
4. Save remaining queue using new version as expected.
5. After bounded retries (default 3), return `Conflict` and retain queue; next edit or `flush()` retries.

Example: this process changes A; save fails and queue is A → another process commits C → this process changes B and hits conflict → load latest data containing C, replay A then B → write once → storage has A, B, C and queue is empty.

Version also lets application file watcher distinguish “someone else changed it” from “my latest write”: if version equals just-saved version, do not reconcile again.

### Three common usage patterns

| Scenario | Usage |
| --- | --- |
| API only, no persistence | One empty editable layer + `NoPersist`; state lost at restart |
| API with persistence | Load editable layer from own storage at startup and `reconcile([user], Some("user"))`; imperative edits save through `Persist` |
| Layered (dsh) | Application assembles bundle, user, home, and CLI layers and marks user editable; file changes trigger reconcile (§12) |

## 9. Config schema (P1)

**Why in P1:** dsh Models/settings pages build config forms from schema; schema also marks volatile fields that do not restart plugin.

**Location:** `Resolved::schema`, supplied by Resolver; **no core changes**. Do not add method to `PluginFactory`; projects not using loader are unaffected.

| Source | How | Stage |
| --- | --- | --- |
| Builtins | Generate with schemars when `C: JsonSchema`; registration may also accept hand-written schema | P1 |
| dylib | Add optional schema to rutis-sdk `PluginMeta`; changes SDK ABI and requires SDK version bump | P3 |
| interop (JS plugin) | Node converts schemastery to JSON Schema. dsh already has `--dump-config-schema`; conversion exists | P6 |

Expose as `EntryInfo::schema` and `Loader::schema_of(name)` (obtain form before creating plugin).

Loader itself does **not** validate using schema; plugin `validate_config` remains authoritative. Schema is for display and comparison only.

## 10. Config `isolate` / `inject`: service-name catalog (P2)

**Problem:** Cordis config uses strings (`isolate: { llm: true }`, `inject: [llm]`); rutis distinguishes services by `TypeKey`. A “service name → `TypeKey`” map is needed.

`ServiceCatalog` is registered by:

- Builtins: `builtins.service::<dyn Llm>("llm")`.
- dylib: service names in rutis-sdk metadata (same SDK change as §9).
- interop: binding generator already knows names (`Bindings::provide("systemPrompt")`); register them too.

**Syntax** (Cordis-like; YAML for readability):

```yaml
- id: agent-a
  name: rutis-agent
  isolate:
    llm: true          # private scope for this entry (label = "entry:<id>")
    tools: shared-x    # entries with same label share scope
  inject: [llm]        # extra gating dependency; wait for llm before start
```

**Implementation:**

- isolate: before spawn, call `ctx.isolate(key, label)` on parent ctx in sequence, then `plugin_with` on returned ctx. This directly uses existing core isolate semantics (same labels merge).
- inject: append corresponding `TypeKey` to entry factory's `injects`.
- Changing isolate or inject rebuilds fiber (PluginId changes); ctx and injects are fixed at spawn. Cordis also remounts on these changes.
- Unknown service name puts entry in `Unresolved`, listing unknown name in error.

**Existing plugin code:**

- **Rust plugins:** unaffected. Code `injects()` / `Deps` / `ctx.isolate` continues to work; configured isolate/inject is layered on top.
- **JS plugins (through interop):** they run in real Cordis in Node, where isolate/inject works as usual. Before P6, an isolate-dependent subtree such as `dsh-agent-preset-registry` is mounted as one interop mount and managed by Node-side Cordis loader. When rutis-loader manages JS plugins individually, `InteropResolver` forwards isolate to Node (P6).

No intercept: rutis `ServiceIntercept` intercepts service reads/writes, unlike Cordis intercept for merging per-service config. No config-layer use was found in dsh plugin source.

## 11. Patch semantics and expressions

### 11.1 `apply_patches` (P1)

Pure function matching Cordis `applyEntryPatches` item by item. Offline tools can reuse it so “dumped” config matches actual startup:

- `{ id, ...fields }`: find by ID and **replace entire fields** (`config` replaced as a whole, not merged). If `name` is supplied and mismatches, skip with warning.
- `{ insert: [...], id? }`: insert entries; if id given, insert in that group (if target is not group, warn and skip).
- Missing target → warn and skip, do not fail.
- Build ID index only once at start, then add inserted entries to it. Thus if a patch replaces an entire group's `config` (replacing children), later patches **cannot see** those new children. This quirk is existing behavior; reproduce it rather than “fixing” it.
- Do not mutate input (deep-copy first), so removing a layer can cleanly recombine.

### 11.2 Expressions (P2)

Cordis config can contain `!!js` expressions, parsed as nodes like `{ "__jsExpr": "<source>" }`. Loader reuses this JSON convention and does not care what file format contained it.

- **`__jsExpr` is reserved:** any object with exactly this one key and string value is an expression node. Ordinary config must not use that shape (same in Cordis). Ordinary strings are always literal, never expressions.
- Expressions may occur at any depth in `disabled` and `config` (so `disabled` type is `Value`, not `bool`).
- **Keep two configs distinct:**
  - **Raw config:** lives in layers/desired tree with expression nodes. `EntryInfo::options` returns it; editable layer and storage keep raw values only.
  - **Evaluated config:** recomputed before every load/update and passed only to plugin (`EntryConfig::value`); never written back or persisted.
- On edit, caller supplies **raw config**. UI needing “effective value” queries `Loader::evaluated(id)` separately. Do not feed evaluated result back to update or expressions become constants.
- Source information such as comments and file positions is not in loader data model; persistence implementation preserves it (§12).

Loader defines hook only, no evaluator implementation:

```rust
pub trait Expressions: Send + Sync + 'static {
    /// Evaluate expression node into JSON value.
    fn evaluate(&self, expr: &str, scope: &ExprScope<'_>) -> Result<Value, LoaderError>;
}
```

Replace evaluator's `&Ctx` with restricted `ExprScope`: only `has(name)` (any catalog name) and `read(name)` (services explicitly registered readable and serializable), structurally enforcing §18-6 so evaluator cannot get full ctx. Evaluate `disabled` against loader root ctx; `config` against row's own ctx (parent groups + row isolate). Re-evaluate every reconcile and update in place if result changes.

- **Evaluation timing** (same as Cordis): loader evaluates `disabled` when deciding whether to load; evaluate config expressions before every load/update and give result to plugin. Desired tree/editable layer always retain raw expression. Group's own `config` (child-entry list) is not evaluated; each child evaluates itself (Cordis “tree container remains literal” rule).
- If no hook is installed, expression node makes entry `Unresolved("no expression evaluator")`.
- JS-subset evaluator belongs to dsh and lives in rutis-dsh (§12).

## 12. rutis-dsh side (dsh-specific)

`crates/rutis-dsh` already hosts dsh through rutis-interop. The following are dsh concepts and belong there, not in rutis-loader.

### Layering rules

Sources: dsh-app-boot `readProfilePatches` / `loadProfileDirectory` / `applyEntryPatches`, and dsh `profile-boot`.

Base `<profile>/cordis.yml` contains `[]` (header comment says “edit cordis.patch.yml, not here”). **All entries come from patches.** rutis-dsh reads layers below and passes them to `Loader::reconcile`; user layer is editable:

| Order | Layer | Source | Missing file / parse failure |
|---|---|---|---|
| 1 | Bundle layer | Order of `dsh.profile.bundles` in profile `package.json`; each bundle package's `dsh.bundle.patch` (one file or ordered file list) | Skip entire bundle and record reason (package missing, no `dsh.bundle`, incompatible version) |
| 2 | User layer (**editable**) | `<profile>/cordis.patch.yml` | Missing = empty layer; parse failure = startup error |
| 3 | Home layer | `~/.dsh/cordis.patch.yml` | Same |
| 4 | CLI layer | Each `--patch <file>` in argument order | Missing or parse failure is an error (user explicitly named file) |
| 5 | Telemetry toggle | If `DSH_TELEMETRY_DISABLED` is nonempty and combined result contains `session-telemetry-otel`, append `{ id: session-telemetry-otel, disabled: true }` | — |

Each patch file must be a YAML top-level array of mappings; otherwise fail the whole file. For an inserted entry whose `name` is relative (`./`, `../`) or absolute path, rewrite it to a `file://` URL relative to that patch file.

### File I/O

- Read/write YAML; convert `!!js` tags to `{ "__jsExpr": ... }` nodes and back without changing source representation.
- Implement `Persist`, writing only user-layer file:
  - Version is content hash.
  - Under profile's **cross-process file lock** (`withFileLock` in dsh), `save` reads current file, compares version, then writes; return `Conflict` on mismatch. Lock makes this operation atomic; version comparison prevents lost updates (§8 multiple writers).
  - Rewrite at patch granularity: preserve unchanged patch source exactly (including comments/format); regenerate changed patch (its comments are lost). Parse result before writing and compare with `patches`; if unequal, regenerate whole layer. dsh-config-editor edits at YAML-node granularity using `yaml`, finer than this; see §19.
  - Use temporary file + atomic rename.

### Hot reload

Watch all layer files (like `dsh-hmr`); on change reread layers and call reconcile. Report nonempty `new_failures`, do not roll back (matching current dsh). If file cannot be read or parsed, warn and keep current runtime; a broken edit must not crash the process.

### Nested include

User config may contain nested include entries pointing to another file's subtree. rutis-dsh expands each to a group; its children come from that file plus its own patches, and child IDs gain `<include id>:` prefix. Entries inside nested includes **cannot be edited through API** because they belong to another file. This matches dsh-config-editor, which edits only under root includes.

### JS-subset evaluator (implements `Expressions`)

Evaluation is **required**. dsh base config uses it extensively; without it, dsh-base, dsh-web-app, and dsh-headless rows cannot start. Actual usage found in dsh packages:

| Category | Example |
|---|---|
| Environment variable + default | `process.env.DSH_PERMISSION_MODE ?? 'workspace-write'`, `process.env.X || 'Y'` |
| Type conversion | `Number(process.env.DSH_CONTEXT_WINDOW ?? 1000000)` |
| Platform check (often `disabled`) | `process.platform === 'win32'` |
| Process info | `process.cwd()` |
| Host function | `dshHomePath('sessions')` |
| Read startup-args service | `ctx.webStartup.port ?? 3080`, `ctx.headlessStartup.task` |
| Check service exists (often `disabled`) | `!ctx.get('profileContext')` |
| Other | `process.getBuiltinModule('node:path').join(...)` (one use in dsh-web-app) |

Copy JS syntax so existing config need not change:

- Support literals, member access, function calls, `??`, `||`, `&&`, `!`, `===`, `!==`, and ternary expressions.
- Only access names in scope:
  - `process.env`, `process.platform` (Node spelling such as `win32` / `darwin` / `linux`), `process.cwd()`, `Number`, `String`.
  - dsh host functions such as `dshHomePath`.
  - `ctx.<service>.<field>` and `ctx.get('<service>')`, restricted as in §18-6.
- Anything outside subset (e.g. `getBuiltinModule`) puts entry in `Unresolved("unsupported expression: ...")`; never silently compute wrong value. Rewrite last example as host function, e.g. register `pathJoin`.

In future P6 individually managed JS plugins may have config expressions evaluated as-is in Node, which provides full JS environment; loader still evaluates `disabled`.

### Migration path

Today `dsh/launcher.ts` starts dsh profile in Node and dsh-app-boot layers config there. With rutis-loader, rutis-dsh will layer in Rust and launcher.ts gradually shrinks to mounting JS plugins. While both layering implementations coexist, compare against `--dump-config` to ensure equivalent results.

## 13. Plugin unloads itself (P5, known gap)

**Cordis behavior:** plugin calls `ctx.fiber.dispose()` to unload itself; loader marks entry `disabled` and writes config. These do **not** count as self-unload: initiated by loader, parent group/tree is unloading, or hot-update replacement.

**rutis today:** plugin cannot access its `FiberView`, so this is missing. No such use was found in dsh plugin source; low priority.

**Solution:**

1. Add small core API `Ctx::dispose_self()` (or `Ctx::fiber_view()`).
2. In loader `watch()`, when entry fiber becomes `Disposed` and neither loader nor parent group initiated unload, treat as `set_disabled(true)`: edit editable layer and persist.

## 14. Differences from Cordis

| Cordis feature | rutis-loader | Notes |
|---|---|---|
| Config file (loader root tree + include) | No file requirement; layered desired state + persistence hook | §8; dsh file behavior in rutis-dsh |
| Config schema | P1 | §9 |
| Config `inject` / `isolate` | P2 | Needs service-name catalog, §10 |
| Config `intercept` | Not supported | Not same as rutis `ServiceIntercept`; dsh does not use it |
| Patch layering | P1 (`apply_patches`) | §11.1 |
| `!!js` expressions | Loader hook; JS-subset evaluator in rutis-dsh | §11.2, §12; unsupported subset errors explicitly |
| Volatile fields (change without restart) | P5 | Requires schema marker contract and plugin-side non-restarting update |
| Plugin self-unload → mark disabled | P5 | Known gap, §13 |
| Write file before validation | Validate and reconcile first, persist last | Stored desired state has passed validation and completed this reconcile |
| Edit persistence | Write editable layer; reject upper-layer override; rollback on failure | §8, matches dsh-config-editor |

## 15. Relationship to existing components

- **Dev channel** (design-host-dev-mode): its `load` / `swap` / `status` are loader `create` / `reload` / `entries` plus socket layer; build directly on loader later.
- **rutis-dylib:** becomes implementation detail of `DylibResolver`. Preserve its `spawn` / `swap` for projects not using loader.
- **rutis-interop:**
  - Current Cordis mount generates Rust bindings at build time (“static plugin”). Generated bindings can register in Builtins, but generated `Config` currently derives only `Serialize`; generator must add `Deserialize` and `JsonSchema` (or pass through Node schema).
  - JS plugins whose names are known only at runtime can be mounted with `Process::mount` by `InteropResolver`, but Rust side can only use untyped `call`; also needs schema export and isolate forwarding. This is P6.
- **rutis-dsh:** all dsh-only behavior lives here (§12).
- **TypedPlugin:** unaffected; `Typed<P>` is an ordinary `Plugin` and can be registered in Builtins.

## 16. Phases

1. **P1 loader core:** data model (EntryOptions / Patch / Layer), `apply_patches`, reconcile, editable layer and imperative API, `Persist` + `NoPersist`, `Builtins` + `Chain`, groups, schema (schemars), `LoaderChanged`. No core changes.
2. **P2 dsh config parity:**
   - rutis-loader: service catalog, configured isolate/inject, expression hook.
   - rutis-dsh: dsh layering, YAML I/O + Persist, file lock, hot reload, nested include, JS-subset evaluator.
   - Small independent core PRs: `impl Plugin for Box<dyn Plugin>`, lookup `FiberView` by `PluginId`, service-binding-change event.
   - Add `Deserialize` to interop-generated `Config`.
3. **P3 `DylibResolver` (Linux) + schema in SDK metadata;** macOS dylib separate effort. (Implemented: `rutis-dylib` `loader` feature; names `dylib:<directory>`. Service names were not put in SDK: catalog needs typed probes that dylib plugin cannot provide, and embedding rutis-loader into SDK ABI was undesirable. Instead host registers shared interface crate.)
4. **P4 dev channel** built on loader. (Implemented in `crates/rutis-dev`. Added loader overlay layer `set_overlay`: dev-loaded entries are not user config and are not removed by application reconcile; `reload` became all-or-nothing.)
5. **P5 volatile fields and plugin self-unload** (core `dispose_self` + loader recognition). (Implemented: when only schema fields marked `"x-volatile": true` change, loader saves config via new core `FiberView::set_config`, without restart, and sends plugin `VolatileUpdate` (`volatile_key(ctx)`, instance-ID event because core allows instance-qualified keys only within that instance's subtree). After plugin calls `ctx.dispose_self()`, loader sets row disabled, persists, and emits `LoaderChanged::SelfDisposed`; loader-initiated unload and group cascade are not mistaken for self-dispose.)
6. **P6 `InteropResolver`:** manage JS plugins individually, including schema export and isolate forwarding. (Implemented in rutis-loader `interop` feature. All JS rows share one Node process and Cordis Context; services between rows resolve natively. Names resolve using Node rules (`exports`) or file paths; schemastery `Config` converts to JSON Schema (`meta.volatile` → `x-volatile`); resolver forwards isolate/inject to Cordis (`Resolved::foreign_scope` + `Loader::row`). JS rows and Rust plugins do not share services; Rust services are exposed to JS rows through host. Since 2026-10-03, shared Node process is rutis-interop `CordisRuntimePlugin`, whose services rows inject; see [design-cordis-runtime-plugin](design-cordis-runtime-plugin-2026-10-03.en.md).)

## 17. Tests to write

**P1 (`rutis-loader`)**

- Create → running; valid update → reload with same PluginId; invalid update → Err, old config still runs, editable layer unchanged, `Persist::save` not called.
- `set_disabled(true)` → unload and editable layer gains `disabled: true`; set false → reload.
- Ownership: lower-layer row `remove` / `move_to` returns `NotOwned`; editable-layer row can be removed/moved.
- Overlay: upper layer overrides row `config` → update returns `OverriddenByLayer`.
- Rollback: edit creates newly failing row → editable layer restored, runtime returns to old config, Err returned.
- Reconcile additions/removals/config/name/parent-group changes take correct path; already-failing rows are not new failures.
- Imperative edit without editable layer → `NoEditableLayer`.
- Persistence: serialize `save` by commit order; failed save returns `PersistFailed` and edit stays queued; next edit or `flush()` writes full queue then clears it.
- Queue + conflict: A save fails → other writer commits C → edit B conflicts → final storage has A/B/C and queue clears.
- Drop during replay: queued A no longer applies (target deleted by other writer) → remove A and emit `PendingEditDropped`; B saves.
- Save contract: simulate storage doing local edit that differs from `patches` → rewrite entire layer and warn.
- Create settles: dry-run passes but apply fails → `ApplyFailed`, new row absent from editable layer, no persistence; dependency not ready → succeeds Pending and persists.
- Restart consistency: for create/update/set_disabled/isolate/inject/rename/move/remove, reconcile saved layers in fresh loader and get same desired tree as after edit.
- Ownership: rename lower-layer row → `NotOwned`; create in lower-layer group with `position` → `Unsupported`; without position appends.
- Multiple writers: two loaders share simulated storage, start same version and edit different rows → both retained; same row → latter reapplies against latest; persistent contention → `Conflict`.
- Apply failure: dry-run passes but new apply fails → reload old config, editable layer/storage unchanged, `ApplyFailed`; if rollback fails, `RollbackFailed` contains both errors, row Failed, old layer/storage retained.
- `apply_patches`: override, insert, patch after insert, missing target warns/skips, name mismatch skips, remove layer restores prior result; reproduce quirk where full group-config replacement hides new children from later patches.
- Disable group → all child plugins unload while child entries remain; enable group → children return in order.
- Change name with same injects → in-place update; changed injects → rebuild with new PluginId.
- Resolve failure → Unresolved; successful reload transitions to running.
- `schema_of` returns schemars schema; plugin without schema still works and returns None.
- Dependency chain A provides, B depends on A; update A evicts/reloads B (regression inheriting core semantics).
- Concurrency: update/remove same row deterministic and deadlock-free; operation during group apply is deadlock-free.
- Operation during host shutdown returns `Closed`.

**P2 (`rutis-loader`)**

- `isolate: true`: two entries provide same-named service but cannot see each other; same string label shares.
- Additional inject gating: Pending until dependency ready, then starts.
- Change isolate/inject rebuilds and changes PluginId; unknown service gives Unresolved and names it.
- Expressions: `disabled` expression controls load; `config` evaluated before each load; editable layer retains source text; no hook gives Unresolved; only sole-key `__jsExpr` object counts as expression, same key among other fields is ordinary object; `evaluated(id)` returns result without changing raw config.

**P2 (`rutis-dsh`)**

- Differential test: for same profile, layers assembled by rutis-dsh and `apply_patches` result match dsh `--dump-config` line by line. Include dsh-base + one mode bundle + repository `aimux.patch.yml` + user layer + `--patch`.
- Relative path names in `insert` resolve against patch file's directory.
- Edits touch only user-layer file and preserve comments; reject edits overridden by home or `--patch`.
- Two processes edit different rows in same profile simultaneously; both edits remain in file.
- Hot reload of malformed file keeps current runtime and warns.
- Nested include prefixes child IDs and included entries cannot be edited.
- JS-subset test for every category above; unsupported expression gives Unresolved.

## 18. Decisions

1. **No name prefix; name itself is identity.** Builtins may register any name, including npm package names (e.g. a Rust rewrite can register as `@deepseek-ai/dsh-llm`). Resolution order: exact Builtins lookup; then explicit prefix such as `dylib:`; finally (P6) npm name to interop. dsh config and patches locate rows by name (patch verifies name match), so migration must preserve names. Replacing JS implementation with Rust should change implementation only, not config.
2. **`create`, like every imperative edit, waits until settled.** No exception: validation failure changes nothing; apply failure rolls back, returns `ApplyFailed`, and does not persist; dependency-not-ready row Pending is settled, succeeds and persists. Earlier “create does not wait for startup” contradicted rollback semantics and was removed. `reconcile` differs: it applies external layers as supplied, keeps rows that cannot start in desired tree as `Unresolved` / `Failed`, does not delete config or roll back.
3. **Writes:** loader serializes `Persist::save` within process and preserves order; cross-process lost updates are prevented with version compare + replay (§8). File lock and atomic replacement belong to file implementation in rutis-dsh (§12).
4. **Editing overridden fields:** as dsh-config-editor does, write to editable layer; reject when upper layer overrides; roll back on failure (§8).
5. **Support `!!js`:** rutis-loader provides expression hook; JS-subset evaluator lives in rutis-dsh (§11.2, §12). “Never evaluate” is not viable.
6. **Expose only host-registered services to expressions.** `ctx.<service>.<field>` reads only services explicitly marked expression-readable by host (e.g. startup-argument services `webStartup`, `headlessStartup`); other services return error. `ctx.get('<name>')` only tests whether service exists, not contents, so it is available for every name in catalog. Config cannot read plugin internals and failures are easier to diagnose.
7. **rutis-loader does no file I/O.** Input is ordered data layers; persistence is a hook. Hard-coded plugin compositions do not need loader; dynamically managed projects choose storage. dsh file rules belong in rutis-dsh.

## 19. Open question

- Preserve user-layer comments at patch granularity (§12). If comments inside a changed patch must also survive, use a Rust YAML editor preserving comments or implement source-position-based node replacement.

Example desired tree:

```text
Root group
 ├─ Entry "llm"      name = "@rutis/dsh-aimux"    config = {...}
 ├─ Entry "tools"    name = "rutis-tools"         disabled = true
 └─ Entry "agents"   group = true
     ├─ Entry "a1"   name = "dylib:agent-x"
     └─ Entry "a2"   ...
```

```rust
/// An entry in the desired tree (same shape as Cordis EntryOptions; no intercept, see §14).
#[derive(Serialize, Deserialize, Clone)]
pub struct EntryOptions {
    pub id: String,            // unique in the complete desired tree
    pub name: String,          // module name passed to Resolver and row identity (§18-1)
    #[serde(default)]
    pub config: Value,         // for group, child EntryOptions array (as in Cordis)
    #[serde(default)]
    pub group: bool,
    #[serde(default)]
    pub disabled: Value,       // bool or expression node (§11)
    #[serde(default)]
    pub inject: Option<Vec<String>>,                  // P2, see §10
    #[serde(default)]
    pub isolate: Option<BTreeMap<String, Isolate>>,   // P2, see §10
}

/// `true` = private scope for this entry; string = shared scope for same label (Cordis LocalRealm / GlobalRealm).
#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Isolate { Private(bool), Shared(String) }

/// A patch (same shape as cordis-plugin-include PatchOptions).
#[derive(Serialize, Deserialize, Clone)]
pub struct Patch {
    pub id: Option<String>,
    pub insert: Option<Vec<EntryOptions>>,
    pub name: Option<String>,
    #[serde(flatten)]
    pub overrides: Map<String, Value>,   // config / disabled / group / inject / isolate …
}

pub struct Layer {
    pub name: String,
    pub patches: Vec<Patch>,
}
```
