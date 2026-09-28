use std::any::TypeId;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::{shrink_if_sparse, Hook};
use crate::{FiberState, PluginId, TypeKey};

/// The callback family of a registered listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ListenerKind {
    Async,
    AsyncWaterfall,
    Sync,
    SyncWaterfall,
}

/// A current registration, without retaining its callback or fiber.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EventSubscription {
    pub id: u64,
    pub key: TypeKey,
    /// Empty for an exact registration; otherwise these prefixes form one group.
    pub prefixes: Vec<Arc<str>>,
    pub owner: PluginId,
    pub kind: ListenerKind,
    pub once: bool,
    pub prepend: bool,
    /// Pattern selections. Exact registrations omit counters to preserve their
    /// dispatch cost; `None` is distinct from a pattern with zero matches.
    pub selected: Option<u64>,
    /// Actual pattern invocations (short circuits can skip a selection).
    pub invoked: Option<u64>,
}

#[derive(Default)]
pub(super) struct HookMetrics {
    pub(super) selected: AtomicU64,
    pub(super) invoked: AtomicU64,
}

pub(super) struct PatternHook<C> {
    key: TypeKey,
    prefixes: Vec<Arc<str>>,
    hook: Arc<Hook<C>>,
}

pub(super) struct HookTable<C> {
    exact: HashMap<TypeKey, Vec<Arc<Hook<C>>>>,
    patterns: HashMap<TypeId, Vec<PatternHook<C>>>,
}

impl<C> Default for HookTable<C> {
    fn default() -> Self {
        Self {
            exact: HashMap::new(),
            patterns: HashMap::new(),
        }
    }
}

impl<C> Deref for HookTable<C> {
    type Target = HashMap<TypeKey, Vec<Arc<Hook<C>>>>;
    fn deref(&self) -> &Self::Target {
        &self.exact
    }
}
impl<C> DerefMut for HookTable<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.exact
    }
}

impl<C> Hook<C> {
    pub(super) fn live(&self) -> bool {
        self.owner.upgrade().is_some_and(|owner| {
            if !owner.alive.load(Ordering::SeqCst)
                || owner.closing.load(Ordering::SeqCst)
                || owner.ctx.registration_open().is_err()
            {
                return false;
            }
            let snapshot = owner.snapshot_rx.borrow();
            snapshot.generation == self.generation
                && matches!(snapshot.state, FiberState::Loading | FiberState::Active)
        })
    }

    pub(super) fn record_call(&self) {
        if self.pattern {
            let metrics = self.metrics.as_ref().expect("pattern counters");
            metrics.invoked.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl<C> HookTable<C> {
    pub(super) fn has_patterns(&self, key: &TypeKey) -> bool {
        key.instance_id().is_none()
            && key.name().is_some()
            && self.patterns.contains_key(&key.type_id())
    }

    pub(super) fn insert(
        &mut self,
        key: TypeKey,
        prefixes: Option<Vec<Arc<str>>>,
        hook: Arc<Hook<C>>,
    ) {
        if let Some(prefixes) = prefixes {
            self.patterns
                .entry(key.type_id())
                .or_default()
                .push(PatternHook {
                    key,
                    prefixes,
                    hook,
                });
        } else {
            let list = self.exact.entry(key).or_default();
            super::insert_hook(list, hook.clone(), hook.prepend);
        }
    }

    pub(super) fn remove(&mut self, key: &TypeKey, hook: &Arc<Hook<C>>) {
        if hook.pattern {
            if let Some(list) = self.patterns.get_mut(&key.type_id()) {
                list.retain(|entry| !Arc::ptr_eq(&entry.hook, hook));
                if list.is_empty() {
                    self.patterns.remove(&key.type_id());
                }
            }
            shrink_if_sparse(&mut self.patterns);
        } else {
            if let Some(list) = self.exact.get_mut(key) {
                super::retain_hook(list, hook);
                if list.is_empty() {
                    self.exact.remove(key);
                }
            }
            shrink_if_sparse(&mut self.exact);
        }
    }

    /// Selection, group matching, ordering and once claiming share one bus lock.
    pub(super) fn take(&mut self, key: &TypeKey, live_exact: bool) -> Vec<Arc<Hook<C>>> {
        let mut snapshot = Vec::new();
        if let Some(list) = self.exact.get_mut(key) {
            // Non-instance async exact listeners keep their legacy snapshot and
            // unload behavior. Patterns and sync listeners always require live owners.
            if live_exact {
                snapshot.extend(list.iter().filter(|hook| hook.live()).cloned());
            } else {
                snapshot = list.clone();
            }
            list.retain(|hook| !hook.once || (live_exact && !hook.live()));
            if list.is_empty() {
                self.exact.remove(key);
                shrink_if_sparse(&mut self.exact);
            }
        }
        let patterns = self.has_patterns(key);
        if patterns {
            let name = key.name().expect("named pattern dispatch");
            if let Some(list) = self.patterns.get_mut(&key.type_id()) {
                list.retain(|entry| {
                    if entry
                        .prefixes
                        .iter()
                        .any(|prefix| name.starts_with(prefix.as_ref()))
                        && entry.hook.live()
                    {
                        snapshot.push(entry.hook.clone());
                        !entry.hook.once
                    } else {
                        true
                    }
                });
                if list.is_empty() {
                    self.patterns.remove(&key.type_id());
                    shrink_if_sparse(&mut self.patterns);
                }
            }
            snapshot.sort_unstable_by(|a, b| {
                b.prepend.cmp(&a.prepend).then_with(|| {
                    let a_id = a.id;
                    let b_id = b.id;
                    if a.prepend {
                        b_id.cmp(&a_id)
                    } else {
                        a_id.cmp(&b_id)
                    }
                })
            });
        }
        if patterns {
            for hook in &snapshot {
                if let Some(metrics) = &hook.metrics {
                    metrics.selected.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        snapshot
    }

    pub(super) fn diagnostics(&self, kind: ListenerKind, out: &mut Vec<EventSubscription>) {
        let mut add = |key: &TypeKey, prefixes: &[Arc<str>], hook: &Hook<C>| {
            if let Some(owner) = hook.owner.upgrade() {
                out.push(EventSubscription {
                    id: hook.id,
                    key: key.clone(),
                    prefixes: prefixes.to_vec(),
                    owner: owner.id,
                    kind,
                    once: hook.once,
                    prepend: hook.prepend,
                    selected: hook
                        .metrics
                        .as_ref()
                        .map(|m| m.selected.load(Ordering::Relaxed)),
                    invoked: hook
                        .metrics
                        .as_ref()
                        .map(|m| m.invoked.load(Ordering::Relaxed)),
                });
            }
        };
        for (key, hooks) in &self.exact {
            for hook in hooks {
                add(key, &[], hook);
            }
        }
        for list in self.patterns.values() {
            for entry in list {
                add(&entry.key, &entry.prefixes, &entry.hook);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn pattern_count(&self) -> usize {
        self.patterns.values().map(Vec::len).sum()
    }
}
