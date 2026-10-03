use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use crate::diagnostics::{BindingDiagnostics, DependencyStatus};
use crate::error::CordisError;
use crate::fiber::{FiberInner, FiberState, Intent, PluginId};
use crate::key::{ScopeId, TypeKey};

pub(crate) type CheckFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// 类型擦除的服务值:内层 `Box<Arc<T>>` 支持 `T: ?Sized`(trait 对象注册)。
#[derive(Clone)]
pub(crate) struct StoredValue(Arc<Box<dyn Any + Send + Sync>>);

impl StoredValue {
    pub(crate) fn new<T: ?Sized + Send + Sync + 'static>(value: Arc<T>) -> Self {
        Self(Arc::new(Box::new(value) as Box<dyn Any + Send + Sync>))
    }

    pub(crate) fn downcast<T: ?Sized + Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        // Box<dyn Any> 擦除的具体类型是内层 Arc<T>(不是 Box<Arc<T>>)
        (**self.0).downcast_ref::<Arc<T>>().cloned()
    }
}

/// 服务绑定。注册表存 `Arc<Binding>`:所有观察者(lookup/依赖解析/摘除)
/// 共享同一身份,`removing` 置位即刻全员可见——不再有克隆快照滞后窗口
///(简化 S1/正确性:旧克隆体的旗标是拍照值,移除中的服务短暂可见)。
pub(crate) struct Binding {
    pub value: ValueSlot,
    pub provider: Weak<FiberInner>,
    pub provider_id: PluginId,
    pub provider_gen: u64,
    pub check: Option<CheckFn>,
    pub check_status: Mutex<Option<DependencyStatus>>,
    /// 摘除已开始(严格解析立即失败;绑定保留至消费者排干,供 provider 子树自访问,§四)。
    pub removing: std::sync::atomic::AtomicBool,
}

/// Ordinary bindings remain immutable; only `provide_mut_as` pays a value lock.
pub(crate) enum ValueSlot {
    Fixed(StoredValue),
    Mutable(Mutex<StoredValue>),
}

impl ValueSlot {
    pub(crate) fn snapshot(&self) -> StoredValue {
        match self {
            Self::Fixed(value) => value.clone(),
            Self::Mutable(value) => value.lock().unwrap().clone(),
        }
    }

    fn replace_mutable(&self, value: StoredValue) -> Option<StoredValue> {
        match self {
            Self::Fixed(_) => None,
            Self::Mutable(slot) => {
                let old = std::mem::replace(&mut *slot.lock().unwrap(), value);
                Some(old)
            }
        }
    }
}

/// 服务注册表(支柱 3)+ 反向依赖索引(支柱 5/D21 三元组)。
pub(crate) struct Registry {
    bindings: Mutex<HashMap<(TypeKey, Option<ScopeId>), Arc<Binding>>>,
    /// 注入索引:TypeKey → 声明依赖它的 fiber。
    inject_index: Mutex<HashMap<TypeKey, Vec<Weak<FiberInner>>>>,
}

/// 稀疏即收缩(0.2.5):条目逻辑删除后容器容量不随历史峰值滞留。
/// 阈值:容量超过 64 槽且长度不足容量 1/4——避免小表抖动,
/// 收缩分摊在每次跨过阈值时。表空(长度 0)自然命中。
fn shrink_if_sparse<K: Eq + std::hash::Hash, V>(map: &mut HashMap<K, V>) {
    if map.capacity() > 64 && map.len() * 4 < map.capacity() {
        map.shrink_to_fit();
    }
}
impl Registry {
    pub(crate) fn new() -> Self {
        Self {
            bindings: Mutex::new(HashMap::new()),
            inject_index: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn table_counts(&self) -> (usize, usize) {
        (
            self.bindings.lock().unwrap().len(),
            self.inject_index.lock().unwrap().len(),
        )
    }

    pub(crate) fn insert_binding(
        &self,
        key: TypeKey,
        scope: Option<ScopeId>,
        binding: Binding,
    ) -> Result<Arc<Binding>, CordisError> {
        let mut bindings = self.bindings.lock().unwrap();
        let entry = (key.clone(), scope.clone());
        if let Some(existing) = bindings.get(&entry) {
            if !existing.removing.load(std::sync::atomic::Ordering::SeqCst) {
                let scope_desc = entry.1.as_deref().unwrap_or("<default>");
                return Err(CordisError::ServiceExists(format!(
                    "{} in scope {scope_desc}",
                    key.describe()
                )));
            }
            // 摘除进行中的绑定可被同键新 provide 替换(TS dispose 同步释放
            // 注册表槽位;旧绑定由驱逐方按 Arc 身份 finalize,不误伤新绑定)
        }
        let stored = Arc::new(binding);
        bindings.insert(entry, stored.clone());
        Ok(stored)
    }

    pub(crate) fn lookup(&self, key: &TypeKey, scope: Option<&ScopeId>) -> Option<Arc<Binding>> {
        let bindings = self.bindings.lock().unwrap();
        bindings.get(&(key.clone(), scope.cloned())).cloned()
    }

    /// Commit a mutable value only while this exact binding still occupies
    /// its key/scope slot. The caller holds the provider transition lock.
    ///
    /// Returns the old value on success, or the *candidate* value on failure
    /// so the caller can drop it outside any framework lock.
    pub(crate) fn replace_mutable_if_current(
        &self,
        key: &TypeKey,
        scope: Option<&ScopeId>,
        expected: &Arc<Binding>,
        value: StoredValue,
    ) -> Result<StoredValue, StoredValue> {
        let bindings = self.bindings.lock().unwrap();
        let current = bindings.get(&(key.clone(), scope.cloned()));
        let Some(current) = current else {
            return Err(value);
        };
        if !Arc::ptr_eq(current, expected)
            || current.removing.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(value);
        }
        match &current.value {
            ValueSlot::Mutable(_) => {
                // Safety: Mutable slot always returns Some.
                Ok(current.value.replace_mutable(value).unwrap())
            }
            ValueSlot::Fixed(_) => Err(value),
        }
    }

    /// Linearize service removal with mutable value commits.
    pub(crate) fn mark_removing_if(
        &self,
        key: &TypeKey,
        scope: Option<&ScopeId>,
        expected: &Arc<Binding>,
    ) {
        let bindings = self.bindings.lock().unwrap();
        if bindings
            .get(&(key.clone(), scope.cloned()))
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            expected
                .removing
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    pub(crate) fn bindings_snapshot(&self) -> Vec<BindingDiagnostics> {
        self.bindings
            .lock()
            .unwrap()
            .iter()
            .map(|((key, scope), binding)| BindingDiagnostics {
                key: key.clone(),
                scope: scope.as_ref().map(|s| s.to_string()),
                provider: binding.provider_id,
                generation: binding.provider_gen,
                removing: binding.removing.load(std::sync::atomic::Ordering::SeqCst),
            })
            .collect()
    }

    pub(crate) fn dependency_status(
        &self,
        key: &TypeKey,
        scope: Option<&ScopeId>,
    ) -> DependencyStatus {
        let Some(binding) = self.lookup(key, scope) else {
            return DependencyStatus::Missing;
        };
        Self::binding_status(&binding)
    }

    /// Cached status of one binding snapshot; never invokes its check callback.
    pub(crate) fn binding_status(binding: &Binding) -> DependencyStatus {
        if binding.removing.load(std::sync::atomic::Ordering::SeqCst) {
            return DependencyStatus::Removing;
        }
        let Some(provider) = binding.provider.upgrade() else {
            return DependencyStatus::Missing;
        };
        let state = provider.state();
        if state != FiberState::Active {
            return DependencyStatus::ProviderInactive(state);
        }
        if binding.check.is_some() {
            return binding
                .check_status
                .lock()
                .unwrap()
                .unwrap_or(DependencyStatus::CheckPending);
        }
        DependencyStatus::Ready
    }

    /// 标记摘除开始:严格解析立即失败;绑定保留至 [`Registry::finalize_binding`]
    /// (清理期自访问,§四)。
    /// 最终摘除绑定(provider 最后;此前保留供清理期自访问,§四)。
    /// 仅当槽位仍是本次摘除的那份绑定才移除——摘除窗口内被新 provide 替换
    /// 过的槽位不动(TS dispose 同步释放槽位语义,对拍 fiber.spec inertia lock 2)。
    pub(crate) fn finalize_binding_if(
        &self,
        key: TypeKey,
        scope: Option<ScopeId>,
        expected: &Arc<Binding>,
    ) -> bool {
        let mut bindings = self.bindings.lock().unwrap();
        let still_old = bindings
            .get(&(key.clone(), scope.clone()))
            .is_some_and(|b| Arc::ptr_eq(b, expected));
        if still_old {
            bindings.remove(&(key, scope));
            shrink_if_sparse(&mut bindings);
        }
        still_old
    }

    /// 该依赖四元组的当前消费者(D21 简化:唯一事实源是各 fiber 的
    /// `last_deps`,不再维护第二份反向索引)。声明注入该键、且 `last_deps`
    /// 含此四元组(含作用域)的 fiber 即为绑定中的消费者——与装载/卸载窗口
    /// 严格一致(last_deps 于 load 的 apply 前设置、drain_effects 清除)。
    /// 四元组含作用域:同一 provider fiber 在不同作用域提供的同键绑定
    /// 互不为对方的消费者(isolate 语义)。
    pub(crate) fn consumers_of(
        &self,
        key: &TypeKey,
        quad: (PluginId, u64, TypeKey, Option<ScopeId>),
    ) -> Vec<Arc<FiberInner>> {
        let index = self.inject_index.lock().unwrap();
        let Some(list) = index.get(key) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|weak| {
                let fiber = weak.upgrade()?;
                let bound = fiber
                    .last_deps
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|deps| deps.contains(&quad));
                bound.then_some(fiber)
            })
            .collect()
    }

    pub(crate) fn register_inject(&self, key: TypeKey, fiber: &Arc<FiberInner>) {
        self.inject_index
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .push(Arc::downgrade(fiber));
    }

    /// 注销终态 fiber 的依赖声明(0.2.1 瞬态释放):驱动已退出的 fiber
    /// 不再参与门控解析与重查通知;条目清空的键一并删除,keyed 声明
    /// (每实例唯一限定名)在长寿 root 下不累积。
    pub(crate) fn unregister_injects(&self, fiber: &Arc<FiberInner>, keys: &[TypeKey]) {
        let weak = Arc::downgrade(fiber);
        let mut index = self.inject_index.lock().unwrap();
        for key in keys {
            let mut empty = false;
            if let Some(list) = index.get_mut(key) {
                list.retain(|w| !Weak::ptr_eq(w, &weak));
                empty = list.is_empty();
            }
            if empty {
                index.remove(key);
            }
        }
        shrink_if_sparse(&mut index);
    }

    /// 通知所有注入该键的 fiber 重查依赖。
    pub(crate) fn notify_key_changed(&self, key: &TypeKey) {
        let fibers: Vec<Arc<FiberInner>> = {
            let index = self.inject_index.lock().unwrap();
            index
                .get(key)
                .map(|list| list.iter().filter_map(|w| w.upgrade()).collect())
                .unwrap_or_default()
        };
        for fiber in fibers {
            fiber.post(Intent::RefreshDeps);
        }
    }

    /// 重查所有声明了依赖的 fiber(`Ctx::refresh`,check() 谓词变更触发)。
    pub(crate) fn refresh_all(&self) {
        let mut seen: HashSet<PluginId> = HashSet::new();
        let index = self.inject_index.lock().unwrap();
        for list in index.values() {
            for weak in list {
                if let Some(fiber) = weak.upgrade() {
                    if seen.insert(fiber.id) {
                        fiber.post(Intent::RefreshDeps);
                    }
                }
            }
        }
    }

    /// 门控解析(支柱 2):存在 + 未在摘除 + provider Active + `check()` 通过。
    pub(crate) fn resolve_dep(
        &self,
        key: &TypeKey,
        scope: Option<&ScopeId>,
    ) -> Option<(PluginId, u64)> {
        let binding = self.lookup(key, scope)?;
        if binding.removing.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let provider = binding.provider.upgrade()?;
        if provider.state() != FiberState::Active {
            return None;
        }
        if let Some(check) = &binding.check {
            // check() 是用户回调:panic 视为不就绪(TS 语义:记日志并删除,
            // fiber.ts:695-698;评审 #6——不得杀调用方所在的驱动任务)
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check()));
            let passed = result.as_ref().copied().unwrap_or(false);
            *binding.check_status.lock().unwrap() = Some(match result {
                Ok(true) => DependencyStatus::Ready,
                Ok(false) => DependencyStatus::CheckRejected,
                Err(_) => DependencyStatus::CheckPanicked,
            });
            if !passed {
                return None;
            }
        }
        Some((binding.provider_id, binding.provider_gen))
    }
}

#[cfg(test)]
mod transient_tests;
