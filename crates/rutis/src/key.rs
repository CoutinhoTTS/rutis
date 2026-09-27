use std::any::TypeId;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A process-local fiber identity. IDs are never reused, including after shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstanceId(NonZeroU64);

impl InstanceId {
    pub(crate) fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let value = NEXT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("rutis InstanceId exhausted");
        Self(NonZeroU64::new(value).expect("InstanceId starts at one"))
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// 限定名(D33):双轨——静态路径零分配,动态路径(Arc)承接运行时名字。
/// PartialEq/Eq/Hash 按**字符串内容**:Static 与 Dynamic 同名等值互通;
/// Debug 同样只显示内容(不显示变体),与 Eq 语义一致——变体是实现细节,
/// 打出来会误导调试。
#[derive(Clone)]
pub(crate) enum Qualifier {
    Static(&'static str),
    Dynamic(Arc<str>),
}

impl Qualifier {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Qualifier::Static(s) => s,
            Qualifier::Dynamic(s) => s,
        }
    }
}

impl std::fmt::Debug for Qualifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq for Qualifier {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Qualifier {}

impl std::hash::Hash for Qualifier {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

/// 服务键 = TypeId + 可选限定名(D21:`ServiceKey = TypeKey`)。
/// D33:限定名放宽为双轨,支持运行时构造的名字(桥事件等);
/// 代价是失去 `Copy`——克隆装载/注册路径,频率低。
/// 携带构造时捕获的类型名,仅用于诊断(`describe`/错误消息);
/// 相等与哈希由 TypeId、限定名和实例号决定；类型名只供诊断。
pub struct TypeKey {
    type_id: TypeId,
    type_name: &'static str,
    qualifier: Option<Qualifier>,
    instance: Option<InstanceId>,
}

impl Clone for TypeKey {
    fn clone(&self) -> Self {
        Self {
            type_id: self.type_id,
            type_name: self.type_name,
            qualifier: self.qualifier.clone(),
            instance: self.instance,
        }
    }
}

impl PartialEq for TypeKey {
    fn eq(&self, other: &Self) -> bool {
        self.type_id == other.type_id
            && self.qualifier == other.qualifier
            && self.instance == other.instance
    }
}

impl Eq for TypeKey {}

impl std::hash::Hash for TypeKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.type_id.hash(state);
        self.qualifier.hash(state);
        self.instance.hash(state);
    }
}

impl std::fmt::Debug for TypeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

impl TypeKey {
    pub(crate) fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// The explicit channel name, if this key has one.
    pub fn name(&self) -> Option<&str> {
        self.qualifier.as_ref().map(Qualifier::as_str)
    }
    pub(crate) fn has_type<T: ?Sized + 'static>(&self) -> bool {
        self.type_id == TypeId::of::<T>()
    }

    pub(crate) fn type_name(&self) -> &'static str {
        self.type_name
    }

    /// 类型主键(默认,无限定名)。
    pub fn of<T: ?Sized + 'static>() -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            qualifier: None,
            instance: None,
        }
    }

    /// 带限定名的键:同接口多实例(shaku Keyed 模式)。静态名零分配。
    pub fn keyed<T: ?Sized + 'static>(qualifier: &'static str) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            qualifier: Some(Qualifier::Static(qualifier)),
            instance: None,
        }
    }

    /// 带动态限定名的键(D33):运行时构造的名字(桥事件名等)。
    /// 与 `keyed` 同名等值互通。
    pub fn keyed_dynamic<T: ?Sized + 'static>(name: impl Into<Arc<str>>) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            type_name: std::any::type_name::<T>(),
            qualifier: Some(Qualifier::Dynamic(name.into())),
            instance: None,
        }
    }

    /// A service or event key owned by one fiber instance.
    pub fn instance<T: ?Sized + 'static>(id: InstanceId) -> Self {
        Self::of::<T>().with_instance(id)
    }

    /// Attach a fiber identity to a qualified or unqualified key.
    pub fn with_instance(mut self, id: InstanceId) -> Self {
        self.instance = Some(id);
        self
    }

    /// Instance identity, if this key is scoped to a fiber subtree.
    pub fn instance_id(&self) -> Option<InstanceId> {
        self.instance
    }

    /// 诊断描述(不参与分发):类型名 + 限定名(0.2.2 起携带构造时捕获的
    /// `type_name::<T>()`,错误消息与装配图可读)。
    pub fn describe(&self) -> String {
        let base = match &self.qualifier {
            Some(q) => format!("{}#{}", self.type_name, q.as_str()),
            None => self.type_name.to_string(),
        };
        match self.instance {
            Some(id) => format!("{base}@{id}"),
            None => base,
        }
    }
}

/// A channel whose payload and result types are fixed by `E`.
///
/// Static and dynamic names compare by content. The default type channel is
/// separate from every named channel; an instance scopes either kind to its
/// fiber subtree. Constructing a name does not check its spelling.
///
/// ```compile_fail
/// use rutis::{Ctx, Event, EventKey};
/// use std::sync::Arc;
/// struct One;
/// struct Two;
/// impl Event for One { const NAME: &'static str = "one"; type Value = (); }
/// impl Event for Two { const NAME: &'static str = "two"; type Value = (); }
/// fn wrong(ctx: &Ctx) {
///     ctx.events().emit(ctx, &EventKey::<One>::named("one"), Arc::new(Two));
/// }
/// ```
pub struct EventKey<E: crate::Event> {
    qualifier: Option<Qualifier>,
    instance: Option<InstanceId>,
    marker: PhantomData<fn(E) -> E>,
}

impl<E: crate::Event> EventKey<E> {
    /// The original, unnamed type channel. This constructor is allocation free.
    pub const fn of() -> Self {
        Self {
            qualifier: None,
            instance: None,
            marker: PhantomData,
        }
    }

    /// An allocation-free static channel name.
    pub const fn named(name: &'static str) -> Self {
        Self {
            qualifier: Some(Qualifier::Static(name)),
            instance: None,
            marker: PhantomData,
        }
    }

    pub fn dynamic(name: impl Into<Arc<str>>) -> Self {
        Self {
            qualifier: Some(Qualifier::Dynamic(name.into())),
            instance: None,
            marker: PhantomData,
        }
    }

    pub const fn instance(mut self, id: InstanceId) -> Self {
        self.instance = Some(id);
        self
    }

    pub fn name(&self) -> Option<&str> {
        self.qualifier.as_ref().map(Qualifier::as_str)
    }

    pub fn instance_id(&self) -> Option<InstanceId> {
        self.instance
    }

    pub fn describe(&self) -> String {
        self.erased().describe()
    }

    pub(crate) fn erased(&self) -> TypeKey {
        TypeKey {
            type_id: TypeId::of::<E>(),
            type_name: std::any::type_name::<E>(),
            qualifier: self.qualifier.clone(),
            instance: self.instance,
        }
    }

    pub(crate) fn from_erased(key: &TypeKey) -> Self {
        debug_assert!(key.has_type::<E>());
        Self {
            qualifier: key.qualifier.clone(),
            instance: key.instance,
            marker: PhantomData,
        }
    }
}

impl<E: crate::Event> Default for EventKey<E> {
    fn default() -> Self {
        Self::of()
    }
}
impl<E: crate::Event> Clone for EventKey<E> {
    fn clone(&self) -> Self {
        Self {
            qualifier: self.qualifier.clone(),
            instance: self.instance,
            marker: PhantomData,
        }
    }
}
impl<E: crate::Event> PartialEq for EventKey<E> {
    fn eq(&self, other: &Self) -> bool {
        self.qualifier == other.qualifier && self.instance == other.instance
    }
}
impl<E: crate::Event> Eq for EventKey<E> {}
impl<E: crate::Event> std::hash::Hash for EventKey<E> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.erased().hash(state);
    }
}
impl<E: crate::Event> std::fmt::Debug for EventKey<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.erased().fmt(f)
    }
}

/// One registration matching any of its prefixes, within one event type.
/// Only named channels without an instance can match. An empty prefix matches
/// every such channel; overlapping prefixes still select the registration once.
pub struct EventPattern<E: crate::Event> {
    prefixes: Vec<Arc<str>>,
    marker: PhantomData<fn(E) -> E>,
}

impl<E: crate::Event> EventPattern<E> {
    pub fn prefix(prefix: impl Into<Arc<str>>) -> Self {
        Self::any_prefix([prefix])
    }

    pub fn any_prefix<S: Into<Arc<str>>>(prefixes: impl IntoIterator<Item = S>) -> Self {
        Self {
            prefixes: prefixes.into_iter().map(Into::into).collect(),
            marker: PhantomData,
        }
    }

    pub fn prefixes(&self) -> &[Arc<str>] {
        &self.prefixes
    }

    pub(crate) fn into_prefixes(self) -> Vec<Arc<str>> {
        self.prefixes
    }
}

impl<E: crate::Event> Clone for EventPattern<E> {
    fn clone(&self) -> Self {
        Self {
            prefixes: self.prefixes.clone(),
            marker: PhantomData,
        }
    }
}
impl<E: crate::Event> std::fmt::Debug for EventPattern<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventPattern")
            .field("type", &std::any::type_name::<E>())
            .field("prefixes", &self.prefixes)
            .finish()
    }
}

/// 服务键别名(D21 裁决:`ServiceKey` 不另建类型)。
pub type ServiceKey = TypeKey;

/// 类型化限定名常量(开放问题 2 的裁决:`Key<T>` newtype,shaku Keyed 精神)。
///
/// ```
/// use rutis::{Key, TypeKey};
/// const PRIMARY: Key<str> = Key::new("primary");
/// let k: TypeKey = PRIMARY.into();
/// assert_eq!(k, TypeKey::keyed::<str>("primary"));
/// ```
pub struct Key<T: ?Sized + 'static> {
    name: &'static str,
    _marker: PhantomData<fn() -> T>,
}

impl<T: ?Sized + 'static> Key<T> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: PhantomData,
        }
    }
}

impl<T: ?Sized + 'static> From<Key<T>> for TypeKey {
    fn from(k: Key<T>) -> Self {
        TypeKey::keyed::<T>(k.name)
    }
}

/// isolate 作用域标识:同 label 字符串合并为同一作用域(TS 语义,D21)。
pub(crate) type ScopeId = std::sync::Arc<str>;

#[cfg(test)]
mod describe_tests {
    use super::*;

    struct ReadableService;

    /// 0.2.2:describe 携带构造时捕获的类型名与限定名,诊断可读。
    #[test]
    fn describe_uses_type_name_and_qualifier() {
        let plain = TypeKey::of::<ReadableService>();
        assert!(plain.describe().contains("ReadableService"));
        let keyed = TypeKey::keyed_dynamic::<ReadableService>("session-1/main");
        assert_eq!(
            keyed.describe(),
            format!(
                "{}#session-1/main",
                std::any::type_name::<ReadableService>()
            )
        );
        // 相等仍只由 TypeId + 限定名决定;诊断字段不参与。
        assert_eq!(plain, TypeKey::of::<ReadableService>());
        assert_ne!(plain, keyed);
    }

    #[test]
    fn instance_is_part_of_key_equality_hash_and_description() {
        use std::collections::HashSet;
        let one = InstanceId::allocate();
        let two = InstanceId::allocate();
        assert_ne!(one, two);
        let a = TypeKey::keyed::<ReadableService>("main").with_instance(one);
        let same = TypeKey::keyed_dynamic::<ReadableService>("main").with_instance(one);
        let other = TypeKey::keyed::<ReadableService>("main").with_instance(two);
        assert_eq!(a, same);
        assert_ne!(a, other);
        assert_ne!(a, TypeKey::keyed::<ReadableService>("main"));
        assert_eq!(
            a.describe(),
            format!("{}#main@{one}", std::any::type_name::<ReadableService>())
        );
        let mut keys = HashSet::new();
        keys.insert(a);
        assert!(keys.contains(&same));
        assert!(!keys.contains(&other));
    }
}
