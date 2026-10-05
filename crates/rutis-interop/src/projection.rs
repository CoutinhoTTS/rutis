//! Publishes imported Cordis service objects as ordinary rutis services.
//!
//! Only public rutis API is used: the first object is registered with
//! `provide_mut_as`; a replacement goes through the returned `ServiceWriter`,
//! so earlier `Arc` snapshots keep their original remote object. An
//! unavailable slot withdraws the binding, which lets native dependency
//! gating stop and restart consumers. When the Node process goes away every
//! slot becomes unavailable.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use rutis::{BoxFuture, CordisError, Ctx, Disposer, ServiceWriter, TypeKey};

use crate::process::ServiceEvents;
use crate::{Error, Process};

/// Moves the native binding to `handle`. A withdrawal returns the future that
/// completes it: no new registration may start before it finishes.
type Apply = Box<
    dyn FnMut(&Ctx, &Arc<Process>, Option<String>) -> Result<Option<BoxFuture<'static, ()>>, Error>
        + Send,
>;

struct Slot {
    /// Newest handle reported by the Cordis side.
    handle: Option<String>,
    /// Handle the native binding currently holds.
    applied: Option<String>,
    /// Handle whose proxy is being published outside the lock.
    publishing: Option<String>,
    /// A withdrawal is still completing.
    withdrawing: bool,
    apply: Option<Apply>,
}

#[derive(Default)]
struct State {
    target: Option<(Ctx, Arc<Process>)>,
    runtime: Option<tokio::runtime::Handle>,
    applying: bool,
    slots: HashMap<String, Slot>,
}

/// Declared before launching, attached once the mounting plugin has a
/// process, closed when that plugin is disposed.
pub struct Projection {
    state: Mutex<State>,
    me: Weak<Projection>,
}

impl Projection {
    pub fn new() -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            state: Mutex::default(),
            me: me.clone(),
        })
    }

    /// Declare an exported slot. `make` builds the native proxy for one handle.
    pub fn service<T: Send + Sync + 'static>(
        &self,
        name: &str,
        make: fn(Arc<Process>, String) -> T,
    ) {
        self.service_keyed(name, TypeKey::of::<T>(), move |process, handle| {
            Arc::new(make(process, handle))
        });
    }

    /// Declare an exported slot published under `key`, for example a
    /// `dyn HostDispatch` under [`crate::host_key`].
    pub fn service_keyed<T: ?Sized + Send + Sync + 'static>(
        &self,
        name: &str,
        key: TypeKey,
        mut make: impl FnMut(Arc<Process>, String) -> Arc<T> + Send + 'static,
    ) {
        let mut binding: Option<(Disposer, ServiceWriter<T>)> = None;
        // A proxy whose publication failed is kept for the retry: it holds its
        // handle, and dropping it would release that handle on the Cordis side.
        let mut candidate: Option<(String, Arc<T>)> = None;
        let apply: Apply = Box::new(move |ctx, process, handle| {
            let Some(handle) = handle else {
                candidate = None;
                if let Some((disposer, _)) = binding.take() {
                    return Ok(Some(Box::pin(async move {
                        let _ = disposer.dispose().await;
                    })));
                }
                return Ok(None);
            };
            let proxy = match candidate.take() {
                Some((kept, proxy)) if kept == handle => proxy,
                _ => make(process.clone(), handle.clone()),
            };
            let published = match binding.as_ref() {
                Some((_, writer)) => writer
                    .set(ctx, proxy.clone())
                    .map_err(|error| Error::Value(error.to_string())),
                None => ctx
                    .provide_mut_as(key.clone(), proxy.clone())
                    .map(|registered| binding = Some(registered))
                    .map_err(|error| Error::Value(error.to_string())),
            };
            if published.is_err() {
                candidate = Some((handle, proxy));
            }
            published.map(|()| None)
        });
        self.state.lock().unwrap().slots.insert(
            name.to_owned(),
            Slot {
                handle: None,
                applied: None,
                publishing: None,
                withdrawing: false,
                apply: Some(apply),
            },
        );
    }

    /// Publish the current handles from the mounting plugin's own context.
    pub fn attach(&self, ctx: &Ctx, process: Arc<Process>) -> Result<(), CordisError> {
        {
            let mut state = self.state.lock().unwrap();
            for (name, slot) in state.slots.iter_mut() {
                slot.handle = process.service(name);
            }
            state.target = Some((ctx.clone(), process.clone()));
            state.runtime = Some(tokio::runtime::Handle::current());
        }
        // A Node process that goes away leaves every slot unavailable: the
        // services are withdrawn and native gating stops their consumers.
        let connection = process.connection().clone();
        let me = self.me.clone();
        tokio::spawn(async move {
            connection.closed().await;
            if let Some(me) = me.upgrade() {
                me.disconnected();
            }
        });
        self.publish().map_err(Into::into)
    }

    fn disconnected(&self) {
        {
            let mut state = self.state.lock().unwrap();
            if state.target.is_none() {
                return; // closed by disposal
            }
            for slot in state.slots.values_mut() {
                slot.handle = None;
            }
        }
        if let Err(error) = self.publish() {
            eprintln!("rutis-interop: cannot withdraw Cordis services: {error}");
        }
    }

    /// Withdraw every published service now and wait until that is done:
    /// the kernel stops their consumers first. Then stop following changes,
    /// as [`Projection::close`]. A plugin that unloads its provider calls
    /// this first, so the provider outlives everything that uses it.
    pub async fn withdraw(&self) {
        let (target, mut slots) = {
            let mut state = self.state.lock().unwrap();
            (state.target.take(), std::mem::take(&mut state.slots))
        };
        let Some((ctx, process)) = target else {
            return;
        };
        let mut withdrawals = Vec::new();
        for slot in slots.values_mut() {
            if let Some(apply) = slot.apply.as_mut() {
                if let Ok(Some(withdrawal)) = apply(&ctx, &process, None) {
                    withdrawals.push(withdrawal);
                }
            }
        }
        for withdrawal in withdrawals {
            withdrawal.await;
        }
        drop(slots);
    }

    /// Stop following changes and drop the bindings' writers, which hold the
    /// process; the plugin's own effects withdraw the registrations.
    pub fn close(&self) {
        let slots = {
            let mut state = self.state.lock().unwrap();
            state.target = None;
            std::mem::take(&mut state.slots)
        };
        drop(slots);
    }

    /// Apply pending handles outside the lock: registering or replacing a
    /// service may run native code that calls back into this projection.
    /// A reentrant or concurrent change is left to the active publisher.
    fn publish(&self) -> Result<(), Error> {
        let mut failure = None;
        // Each (slot, handle) is attempted at most once per pass: a failure
        // keeps the slot pending for the next change, while a newer handle
        // that arrived meanwhile is still published in this pass.
        let mut attempted = HashSet::new();
        {
            let mut state = self.state.lock().unwrap();
            if state.applying {
                return Ok(());
            }
            state.applying = true;
        }
        loop {
            let work = {
                let mut state = self.state.lock().unwrap();
                let next = state.target.clone().and_then(|target| {
                    state
                        .slots
                        .iter_mut()
                        .find(|(name, slot)| {
                            slot.handle != slot.applied
                                && !slot.withdrawing
                                && slot.apply.is_some()
                                && !attempted.contains(&(name.to_string(), slot.handle.clone()))
                        })
                        .map(|(name, slot)| {
                            slot.publishing = slot.handle.clone();
                            let work = (
                                name.clone(),
                                slot.handle.clone(),
                                slot.apply.take().unwrap(),
                            );
                            (target, work)
                        })
                });
                if next.is_none() {
                    state.applying = false;
                }
                next
            };
            let Some(((ctx, process), (name, handle, mut apply))) = work else {
                return failure.map_or(Ok(()), Err);
            };
            attempted.insert((name.clone(), handle.clone()));
            let result = apply(&ctx, &process, handle.clone());
            let mut state = self.state.lock().unwrap();
            let runtime = state.runtime.clone();
            let Some(slot) = state.slots.get_mut(&name) else {
                continue; // closed meanwhile
            };
            slot.apply = Some(apply);
            slot.publishing = None;
            match result {
                Ok(None) => slot.applied = handle,
                Ok(Some(withdrawal)) => {
                    slot.applied = None;
                    slot.withdrawing = true;
                    let me = self.me.clone();
                    let name = name.clone();
                    let finish = async move {
                        withdrawal.await;
                        if let Some(me) = me.upgrade() {
                            if let Some(slot) = me.state.lock().unwrap().slots.get_mut(&name) {
                                slot.withdrawing = false;
                            }
                            if let Err(error) = me.publish() {
                                eprintln!(
                                    "rutis-interop: cannot publish Cordis service {name}: {error}"
                                );
                            }
                        }
                    };
                    match runtime {
                        Some(runtime) => {
                            runtime.spawn(finish);
                        }
                        None => {
                            tokio::spawn(finish);
                        }
                    }
                }
                Err(error) => failure = failure.or(Some(error)),
            }
        }
    }
}

impl ServiceEvents for Projection {
    fn changed(&self, name: &str, handle: Option<String>, _version: u64) {
        let skipped = {
            let mut state = self.state.lock().unwrap();
            let process = state.target.as_ref().map(|(_, process)| process.clone());
            let Some(slot) = state.slots.get_mut(name) else {
                return;
            };
            let previous = std::mem::replace(&mut slot.handle, handle);
            // A handle replaced before it got a native proxy is released here;
            // applied or in-flight proxies release their handle when dropped.
            previous
                .filter(|previous| {
                    slot.applied.as_ref() != Some(previous)
                        && slot.publishing.as_ref() != Some(previous)
                })
                .zip(process)
        };
        if let Some((handle, process)) = skipped {
            process.release(&handle);
        }
        if let Err(error) = self.publish() {
            eprintln!("rutis-interop: cannot publish Cordis service {name}: {error}");
        }
    }
}
