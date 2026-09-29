//! Publishes imported Cordis service objects as ordinary rutis services.
//!
//! Only public rutis API is used: the first object is registered with
//! `provide_mut_as`; a replacement goes through the returned `ServiceWriter`,
//! so earlier `Arc` snapshots keep their original remote object. An
//! unavailable slot withdraws the binding, which lets native dependency
//! gating stop and restart consumers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rutis::{CordisError, Ctx, Disposer, ServiceWriter, TypeKey};

use crate::process::ServiceEvents;
use crate::{Error, Process};

type Apply = Box<dyn FnMut(&Ctx, &Arc<Process>, Option<String>) -> Result<(), Error> + Send>;

struct Slot {
    handle: Option<String>,
    applied: Option<String>,
    apply: Option<Apply>,
}

#[derive(Default)]
struct State {
    target: Option<(Ctx, Arc<Process>)>,
    applying: bool,
    slots: HashMap<String, Slot>,
}

/// Declared before launching, attached once the mounting plugin has a
/// process, closed when that plugin is disposed.
#[derive(Default)]
pub struct Projection {
    state: Mutex<State>,
}

impl Projection {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Declare an exported slot. `make` builds the native proxy for one handle.
    pub fn service<T: Send + Sync + 'static>(
        &self,
        name: &str,
        make: fn(Arc<Process>, String) -> T,
    ) {
        let mut binding: Option<(Disposer, ServiceWriter<T>)> = None;
        let apply: Apply = Box::new(move |ctx, process, handle| {
            match (handle, binding.as_ref()) {
                (Some(handle), Some((_, writer))) => writer
                    .set(ctx, Arc::new(make(process.clone(), handle)))
                    .map_err(|error| Error::Value(error.to_string()))?,
                (Some(handle), None) => {
                    let registered = ctx
                        .provide_mut_as(TypeKey::of::<T>(), Arc::new(make(process.clone(), handle)))
                        .map_err(|error| Error::Value(error.to_string()))?;
                    binding = Some(registered);
                }
                (None, Some(_)) => {
                    let (disposer, _) = binding.take().unwrap();
                    tokio::runtime::Handle::current().spawn(disposer.dispose());
                }
                (None, None) => {}
            }
            Ok(())
        });
        self.state.lock().unwrap().slots.insert(
            name.to_owned(),
            Slot {
                handle: None,
                applied: None,
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
            state.target = Some((ctx.clone(), process));
        }
        self.publish().map_err(Into::into)
    }

    /// Stop following changes; the plugin's own effects withdraw bindings.
    pub fn close(&self) {
        self.state.lock().unwrap().target = None;
    }

    /// Apply pending handles outside the lock: registering or replacing a
    /// service may run native code that calls back into this projection.
    /// A reentrant or concurrent change is left to the active publisher.
    fn publish(&self) -> Result<(), Error> {
        let mut failure = None;
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
                let target = state.target.clone();
                let next = target.and_then(|target| {
                    state
                        .slots
                        .iter_mut()
                        .find_map(|(name, slot)| {
                            (slot.handle != slot.applied && slot.apply.is_some()).then(|| {
                                (
                                    name.clone(),
                                    slot.handle.clone(),
                                    slot.apply.take().unwrap(),
                                )
                            })
                        })
                        .map(|work| (target, work))
                });
                if next.is_none() {
                    state.applying = false;
                }
                next
            };
            let Some(((ctx, process), (name, handle, mut apply))) = work else {
                return failure.map_or(Ok(()), Err);
            };
            let result = apply(&ctx, &process, handle.clone());
            let mut state = self.state.lock().unwrap();
            let slot = state.slots.get_mut(&name).unwrap();
            slot.apply = Some(apply);
            match result {
                Ok(()) => slot.applied = handle,
                Err(error) => {
                    // Do not retry the same failing handle in this pass.
                    slot.applied = slot.handle.clone();
                    failure = failure.or(Some(error));
                }
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
            // published proxies release their handle when dropped.
            previous
                .filter(|previous| slot.applied.as_ref() != Some(previous))
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
