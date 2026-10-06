//! `rutis.json`: what a host runs.
//!
//! ```json
//! {
//!   "id": "main",
//!   "runtimes": {
//!     "node": { "project": "." },
//!     "py": { "project": ".", "python": ".venv/bin/python" }
//!   },
//!   "listen": [{ "name": "public", "address": "0.0.0.0:7443", "cert": "server.pem", "key": "server.key" }],
//!   "rows": [
//!     { "id": "weather", "name": "weather-plugin", "config": { "city": "Oslo" } },
//!     { "id": "office", "name": "rutis-bridge/peer", "config": { "peer": "office", "dial": "wss://office.example.com/rutis" } }
//!   ]
//! }
//! ```
//!
//! Paths are relative to the file. Rows are rutis-loader rows; every service
//! name crosses between languages and nodes by name.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// This node's endpoint id, which linked peers see.
    #[serde(default = "default_id")]
    pub id: String,
    #[serde(default)]
    pub runtimes: Runtimes,
    /// WebSocket listeners links can register on (`"listen": "<name>"` in a
    /// `rutis-bridge/peer` row).
    #[serde(default)]
    pub listen: Vec<Listener>,
    #[serde(default)]
    pub rows: Vec<Value>,
}

fn default_id() -> String {
    "host".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtimes {
    pub node: Option<NodeRuntime>,
    pub py: Option<PythonRuntime>,
    /// Runtimes on other machines, reached through a `rutis-bridge/peer` row
    /// with `"runtime": "<name>"`.
    #[serde(default)]
    pub remote: Vec<RemoteRuntime>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRuntime {
    pub name: String,
    /// `python`: rows `<name>:<module>`; `node`: npm names, resolved there
    /// when no runtime before it has them.
    pub language: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRuntime {
    /// The Node project plugins are installed in (its package.json).
    #[serde(default = "here")]
    pub project: PathBuf,
    /// The `@arcships/rutis-runtime` package; by default the project's.
    #[serde(default)]
    pub runtime: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonRuntime {
    /// Where plugin modules that are not installed are found.
    #[serde(default = "here")]
    pub project: PathBuf,
    /// The interpreter; by default `$VIRTUAL_ENV/bin/python`, then the
    /// project's `.venv/bin/python`, then `python3`.
    #[serde(default)]
    pub python: Option<PathBuf>,
}

fn here() -> PathBuf {
    PathBuf::from(".")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub name: String,
    pub address: SocketAddr,
    /// TLS certificate and key (PEM); `RUTIS_CERT` / `RUTIS_KEY` otherwise.
    #[serde(default)]
    pub cert: Option<PathBuf>,
    #[serde(default)]
    pub key: Option<PathBuf>,
}

impl HostConfig {
    /// Read `path`; relative paths in it become relative to its directory.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut config: Self =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        config.rebase(base);
        Ok(config)
    }

    pub fn rebase(&mut self, base: &Path) {
        let at = |path: &mut PathBuf| {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        };
        if let Some(node) = &mut self.runtimes.node {
            at(&mut node.project);
            if let Some(runtime) = &mut node.runtime {
                at(runtime);
            }
        }
        if let Some(py) = &mut self.runtimes.py {
            at(&mut py.project);
            // A bare program name is looked up on PATH, not under the base.
            if let Some(python) = &mut py.python {
                if python.components().count() > 1 {
                    at(python);
                }
            }
        }
        for listener in &mut self.listen {
            for path in [&mut listener.cert, &mut listener.key]
                .into_iter()
                .flatten()
            {
                at(path);
            }
        }
        // A plugin file named relative to the configuration.
        for row in &mut self.rows {
            if let Some(name) = row["name"].as_str() {
                if name.starts_with("./") || name.starts_with("../") {
                    row["name"] = json!(format!("file://{}", base.join(name).display()));
                }
            }
        }
    }

    /// Add `other`'s runtimes (where this has none), listeners and rows.
    pub fn merge(&mut self, other: HostConfig) {
        if self.runtimes.node.is_none() {
            self.runtimes.node = other.runtimes.node;
        }
        if self.runtimes.py.is_none() {
            self.runtimes.py = other.runtimes.py;
        }
        self.runtimes.remote.extend(other.runtimes.remote);
        self.listen.extend(other.listen);
        self.rows.extend(other.rows);
    }

    /// The rows as the loader takes them: `rutis-bridge/peer` rows get the
    /// transport their address needs and this host's identity.
    pub fn rows(&self) -> Vec<Value> {
        self.rows
            .iter()
            .map(|row| {
                let mut row = row.clone();
                if row["name"] == "rutis-bridge/peer" {
                    let config = &mut row["config"];
                    if config.is_null() {
                        *config = json!({});
                    }
                    if config.get("transport").is_none() {
                        let dial = config["dial"].as_str().unwrap_or("");
                        let network = dial.starts_with("ws://")
                            || dial.starts_with("wss://")
                            || config.get("listen").is_some();
                        config["transport"] = json!(if network { "websocket" } else { "local" });
                    }
                    if config.get("identity").is_none() {
                        config["identity"] = json!("host");
                    }
                }
                row
            })
            .collect()
    }

    /// The peers rows link to: (peer, dials).
    pub fn peers(&self) -> Vec<(String, bool)> {
        self.rows
            .iter()
            .filter(|row| row["name"] == "rutis-bridge/peer")
            .filter_map(|row| {
                let peer = row["config"]["peer"].as_str()?.to_owned();
                Some((peer, row["config"].get("dial").is_some()))
            })
            .collect()
    }
}

/// The token presented to, or accepted from, `peer`:
/// `RUTIS_TOKEN_<PEER>` (upper case, `-` as `_`), else `RUTIS_TOKEN`.
pub fn token(peer: &str) -> Option<String> {
    let specific = format!("RUTIS_TOKEN_{}", peer.to_uppercase().replace('-', "_"));
    std::env::var(specific)
        .or_else(|_| std::env::var("RUTIS_TOKEN"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_rows_get_their_transport_and_identity() {
        let config: HostConfig = serde_json::from_value(json!({
            "rows": [
                { "id": "a", "name": "rutis-bridge/peer", "config": { "peer": "a", "dial": "wss://a/rutis" } },
                { "id": "b", "name": "rutis-bridge/peer", "config": { "peer": "b", "listen": "public" } },
                { "id": "c", "name": "rutis-bridge/peer", "config": { "peer": "c", "dial": "/tmp/c.sock" } },
                { "id": "w", "name": "weather" }
            ]
        }))
        .unwrap();
        let rows = config.rows();
        assert_eq!(rows[0]["config"]["transport"], "websocket");
        assert_eq!(rows[1]["config"]["transport"], "websocket");
        assert_eq!(rows[2]["config"]["transport"], "local");
        assert_eq!(rows[0]["config"]["identity"], "host");
        assert_eq!(rows[3], json!({ "id": "w", "name": "weather" }));
        assert_eq!(
            config.peers(),
            vec![("a".into(), true), ("b".into(), false), ("c".into(), true)]
        );
    }

    #[test]
    fn paths_are_relative_to_the_file() {
        let mut config: HostConfig = serde_json::from_value(json!({
            "runtimes": { "node": {}, "py": { "python": ".venv/bin/python" } }
        }))
        .unwrap();
        config.rows = vec![
            json!({ "id": "llm", "name": "./dev/fake.ts" }),
            json!({ "id": "w", "name": "weather" }),
        ];
        config.rebase(Path::new("/srv/app"));
        assert_eq!(config.rows[0]["name"], "file:///srv/app/./dev/fake.ts");
        assert_eq!(config.rows[1]["name"], "weather");
        assert_eq!(
            config.runtimes.node.unwrap().project,
            Path::new("/srv/app/.")
        );
        assert_eq!(
            config.runtimes.py.unwrap().python.unwrap(),
            Path::new("/srv/app/.venv/bin/python")
        );
        assert!(serde_json::from_value::<HostConfig>(json!({ "unknown": 1 })).is_err());
    }
}
