# TypedPlugin: Typed Dependencies (Draft)

Status: implemented (#94; closes #50), third revision. Date: 2026-10-02.

The second revision addressed four review comments:
- Runtime keys (qualifiers and instances) now have an interface.
- The factory and declaration come from the same dependency description.
- Trait-object services are supported.
- Loss of a dependency is proven by a failed strict read in the current generation (revision 3 fixes two “binding identity” holes from revision 2).

## Goal

A plugin writes one dependency description. The framework gates on it and passes the resolved values to `apply` as arguments. Declarations and reads come from the same description; disagreement becomes a compile error.

In the existing API, `injects()` returns runtime `TypeKey` values, while `ctx.get` / `ctx.require` spell the types again. A mismatch (declaring A but reading B, or reading an undeclared dependency) appears only at runtime.

## Relationship to Cordis and the existing `Plugin`

- **Behavior is unchanged:** `Typed<P>` / `TypedFactory<F, C>` wrap typed plugins and factories as ordinary `Plugin` / `PluginFactory`. Gating, eviction, reload, and invisibility during unload work exactly as for other plugins. The core gains only one additional rule (see “Dependency lost after gating”).
- **The syntax is Rust-specific:** Cordis `inject` is a string list and `ctx.xxx` obtains types through module augmentation; these are also unlinked. rutis aligns with Cordis behavior, while its interface (`EventKey`, `TypeKey`) is idiomatic Rust. This layer is a similar language-specific difference.
- **Can be mixed:** the `Plugin` trait is unchanged. Typed and untyped plugins can provide services to and depend on each other. Cordis plugins mounted through rutis-interop are unaffected.

## Dependency description: type + runtime key

```rust
pub trait Deps: Sized + Send + 'static {
    type Keys: Clone + Send + Sync + 'static;            // Runtime portion
    fn injects(keys: &Self::Keys, out: &mut Vec<TypeKey>);
    fn resolve(keys: &Self::Keys, ctx: &Ctx) -> Result<Self, CordisError>;
}
```

Types determine “what is needed and what is received”; `Keys` carries information known only at mount time (name, instance). Both gating declarations and reads are generated from the same `(Deps, Keys)` pair, so they cannot diverge.

| Form | Keys | Gating | Passed to plugin |
|---|---|---|---|
| `Arc<T>` | `()` | `TypeKey::of::<T>()` | Service |
| `Option<Arc<T>>` | `()` | No | Present if available at load time |
| `Keyed<T>` | `DepKey<T>` | That key | Service |
| `Option<Keyed<T>>` | `DepKey<T>` | No | Present if available at load time |
| `Gate<T>` | `()` | `TypeKey::of::<T>()` | Nothing (gating only) |
| `KeyedGate<T>` | `DepKey<T>` | That key | Nothing (gating only) |
| `()` and tuples up to arity 8 | Tuple of member keys | All members | Each member |

- **`T` may be unsized** (`Arc<dyn LanguageModel>`), read through `require_as` / `get_as`.
- **`DepKey<T>` is a typed key:** constructors `of` / `named` / `dynamic` / `.instance(id)` and `From<Key<T>>` all state `T`, so the key cannot refer to a service of another type. Its default is `of()`.
- **Gating-only dependencies are part of the description** (`Gate` / `KeyedGate`); the first version's `gates()` method is removed so plugins and factories no longer keep separate declarations.
- **Optional dependencies are load-time snapshots:** they are not gated, and appearing/disappearing does not trigger reload, matching current “do not declare; call get in apply” behavior. Diagnostics record this as an undeclared access.

## Mounting

```rust
ctx.plugin(Typed::new(plugin));                     // Keys: Default (determined by types)
ctx.plugin(Typed::with_keys(plugin, keys));         // Choose names/instances at mount time

ctx.plugin_with(TypedFactory::new(factory), config);
ctx.plugin_with(TypedFactory::with_keys(factory, keys), config);
```

`TypedPluginFactory<C>::build` returns a typed plugin. `TypedFactory::injects()` is generated from `Plugin::Deps` and the keys. Each constructed plugin reads using that same key set, so a hot configuration update does not change the declaration, matching the static declaration rule of `PluginFactory`.

## Coverage of #50 design requirements

| Requirement | Mechanism | Test |
|---|---|---|
| Qualifier | `Keyed<T>` + `DepKey::named` / `dynamic` / `Key<T>` | `named_keys_are_chosen_when_mounting` |
| Multiple instances of a type | `DepKey::of().instance(id)`, passed at mount | `instance_keys_resolve_inside_their_instance_only` |
| isolate | Resolve reads by scope, same as untyped plugins | `isolated_scopes_pass_their_own_service` |
| Optional dependency | `Option<Arc<T>>` / `Option<Keyed<T>>` | `an_optional_dependency_is_passed_when_present` |
| Check gating | Provider registers; consumer gating includes it automatically | `a_failing_check_keeps_the_plugin_pending` |
| Factory | `TypedFactory` | `a_typed_factory_declares_from_the_same_description` |
| Trait object | `T: ?Sized` | `trait_object_services_are_dependencies` |

## Dependency lost after gating

If a required dependency becomes unavailable after gating but before `apply` reads it, the plugin returns to `Pending`, not `Failed`.

- A typed read that encounters `Unavailable` returns `CordisError::InjectUnsatisfied`.
- **Core rule:** when `apply` returns `InjectUnsatisfied`, the core checks this generation's access log (cleared at the start of each load). If the log contains a failed strict read (`require` / `require_as`) of a declared dependency due to `Unavailable`, this generation is evicted: cleanup drains in normal LIFO order, cleanup errors go to ErrorSink, then it returns to `Pending`. If the dependency has already returned, a recheck is queued immediately and the plugin reloads.
- **Why use a failed read as evidence:** the first two revisions checked “is it absent now?” and “is this still the binding that passed gating?”; both had holes:
  - The dependency may return before the error is handled.
  - The same provider may be withdrawn and re-provided within one generation.
  - A late snapshot may miss a withdrawal between gating and the snapshot.
  - A `check` on the same binding may change from pass to reject.

  A failed strict read is a fact recorded by the core itself; it proves the dependency became unavailable after gating, independent of timing or snapshots. Causes include removal, inactive provider, or rejected `check`.
- **Avoid loops:** without such a read in this generation, do not accept this reason; treat it as an ordinary failure and enter `Failed`.
- This generation's `InjectUnsatisfied` is not sent to ErrorSink (silent like eviction). A `restart()` in this window returns `Ok`; the fiber returns to `Pending` or reloads.
- **Untyped plugins:** calling `require` / `require_as` and returning this error has the same effect; `get` does not count as evidence. `rutis-agent` driver and TUI use `get_as`, so they remain ordinary failures as before.
- **Difference from Cordis:** Cordis treats an `apply` exception in this window as `FAILED` (and retries when the dependency returns). rutis relaxes only this explicit error variant with proof of a failed read; other errors remain sticky `Failed`.

## Claim boundary

`apply` still receives the full `Ctx`. The valid claim is “dependencies obtained through `Deps` are aligned at compile time,” not “all dependency mistakes fail to compile.”

## Settled decisions

1. **Wrapping:** use explicit `Typed::new` / `Typed::with_keys`; do not add methods to `Ctx`. Do not use blanket `impl<P: TypedPlugin> Plugin for P`: `injects()` returns a borrowed slice that must be stored in the instance, and runtime keys also need storage.
2. **Dependency lost after gating:** return to `Pending` (above).

## Open questions

- Should diagnostics label optional dependencies separately? They are currently recorded as undeclared access.
- Is a generic `Option<D>` implementation (such as an optional tuple) needed? Currently only individual optional services are supported.
