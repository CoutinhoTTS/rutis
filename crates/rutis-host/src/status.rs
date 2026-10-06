//! What the host prints: each row's state as it changes.

use std::collections::BTreeMap;
use std::time::Duration;

use rutis::FiberState;
use rutis_loader::{EntryStatus, Loader};

/// One line for a row's state.
pub fn describe(status: &EntryStatus) -> String {
    match status {
        EntryStatus::Disabled => "disabled".into(),
        EntryStatus::Inactive => "inactive".into(),
        EntryStatus::Unresolved(error) => format!("unresolved: {error}"),
        EntryStatus::Running(snapshot) => match snapshot.state {
            FiberState::Pending => "waiting for its services".into(),
            FiberState::Loading => "starting".into(),
            FiberState::Active => "running".into(),
            FiberState::Failed => match &snapshot.error {
                Some(error) => format!("failed: {error}"),
                None => "failed".into(),
            },
            FiberState::Disposed => "stopped".into(),
            _ => format!("{:?}", snapshot.state).to_lowercase(),
        },
    }
}

/// Print every change of a row's state, for as long as the host runs.
pub fn follow(loader: Loader) {
    tokio::spawn(async move {
        let mut shown = BTreeMap::<String, String>::new();
        loop {
            let mut now = BTreeMap::new();
            for entry in loader.entries() {
                now.insert(entry.id.clone(), describe(&entry.status));
            }
            for (id, line) in &now {
                if shown.get(id) != Some(line) {
                    println!("{id}: {line}");
                }
            }
            for id in shown.keys().filter(|id| !now.contains_key(*id)) {
                println!("{id}: removed");
            }
            shown = now;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
}
