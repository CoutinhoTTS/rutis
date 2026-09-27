//! Rewrite a candidate while the caller holds a std Mutex, then commit the
//! checked result. Run with `cargo run -p rutis --example sync_decision`.

use std::sync::Mutex;

use rutis::{CordisError, Ctx, Event, EventKey, SyncEvent, SyncNext};

struct BeforeSave {
    candidate: u64,
}
impl Event for BeforeSave {
    const NAME: &'static str = "example::BeforeSave";
    type Value = u64;
}
impl SyncEvent for BeforeSave {}
const BEFORE_SAVE: EventKey<BeforeSave> = EventKey::named("before-save");

fn save(ctx: &Ctx, state: &Mutex<u64>, candidate: u64) -> Result<u64, CordisError> {
    let mut current = state.lock().unwrap();
    let proposed =
        ctx.events()
            .waterfall_sync(ctx, &BEFORE_SAVE, &BeforeSave { candidate }, |_, event| {
                Ok((*current).max(event.candidate))
            })?;
    // Middleware only proposes a value; the caller validates before writing.
    if proposed < *current {
        return Err(CordisError::Validation {
            issues: vec!["saved value must not decrease".into()],
        });
    }
    *current = proposed;
    Ok(proposed)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = Ctx::root()?;
    root.events().on_waterfall_sync(
        &root,
        &BEFORE_SAVE,
        |_: &Ctx, _: &BeforeSave, next: SyncNext<'_, BeforeSave>| {
            // Fixed input: rewrite the downstream result as it returns.
            Ok(next.call()?.min(100))
        },
    )?;
    let state = Mutex::new(10);
    assert_eq!(save(&root, &state, 120)?, 100);
    println!("saved {}", state.lock().unwrap());
    root.shutdown().await?;
    Ok(())
}
