//! One ordered, bidirectional connection for invocation, await and owned references.
//! Socket readers admit frames and pin references; they never execute plugin code.
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;

use rutis::BoxFuture;
use rutis_channel::{Channel, ChannelError, Closer, Sender};
use serde_json::Value as Json;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::{oneshot, watch, Notify};

pub use crate::protocol::Implementation;
use crate::protocol::{Frame, Kind, WireValue, VERSION};
use crate::{Error, Handshake};
use rutis_channel::PeerId;

pub type Reply = Result<Value, Error>;
type Callback = dyn Fn(Value) -> Reply + Send + Sync;

/// The current value slice supports JSON data, owned functions and async results.
/// `invoke` returns a Future reference; only `wait`/`wait_async` await it.
#[derive(Clone, Debug)]
pub enum Value {
    Undefined,
    Data(Json),
    List(Vec<Value>),
    /// A plain object whose fields contain references.
    Record(std::collections::BTreeMap<String, Value>),
    Reference(Reference),
    /// As an argument: an AbortSignal the callee receives, aborted when this
    /// call is cancelled (its future dropped).
    Signal,
}
impl Value {
    pub fn list(self) -> Result<Vec<Value>, Error> {
        match self {
            Self::List(values) => Ok(values),
            Self::Data(Json::Array(values)) => Ok(values.into_iter().map(Self::Data).collect()),
            _ => Err(Error::Value("expected argument array".into())),
        }
    }
    pub fn json(self) -> Result<Json, Error> {
        match self {
            Self::Undefined => Ok(Json::Null),
            Self::Data(value) => Ok(value),
            Self::List(values) => values
                .into_iter()
                .map(Self::json)
                .collect::<Result<Vec<_>, _>>()
                .map(Json::Array),
            Self::Record(fields) => fields
                .into_iter()
                .map(|(key, value)| value.json().map(|value| (key, value)))
                .collect::<Result<serde_json::Map<_, _>, _>>()
                .map(Json::Object),
            Self::Reference(_) => Err(Error::Value("expected data, received a reference".into())),
            Self::Signal => Err(Error::Value("expected data, received a signal".into())),
        }
    }
    pub fn reference(self) -> Result<Reference, Error> {
        match self {
            Self::Reference(value) => Ok(value),
            _ => Err(Error::Value("expected a reference".into())),
        }
    }
    pub fn future(future: impl Future<Output = Reply> + Send + 'static) -> Self {
        Self::future_inner(future, true)
    }
    pub(crate) fn control_future(future: impl Future<Output = Reply> + Send + 'static) -> Self {
        Self::future_inner(future, false)
    }
    fn future_inner(future: impl Future<Output = Reply> + Send + 'static, business: bool) -> Self {
        Self::future_on(future, business, Executor::current())
    }
    fn future_on(
        future: impl Future<Output = Reply> + Send + 'static,
        business: bool,
        executor: Executor,
    ) -> Self {
        Self::Reference(Reference(ReferenceInner::Local(Arc::new(Object {
            executor,
            business,
            origin: current_path(),
            body: Body::Future(AsyncResult {
                future: Mutex::new(FutureState {
                    pending: Some(Box::pin(future)),
                    task: None,
                }),
                result: watch::channel(None).0,
            }),
        }))))
    }
    /// The closure runs on its captured runtime, or on the originating sync
    /// caller's thread when that caller is pumping this invocation chain.
    pub fn callback(callback: impl Fn(Value) -> Reply + Send + Sync + 'static) -> Self {
        Self::Reference(Reference(ReferenceInner::Local(Arc::new(Object {
            executor: Executor::current(),
            business: true,
            origin: current_path(),
            body: Body::Function(Arc::new(callback)),
        }))))
    }
}
impl From<Json> for Value {
    fn from(value: Json) -> Self {
        Self::Data(value)
    }
}

#[derive(Clone)]
pub struct Reference(ReferenceInner);
impl std::fmt::Debug for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reference")
            .field("future", &self.is_future())
            .finish_non_exhaustive()
    }
}
/// Identity: the same exported object, or the same import of a remote one
/// (repeated grants of one remote object share their import).
impl PartialEq for Reference {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (ReferenceInner::Local(a), ReferenceInner::Local(b)) => Arc::ptr_eq(a, b),
            (ReferenceInner::Remote(a), ReferenceInner::Remote(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
#[derive(Clone)]
enum ReferenceInner {
    Local(Arc<Object>),
    Remote(Arc<Import>),
}
impl Reference {
    pub fn is_future(&self) -> bool {
        match &self.0 {
            ReferenceInner::Local(object) => object.kind() == Kind::Future,
            ReferenceInner::Remote(import) => import.kind == Kind::Future,
        }
    }
    pub fn is_function(&self) -> bool {
        match &self.0 {
            ReferenceInner::Local(object) => object.kind() == Kind::Function,
            ReferenceInner::Remote(import) => import.kind == Kind::Function,
        }
    }
    pub fn is_object(&self) -> bool {
        matches!(&self.0, ReferenceInner::Remote(import) if import.kind == Kind::Object)
    }
    pub fn call(&self, args: Value) -> Reply {
        match &self.0 {
            ReferenceInner::Local(object) => object.call(args),
            ReferenceInner::Remote(import) => import
                .connection()?
                .request_sync(Operation::Call(import.id, None, args)),
        }
    }
    pub async fn call_async(&self, args: Value) -> Reply {
        match &self.0 {
            ReferenceInner::Local(object) => object.call(args),
            ReferenceInner::Remote(import) => {
                import
                    .connection()?
                    .request_async(Operation::Call(import.id, None, args))
                    .await
            }
        }
    }
    fn remote_object(&self) -> Result<&Arc<Import>, Error> {
        match &self.0 {
            ReferenceInner::Remote(import) if import.kind == Kind::Object => Ok(import),
            _ => Err(Error::Value("reference is not a remote object".into())),
        }
    }
    /// Call a method of a remote object and wait for it to return.
    pub fn call_method(&self, method: &str, args: Value) -> Reply {
        let import = self.remote_object()?;
        import
            .connection()?
            .request_sync(Operation::Call(import.id, Some(method.into()), args))
    }
    /// Call a method of a remote object without blocking the caller.
    pub async fn call_method_async(&self, method: &str, args: Value) -> Reply {
        let import = self.remote_object()?;
        import
            .connection()?
            .request_async(Operation::Call(import.id, Some(method.into()), args))
            .await
    }
    /// Read a property of a remote object; every read is live.
    pub fn get(&self, property: &str) -> Reply {
        let import = self.remote_object()?;
        import
            .connection()?
            .request_sync(Operation::Get(import.id, property.into()))
    }
    pub async fn wait_async(&self) -> Reply {
        match &self.0 {
            ReferenceInner::Local(object) => object.wait(false, current_path()).await,
            ReferenceInner::Remote(import) => {
                import
                    .connection()?
                    .request_async(Operation::Await(import.id, import.origin.clone()))
                    .await
            }
        }
    }
    pub fn wait(&self) -> Reply {
        match &self.0 {
            ReferenceInner::Remote(import) => import
                .connection()?
                .request_sync(Operation::Await(import.id, import.origin.clone())),
            ReferenceInner::Local(_) => {
                Err(Error::Value("local future requires wait_async".into()))
            }
        }
    }
}

#[derive(Clone)]
struct Executor {
    handle: Handle,
    thread: ThreadId,
}
impl Executor {
    fn current() -> Self {
        Self {
            handle: Handle::current(),
            thread: std::thread::current().id(),
        }
    }
    fn blocked_here(&self) -> bool {
        self.handle.runtime_flavor() == RuntimeFlavor::CurrentThread
            && self.thread == std::thread::current().id()
    }
}
/// Lazily started once per connection. Closing drops the stop sender; the
/// runtime cancels its tasks without blocking the caller or joining itself.
struct Background {
    executor: Executor,
    _stop: oneshot::Sender<()>,
}
impl Background {
    fn start() -> Result<Self, Error> {
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        std::thread::Builder::new()
            .name("rutis-interop-async".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => {
                        let _ = ready.send(Ok(Executor {
                            handle: runtime.handle().clone(),
                            thread: std::thread::current().id(),
                        }));
                        runtime.block_on(async {
                            let _ = stopped.await;
                        });
                        runtime.shutdown_background();
                    }
                    Err(error) => {
                        let _ = ready.send(Err(transport(error)));
                    }
                }
            })
            .map_err(transport)?;
        Ok(Self {
            executor: started.recv().map_err(transport)??,
            _stop: stop,
        })
    }
}
struct Object {
    executor: Executor,
    body: Body,
    business: bool,
    origin: Vec<String>,
}
enum Body {
    Function(Arc<Callback>),
    Future(AsyncResult),
    /// Stands in for a reference imported on another session: calls, reads
    /// and awaits are forwarded to it. Holding the import keeps the original
    /// alive; dropping the relay releases it on its own session.
    Relay(Arc<Import>),
}
struct AsyncResult {
    future: Mutex<FutureState>,
    result: watch::Sender<Option<Reply>>,
}
struct FutureState {
    pending: Option<BoxFuture<'static, Reply>>,
    task: Option<tokio::task::AbortHandle>,
}
impl Drop for AsyncResult {
    fn drop(&mut self) {
        if let Some(task) = self.future.get_mut().unwrap().task.take() {
            task.abort();
        }
    }
}
fn poll_future(future: &mut BoxFuture<'static, Reply>, cx: &mut Context<'_>) -> Poll<Reply> {
    catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx)))
        .unwrap_or_else(|panic| Poll::Ready(Err(panic_error(panic))))
}
impl Object {
    fn kind(&self) -> Kind {
        match &self.body {
            Body::Function(_) => Kind::Function,
            Body::Future(_) => Kind::Future,
            Body::Relay(import) => import.kind,
        }
    }
    fn call(&self, args: Value) -> Reply {
        match &self.body {
            Body::Function(callback) => protected(|| callback(args)),
            _ => Err(Error::Value("reference is not callable".into())),
        }
    }
    fn ready_on_blocked_executor(&self) -> Reply {
        let Body::Future(state) = &self.body else {
            return Err(Error::Value("reference is not awaitable".into()));
        };
        if let Some(result) = state.result.borrow().clone() {
            return result;
        }
        let pending = state.future.lock().unwrap().pending.take();
        if let Some(mut future) = pending {
            let _entered = self.executor.handle.enter();
            let waker = Waker::noop();
            match poll_future(&mut future, &mut Context::from_waker(waker)) {
                Poll::Ready(result) => {
                    state.result.send_replace(Some(result.clone()));
                    return result;
                }
                Poll::Pending => {
                    let result = state.result.clone();
                    let task =
                        self.executor
                            .handle
                            .spawn(ASYNC_PATH.scope(current_path(), async move {
                                let reply =
                                    std::future::poll_fn(|cx| poll_future(&mut future, cx)).await;
                                result.send_replace(Some(reply));
                            }));
                    state.future.lock().unwrap().task = Some(task.abort_handle());
                }
            }
        }
        Err(Error::SyncWaitCycle(
            format!("await needs the Rust current_thread executor occupied by its parent synchronous call; origin: {:?}", self.origin),
        ))
    }
    async fn wait(self: &Arc<Self>, _sync: bool, path: Vec<String>) -> Reply {
        let Body::Future(state) = &self.body else {
            return Err(Error::Value("reference is not awaitable".into()));
        };
        {
            let mut pending = state.future.lock().unwrap();
            if let Some(mut future) = pending.pending.take() {
                let result = state.result.clone();
                let task = self
                    .executor
                    .handle
                    .spawn(ASYNC_PATH.scope(path, async move {
                        let reply = std::future::poll_fn(|cx| poll_future(&mut future, cx)).await;
                        result.send_replace(Some(reply));
                    }));
                pending.task = Some(task.abort_handle());
            }
        }
        let mut result = state.result.subscribe();
        loop {
            if let Some(result) = result.borrow().clone() {
                return result;
            }
            result
                .changed()
                .await
                .map_err(|_| Error::Transport("future executor stopped".into()))?;
        }
    }
}

/// Wait for a returned Promise / Future reference; other values pass through.
pub async fn settle(value: Value) -> Reply {
    match value {
        Value::Reference(reference) if reference.is_future() => reference.wait_async().await,
        value => Ok(value),
    }
}

fn protected(call: impl FnOnce() -> Reply) -> Reply {
    catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|panic| Err(panic_error(panic)))
}
fn panic_error(panic: Box<dyn std::any::Any + Send>) -> Error {
    match panic.downcast::<Error>() {
        Ok(error) => *error,
        Err(panic) => crate::native_error(
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "native callback panicked".into()),
        ),
    }
}
tokio::task_local! { static ASYNC_PATH: Vec<String>; }
thread_local! { static SYNC_PATH: RefCell<Option<Vec<String>>> = const { RefCell::new(None) }; }
thread_local! { static CALLER: RefCell<Option<Connection>> = const { RefCell::new(None) }; }
/// The session whose call (an `invoke`, or a call of a function this side
/// exported) is being dispatched on this thread, if any. Code that calls
/// another session on its behalf passes it to [`Connection::forward`].
pub fn caller() -> Option<Connection> {
    CALLER.with(|caller| caller.borrow().clone())
}
thread_local! { static PUMPING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
/// Marks a thread that runs an invocation pumped by its own synchronous
/// wait (`request_sync`), so code that relies on running there can check it.
struct Pumping;
impl Pumping {
    fn enter() -> Self {
        PUMPING.with(|depth| depth.set(depth.get() + 1));
        Self
    }
    fn active() -> bool {
        PUMPING.with(|depth| depth.get() > 0)
    }
}
impl Drop for Pumping {
    fn drop(&mut self) {
        PUMPING.with(|depth| depth.set(depth.get() - 1));
    }
}
fn current_path() -> Vec<String> {
    SYNC_PATH
        .with(|p| p.borrow().clone())
        .or_else(|| ASYNC_PATH.try_with(Clone::clone).ok())
        .unwrap_or_default()
}
struct PathGuard(Option<Vec<String>>);
impl PathGuard {
    fn enter(path: Vec<String>) -> Self {
        Self(SYNC_PATH.with(|p| p.replace(Some(path))))
    }
}
impl Drop for PathGuard {
    fn drop(&mut self) {
        SYNC_PATH.with(|p| p.replace(self.0.take()));
    }
}
/// Poll `future` with `path` as the current invocation chain. The path is
/// entered on every poll, so it wins over whatever chain the polling thread
/// or task carries.
async fn with_path<F: Future>(path: Vec<String>, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let _path = PathGuard::enter(path.clone());
        future.as_mut().poll(cx)
    })
    .await
}

/// Rewrite an invocation chain from session `from` for a call sent to
/// session `to` (both are [`Connection::tag`]s). Call ids are unique only
/// within a session, so entries of other sessions carry their session's tag
/// (`"s1/node:3"`): untagged entries belong to `from` and get its tag;
/// entries tagged `to` return to their original ids, so `to` recognises the
/// calls it is synchronously waiting for; entries of third sessions pass
/// through. A tagged entry never equals a native `node:`/`rust:` id, so no
/// session mistakes it for one of its own calls.
pub fn rebase(path: &[String], from: &str, to: &str) -> Vec<String> {
    let home = format!("{to}/");
    path.iter()
        .map(|entry| match entry.strip_prefix(&home) {
            Some(native) => native.to_owned(),
            None if entry.contains('/') => entry.clone(),
            None => format!("{from}/{entry}"),
        })
        .collect()
}
static NEXT_TAG: AtomicU64 = AtomicU64::new(1);

struct Import {
    peer: Weak<SessionState>,
    id: u64,
    kind: Kind,
    grants: Mutex<u64>,
    origin: Vec<String>,
}
impl Import {
    fn connection(&self) -> Result<Connection, Error> {
        self.peer
            .upgrade()
            .map(Connection)
            .ok_or_else(|| Error::Transport("session closed".into()))
    }
}
impl Drop for Import {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.upgrade() {
            {
                let mut imports = peer.imports.lock().unwrap();
                if imports
                    .get(&self.id)
                    .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
                {
                    imports.remove(&self.id);
                }
            }
            let _ = Connection(peer).write(Frame::Release {
                reference: self.id,
                count: *self.grants.get_mut().unwrap(),
            });
        }
    }
}
struct Export {
    object: Arc<Object>,
    grants: u64,
}
#[derive(Default)]
struct Exports {
    next: u64,
    entries: HashMap<u64, Export>,
    identities: HashMap<usize, u64>,
    /// Relays by the import they forward to, so forwarding one import twice
    /// grants the same reference again.
    relays: HashMap<usize, u64>,
}
impl Exports {
    fn remove(&mut self, id: u64) -> Option<Export> {
        let entry = self.entries.remove(&id)?;
        self.identities
            .remove(&(Arc::as_ptr(&entry.object) as usize));
        if let Body::Relay(import) = &entry.object.body {
            self.relays.remove(&(Arc::as_ptr(import) as usize));
        }
        Some(entry)
    }
}
enum Waiting {
    Sync {
        sender: mpsc::Sender<Message>,
        origins: Vec<String>,
    },
    Async(oneshot::Sender<Reply>),
}
enum Message {
    Reply(Reply),
    Invoke(Incoming),
}
enum Operation {
    Invoke(String, String, Value),
    Call(u64, Option<String>, Value),
    Get(u64, String),
    Await(u64, Vec<String>),
}
enum Accepted {
    Invoke(String, String, Value),
    Call(Arc<Object>, Value),
    Await(Arc<Object>),
    /// Method calls and property reads reach relays of remote objects only.
    Method(Arc<Object>, String, Value),
    Get(Arc<Object>, String),
}
struct Awaiting {
    object: Arc<Object>,
    path: Vec<String>,
    _cancel: oneshot::Sender<()>,
}
struct Incoming {
    id: String,
    path: Vec<String>,
    call: Accepted,
    _flight: Option<Flight>,
}
#[derive(Default)]
struct Calls {
    next: u64,
    waiting: HashMap<String, Waiting>,
    incoming: HashMap<String, Incoming>,
    awaiting: HashMap<String, Awaiting>,
    /// Calls this side cancelled; their late replies are discarded.
    cancelled: std::collections::HashSet<String>,
    /// Late replies discarded after a cancellation.
    orphans: u64,
    received: u64,
    closed: Option<Error>,
}

/// Cancels an async call whose future is dropped before it completes.
struct CancelOnDrop {
    peer: Weak<SessionState>,
    id: String,
    done: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Some(peer) = self.peer.upgrade() {
            Connection(peer).cancel(&self.id);
        }
    }
}
struct Activity {
    count: Mutex<usize>,
    changed: Notify,
}
struct Flight(Arc<Activity>);
impl Drop for Flight {
    fn drop(&mut self) {
        *self.0.count.lock().unwrap() -= 1;
        self.0.changed.notify_waiters();
    }
}

pub trait Dispatch: Send + Sync + 'static {
    fn invoke(&self, peer: &Connection, target: &str, method: &str, args: Value) -> Reply;
}
/// How a session identifies itself and its calls.
#[derive(Clone, Debug, Default)]
pub enum Format {
    /// Protocol 2, as local runtimes speak it: this side calls as `rust:`,
    /// the far end as `node:`, and the handshake carries only the version.
    #[default]
    Compat,
    /// The endpoint format ([`crate::ENDPOINT_PROTOCOL`]): each side names
    /// itself in the handshake and calls as `<endpoint id>:<n>`.
    Endpoint(Endpoint),
}

/// This side of an endpoint-format session.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub local: PeerId,
    /// The far end this side expects; the session fails if the handshake
    /// names another.
    pub expected: Option<PeerId>,
    pub implementation: Implementation,
    /// What this side supports: `objects`, `signals`, `reentrant-sync`,
    /// `forwarding`. Capabilities grant no permission.
    pub capabilities: Vec<String>,
}

impl Endpoint {
    /// This implementation (rutis-interop), as `local`, with what it
    /// supports.
    pub fn rust(local: PeerId) -> Self {
        Self {
            local,
            expected: None,
            implementation: Implementation {
                name: "rutis-interop".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            capabilities: ["objects", "signals", "reentrant-sync", "forwarding"]
                .map(String::from)
                .to_vec(),
        }
    }

    pub fn expect(mut self, peer: PeerId) -> Self {
        self.expected = Some(peer);
        self
    }
}

/// What the far end said of itself in an endpoint-format handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Greeting {
    pub endpoint: PeerId,
    pub implementation: Option<Implementation>,
    pub capabilities: Vec<String>,
}

/// One session's state. The session is reached through [`Connection`];
/// the channel underneath is any [`Channel`].
struct SessionState {
    /// Process-unique; marks this session's call ids in chains forwarded
    /// to other sessions (see [`rebase`]).
    tag: String,
    format: Format,
    /// The far end as the channel's connector verified it, if it did.
    verified: Option<PeerId>,
    /// This side's call ids: `rust:` or `<endpoint>:`.
    local_prefix: String,
    /// The far end's: `node:`, or `<endpoint>:` once it greeted.
    remote_prefix: std::sync::OnceLock<String>,
    greeting: std::sync::OnceLock<Greeting>,
    writer: Mutex<Box<dyn Sender>>,
    closer: Arc<dyn Closer>,
    calls: Mutex<Calls>,
    exports: Mutex<Exports>,
    imports: Mutex<HashMap<u64, Weak<Import>>>,
    executor: Handle,
    background: Mutex<Option<Background>>,
    dispatch: Arc<dyn Dispatch>,
    ready: watch::Sender<Option<Result<(), Error>>>,
    ended: watch::Sender<bool>,
    activity: Arc<Activity>,
}
impl Drop for SessionState {
    fn drop(&mut self) {
        self.closer.close("session dropped");
    }
}

#[derive(Clone)]
pub struct Connection(Arc<SessionState>);
impl Connection {
    /// Start a session on `channel`: sends `hello` and reads frames on a
    /// thread of its own. When the channel ends, the session closes with
    /// `peer disconnected` (a normal end) or the channel's reason.
    pub fn open(channel: Channel, dispatch: Arc<dyn Dispatch>) -> Result<Self, Error> {
        Self::open_with(channel, dispatch, Format::Compat)
    }

    /// [`Connection::open`] in `format`. In the endpoint format the session
    /// is ready only once the far end greeted with the endpoint the channel
    /// verified (if it verified one) and this side expects (if it expects
    /// one); otherwise [`Connection::ready`] fails with [`Error::Handshake`].
    pub fn open_with(
        channel: Channel,
        dispatch: Arc<dyn Dispatch>,
        format: Format,
    ) -> Result<Self, Error> {
        let Channel {
            sender,
            mut receiver,
            closer,
            info,
        } = channel;
        let label = info.label;
        let (local_prefix, remote_prefix, hello) = match &format {
            Format::Compat => (
                "rust:".to_owned(),
                std::sync::OnceLock::from("node:".to_owned()),
                Frame::Hello {
                    version: VERSION,
                    endpoint: None,
                    implementation: None,
                    capabilities: None,
                },
            ),
            Format::Endpoint(endpoint) => (
                format!("{}:", endpoint.local),
                std::sync::OnceLock::new(),
                Frame::Hello {
                    version: crate::ENDPOINT_PROTOCOL,
                    endpoint: Some(endpoint.local.to_string()),
                    implementation: Some(endpoint.implementation.clone()),
                    capabilities: Some(endpoint.capabilities.clone()),
                },
            ),
        };
        let peer = Self(Arc::new(SessionState {
            tag: format!("s{}", NEXT_TAG.fetch_add(1, Ordering::Relaxed)),
            format,
            verified: info.peer,
            local_prefix,
            remote_prefix,
            greeting: std::sync::OnceLock::new(),
            closer,
            writer: Mutex::new(sender),
            calls: Mutex::new(Calls::default()),
            exports: Mutex::new(Exports::default()),
            imports: Mutex::new(HashMap::new()),
            executor: Handle::current(),
            background: Mutex::new(None),
            dispatch,
            ready: watch::channel(None).0,
            ended: watch::channel(false).0,
            activity: Arc::new(Activity {
                count: Mutex::new(0),
                changed: Notify::new(),
            }),
        }));
        let weak = Arc::downgrade(&peer.0);
        std::thread::Builder::new()
            .name("rutis-interop-reader".into())
            .spawn(move || {
                let result: Result<(), Error> = (|| loop {
                    let message = match receiver.recv() {
                        Ok(Some(message)) => message,
                        Ok(None) => return Err(Error::Transport("peer disconnected".into())),
                        Err(error) => return Err(ended(&label, error)),
                    };
                    let Some(peer) = weak.upgrade().map(Self) else {
                        return Ok(());
                    };
                    peer.receive(serde_json::from_slice(&message).map_err(transport)?)?;
                })();
                if let (Err(error), Some(peer)) = (result, weak.upgrade()) {
                    Self(peer).close(error);
                }
            })
            .map_err(transport)?;
        peer.write(hello)?;
        Ok(peer)
    }

    /// What the far end said of itself (endpoint format), once it greeted.
    pub fn greeting(&self) -> Option<&Greeting> {
        self.0.greeting.get()
    }

    /// Whether the far end declared `capability` (endpoint format). A
    /// compat session declares none.
    pub fn supports(&self, capability: &str) -> bool {
        self.greeting()
            .is_some_and(|greeting| greeting.capabilities.iter().any(|c| c == capability))
    }
    pub async fn ready(&self) -> Result<(), Error> {
        let mut ready = self.0.ready.subscribe();
        loop {
            if let Some(result) = ready.borrow().clone() {
                return result;
            }
            ready.changed().await.map_err(transport)?;
        }
    }
    pub async fn closed(&self) {
        let mut ended = self.0.ended.subscribe();
        while !*ended.borrow() {
            if ended.changed().await.is_err() {
                break;
            }
        }
    }
    pub fn close(&self, error: Error) {
        // Interrupt a blocked write before taking the writer lock. Serialize
        // table teardown with encoding/sending so no export can be added after
        // teardown and no partial encoding rollback races the cleared table.
        self.0.closer.close(&error.to_string());
        let writer = self.0.writer.lock().unwrap();
        let (waiting, incoming, awaiting) = {
            let mut calls = self.0.calls.lock().unwrap();
            if calls.closed.is_some() {
                return;
            }
            calls.closed = Some(error.clone());
            (
                std::mem::take(&mut calls.waiting),
                std::mem::take(&mut calls.incoming),
                std::mem::take(&mut calls.awaiting),
            )
        };
        let exports = std::mem::take(&mut *self.0.exports.lock().unwrap());
        self.0.imports.lock().unwrap().clear();
        drop(writer);
        self.0.ready.send_if_modified(|ready| {
            if ready.is_none() {
                *ready = Some(Err(error.clone()));
                true
            } else {
                false
            }
        });
        self.0.ended.send_replace(true);
        for call in waiting.into_values() {
            finish(call, Err(error.clone()));
        }
        drop(exports);
        drop(incoming);
        drop(awaiting);
        self.0.background.lock().unwrap().take();
    }
    /// Create an owned async result on this connection's shared executor.
    ///
    /// Adapter code must establish that the factory and Future can progress
    /// independently of the caller's executor: `Send` alone is insufficient.
    /// Create timers/tasks inside the factory, not on the caller's runtime.
    /// Arbitrary captured runtime handles or lifecycle tasks are not migrated.
    pub fn independent_future<F, Fut>(&self, factory: F) -> Result<Value, Error>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Reply> + Send + 'static,
    {
        Ok(Value::future_on(
            async move { factory().await },
            true,
            self.background()?,
        ))
    }
    fn background(&self) -> Result<Executor, Error> {
        let mut background = self.0.background.lock().unwrap();
        if let Some(error) = &self.0.calls.lock().unwrap().closed {
            return Err(error.clone());
        }
        if background.is_none() {
            *background = Some(Background::start()?);
        }
        Ok(background.as_ref().unwrap().executor.clone())
    }
    /// This session's tag, unique in the process (`"s3"`).
    pub fn tag(&self) -> &str {
        &self.0.tag
    }
    /// Run `call`, which calls into this session on behalf of a call that
    /// arrived on `source`, with the current invocation chain rebased from
    /// `source` to this session. Without it the ids of the two sessions mix,
    /// and a reverse call can be routed to the wrong waiting thread.
    pub fn forward<R>(&self, source: &Connection, call: impl FnOnce() -> R) -> R {
        let _path = PathGuard::enter(rebase(&current_path(), source.tag(), self.tag()));
        call()
    }
    /// The async form of [`Connection::forward`]: the chain is taken when
    /// this is called and is current whenever `future` is polled.
    pub fn forward_async<F: Future>(
        &self,
        source: &Connection,
        future: F,
    ) -> impl Future<Output = F::Output> {
        with_path(rebase(&current_path(), source.tag(), self.tag()), future)
    }
    pub fn invoke(&self, target: &str, method: &str, args: Value) -> Reply {
        self.request_sync(Operation::Invoke(target.into(), method.into(), args))
    }
    pub async fn invoke_async(&self, target: &str, method: &str, args: Value) -> Reply {
        self.request_async(Operation::Invoke(target.into(), method.into(), args))
            .await
    }
    pub async fn drain(&self) {
        loop {
            let changed = self.0.activity.changed.notified();
            if *self.0.activity.count.lock().unwrap() == 0 {
                return;
            }
            changed.await;
        }
    }
    fn write(&self, frame: Frame) -> Result<(), Error> {
        let result = {
            let mut writer = self.0.writer.lock().unwrap();
            self.write_locked(&mut writer, frame)
        };
        if let Err(error) = &result {
            self.close(error.clone());
        }
        result
    }
    fn write_locked(&self, writer: &mut Box<dyn Sender>, frame: Frame) -> Result<(), Error> {
        if let Some(error) = &self.0.calls.lock().unwrap().closed {
            return Err(error.clone());
        }
        let bytes = serde_json::to_vec(&frame).map_err(transport)?;
        writer.send(&bytes).map_err(|error| ended("", error))
    }
    fn send(&self, call: Operation, mut waiting: Waiting) -> Result<String, Error> {
        let mut writer = self.0.writer.lock().unwrap();
        if !matches!(*self.0.ready.borrow(), Some(Ok(()))) {
            return Err(Error::Transport("protocol handshake incomplete".into()));
        }
        let id = {
            let mut calls = self.0.calls.lock().unwrap();
            if let Some(error) = &calls.closed {
                return Err(error.clone());
            }
            calls.next = calls
                .next
                .checked_add(1)
                .ok_or_else(|| transport("call identifiers exhausted"))?;
            format!("{}{}", self.0.local_prefix, calls.next)
        };
        let mut path = current_path();
        if let Operation::Await(_, origin) = &call {
            if let Waiting::Sync { origins, .. } = &mut waiting {
                *origins = origin.clone();
            }
            for ancestor in origin {
                if !path.contains(ancestor) {
                    path.push(ancestor.clone());
                }
            }
        }
        let frame = match &call {
            Operation::Invoke(target, method, args) => Frame::Invoke {
                id: id.clone(),
                path,
                target: target.clone(),
                method: method.clone(),
                args: self.encode(args)?,
            },
            Operation::Call(reference, method, args) => Frame::Call {
                id: id.clone(),
                path,
                reference: *reference,
                method: method.clone(),
                args: self.encode(args)?,
            },
            Operation::Get(reference, property) => Frame::Get {
                id: id.clone(),
                path,
                reference: *reference,
                property: property.clone(),
            },
            Operation::Await(reference, _) => Frame::Await {
                id: id.clone(),
                path,
                reference: *reference,
            },
        };
        let mut blocked = Vec::new();
        {
            let mut calls = self.0.calls.lock().unwrap();
            if let Waiting::Sync { sender, origins } = &waiting {
                for (id, awaiting) in &calls.awaiting {
                    // Forwarded relay calls run on the background executor
                    // and never wait on this one.
                    if matches!(awaiting.object.body, Body::Future(_))
                        && awaiting.object.executor.blocked_here()
                        && origins.iter().any(|origin| awaiting.path.contains(origin))
                    {
                        blocked.push((id.clone(), awaiting.object.clone()));
                    }
                }
                // Claim queued work atomically with registering this waiter.
                // A runtime task may already be scheduled, but has not begun.
                let mut related: Vec<_> = calls
                    .incoming
                    .iter()
                    .filter(|(_, incoming)| {
                        incoming.path.contains(&id)
                            || origins.iter().any(|origin| incoming.path.contains(origin))
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                let remote = self.remote_prefix();
                related.sort_by_key(|id| id.strip_prefix(remote).unwrap().parse::<u64>().unwrap());
                for id in related {
                    let incoming = calls.incoming.remove(&id).unwrap();
                    sender.send(Message::Invoke(incoming)).map_err(transport)?;
                }
            }
            calls.waiting.insert(id.clone(), waiting);
        }
        let result = self.write_locked(&mut writer, frame);
        drop(writer);
        if let Err(error) = result {
            self.close(error.clone());
            return Err(error);
        }
        for (id, object) in blocked {
            self.finish_await(id, object.ready_on_blocked_executor());
        }
        Ok(id)
    }
    fn request_sync(&self, call: Operation) -> Reply {
        let (sender, receiver) = mpsc::channel();
        self.send(
            call,
            Waiting::Sync {
                sender,
                origins: Vec::new(),
            },
        )?;
        loop {
            match receiver.recv().map_err(transport)? {
                Message::Reply(result) => return result,
                Message::Invoke(incoming) => {
                    let _pumping = Pumping::enter();
                    self.execute(incoming, true)
                }
            }
        }
    }
    async fn request_async(&self, call: Operation) -> Reply {
        let (sender, receiver) = oneshot::channel();
        let id = self.send(call, Waiting::Async(sender))?;
        let mut guard = CancelOnDrop {
            peer: Arc::downgrade(&self.0),
            id,
            done: false,
        };
        let result = receiver.await.map_err(transport);
        guard.done = true;
        result?
    }
    /// Give up on an outstanding call: the callee's AbortSignal (if any) is
    /// aborted and a late reply is discarded.
    fn cancel(&self, id: &str) {
        {
            let mut calls = self.0.calls.lock().unwrap();
            if calls.closed.is_some() {
                return;
            }
            // Still waiting: a late reply will be discarded. Already answered
            // but not consumed (e.g. a returned Promise the caller has not yet
            // awaited): the callee may still be running it, so cancel anyway.
            if calls.waiting.remove(id).is_some() {
                calls.cancelled.insert(id.to_owned());
            }
        }
        let _ = self.write(Frame::Cancel { id: id.to_owned() });
    }
    /// Late replies discarded after cancellations.
    pub fn orphans(&self) -> u64 {
        self.0.calls.lock().unwrap().orphans
    }
    fn encode(&self, value: &Value) -> Result<WireValue, Error> {
        let mut grants = Vec::new();
        let result = self.encode_inner(value, &mut grants);
        if result.is_err() {
            let mut exports = self.0.exports.lock().unwrap();
            for id in grants {
                let Some(entry) = exports.entries.get_mut(&id) else {
                    continue;
                };
                entry.grants -= 1;
                if entry.grants == 0 {
                    // The value being encoded still holds the import of a
                    // dropped relay, so no release is written under the lock.
                    exports.remove(id);
                }
            }
        }
        result
    }
    fn encode_inner(&self, value: &Value, grants: &mut Vec<u64>) -> Result<WireValue, Error> {
        Ok(match value {
            Value::Undefined => WireValue::Undefined,
            Value::Data(value) => WireValue::Data(value.clone()),
            Value::List(values) => WireValue::List(
                values
                    .iter()
                    .map(|value| self.encode_inner(value, grants))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Record(fields) => WireValue::Record(
                fields
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), self.encode_inner(value, grants)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            Value::Signal => WireValue::Signal,
            Value::Reference(Reference(ReferenceInner::Remote(import))) => {
                self.encode_import(import, grants)?
            }
            // Decoding unwraps relays, so Rust code should never hold one;
            // if it does, forward from the innermost import all the same.
            Value::Reference(Reference(ReferenceInner::Local(object))) => match &object.body {
                Body::Relay(import) => self.encode_import(import, grants)?,
                _ => self.encode_local(object, grants)?,
            },
        })
    }
    fn encode_local(
        &self,
        object: &Arc<Object>,
        grants: &mut Vec<u64>,
    ) -> Result<WireValue, Error> {
        let identity = Arc::as_ptr(object) as usize;
        let mut exports = self.0.exports.lock().unwrap();
        let id = match exports.identities.get(&identity) {
            Some(id) => *id,
            None => {
                let id = allocate(&mut exports, object.clone())?;
                exports.identities.insert(identity, id);
                id
            }
        };
        grant(&mut exports, id, grants)
    }
    fn decode(&self, value: WireValue) -> Reply {
        Ok(match value {
            WireValue::Undefined => Value::Undefined,
            WireValue::Data(value) => Value::Data(value),
            WireValue::List(values) => Value::List(
                values
                    .into_iter()
                    .map(|value| self.decode(value))
                    .collect::<Result<_, _>>()?,
            ),
            WireValue::Signal => return Err(transport("Rust receives no AbortSignal values")),
            WireValue::Record(fields) => Value::Record(
                fields
                    .into_iter()
                    .map(|(key, value)| Ok((key, self.decode(value)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            WireValue::Reference {
                id,
                home: true,
                kind,
                origin: _,
            } => {
                let object = self.export(id)?;
                if object.kind() != kind {
                    return Err(transport("reference kind mismatch"));
                }
                // A relay coming home is the reference it forwards to, so
                // Rust code compares it equal to the import it sent.
                match &object.body {
                    Body::Relay(import) => {
                        Value::Reference(Reference(ReferenceInner::Remote(import.clone())))
                    }
                    _ => Value::Reference(Reference(ReferenceInner::Local(object))),
                }
            }
            WireValue::Reference {
                id,
                home: false,
                kind,
                origin,
            } => {
                if id == 0 || id > 9_007_199_254_740_991 {
                    return Err(transport("invalid reference identity"));
                }
                let mut imports = self.0.imports.lock().unwrap();
                let import = match imports.get(&id).and_then(Weak::upgrade) {
                    Some(import) => {
                        if import.kind != kind || import.origin != origin {
                            return Err(transport("reference kind mismatch"));
                        }
                        let mut grants = import.grants.lock().unwrap();
                        *grants = grants
                            .checked_add(1)
                            .ok_or_else(|| transport("import grant overflow"))?;
                        drop(grants);
                        import
                    }
                    None => {
                        let import = Arc::new(Import {
                            peer: Arc::downgrade(&self.0),
                            id,
                            kind,
                            grants: Mutex::new(1),
                            origin,
                        });
                        imports.insert(id, Arc::downgrade(&import));
                        import
                    }
                };
                Value::Reference(Reference(ReferenceInner::Remote(import)))
            }
        })
    }
    /// The waiter for a reply, `None` for a late reply to a cancelled call.
    /// The far end's call id prefix; empty (matching nothing a valid far
    /// end sends) before it greeted.
    fn remote_prefix(&self) -> &str {
        self.0.remote_prefix.get().map_or("\0", String::as_str)
    }
    /// Check the far end's handshake against this session's format.
    fn greet(
        &self,
        version: u32,
        endpoint: Option<String>,
        implementation: Option<Implementation>,
        capabilities: Option<Vec<String>>,
    ) -> Result<(), Error> {
        let incompatible = |reason: String| Error::Handshake(Handshake::Incompatible(reason));
        let Format::Endpoint(local) = &self.0.format else {
            if version != VERSION || endpoint.is_some() {
                return Err(incompatible(format!(
                    "the far end speaks protocol {version}, this side {VERSION}"
                )));
            }
            return Ok(());
        };
        if version != crate::ENDPOINT_PROTOCOL {
            return Err(incompatible(format!(
                "the far end speaks protocol {version}, this side {}",
                crate::ENDPOINT_PROTOCOL
            )));
        }
        let endpoint = endpoint
            .ok_or_else(|| incompatible("the far end named no endpoint".into()))
            .and_then(|endpoint| {
                PeerId::new(endpoint).map_err(|error| incompatible(error.to_string()))
            })?;
        for (whose, expected) in [
            ("verified", &self.0.verified),
            ("expected", &local.expected),
        ] {
            if let Some(expected) = expected {
                if expected != &endpoint {
                    return Err(Error::Handshake(Handshake::IdentityMismatch(format!(
                        "the far end greeted as {endpoint}, but {expected} is the {whose} endpoint"
                    ))));
                }
            }
        }
        if endpoint == local.local {
            return Err(Error::Handshake(Handshake::IdentityMismatch(format!(
                "the far end greeted as this endpoint ({endpoint})"
            ))));
        }
        let _ = self.0.remote_prefix.set(format!("{endpoint}:"));
        let _ = self.0.greeting.set(Greeting {
            endpoint,
            implementation,
            capabilities: capabilities.unwrap_or_default(),
        });
        Ok(())
    }
    fn reply_target(&self, id: &str) -> Result<Option<Waiting>, Error> {
        let mut calls = self.0.calls.lock().unwrap();
        if let Some(waiting) = calls.waiting.remove(id) {
            return Ok(Some(waiting));
        }
        if calls.cancelled.remove(id) {
            calls.orphans += 1;
            return Ok(None);
        }
        Err(transport("response for unknown call"))
    }
    fn export(&self, id: u64) -> Result<Arc<Object>, Error> {
        self.0
            .exports
            .lock()
            .unwrap()
            .entries
            .get(&id)
            .map(|e| e.object.clone())
            .ok_or_else(|| transport("unknown or released reference"))
    }
    fn receive(&self, frame: Frame) -> Result<(), Error> {
        if let Some(error) = &self.0.calls.lock().unwrap().closed {
            return Err(error.clone());
        }
        if let Frame::Hello {
            version,
            endpoint,
            implementation,
            capabilities,
        } = frame
        {
            if self.0.ready.borrow().is_some() {
                return Err(transport("duplicate protocol handshake"));
            }
            self.greet(version, endpoint, implementation, capabilities)?;
            self.0.ready.send_replace(Some(Ok(())));
            return Ok(());
        }
        if !matches!(*self.0.ready.borrow(), Some(Ok(()))) {
            return Err(transport("request before protocol handshake"));
        }
        let (id, mut path, call) = match frame {
            Frame::Return { id, value } => {
                // Decode first: a discarded late reply still releases the
                // references it granted when its values drop.
                let value = self.decode(value)?;
                if let Some(waiting) = self.reply_target(&id)? {
                    finish(waiting, Ok(value));
                }
                return Ok(());
            }
            Frame::Throw { id, error } => {
                if let Some(waiting) = self.reply_target(&id)? {
                    finish(waiting, Err(error.into()));
                }
                return Ok(());
            }
            Frame::Cancel { id } => {
                // Stop awaiting on the caller's behalf; queued work is dropped.
                // A synchronous call already running completes normally.
                let (awaiting, incoming) = {
                    let mut calls = self.0.calls.lock().unwrap();
                    (calls.awaiting.remove(&id), calls.incoming.remove(&id))
                };
                drop((awaiting, incoming));
                return Ok(());
            }
            Frame::Release { reference, count } => {
                let removed = {
                    let mut exports = self.0.exports.lock().unwrap();
                    let entry = exports
                        .entries
                        .get_mut(&reference)
                        .ok_or_else(|| transport("release of unknown reference"))?;
                    if count == 0 || count > entry.grants {
                        return Err(transport("invalid reference release count"));
                    }
                    entry.grants -= count;
                    if entry.grants == 0 {
                        exports.remove(reference)
                    } else {
                        None
                    }
                };
                // Outside the lock: a dropped relay may release its import,
                // which writes to the other session.
                drop(removed);
                return Ok(());
            }
            Frame::Invoke {
                id,
                path,
                target,
                method,
                args,
            } => (
                id,
                path,
                Accepted::Invoke(target, method, self.decode(args)?),
            ),
            // Rust exports objects only as relays of other sessions' objects.
            Frame::Call {
                id,
                path,
                reference,
                method: Some(method),
                args,
            } => (
                id,
                path,
                Accepted::Method(self.export_object(reference)?, method, self.decode(args)?),
            ),
            Frame::Get {
                id,
                path,
                reference,
                property,
            } => (
                id,
                path,
                Accepted::Get(self.export_object(reference)?, property),
            ),
            Frame::Call {
                id,
                path,
                reference,
                method: None,
                args,
            } => (
                id,
                path,
                Accepted::Call(self.export(reference)?, self.decode(args)?),
            ),
            Frame::Await {
                id,
                path,
                reference,
            } => (id, path, Accepted::Await(self.export(reference)?)),
            Frame::Hello { .. } => unreachable!(),
        };
        if !id.starts_with(self.remote_prefix()) || path.contains(&id) {
            return Err(transport("invalid invocation identity/path"));
        }
        path.push(id.clone());
        let business = match &call {
            Accepted::Invoke(target, _, _) => !target.is_empty(),
            Accepted::Call(object, _)
            | Accepted::Await(object)
            | Accepted::Method(object, _, _)
            | Accepted::Get(object, _) => object.business,
        };
        let flight = business.then(|| {
            *self.0.activity.count.lock().unwrap() += 1;
            Flight(self.0.activity.clone())
        });
        let incoming = Incoming {
            id,
            path,
            call,
            _flight: flight,
        };
        let handle = match &incoming.call {
            Accepted::Call(object, _)
            | Accepted::Await(object)
            | Accepted::Method(object, _, _)
            | Accepted::Get(object, _) => object.executor.handle.clone(),
            Accepted::Invoke(..) => self.0.executor.clone(),
        };
        let delivery = {
            let mut calls = self.0.calls.lock().unwrap();
            if let Some(error) = &calls.closed {
                return Err(error.clone());
            }
            let sequence = incoming
                .id
                .strip_prefix(self.remote_prefix())
                .and_then(|n| n.parse::<u64>().ok())
                .filter(|n| *n > calls.received && *n <= 9_007_199_254_740_991)
                .ok_or_else(|| transport("invalid or repeated invocation identity"))?;
            calls.received = sequence;
            let waiter = incoming
                .path
                .iter()
                .rev()
                .find_map(|id| match calls.waiting.get(id) {
                    Some(Waiting::Sync { sender, .. }) => Some(sender.clone()),
                    _ => None,
                })
                .or_else(|| {
                    calls.waiting.values().find_map(|waiting| match waiting {
                        Waiting::Sync { sender, origins }
                            if origins.iter().any(|id| incoming.path.contains(id)) =>
                        {
                            Some(sender.clone())
                        }
                        _ => None,
                    })
                });
            if let Some(waiter) = waiter {
                Ok((waiter, incoming))
            } else {
                let id = incoming.id.clone();
                calls.incoming.insert(id.clone(), incoming);
                Err(id)
            }
        };
        if let Err(id) = delivery {
            let peer = self.clone();
            handle.spawn(async move {
                let incoming = peer.0.calls.lock().unwrap().incoming.remove(&id);
                if let Some(incoming) = incoming {
                    peer.execute(incoming, false);
                }
            });
        } else if let Ok((waiter, incoming)) = delivery {
            // A dropped receiver may release imported arguments. Do not run
            // their destructors (and protocol writes) under the calls lock.
            waiter.send(Message::Invoke(incoming)).map_err(transport)?;
        }
        Ok(())
    }
    fn execute(&self, incoming: Incoming, sync: bool) {
        if self.0.calls.lock().unwrap().closed.is_some() {
            return;
        }
        let Incoming {
            id,
            path,
            call,
            _flight,
        } = incoming;
        let _path = PathGuard::enter(path.clone());
        let call = match call {
            Accepted::Call(object, args) if matches!(object.body, Body::Relay(_)) => {
                return self.relay(id, path, object, Relayed::Call(None, args), sync, _flight);
            }
            Accepted::Await(object) if matches!(object.body, Body::Relay(_)) => {
                return self.relay(id, path, object, Relayed::Await, sync, _flight);
            }
            Accepted::Method(object, method, args) => {
                return self.relay(
                    id,
                    path,
                    object,
                    Relayed::Call(Some(method), args),
                    sync,
                    _flight,
                );
            }
            Accepted::Get(object, property) => {
                return self.relay(id, path, object, Relayed::Get(property), sync, _flight);
            }
            call => call,
        };
        match call {
            Accepted::Method(..) | Accepted::Get(..) => unreachable!("objects are relays"),
            Accepted::Await(object) => {
                // Check on the pumping thread before handing the async waiter
                // to the executor that might itself be blocked by this stack.
                if sync && object.executor.blocked_here() {
                    self.respond(id, object.ready_on_blocked_executor());
                    drop(_flight);
                    return;
                }
                let (cancel, cancelled) = oneshot::channel();
                {
                    let mut calls = self.0.calls.lock().unwrap();
                    if calls.closed.is_some() {
                        return;
                    }
                    calls.awaiting.insert(
                        id.clone(),
                        Awaiting {
                            object: object.clone(),
                            path: path.clone(),
                            _cancel: cancel,
                        },
                    );
                }
                let peer = self.clone();
                object
                    .executor
                    .handle
                    .clone()
                    .spawn(ASYNC_PATH.scope(path.clone(), async move {
                        tokio::select! {
                            result = object.wait(false, path) => peer.finish_await(id, result),
                            _ = cancelled => {},
                        }
                        drop(_flight);
                    }));
            }
            Accepted::Call(object, args) => {
                // A function this side exported, called by this session: code
                // in it that calls another session forwards from this one.
                let previous = CALLER.with(|caller| caller.replace(Some(self.clone())));
                let result = object.call(args);
                CALLER.with(|caller| caller.replace(previous));
                self.respond(id, result);
                drop(_flight);
            }
            Accepted::Invoke(target, method, args) => {
                let previous = CALLER.with(|caller| caller.replace(Some(self.clone())));
                let result = protected(|| self.0.dispatch.invoke(self, &target, &method, args));
                CALLER.with(|caller| caller.replace(previous));
                self.respond(id, result);
                drop(_flight);
            }
        }
    }
    fn finish_await(&self, id: String, result: Reply) {
        let awaiting = self.0.calls.lock().unwrap().awaiting.remove(&id);
        if awaiting.is_some() {
            self.respond(id, result);
        }
    }
    fn respond(&self, id: String, result: Reply) {
        let result = {
            let mut writer = self.0.writer.lock().unwrap();
            let frame = match &result {
                Ok(value) => match self.encode(value) {
                    Ok(value) => Frame::Return { id, value },
                    Err(error) => Frame::Throw {
                        id,
                        error: error.into(),
                    },
                },
                Err(error) => Frame::Throw {
                    id,
                    error: error.clone().into(),
                },
            };
            self.write_locked(&mut writer, frame)
        };
        if let Err(error) = result {
            self.close(error);
        }
    }
}
fn allocate(exports: &mut Exports, object: Arc<Object>) -> Result<u64, Error> {
    exports.next = exports
        .next
        .checked_add(1)
        .filter(|n| *n <= 9_007_199_254_740_991)
        .ok_or_else(|| transport("reference identifiers exhausted"))?;
    let id = exports.next;
    exports.entries.insert(id, Export { object, grants: 0 });
    Ok(id)
}
fn grant(exports: &mut Exports, id: u64, grants: &mut Vec<u64>) -> Result<WireValue, Error> {
    let export = exports.entries.get_mut(&id).unwrap();
    export.grants = export
        .grants
        .checked_add(1)
        .ok_or_else(|| transport("reference grant overflow"))?;
    grants.push(id);
    Ok(WireValue::Reference {
        id,
        kind: export.object.kind(),
        home: false,
        origin: export.object.origin.clone(),
    })
}
fn finish(waiting: Waiting, result: Reply) {
    match waiting {
        Waiting::Sync { sender, .. } => {
            let _ = sender.send(Message::Reply(result));
        }
        Waiting::Async(sender) => {
            let _ = sender.send(result);
        }
    }
}
fn transport(error: impl std::fmt::Display) -> Error {
    Error::Transport(error.to_string())
}
/// The error a channel failure ends a session with: `"<label>: <reason>"`,
/// or the bare reason for an unlabelled channel.
fn ended(label: &str, error: ChannelError) -> Error {
    match label {
        "" => transport(error),
        label => Error::Transport(format!("{label}: {error}")),
    }
}

mod relay;
use relay::Relayed;

// The tests script the far end over a memory channel.
#[cfg(test)]
mod tests;
