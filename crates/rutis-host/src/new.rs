//! `rutis-host new <name> --lang node|python`: a plugin project from a
//! template, with a working plugin, its test, a dev configuration and a
//! publish workflow.

use std::path::Path;

/// The release train this program belongs to: the projects depend on the
/// SDK of the same version.
const VERSION: &str = env!("CARGO_PKG_VERSION");

const NODE: &[(&str, &str)] = &[
    (
        "package.json",
        include_str!("../templates/node/package.json"),
    ),
    (
        "tsconfig.json",
        include_str!("../templates/node/tsconfig.json"),
    ),
    (
        "src/index.ts",
        include_str!("../templates/node/src/index.ts"),
    ),
    (
        "test/index.test.ts",
        include_str!("../templates/node/test/index.test.ts"),
    ),
    (
        "rutis.dev.json",
        include_str!("../templates/node/rutis.dev.json"),
    ),
    ("README.md", include_str!("../templates/node/README.md")),
    (".gitignore", include_str!("../templates/node/gitignore")),
    (
        ".github/workflows/publish.yml",
        include_str!("../templates/node/.github/workflows/publish.yml"),
    ),
];

const PYTHON: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        include_str!("../templates/python/pyproject.toml"),
    ),
    (
        "src/__module__/__init__.py",
        include_str!("../templates/python/src/__module__/__init__.py"),
    ),
    (
        "tests/test_plugin.py",
        include_str!("../templates/python/tests/test_plugin.py"),
    ),
    (
        "rutis.dev.json",
        include_str!("../templates/python/rutis.dev.json"),
    ),
    ("README.md", include_str!("../templates/python/README.md")),
    (".gitignore", include_str!("../templates/python/gitignore")),
    (
        ".github/workflows/publish.yml",
        include_str!("../templates/python/.github/workflows/publish.yml"),
    ),
];

/// Create the project `name` in `parent`/`name`.
pub fn create(parent: &Path, name: &str, lang: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && name.starts_with(|c: char| c.is_ascii_lowercase());
    if !valid {
        return Err(format!(
            "{name:?}: a plugin name is lower case letters, digits and -, starting with a letter"
        ));
    }
    let files = match lang {
        "node" | "ts" | "typescript" | "js" => NODE,
        "python" | "py" => PYTHON,
        other => return Err(format!("{other:?}: the language is node or python")),
    };
    let dir = parent.join(name);
    if dir.exists() {
        return Err(format!("{} already exists", dir.display()));
    }
    let module = name.replace('-', "_");
    let next = next_minor(VERSION);
    for (path, text) in files {
        let path = path.replace("__module__", &module);
        let text = text
            .replace("{{name}}", name)
            .replace("{{id}}", name)
            .replace("{{module}}", &module)
            .replace("{{version}}", VERSION)
            .replace("{{next}}", &next);
        let target = dir.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        std::fs::write(&target, text).map_err(|error| format!("{}: {error}", target.display()))?;
    }
    Ok(())
}

/// The first version past this one's compatible range: `0.2.x` → `0.3`,
/// `1.4.0` → `2`.
fn next_minor(version: &str) -> String {
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    match major {
        0 => format!("0.{}", minor + 1),
        major => format!("{}", major + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_created_with_their_names_filled_in() {
        let dir = tempfile::tempdir().unwrap();
        create(dir.path(), "weather-plugin", "python").unwrap();
        let pyproject =
            std::fs::read_to_string(dir.path().join("weather-plugin/pyproject.toml")).unwrap();
        assert!(
            pyproject.contains("weather-plugin = \"weather_plugin\""),
            "{pyproject}"
        );
        assert!(dir
            .path()
            .join("weather-plugin/src/weather_plugin/__init__.py")
            .exists());
        create(dir.path(), "greeter", "node").unwrap();
        let package = std::fs::read_to_string(dir.path().join("greeter/package.json")).unwrap();
        assert!(package.contains(&format!("\"@arcships/rutis\": \"^{VERSION}\"")));
        assert!(
            create(dir.path(), "greeter", "node").is_err(),
            "an existing directory is kept"
        );
        assert!(create(dir.path(), "Bad_Name", "node").is_err());
        assert_eq!(next_minor("0.2.0"), "0.3");
        assert_eq!(next_minor("1.4.2"), "2");
    }
}
