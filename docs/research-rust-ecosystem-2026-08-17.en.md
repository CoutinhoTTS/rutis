# Research: Precedents in the Rust Plugin / DI / Middleware Ecosystem (2026-08-17)

> Researcher: deepseek-v4-pro. Sources: official Bevy / tower / shaku / Tauri docs and community material. For: `design-rust-port.md` v4.

## 1. Plugin system precedents

- **Bevy `Plugin` trait** (`Plugin: Downcast + Any + Send + Sync`): `build(&mut App)` registers resources/systems/subplugins immediately; `ready` / `finish` / `cleanup` form a three-stage lifecycle (“wait until all ready → finish → cleanup”); `name()` is a deduplication identifier and `is_unique()` controls uniqueness. Plugins return no service handle; all state lives in App/World. This differs fundamentally from Cordis's “return an effect.”
- **Tauri v2:** chained callbacks such as `Builder::new(name).setup(...).on_event(...).on_drop(...)`; cleanup uses `on_drop`, with RAII `Drop` as a fallback; state is accessed through `app.manage(T)`.
- Key finding: **neither supports hot unload/reload while running**. Bevy cleanup is startup cleanup. Dependency-gated hot reload distinguishes the Cordis model, with no precedent to copy; rutis needs its own reverse-dependency index and unload-order protocol.
- Sources: `docs.rs/bevy` `trait.Plugin`; `v2.tauri.app/develop/plugins`.
- **Fit:** use Bevy's trait skeleton (name / Any / multiple registrations) and Tauri-style `on_drop` cleanup; build hot reload ourselves.

## 2. Middleware / continuation (`tower`)

- **tower `Service<Request>` (`poll_ready` + `call -> Self::Future`) and `Layer` are the de facto standard for async request paths** (used by hyper/axum/tonic). Tokio's official blog post “Inventing the Service trait” explains the evolution from `Fn -> Future` to a trait.
- But `poll_ready` backpressure targets request pipelines. **It does not fit lifecycle/event hooks**, and forcing it in would add `Pin<Box<dyn Future>>` boilerplate.
- Recommendation: keep the core lightweight with `call -> BoxFuture`; provide optional tower compatibility adapters.
- Sources: `docs.rs/tower` `trait.Service`; `tokio.rs/blog/2021-05-14-inventing-the-service-trait`.
- **Fit:** v4 D15—do not force tower into the core; adapters remain optional.

## 3. Type-keyed registries

- `HashMap<TypeId, Box<dyn Any>>` (or `Arc` with Send + Sync) is the recognized internal shape for Bevy Resources; “Types must be unique.” Tauri `state::<T>()` and shaku follow the same direction.
- Ecosystem view: **type keys are mainstream** (no name collisions, type-safe, one instance per type); **multiple instances use explicit keys** (shaku `Keyed` / `HasComponentMap`).
- Trap: standard `TypeId` is unstable across binaries/dylibs (Bevy StableTypeId issue); dynamic dispatch within one binary is unaffected.
- Sources: Bevy cheatbook resources; taintedcoders Building Bevy; `docs.rs/shaku`.
- **Fit:** v4 D2/D13—`TypeId` as primary key plus explicit keys for multiple instances.

## 4. Event bus shapes

- Ecosystem consensus distinguishes **hooks / lifecycle** (synchronous, immediate, may return values or veto) from **data streams / domain events** (decoupled, multiple consumers, cross-task). Use a callback registry for hooks, storing owned closures (`Box<dyn FnMut>` / `Arc<dyn Fn>`) and returning cancellation handles. Use typed queues for data streams, such as Bevy's double-buffered `Events<T>` plus EventReader cursors, or Tokio broadcast/watch.
- Callbacks are push-style immediate execution; channels are pull-style buffered delivery. **Do not unify them into one mechanism.**
- Sources: `users.rust-lang.org/t/58996`; Bevy cheatbook events; Tauri v2 listen/emit.
- **Fit:** v4 D3 uses a callback registry for hooks; optional M4 data streams use a separate queue.

## 5. Service locator debate

- **Typed locators are core APIs in mainstream Rust frameworks, not an anti-pattern:** Bevy `Res<T>` / `ResMut<T>` (the docs call system-parameter injection DI) and Tauri `Manager::state::<T>()`. Criticism targets untyped string-keyed locators.
- Boundary: locators are legitimate in framework/infrastructure layers; business objects should use explicit constructor injection. Plugins declare dependencies during assembly (required for dependency gating).
- Sources: Bevy cheatbook resources; Tauri state management; jimmybogard.com service-locator-is-not-an-anti-pattern.
- **Fit:** v4 D13—explicit `get::<T>() -> Option` plus dependency declarations at assembly time.

## 6. Dynamic payloads

- Strong default: **strongly typed generic or enum events** (Bevy says events are simple Rust structs or enums; use an enum for multiple event types, Bevy #1431).
- `Box<dyn Any>` is only for in-process heterogeneous erasure (consumer must know T first); `serde_json::Value` belongs at serialization boundaries (cross-process / frontend / storage), as a boundary DTO, not in the core bus.
- Layering: enumerable types → generics/enums; framework-internal erasure → Any + TypeId; cross-boundary representation → serde.
- Sources: Bevy cheatbook events; github.com/bevyengine/bevy/discussions/1431.
- **Fit:** v4 D2 removes the `DynamicValue` enum; use generic events + internal Any erasure + `Value` only at boundaries, closer to ecosystem practice than v3.

## Summary

| Decision | Ecosystem position | v4 |
|---|---|---|
| Plugin lifecycle | Bevy trait skeleton; no hot-reload precedent | Adopt skeleton; build and test hot reload ourselves |
| Middleware | tower is standard for request paths | Do not force it into the core; optional adapter |
| Registry | `TypeId` + Any is recognized; explicit keys for multiple instances | Same |
| Event bus | Hook callback tables and data-stream queues coexist | Hook callback tables; queue in M4 |
| Locator | Typed locators are legitimate (Bevy/Tauri precedents) | Same |
| Dynamic payloads | Generics first; Any internally; Value at boundaries | Same |
