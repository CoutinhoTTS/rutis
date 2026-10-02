//! JSON views of the runtime: diagnostics, loader rows and bus events.

use rutis::{FiberStatusChanged, PluginId, RuntimeDiagnostics, ServiceChange, ServiceChanged};
use rutis_loader::{EntryInfo, EntryStatus, LoaderChanged, PendingEditDropped};
use serde_json::{json, Value};

fn id(plugin: PluginId) -> Value {
    json!(plugin.0)
}

/// Every live fiber, service binding and event backlog.
pub fn diagnostics(d: &RuntimeDiagnostics) -> Value {
    json!({
        "shuttingDown": d.shutting_down,
        "plugins": d.plugins.iter().map(|p| json!({
            "id": id(p.id),
            "instance": p.instance.to_string(),
            "parent": p.parent.map(id),
            "name": p.name,
            "state": format!("{:?}", p.state),
            "generation": p.generation,
            "error": p.error.as_ref().map(|e| e.to_string()),
            "injects": p.injects.iter().map(|i| json!({
                "key": i.key.describe(),
                "scope": i.scope,
                "status": i.status.to_string(),
            })).collect::<Vec<_>>(),
            "resolved": p.resolved_dependencies.iter().map(|r| json!({
                "key": r.key.describe(),
                "scope": r.scope,
                "provider": id(r.provider),
                "generation": r.generation,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "bindings": d.bindings.iter().map(|b| json!({
            "key": b.key.describe(),
            "scope": b.scope,
            "provider": id(b.provider),
            "generation": b.generation,
            "removing": b.removing,
        })).collect::<Vec<_>>(),
        "eventBacklogs": d.event_backlogs.iter().map(|b| json!({
            "key": b.key.describe(),
            "pending": b.pending,
            "oldestMs": b.oldest.as_millis() as u64,
        })).collect::<Vec<_>>(),
    })
}

/// One loader row. `dev` marks rows the channel loaded.
pub fn entry(e: &EntryInfo, dev: bool) -> Value {
    let (status, state, error) = match &e.status {
        EntryStatus::Disabled => ("disabled", None, None),
        EntryStatus::Inactive => ("inactive", None, None),
        EntryStatus::Unresolved(error) => ("unresolved", None, Some(error.to_string())),
        EntryStatus::Running(s) => (
            "running",
            Some(format!("{:?}", s.state)),
            s.error.as_ref().map(|e| e.to_string()),
        ),
    };
    json!({
        "id": e.id,
        "name": e.options.get("name"),
        "parent": e.parent,
        "status": status,
        "state": state,
        "error": error,
        "rejected": e.rejected.as_ref().map(|r| r.to_string()),
        "plugin": e.plugin.map(id),
        "dev": dev,
        "meta": e.meta,
        "hasSchema": e.schema.is_some(),
    })
}

pub fn fiber_status(e: &FiberStatusChanged) -> Value {
    json!({
        "event": "fiber",
        "plugin": id(e.plugin_id),
        "seq": e.seq,
        "generation": e.generation,
        "from": format!("{:?}", e.from),
        "to": format!("{:?}", e.to),
    })
}

pub fn service(e: &ServiceChanged) -> Value {
    json!({
        "event": "service",
        "key": e.key.describe(),
        "scope": e.scope,
        "provider": id(e.provider),
        "generation": e.generation,
        "change": match e.change {
            ServiceChange::Provided => "provided",
            ServiceChange::Removed => "removed",
            _ => "other",
        },
    })
}

pub fn loader(e: &LoaderChanged) -> Value {
    let change = match e {
        LoaderChanged::Reconciled => json!("reconciled"),
        LoaderChanged::Edited(edit) => json!({ "edited": edit }),
        LoaderChanged::Reloaded(id) => json!({ "reloaded": id }),
        LoaderChanged::Overlay(name) => json!({ "overlay": name }),
        _ => json!("other"),
    };
    json!({ "event": "loader", "change": change })
}

pub fn dropped(e: &PendingEditDropped) -> Value {
    json!({ "event": "pendingEditDropped", "edit": e.edit, "error": e.error.to_string() })
}
