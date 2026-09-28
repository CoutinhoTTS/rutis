use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::bus::EventBus;
use crate::diagnostics::{
    DependencyDiagnostics, DependencyStatus, PluginDiagnostics, ResolvedDependency,
    RuntimeDiagnostics, ServiceAccess,
};
use crate::effect::{Disposer, Effect, EffectRecord};
use crate::error::{default_sink, CordisError, ErrorSink, ServiceReadError, ServiceReadFailure};
use crate::fiber::{
    join_task, spawn_fiber, DisposeWaitError, FiberInner, FiberState, FiberView, Intent, PluginId,
    TransitionTask,
};
use crate::intercept::{ServiceInterceptors, ServiceWriter};
use crate::key::{InstanceId, ScopeId, TypeKey};
use crate::registry::{Binding, CheckFn, Registry, StoredValue, ValueSlot};
use crate::Plugin;
use crate::PluginFactory;

pub(crate) struct Shared {
    pub admission: Mutex<()>,
    pub handle: Handle,
    pub bus: EventBus,
    pub registry: Registry,
    pub interceptors: Arc<ServiceInterceptors>,
    pub error_sink: ErrorSink,
    pub next_plugin_id: AtomicU64,
    pub closing: AtomicBool,
    pub shutdown_task: Mutex<Option<Arc<TransitionTask>>>,
}

pub(crate) struct CtxInner {
    pub(crate) shared: Arc<Shared>,
    pub(crate) parent: Option<Ctx>,
    pub(crate) fiber: Weak<FiberInner>,
    pub(crate) isolate: Option<(TypeKey, ScopeId)>,
    pub(crate) instance: InstanceId,
    pub(crate) plugin_id: PluginId,
    pub(crate) closing: Arc<AtomicBool>,
    /// The load generation that handed this context to plugin code. Root and
    /// framework-owned contexts have no generation; isolated contexts inherit it.
    pub(crate) generation: Option<u64>,
    pub(crate) generation_token: Option<CancellationToken>,
}

/// 上下文 = `Arc<CtxInner>`(所有权模型:Clone 廉价;isolate/plugin 返回共享内核的新 Ctx)。
#[derive(Clone)]
pub struct Ctx(Arc<CtxInner>);

struct AccessOutcome {
    found: Option<(PluginId, u64, bool)>,
    declared: bool,
    out_of_scope: bool,
    strict: bool,
    failure: Option<ServiceReadFailure>,
}

struct EffectFactoryLease(Arc<FiberInner>);

impl Drop for EffectFactoryLease {
    fn drop(&mut self) {
        self.0.finish_event();
    }
}

impl Ctx {
    pub(crate) fn new_child(
        shared: Arc<Shared>,
        parent: &Ctx,
        fiber: Weak<FiberInner>,
        isolate: Option<(TypeKey, ScopeId)>,
        instance: InstanceId,
        plugin_id: PluginId,
        closing: Arc<AtomicBool>,
    ) -> Self {
        Self(Arc::new(CtxInner {
            shared,
            parent: Some(parent.clone()),
            fiber,
            isolate,
            instance,
            plugin_id,
            closing,
            generation: None,
            generation_token: None,
        }))
    }

    pub(crate) fn new_root(
        shared: Arc<Shared>,
        fiber: Weak<FiberInner>,
        instance: InstanceId,
        plugin_id: PluginId,
        closing: Arc<AtomicBool>,
    ) -> Self {
        Self(Arc::new(CtxInner {
            shared,
            parent: None,
            fiber,
            isolate: None,
            instance,
            plugin_id,
            closing,
            generation: None,
            generation_token: None,
        }))
    }

    pub(crate) fn weak_fiber(&self) -> Weak<FiberInner> {
        self.0.fiber.clone()
    }

    pub(crate) fn for_generation(&self, generation: u64) -> Self {
        self.for_generation_with_token(generation, self.cancellation_token())
    }

    fn for_generation_with_token(&self, generation: u64, token: CancellationToken) -> Self {
        Self(Arc::new(CtxInner {
            shared: self.0.shared.clone(),
            parent: self.0.parent.clone(),
            fiber: self.0.fiber.clone(),
            isolate: self.0.isolate.clone(),
            instance: self.0.instance,
            plugin_id: self.0.plugin_id,
            closing: self.0.closing.clone(),
            generation: Some(generation),
            generation_token: Some(token),
        }))
    }

    fn check_generation(&self, current: u64, state: FiberState) -> Result<(), CordisError> {
        if let Some(expected) = self.0.generation {
            if expected != current {
                return Err(CordisError::StaleGeneration { expected, current });
            }
            if matches!(state, FiberState::Unloading | FiberState::Disposed) {
                return Err(CordisError::InactiveEffect);
            }
            if !matches!(state, FiberState::Loading | FiberState::Active) {
                return Err(CordisError::InactiveGeneration {
                    generation: expected,
                    state,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn plugin_id(&self) -> PluginId {
        self.0.plugin_id
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.0.shared
    }

    /// Identity of the fiber owning this context. Isolated contexts retain it.
    pub fn instance(&self) -> InstanceId {
        self.0.instance
    }

    /// True only while a live ancestor fiber owns this identity.
    pub(crate) fn in_instance(&self, id: InstanceId) -> bool {
        let mut current = self.0.fiber.upgrade();
        while let Some(fiber) = current {
            if fiber.instance == id {
                return true;
            }
            current = fiber.parent_fiber.as_ref().and_then(Weak::upgrade);
        }
        false
    }

    pub(crate) fn instance_owner(&self, id: InstanceId) -> Option<Arc<FiberInner>> {
        let mut current = self.0.fiber.upgrade();
        while let Some(fiber) = current {
            if fiber.instance == id {
                return Some(fiber);
            }
            current = fiber.parent_fiber.as_ref().and_then(Weak::upgrade);
        }
        None
    }

    pub(crate) fn check_instance(&self, key: &TypeKey) -> Result<(), CordisError> {
        if let Some(id) = key.instance_id() {
            if !self.in_instance(id) {
                return Err(CordisError::InstanceOutOfScope { instance: id });
            }
        }
        Ok(())
    }

    pub(crate) fn in_instance_key(&self, key: &TypeKey) -> bool {
        key.instance_id().is_none_or(|id| self.in_instance(id))
    }

    pub(crate) fn subtree_closing(&self) -> bool {
        let mut current = Some(self.clone());
        while let Some(ctx) = current {
            if ctx.0.closing.load(Ordering::SeqCst) {
                return true;
            }
            current = ctx.0.parent.clone();
        }
        false
    }

    pub(crate) fn registration_open(&self) -> Result<(), CordisError> {
        if self.0.shared.closing.load(Ordering::SeqCst) || self.subtree_closing() {
            Err(CordisError::Closed)
        } else {
            Ok(())
        }
    }

    /// Common precedence for public service and instance-event operations:
    /// permanent closure, inactive fiber, then key/instance validation.
    /// Registration itself repeats the state check under the admission lock.
    pub(crate) fn registration_preflight(&self) -> Result<(), CordisError> {
        self.registration_open()?;
        let fiber = self.0.fiber.upgrade().ok_or(CordisError::InactiveEffect)?;
        let tr = fiber.transition.lock().unwrap();
        self.check_generation(tr.generation, tr.state)?;
        if matches!(tr.state, FiberState::Unloading | FiberState::Disposed) {
            return Err(CordisError::InactiveEffect);
        }
        Ok(())
    }

    /// Non-instance event dispatch is not a registration. Preserve its
    /// historical observer behavior for an old context while its fiber is
    /// active again; instance dispatch still uses registration_preflight.
    pub(crate) fn dispatch_preflight(&self) -> Result<(), CordisError> {
        self.registration_open()?;
        let fiber = self.0.fiber.upgrade().ok_or(CordisError::InactiveEffect)?;
        if matches!(fiber.state(), FiberState::Unloading | FiberState::Disposed) {
            return Err(CordisError::InactiveEffect);
        }
        Ok(())
    }

    pub fn handle(&self) -> &Handle {
        &self.0.shared.handle
    }

    /// 错误路由:插件运行期(apply 之外)的异步错误经此上报,不崩 root。
    /// 供外部插件在 turn 边界兜底(如 session 落盘失败),与框架内部
    /// 路由同一 sink——可观测,不静默。
    pub fn error_sink(&self) -> ErrorSink {
        self.0.shared.error_sink.clone()
    }

    /// Take completed, early-disposed effect errors owned by this fiber.
    /// `Disposer::dispose()` returns an error immediately without sending it
    /// to the error sink. Each error is returned here once; taken errors no
    /// longer appear in a later unload result or restart sink notification.
    /// Errors from effects still draining remain available to a subsequent
    /// call or to the unload that owns their completion.
    pub fn take_cleanup_errors(&self) -> Vec<Arc<CordisError>> {
        self.0
            .fiber
            .upgrade()
            .map(|fiber| std::mem::take(&mut *fiber.drained_errors.lock().unwrap()))
            .unwrap_or_default()
    }

    /// 自动路径:`Handle::try_current()` 失败返回明确错误,绝不隐式建 runtime(D8)。
    pub fn root() -> Result<Ctx, CordisError> {
        let handle = Handle::try_current().map_err(|_| {
            CordisError::PluginFailed(
                "no tokio runtime in scope; construct inside #[tokio::test]/runtime, or use Ctx::root_with(handle)".into(),
            )
        })?;
        Ok(Self::root_with_sink(handle, default_sink()))
    }

    /// 注入构造(优先路径,D8)。
    pub fn root_with(handle: Handle) -> Ctx {
        Self::root_with_sink(handle, default_sink())
    }

    /// 注入构造 + 自定义 ErrorSink。
    pub fn root_with_sink(handle: Handle, sink: ErrorSink) -> Ctx {
        let shared = Arc::new(Shared {
            admission: Mutex::new(()),
            handle,
            bus: EventBus::new(),
            registry: Registry::new(),
            interceptors: Arc::new(ServiceInterceptors::default()),
            error_sink: sink,
            next_plugin_id: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            shutdown_task: Mutex::new(None),
        });
        let root_fiber = spawn_fiber(&shared, None, None, true);
        root_fiber.ctx.clone()
    }

    /// root fiber 句柄(root dispose 清子树 / root restart,§五 root_restart)。
    /// 最终 shutdown 并释放所有 `FiberView` 后返回 None。
    pub fn root_view(&self) -> Option<FiberView> {
        let mut current = self.clone();
        while let Some(parent) = current.0.parent.clone() {
            current = parent;
        }
        current.0.fiber.upgrade().map(FiberView::from_inner)
    }

    /// Read-only, best-effort snapshot of live fibers and services.
    ///
    /// This scans fibers and bindings under separate locks. Concurrent lifecycle
    /// changes may therefore mix states or generations from different moments,
    /// even within one plugin's state, dependency, and binding entries. The
    /// result is useful for diagnosis, not an atomic transaction or a change
    /// stream. Dependency checks are never called here: their status is the
    /// last result recorded by normal gate resolution, or `CheckPending` if
    /// that binding has not been checked yet. Reading does not call plugin
    /// `name`, `injects`, or other user code and does not advance a fiber.
    pub fn diagnostics(&self) -> RuntimeDiagnostics {
        let mut plugins = Vec::new();
        if let Some(root) = self.root_view() {
            let mut pending = vec![root.inner];
            while let Some(fiber) = pending.pop() {
                pending.extend(
                    fiber
                        .children
                        .lock()
                        .unwrap()
                        .iter()
                        .filter_map(Weak::upgrade),
                );
                let snapshot = fiber.state_snapshot();
                let injects = fiber
                    .declared_injects
                    .iter()
                    .map(|key| {
                        let scope = fiber.ctx.scope_for(key);
                        let status = if !fiber.ctx.in_instance_key(key) {
                            DependencyStatus::OutOfScope
                        } else {
                            self.0
                                .shared
                                .registry
                                .dependency_status(key, scope.as_ref())
                        };
                        DependencyDiagnostics {
                            key: key.clone(),
                            scope: scope.as_ref().map(|s| s.to_string()),
                            status,
                        }
                    })
                    .collect();
                let resolved_dependencies = fiber
                    .last_deps
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|deps| {
                        deps.iter()
                            .map(|(provider, generation, key, scope)| ResolvedDependency {
                                key: key.clone(),
                                scope: scope.as_ref().map(|s| s.to_string()),
                                provider: *provider,
                                generation: *generation,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                plugins.push(PluginDiagnostics {
                    id: fiber.id,
                    instance: fiber.instance,
                    parent: fiber
                        .parent_fiber
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .map(|p| p.id),
                    name: fiber.name.clone(),
                    state: snapshot.state,
                    generation: snapshot.generation,
                    error: snapshot.error,
                    injects,
                    resolved_dependencies,
                    accesses: fiber.accesses.lock().unwrap().clone(),
                });
            }
        }
        RuntimeDiagnostics {
            shutting_down: self.0.shared.closing.load(Ordering::SeqCst),
            plugins,
            bindings: self.0.shared.registry.bindings_snapshot(),
        }
    }

    /// 最终关闭 root。重复或并发调用共享同一完成结果；现有 `dispose`
    /// 仍可 `restart`。关闭会拒绝新注册并等待准入关闭前完成挂载的子树
    /// 及其清理。创建已开始但挂载被关闭拒绝的子 fiber 不会进入 `apply`，
    /// 会独立关闭；其返回的 `FiberView::shutdown()` 可等待该 driver 退出。
    /// root 的完成结果不包含这种未挂载子 fiber。
    /// 不协作的代码可能使等待无限延长，可用 `shutdown_with_timeout` 限制等待。
    pub fn shutdown(&self) -> crate::BoxFuture<'static, Result<(), Arc<CordisError>>> {
        let _admission = self.0.shared.admission.lock().unwrap();
        let task = {
            let mut slot = self.0.shared.shutdown_task.lock().unwrap();
            if let Some(task) = slot.as_ref() {
                task.clone()
            } else {
                let task = TransitionTask::new();
                *slot = Some(task.clone());
                self.0.shared.closing.store(true, Ordering::SeqCst);
                if let Some(root) = self.root_view() {
                    root.inner.cancel_current();
                    root.inner.post(Intent::Shutdown(task.clone()));
                } else {
                    task.complete(Some(Arc::new(CordisError::Closed)));
                }
                task
            }
        };
        Box::pin(async move { join_task(&task).await })
    }

    /// 限制等待最终关闭的时间。超时后关闭仍在后台进行，重复调用
    /// `shutdown()` 可继续 join 同一结果。
    pub fn shutdown_with_timeout(
        &self,
        limit: Duration,
    ) -> crate::BoxFuture<'static, Result<(), DisposeWaitError>> {
        let root = self.root_view();
        let pending = self.shutdown();
        let Some(root) = root else {
            // No root view remains only after its driver has exited; the
            // cached shutdown result is already available without a deadline.
            return Box::pin(async move { pending.await.map_err(DisposeWaitError::Failed) });
        };
        Box::pin(async move {
            let started = Instant::now();
            match tokio::time::timeout(limit, pending).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(DisposeWaitError::Failed(error)),
                Err(_) => {
                    let snapshot = root.state();
                    Err(DisposeWaitError::TimedOut {
                        plugin_id: root.id,
                        generation: snapshot.generation,
                        state: snapshot.state,
                        elapsed: started.elapsed(),
                    })
                }
            }
        })
    }

    /// 事件总线(全局唯一;事件分发不跨 isolate 过滤,D29)。
    pub fn events(&self) -> &EventBus {
        &self.0.shared.bus
    }

    /// scope 解析:沿 Ctx 父链回溯,取该键最近的 isolate 覆盖(§四:保留父链查找)。
    pub(crate) fn scope_for(&self, key: &TypeKey) -> Option<ScopeId> {
        let mut current = Some(self.clone());
        while let Some(ctx) = current {
            if let Some((k, scope)) = &ctx.0.isolate {
                if k == key {
                    return Some(scope.clone());
                }
            }
            current = ctx.0.parent.clone();
        }
        None
    }

    /// isolate 作用域(支柱 3):按 ServiceKey 隔离,同 label 合并(TS 语义,D21);
    /// 返回的 Ctx 保留原 fiber 所有权(D28)。
    pub fn isolate(&self, key: impl Into<TypeKey>, label: &str) -> Ctx {
        let isolated = Ctx::new_child(
            self.0.shared.clone(),
            self,
            self.0.fiber.clone(),
            Some((key.into(), Arc::from(label))),
            self.instance(),
            self.0.plugin_id,
            self.0.closing.clone(),
        );
        match self.0.generation {
            Some(generation) => {
                isolated.for_generation_with_token(generation, self.cancellation_token())
            }
            None => isolated,
        }
    }

    /// 类型键读取(显式定位器,D13):沿父链解析作用域;
    /// provider 非 Active 时不可见,但其子树内自访问除外(清理期自访问,§四)。
    /// 访问方自身失活(Unloading/Disposed)时同样不可见——TS inactive context
    /// 语义(reflect.spec 'service inject leak' 的语言无关内核;provider 子树
    /// 内自访问豁免,与清理期自访问同一条规则)。
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.get_as::<T>(TypeKey::of::<T>())
    }

    /// 带显式 key 的读取(多实例,shaku Keyed 模式)。
    pub fn get_as<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: impl Into<TypeKey>,
    ) -> Option<Arc<T>> {
        let key = key.into();
        let scope = self.scope_for(&key);
        let out_of_scope = !self.in_instance_key(&key);
        let (result, found) = if out_of_scope {
            (None, None)
        } else {
            let binding = self.0.shared.registry.lookup(&key, scope.as_ref());
            self.read_binding_as::<T>(binding.as_ref())
        };
        if let Some(fiber) = self.0.fiber.upgrade() {
            if fiber.state() == FiberState::Loading {
                self.record_access(
                    &fiber,
                    &key,
                    scope.as_ref(),
                    AccessOutcome {
                        found,
                        declared: fiber.declared_injects.contains(&key),
                        out_of_scope,
                        strict: false,
                        failure: None,
                    },
                );
            }
        }
        result
    }

    /// Read a declared service, or return a call-site error. Unlike `get`,
    /// this enforces the caller's dependency declaration at every invocation.
    #[track_caller]
    pub fn require<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, ServiceReadError> {
        self.require_as(TypeKey::of::<T>())
    }

    /// Strict keyed read. A declaration on an ancestor fiber is usable only
    /// when that ancestor sees the same isolate scope as this context.
    #[track_caller]
    pub fn require_as<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: impl Into<TypeKey>,
    ) -> Result<Arc<T>, ServiceReadError> {
        let location = std::panic::Location::caller();
        let key = key.into();
        let scope = self.scope_for(&key);
        let caller = self.0.fiber.upgrade();
        let own_service = caller.as_ref().is_some_and(|fiber| {
            fiber
                .provided
                .lock()
                .unwrap()
                .iter()
                .any(|(provided_key, provided_scope)| {
                    provided_key == &key && provided_scope.as_ref() == scope.as_ref()
                })
        });
        let mut current = caller.clone();
        let mut declared_by_injects = false;
        while let Some(fiber) = current {
            if fiber.declared_injects.contains(&key)
                && fiber.ctx.scope_for(&key).as_ref() == scope.as_ref()
            {
                declared_by_injects = true;
                break;
            }
            current = fiber
                .parent_fiber
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
        }
        let declared = own_service || declared_by_injects;
        let failure = if !self.in_instance_key(&key) {
            Some(ServiceReadFailure::OutOfScope)
        } else if !key.has_type::<T>() {
            Some(ServiceReadFailure::TypeMismatch)
        } else if caller.as_ref().is_none_or(|fiber| {
            matches!(fiber.state(), FiberState::Unloading | FiberState::Disposed)
        }) && !own_service
        {
            Some(ServiceReadFailure::Inactive)
        } else if !declared {
            Some(ServiceReadFailure::Undeclared)
        } else {
            None
        };
        if let Some(reason) = failure {
            if let Some(fiber) = caller.as_ref() {
                self.record_access(
                    fiber,
                    &key,
                    scope.as_ref(),
                    AccessOutcome {
                        found: None,
                        declared,
                        out_of_scope: reason == ServiceReadFailure::OutOfScope,
                        strict: true,
                        failure: Some(reason),
                    },
                );
            }
            return Err(self.read_error(key, reason, location));
        }

        let binding = self.0.shared.registry.lookup(&key, scope.as_ref());
        // The cached status is read-only: this path never invokes user check().
        // Self-provided services retain the existing loading/cleanup exemption.
        if !own_service {
            let status = binding
                .as_ref()
                .map_or(DependencyStatus::Missing, |binding| {
                    Registry::binding_status(binding)
                });
            if status != DependencyStatus::Ready {
                let reason = ServiceReadFailure::Unavailable(status);
                if let Some(fiber) = caller.as_ref() {
                    self.record_access(
                        fiber,
                        &key,
                        scope.as_ref(),
                        AccessOutcome {
                            found: None,
                            declared,
                            out_of_scope: false,
                            strict: true,
                            failure: Some(reason),
                        },
                    );
                }
                return Err(self.read_error(key, reason, location));
            }
        }
        let (value, found) = self.read_binding_as::<T>(binding.as_ref());
        // During removal, a new provider may take the same slot before this
        // fiber's own provide record is cleared. Do not grant that replacement
        // the self-access exemption.
        if own_service && found.is_some_and(|(provider, _, _)| provider != self.0.plugin_id) {
            let reason = if caller.as_ref().is_none_or(|fiber| {
                matches!(fiber.state(), FiberState::Unloading | FiberState::Disposed)
            }) {
                Some(ServiceReadFailure::Inactive)
            } else if !declared_by_injects {
                Some(ServiceReadFailure::Undeclared)
            } else {
                let status = binding
                    .as_ref()
                    .map_or(DependencyStatus::Missing, |binding| {
                        Registry::binding_status(binding)
                    });
                (status != DependencyStatus::Ready)
                    .then_some(ServiceReadFailure::Unavailable(status))
            };
            if let Some(reason) = reason {
                if let Some(fiber) = caller.as_ref() {
                    self.record_access(
                        fiber,
                        &key,
                        scope.as_ref(),
                        AccessOutcome {
                            found: None,
                            declared: declared_by_injects,
                            out_of_scope: false,
                            strict: true,
                            failure: Some(reason),
                        },
                    );
                }
                return Err(self.read_error(key, reason, location));
            }
        }
        let mut reason = value.is_none().then(|| {
            if !own_service
                && caller.as_ref().is_none_or(|fiber| {
                    matches!(fiber.state(), FiberState::Unloading | FiberState::Disposed)
                })
            {
                return ServiceReadFailure::Inactive;
            }
            let status = binding
                .as_ref()
                .map_or(DependencyStatus::Missing, |binding| {
                    Registry::binding_status(binding)
                });
            // The binding may have changed state since the failed read.
            // Report the failed read, not a contradictory Ready.
            ServiceReadFailure::Unavailable(if status == DependencyStatus::Ready {
                DependencyStatus::Missing
            } else {
                status
            })
        });
        let result = match value {
            Some(value) => self
                .apply_read_interceptors(&key, scope.as_ref(), value)
                .map_err(|failure| {
                    reason = Some(failure);
                    self.read_error(key.clone(), failure, location)
                }),
            None => Err(self.read_error(key.clone(), reason.unwrap(), location)),
        };
        if let Some(fiber) = caller.as_ref() {
            self.record_access(
                fiber,
                &key,
                scope.as_ref(),
                AccessOutcome {
                    found,
                    declared,
                    out_of_scope: false,
                    strict: true,
                    failure: reason,
                },
            );
        }
        result
    }

    fn read_error(
        &self,
        key: TypeKey,
        reason: ServiceReadFailure,
        location: &'static std::panic::Location<'static>,
    ) -> ServiceReadError {
        ServiceReadError {
            key,
            plugin_id: self.0.plugin_id,
            instance: self.instance(),
            location,
            reason,
        }
    }

    fn read_binding_as<T: ?Sized + Send + Sync + 'static>(
        &self,
        binding: Option<&Arc<Binding>>,
    ) -> (Option<Arc<T>>, Option<(PluginId, u64, bool)>) {
        let Some(binding) = binding else {
            return (None, None);
        };
        let Some(provider) = binding.provider.upgrade() else {
            return (None, None);
        };
        let self_access = self.in_subtree_of(&provider);
        let found = Some((binding.provider_id, binding.provider_gen, !self_access));
        if !self_access {
            match self.0.fiber.upgrade() {
                None => return (None, found),
                Some(accessor)
                    if matches!(
                        accessor.state(),
                        FiberState::Unloading | FiberState::Disposed
                    ) =>
                {
                    return (None, found);
                }
                _ => {}
            }
            if provider.state() != FiberState::Active || binding.removing.load(Ordering::SeqCst) {
                return (None, found);
            }
        }
        (binding.value.snapshot().downcast::<T>(), found)
    }

    fn record_access(
        &self,
        fiber: &Arc<FiberInner>,
        key: &TypeKey,
        scope: Option<&ScopeId>,
        outcome: AccessOutcome,
    ) {
        let (provider, generation, external) = outcome
            .found
            .map(|(p, g, x)| (Some(p), Some(g), x))
            .unwrap_or((None, None, false));
        let access = ServiceAccess {
            key: key.clone(),
            scope: scope.map(|s| s.to_string()),
            provider,
            generation,
            declared: outcome.declared,
            external,
            out_of_scope: outcome.out_of_scope,
            strict: outcome.strict,
            failure: outcome.failure,
        };
        let mut accesses = fiber.accesses.lock().unwrap();
        if !accesses.contains(&access) {
            accesses.push(access);
        }
    }

    fn in_subtree_of(&self, other: &Arc<FiberInner>) -> bool {
        let mut current = self.0.fiber.upgrade();
        while let Some(fiber) = current {
            if Arc::ptr_eq(&fiber, other) {
                return true;
            }
            current = fiber.parent_fiber.as_ref().and_then(|w| w.upgrade());
        }
        false
    }

    /// 值语义注册便捷入口(D13)。
    pub fn provide<T: Send + Sync + 'static>(&self, value: T) -> Result<Disposer, CordisError> {
        self.provide_as::<T>(TypeKey::of::<T>(), Arc::new(value))
    }

    /// trait 对象 / 共享实例注册入口。
    pub fn provide_as<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: impl Into<TypeKey>,
        value: Arc<T>,
    ) -> Result<Disposer, CordisError> {
        self.provide_inner(key.into(), value, None, false)
            .map(|(disposer, _)| disposer)
    }

    /// 带 `check()` 谓词的注册(§四:check 门控保留)。
    pub fn provide_as_with_check<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: impl Into<TypeKey>,
        value: Arc<T>,
        check: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Disposer, CordisError> {
        self.provide_inner(key.into(), value, Some(Arc::new(check)), false)
            .map(|(disposer, _)| disposer)
    }

    /// Register a provider-owned mutable binding and return a generation-bound
    /// writer. A write replaces the registered Arc, not existing Arc snapshots.
    pub fn provide_mut_as<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: impl Into<TypeKey>,
        value: Arc<T>,
    ) -> Result<(Disposer, ServiceWriter<T>), CordisError> {
        let key = key.into();
        let scope = self.scope_for(&key);
        let (disposer, binding) = self.provide_inner(key.clone(), value, None, true)?;
        let writer = ServiceWriter::new(key, scope, binding, self.0.shared.clone());
        Ok((disposer, writer))
    }

    fn provide_inner<T: ?Sized + Send + Sync + 'static>(
        &self,
        key: TypeKey,
        value: Arc<T>,
        check: Option<CheckFn>,
        mutable: bool,
    ) -> Result<(Disposer, Arc<Binding>), CordisError> {
        self.registration_preflight()?;
        self.check_instance(&key)?;
        if !key.has_type::<T>() {
            return Err(CordisError::Validation {
                issues: vec![format!(
                    "service key {} expects value of type {}, got {}",
                    key.describe(),
                    key.type_name(),
                    std::any::type_name::<T>()
                )],
            });
        }
        let scope = self.scope_for(&key);
        let shared = self.0.shared.clone();
        let label = format!("service provide: {}", key.describe());
        let inserted = Arc::new(Mutex::new(None));
        let inserted_slot = inserted.clone();
        let disposer =
            self.register_internal_effect_named(label, move |fiber, provider_gen, state| {
                // Admission and the fiber transition lock cover both the binding and
                // its cleanup record, so shutdown cannot miss a committed service.
                let binding = shared.registry.insert_binding(
                    key.clone(),
                    scope.clone(),
                    Binding {
                        value: if mutable {
                            ValueSlot::Mutable(Mutex::new(StoredValue::new(value)))
                        } else {
                            ValueSlot::Fixed(StoredValue::new(value))
                        },
                        provider: Arc::downgrade(fiber),
                        provider_id: fiber.id,
                        provider_gen,
                        check,
                        check_status: Mutex::new(None),
                        removing: AtomicBool::new(false),
                    },
                )?;
                *inserted_slot.lock().unwrap() = Some(binding);
                fiber
                    .provided
                    .lock()
                    .unwrap()
                    .push((key.clone(), scope.clone()));
                let cleanup_shared = shared.clone();
                let provider = Arc::downgrade(fiber);
                let pid = fiber.id;
                let evict_scope = scope.clone();
                let evict_key = key.clone();
                if state == FiberState::Active {
                    shared.registry.notify_key_changed(&key);
                }
                Ok(Effect::AsyncDisposer(Box::new(move || {
                    let shared = cleanup_shared.clone();
                    let provider = provider.clone();
                    let scope = evict_scope.clone();
                    Box::pin(async move {
                        evict_and_finalize(&shared, provider, pid, provider_gen, evict_key, scope)
                            .await
                    })
                })))
            })?;
        let binding = inserted
            .lock()
            .unwrap()
            .take()
            .expect("binding inserted before registration returns");
        Ok((disposer, binding))
    }

    /// 注册清理效应(D23):`f` 立即执行,返回的清理在卸载时 LIFO 执行。
    /// fiber 已 Disposed/Unloading 时返回 `InactiveEffect`(§四:重入报错)。
    pub fn effect(&self, f: impl FnOnce() -> Effect) -> Result<Disposer, CordisError> {
        self.effect_named("anonymous", f)
    }

    /// Register a cleanup with a label visible through [`FiberView::effects`].
    pub fn effect_named(
        &self,
        label: impl Into<String>,
        f: impl FnOnce() -> Effect,
    ) -> Result<Disposer, CordisError> {
        let record = self.register_effect_named(label.into(), f)?;
        let handle = self.handle().clone();
        Ok(Disposer::new(Box::new(move || {
            let record = record.clone();
            let handle = handle.clone();
            Box::pin(async move { record.drain(&handle).await })
        })))
    }

    /// Framework-owned registration: admission, state check, insertion and
    /// cleanup ownership commit at one synchronous point. `f` must not call
    /// user code or try to acquire the admission lock again.
    pub(crate) fn register_internal_effect_named(
        &self,
        label: String,
        f: impl FnOnce(&Arc<FiberInner>, u64, FiberState) -> Result<Effect, CordisError>,
    ) -> Result<Disposer, CordisError> {
        let _admission = self.0.shared.admission.lock().unwrap();
        self.registration_open()?;
        let fiber = self.0.fiber.upgrade().ok_or(CordisError::InactiveEffect)?;
        let tr = fiber.transition.lock().unwrap();
        self.check_generation(tr.generation, tr.state)?;
        if matches!(tr.state, FiberState::Unloading | FiberState::Disposed) {
            return Err(CordisError::InactiveEffect);
        }
        let effect = f(&fiber, tr.generation, tr.state)?;
        let record = EffectRecord::new(effect, label, self.0.fiber.clone());
        fiber.push_effect(record.clone());
        drop(tr);
        let handle = self.handle().clone();
        Ok(Disposer::new(Box::new(move || {
            let record = record.clone();
            let handle = handle.clone();
            Box::pin(async move { record.drain(&handle).await })
        })))
    }

    fn register_mount_effect(
        &self,
        child: &Arc<FiberInner>,
        make: impl FnOnce() -> Effect,
    ) -> Result<Arc<EffectRecord>, CordisError> {
        let _admission = self.0.shared.admission.lock().unwrap();
        self.registration_open()?;
        if child.closing.load(Ordering::SeqCst) || !child.alive.load(Ordering::SeqCst) {
            return Err(CordisError::Closed);
        }
        let parent = self.0.fiber.upgrade().ok_or(CordisError::InactiveEffect)?;
        let tr = parent.transition.lock().unwrap();
        self.check_generation(tr.generation, tr.state)?;
        if matches!(tr.state, FiberState::Unloading | FiberState::Disposed) {
            return Err(CordisError::InactiveEffect);
        }
        let record = EffectRecord::new(
            make(),
            format!("plugin mount: {} #{}", child.name, child.id.0),
            self.0.fiber.clone(),
        );
        parent.push_effect(record.clone());
        *child.mount.lock().unwrap() = Some(record.clone());
        Ok(record)
    }

    /// `effect` 的内部形态:返回记录本体,供 mount 登记等持有引用。
    /// Disposer 语义不变:drop 不触发清理,fiber 卸载仍兜底(D28)。
    pub(crate) fn register_effect_named(
        &self,
        label: String,
        f: impl FnOnce() -> Effect,
    ) -> Result<Arc<EffectRecord>, CordisError> {
        let fiber = self.0.fiber.upgrade();
        if self.0.shared.closing.load(Ordering::SeqCst)
            || (fiber.is_none() && self.subtree_closing())
        {
            return Err(CordisError::Closed);
        }
        let fiber = fiber.ok_or(CordisError::InactiveEffect)?;
        let lease = {
            let _admission = self.0.shared.admission.lock().unwrap();
            let tr = fiber.transition.lock().unwrap();
            if self.0.shared.closing.load(Ordering::SeqCst)
                || (self.subtree_closing() && tr.state != FiberState::Loading)
            {
                return Err(CordisError::Closed);
            }
            self.check_generation(tr.generation, tr.state)?;
            if matches!(tr.state, FiberState::Unloading | FiberState::Disposed) {
                return Err(CordisError::InactiveEffect);
            }
            fiber.begin_event();
            EffectFactoryLease(fiber.clone())
        };
        // factory 锁外执行;但"状态检查 + effects 入队"必须在同一临界区
        //(transition → effects 嵌套,锁序无反向):否则检查通过后驱动恰好
        // 卸载取走 effects,新记录漏掉本轮清理而泄漏(评审 P1)
        let record = EffectRecord::new(f(), label, self.0.fiber.clone());
        let handle = self.handle().clone();
        {
            let tr = fiber.transition.lock().unwrap();
            let stale = self.check_generation(tr.generation, tr.state).err();
            if stale.is_some() || matches!(tr.state, FiberState::Unloading | FiberState::Disposed) {
                drop(tr);
                // 生命周期已越过登记点:f() 可能已有副作用(如插入了监听器),
                // 立即排干该记录的清理并返失败
                let sink = self.error_sink();
                let drain_handle = handle.clone();
                handle.spawn(async move {
                    if let Err(e) = record.drain(&drain_handle).await {
                        sink(e);
                    }
                    drop(lease);
                });
                return Err(stale.unwrap_or(CordisError::InactiveEffect));
            }
            fiber.push_effect(record.clone());
        }
        drop(lease);
        Ok(record)
    }

    /// 装载插件(支柱 1)。返回 FiberView;级联卸载:child dispose 注册为
    /// parent fiber 的 effect(D28:child plugin 自动归 parent fiber 所有)。
    pub fn plugin(&self, p: impl Plugin) -> FiberView {
        let fiber = spawn_fiber(&self.0.shared, Some(self), Some(Arc::new(p)), false);
        self.mount_fiber(fiber)
    }

    /// 工厂模式装载(D32:配置热更新):依赖门控声明在注册时取自工厂并固定,
    /// 每代装载用当前 config 构造实例;返回的 FiberView 可 `update(config)`。
    pub fn plugin_with<C: Send + Sync + 'static>(
        &self,
        factory: impl PluginFactory<C>,
        config: C,
    ) -> FiberView {
        let fiber =
            crate::fiber::spawn_factory_fiber(&self.0.shared, Some(self), factory, config, false);
        self.mount_fiber(fiber)
    }

    /// 工厂模式装载的闭包便捷形态(D32):零依赖声明的单方法工厂。
    /// 需要声明 `injects`/`validate_config` 时实现 [`PluginFactory`]。
    pub fn plugin_from<C: Send + Sync + 'static>(
        &self,
        build: impl Fn(&C) -> Result<Box<dyn Plugin>, CordisError> + Send + Sync + 'static,
        config: C,
    ) -> FiberView {
        struct ClosureFactory<F> {
            build: F,
        }
        impl<C, F> PluginFactory<C> for ClosureFactory<F>
        where
            C: Send + Sync + 'static,
            F: Fn(&C) -> Result<Box<dyn Plugin>, CordisError> + Send + Sync + 'static,
        {
            fn build(&self, config: &C) -> Result<Box<dyn Plugin>, CordisError> {
                (self.build)(config)
            }
        }
        self.plugin_with(ClosureFactory { build }, config)
    }

    /// 装配收尾:级联卸载 effect + 初始装载意图(评审 #10:parent 失活时
    /// 处置子 fiber,不再触发装载)。
    fn mount_fiber(&self, fiber: std::sync::Arc<crate::fiber::FiberInner>) -> FiberView {
        let view = FiberView::from_inner(fiber.clone());
        if !fiber.alive.load(Ordering::SeqCst) {
            return view;
        }
        let child = view.clone();
        let sink = self.error_sink();
        let registered = self.register_mount_effect(&fiber, move || {
            Effect::AsyncDisposer(Box::new(move || {
                let child = child.clone();
                let sink = sink.clone();
                Box::pin(async move {
                    let closing_task = {
                        let _admission = child.inner.ctx.shared().admission.lock().unwrap();
                        child.inner.shutdown_inner.lock().unwrap().clone()
                    };
                    if let Some(task) = closing_task {
                        // Wait for the child's own cleanup, not its public
                        // shutdown result: the latter waits for this mount to
                        // detach and would form a parent/child cycle.
                        let _ = join_task(&task).await;
                        return Ok(());
                    }
                    // 级联 dispose:parent 卸载时子尚未处置,错误仅经 sink 可见;
                    // 子已 Disposed(调用方已从 dispose() 收到同一错误)则不再
                    // 重复上报——0.2.1 子终态退出也会 drain 本记录
                    let delivered = matches!(child.state().state, FiberState::Disposed);
                    if let Err(e) = child.dispose().await {
                        if !delivered {
                            sink(e);
                        }
                    }
                    Ok(())
                })
            }))
        });
        match registered {
            Ok(record) => {
                // Mount ownership was committed under the admission lock.
                drop(record);
                fiber.post(Intent::RefreshDeps);
            }
            Err(_) => {
                // parent 已失活:处置子 fiber,且不再触发装载(评审 #10:
                // 避免 Dispose 之后入队的重载意图无人处理)
                if fiber.closing.load(Ordering::SeqCst) {
                    // The subtree coordinator already owns the terminal intent.
                } else if self.0.shared.closing.load(Ordering::SeqCst) {
                    // Keep the orphan's terminal result available to its
                    // FiberView, even when root shutdown won the mount race.
                    drop(view.shutdown());
                } else {
                    fiber.post(Intent::Dispose);
                }
            }
        }
        view
    }

    /// 当前 fiber 代的取消 token(D27:每代独立 token,卸载第②步取消)。
    pub fn cancellation_token(&self) -> CancellationToken {
        if let Some(token) = &self.0.generation_token {
            return token.clone();
        }
        match self.0.fiber.upgrade() {
            Some(fiber) => fiber.current_token(),
            // fiber 已析构 ≡ 代已结束:返回预取消 token,cancelled() 不永等(评审 P2)
            None => {
                let token = CancellationToken::new();
                token.cancel();
                token
            }
        }
    }

    /// 等待当前 fiber 代被取消(协作取消;不观察则 dispose 无限等待,D27 限制)。
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + 'static {
        let token = self.cancellation_token();
        async move { token.cancelled().await }
    }

    /// 触发依赖重查(check() 谓词结果变更等场景)。
    pub fn refresh(&self) {
        self.0.shared.registry.refresh_all();
    }
}

/// 服务摘除(D14):①捕获本次绑定并标记摘除(严格解析立即失败,绑定保留供
/// 清理期自访问)→ ②预取消 + 可 join 的依赖重查(驱动已退出的消费者任务即刻
/// 完成,不 join 永等,评审 #3)→ ③并发排干后按 Arc 身份最终摘除——
/// 摘除窗口内被新 provide 替换过的槽位不动(TS dispose 同步释放槽位语义,
/// 对拍 fiber.spec inertia lock 2)→ ④摘除 provider 的 `provided` 记账
/// (0.2.1:长寿 root 上反复 provide/dispose 不累积)。
async fn evict_and_finalize(
    shared: &Arc<Shared>,
    provider: Weak<FiberInner>,
    pid: crate::PluginId,
    provider_gen: u64,
    key: TypeKey,
    scope: Option<crate::key::ScopeId>,
) -> Result<(), CordisError> {
    let old = shared
        .registry
        .lookup(&key, scope.as_ref())
        .filter(|b| b.provider_id == pid && b.provider_gen == provider_gen);
    if let Some(binding) = &old {
        shared
            .registry
            .mark_removing_if(&key, scope.as_ref(), binding);
    }
    let consumers: Vec<Arc<FiberInner>> = shared
        .registry
        .consumers_of(&key, (pid, provider_gen, key.clone(), scope.clone()));
    let mut tasks = Vec::new();
    for fiber in &consumers {
        if fiber.id == pid {
            continue;
        }
        fiber.cancel_current();
        if fiber.closing.load(Ordering::SeqCst) {
            let task = {
                let _admission = shared.admission.lock().unwrap();
                fiber.shutdown_inner.lock().unwrap().clone()
            };
            if let Some(task) = task {
                tasks.push(task);
            }
            continue;
        }
        let task = TransitionTask::new();
        // post_join 返回 false 时任务已在 post 内即刻完成(评审 #3)
        fiber.post_join(task.clone(), Intent::RefreshDepsJoin);
        tasks.push(task);
    }
    // 其它注入该键的 fiber(Pending 者)也重查(不取消:可能是等值合并)
    shared.registry.notify_key_changed(&key);
    for task in tasks {
        let _ = join_task(&task).await;
    }
    // 清理期自访问结束,最终摘除(仅当槽位未被替换)
    if let Some(binding) = old {
        shared
            .registry
            .finalize_binding_if(key.clone(), scope.clone(), &binding);
    }
    // ④摘除 provider 的 provided 记账:仅移除本键本作用域的一条(同键
    // 新 provide 的条目保留;fiber 卸载整表清空后此处自然 no-op)
    if let Some(fiber) = provider.upgrade() {
        let mut provided = fiber.provided.lock().unwrap();
        if let Some(pos) = provided.iter().position(|(k, s)| *k == key && *s == scope) {
            provided.swap_remove(pos);
        }
        // 稀疏即收缩(0.2.5):长寿 root 的瞬态记账容量不随历史 provide 滞留
        //(常驻键使表永不为空,按浪费率收缩)。
        if provided.capacity() > 64 && provided.len() * 4 < provided.capacity() {
            provided.shrink_to_fit();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ApplyProbe(Arc<AtomicUsize>);

    impl Plugin for ApplyProbe {
        fn name(&self) -> &str {
            "apply-probe"
        }

        fn apply<'a>(&'a self, _ctx: &'a Ctx) -> crate::BoxFuture<'a, Result<Effect, CordisError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Effect::Done) })
        }
    }

    #[tokio::test]
    async fn child_created_before_root_closes_but_mounted_afterward_never_applies() {
        let ctx = Ctx::root().unwrap();
        let root_view = ctx.root_view().unwrap();
        let applies = Arc::new(AtomicUsize::new(0));
        // Split the public plugin path at its two admission points to force
        // the otherwise narrow spawn/mount race without timing sleeps.
        let fiber = spawn_fiber(
            ctx.shared(),
            Some(&ctx),
            Some(Arc::new(ApplyProbe(applies.clone()))),
            false,
        );
        let root_shutdown = ctx.shutdown();
        let child = ctx.mount_fiber(fiber);
        root_shutdown.await.unwrap();
        child.shutdown().await.unwrap();
        assert_eq!(applies.load(Ordering::SeqCst), 0);
        assert_eq!(child.state().state, FiberState::Disposed);
        assert!(child.inner.effects.lock().unwrap().is_empty());
        assert!(child.inner.driver.lock().unwrap().is_none());
        assert!(ctx.shared().registry.bindings_snapshot().is_empty());
        assert!(root_view.inner.children.lock().unwrap().is_empty());
    }
}
