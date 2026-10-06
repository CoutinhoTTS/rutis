//! `rutis-host dev`: the plugin project in the current directory as a host
//! configuration. A Node project (package.json) or a Python project
//! (pyproject.toml) becomes one row, the plugin itself; `rutis.dev.json`
//! adds what it needs to run (fake services, config, other plugins).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::{HostConfig, NodeRuntime, PythonRuntime, Runtimes};

/// The configuration that runs the plugin project in `dir`, and the id of
/// its row.
pub fn dev_config(dir: &Path) -> Result<(HostConfig, String), String> {
    let dir = dir
        .canonicalize()
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    let (row, runtimes) = if dir.join("package.json").exists() {
        node_row(&dir)?
    } else if dir.join("pyproject.toml").exists() {
        python_row(&dir)?
    } else {
        return Err(format!(
            "{} has neither a package.json nor a pyproject.toml: run rutis-host dev in a plugin project",
            dir.display()
        ));
    };
    let id = row["id"].as_str().unwrap_or("plugin").to_owned();
    let mut config = HostConfig {
        id: "dev".into(),
        runtimes,
        listen: Vec::new(),
        rows: vec![row],
    };
    let extra = dir.join("rutis.dev.json");
    if extra.exists() {
        let extra = HostConfig::read(&extra)?;
        // The dev file may configure the plugin's own row.
        let (own, others): (Vec<Value>, Vec<Value>) = extra
            .rows
            .iter()
            .cloned()
            .partition(|row| row["id"] == id.as_str());
        if let Some(own) = own.into_iter().next() {
            if let Some(object) = config.rows[0].as_object_mut() {
                for (key, value) in own.as_object().into_iter().flatten() {
                    if key != "name" {
                        object.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        config.merge(HostConfig {
            rows: others,
            ..extra
        });
    }
    Ok((config, id))
}

fn node_row(dir: &Path) -> Result<(Value, Runtimes), String> {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("package.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("package.json: {error}"))?;
    let name = manifest["name"].as_str().unwrap_or("plugin");
    // The source while developing: src/index.ts or .js, else what the
    // package exports.
    let entry = [
        "src/index.ts",
        "src/index.mts",
        "src/index.js",
        "src/index.mjs",
    ]
    .iter()
    .map(|path| dir.join(path))
    .find(|path| path.exists())
    .or_else(|| {
        let exported = manifest["exports"]["."]["default"]
            .as_str()
            .or_else(|| manifest["exports"]["."].as_str())
            .or_else(|| manifest["exports"].as_str())
            .or_else(|| manifest["main"].as_str())?;
        Some(dir.join(exported))
    })
    .ok_or("package.json names no entry, and there is no src/index.ts")?;
    let row = json!({ "id": id_of(name), "name": file_url(&entry), "config": {} });
    let runtimes = Runtimes {
        node: Some(NodeRuntime {
            project: dir.to_owned(),
            runtime: None,
        }),
        py: None,
    };
    Ok((row, runtimes))
}

fn python_row(dir: &Path) -> Result<(Value, Runtimes), String> {
    let text =
        std::fs::read_to_string(dir.join("pyproject.toml")).map_err(|error| error.to_string())?;
    // The module of the first entry point of the group rutis.plugins (so it
    // runs from the source, installed or not), else the project name as a
    // module.
    let (id, module) = entry_point(&text)
        .or_else(|| project_name(&text).map(|name| (name.clone(), name.replace('-', "_"))))
        .ok_or("pyproject.toml has no [project] name")?;
    let row = json!({ "id": id_of(&id), "name": format!("py:{module}"), "config": {} });
    // Plugin modules are found in the project, or in src/ for that layout.
    let project = match dir.join("src").is_dir() {
        true => dir.join("src"),
        false => dir.to_owned(),
    };
    let runtimes = Runtimes {
        node: None,
        py: Some(PythonRuntime {
            project,
            python: Some(dir.join(".venv/bin/python")).filter(|venv| venv.exists()),
        }),
    };
    Ok((row, runtimes))
}

/// The first entry under `[project.entry-points."rutis.plugins"]`: its name
/// and its module.
fn entry_point(pyproject: &str) -> Option<(String, String)> {
    let mut inside = false;
    for line in pyproject.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == r#"[project.entry-points."rutis.plugins"]"#;
            continue;
        }
        if inside {
            if let Some((name, value)) = line.split_once('=') {
                let name = name.trim().trim_matches('"');
                let module = value
                    .trim()
                    .trim_matches('"')
                    .split(':')
                    .next()
                    .unwrap_or("");
                if !name.is_empty() && !name.starts_with('#') && !module.is_empty() {
                    return Some((name.to_owned(), module.to_owned()));
                }
            }
        }
    }
    None
}

fn project_name(pyproject: &str) -> Option<String> {
    let mut inside = false;
    for line in pyproject.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[project]";
            continue;
        }
        if let (true, Some(("name", value))) =
            (inside, line.split_once('=').map(|(k, v)| (k.trim(), v)))
        {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// A row id from a package or module name.
fn id_of(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// The files a change in which reloads the plugin: sources, not
/// dependencies or build output.
pub fn sources(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    const SKIP: &[&str] = &[
        "node_modules",
        ".git",
        ".venv",
        "venv",
        "target",
        "dist",
        "build",
        "__pycache__",
        ".pytest_cache",
    ];
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && name != ".env" || SKIP.contains(&name.as_ref()) {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(path),
                Ok(kind) if kind.is_file() => {
                    if let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) {
                        found.push((path, modified));
                    }
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_project_runs_its_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "@acme/weather-plugin" }"#,
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/index.ts"), "").unwrap();
        std::fs::write(
            dir.path().join("rutis.dev.json"),
            r#"{ "rows": [{ "id": "weather-plugin", "config": { "city": "Oslo" } }, { "id": "llm", "name": "./fake.mjs" }] }"#,
        )
        .unwrap();
        let (config, id) = dev_config(dir.path()).unwrap();
        assert_eq!(id, "weather-plugin");
        assert!(config.rows[0]["name"]
            .as_str()
            .unwrap()
            .ends_with("/src/index.ts"));
        assert_eq!(config.rows[0]["config"]["city"], "Oslo");
        assert_eq!(config.rows[1]["id"], "llm");
        assert!(config.runtimes.node.is_some());
    }

    #[test]
    fn a_python_project_runs_its_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"weather-plugin\"\n\n[project.entry-points.\"rutis.plugins\"]\nweather = \"weather_plugin\"\n",
        )
        .unwrap();
        let (config, id) = dev_config(dir.path()).unwrap();
        assert_eq!(
            (id.as_str(), &config.rows[0]["name"]),
            ("weather", &json!("py:weather_plugin"))
        );
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"weather-plugin\"\n",
        )
        .unwrap();
        assert_eq!(
            dev_config(dir.path()).unwrap().0.rows[0]["name"],
            "py:weather_plugin"
        );
    }
}
