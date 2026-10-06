# Research: Rust async / Tokio Best Practices (2026-08-17)

> Researcher: gpt-5.6-terra. Sources: Tokio, futures, and official Rust documentation. For: `design-rust-port.md` v4.
> Each topic includes a conclusion, source, and fit with the design decision.

## 1. Async traits compatible with `dyn`

- Native AFIT cannot be used directly behind `dyn` (`async fn` has an opaque return type under the Rust Reference's dyn-compatibility rules).
- Recommended: explicitly define `type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>`; **this exactly matches `futures::future::BoxFuture`**.
- `async-trait` (0.1.89, maintained) expands to this shape and is suitable when author ergonomics come first; dynosaur is a specialized solution, not the ecosystem default.
- Sources: Rust Reference dyn compatibility; `docs.rs/async-trait`; `docs.rs/futures` `BoxFuture`.
- **Fit:** hand-written `BoxFuture` as the core ABI (high confidence).

## 2. Do not hold locks across await

- Tokio's guidance: `std::sync::Mutex` is correct for short, in-memory, low-contention critical sections; **never hold its guard across `.await`**. Use `tokio::sync::Mutex` only for I/O resources that must remain locked across await. Under high contention, first consider redesign or message passing.
- The standard `RwLock` docs show a deadlock from holding a read lock, waiting for a write lock, then reading again. **Do not call potentially reentrant callbacks while holding a lock.**
- “Snapshot/clone under the lock, release it, then invoke” matches the official guidance to drop lock guards before await and is a robust framework pattern, provided snapshot values have valid semantics.
- Sources: Tokio shared-state tutorial; `docs.rs/tokio` `tokio::sync::Mutex`; standard `RwLock` docs.
- **Fit:** fully aligned; make this a hard rule.

## 3. Spawn from synchronous contexts

- `tokio::spawn` needs a runtime context or panics. `Handle::current()` also panics without a runtime; `Handle::try_current()` returns an error. A `Handle` can be cloned freely.
- **Library API best practice:** prefer injecting a `Handle`; if acquiring automatically, use `try_current()` and return a clear error; never create a runtime implicitly.
- Source: `docs.rs/tokio` `tokio::runtime::Handle`.
- **Fit:** revise v3's “capture current at construction” to “inject first, `try_current()` fallback.”

## 4. One-shot tasks with multiple observers and error identity

- A `JoinHandle` is an owned permission to join, not a multi-observer broadcast.
- `FutureExt::shared()` requires `Output: Clone` and clones results for observers. `Result<T, Arc<E>>` can preserve error identity, but success values must also be Clone and late subscription is unavailable.
- **`tokio::sync::watch` (MPMC, last value, independent receiver cursors) best matches “one completion, many waiters, late subscribers, cached terminal state, shared `Arc` error identity.”**
- `oneshot` is one sender / one receiver and needs custom fan-out, so it is a partial fit.
- Sharing identity through `Arc<Error>` is established practice (`anyhow::Error` is not Clone; identity can be checked with `Arc::ptr_eq`).
- Sources: Tokio `JoinHandle` / watch / oneshot; futures-util `FutureExt::shared`; anyhow docs.
- **Fit:** v4 switches to watch (high confidence).

## 5. Cooperative cancellation

- **Prefer `tokio_util::sync::CancellationToken`:** clones broadcast cancellation, `cancelled().await` is cancel-safe, and child tokens support hierarchy. Tokio's graceful shutdown tutorial recommends it.
- `AtomicBool` is suitable only for synchronous polling fast paths; it does not wake tasks waiting in await and requires custom memory ordering, wakeups, and hierarchy.
- Sources: Tokio graceful shutdown; `docs.rs/tokio-util` CancellationToken.
- **Fit:** v4 consistently uses `CancellationToken`.

## 6. `yield_now` semantics

- `tokio::task::yield_now()` puts the task at the back of the pending queue but **does not guarantee another task runs first; the current task may be polled again immediately, and polling order changes are not breaking changes**. A higher-level combinator may intercept it.
- It is **not equivalent to a JS `await Promise.resolve()` microtask barrier** and cannot guarantee visibility/order such as “other tasks have observed state.” Lock release, channel/watch versions, or acknowledgements provide commit visibility.
- It is reasonable as a fairness/responsiveness hint.
- Source: `docs.rs/tokio` `tokio::task::yield_now`.
- **Fit:** v4 treats it only as a hint, correcting v3's checkpoint semantics.

## 7. Library error design

- API guideline C-GOOD-ERR: errors implement `std::error::Error` and are usually `Send + Sync + 'static`.
- **Do not require Clone** (the guideline does not; errors often contain non-Clone sources/backtraces). Use `Arc<E>` for concurrent sharing.
- Derive with thiserror without leaking it into the public API. Aggregate errors in `Vec<E>` (or `Vec<PluginFailure>` with plugin IDs), or use a named `AggregateError { errors }` (noting `source()` is single-chain and custom accessors are needed). Consider error-stack only for richer context requirements.
- Sources: Rust API Guidelines C-GOOD-ERR; thiserror; error-stack.
- **Fit:** v4 `CordisError` enum + thiserror + shared `Arc` + `Vec` aggregation (high confidence).
