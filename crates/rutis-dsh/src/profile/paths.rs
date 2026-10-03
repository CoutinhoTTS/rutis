//! Node's POSIX path rules and dsh's home directory, as the JavaScript side
//! computes them.

use std::path::{Path, PathBuf};

/// `path.posix.normalize` of a non-empty path.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(last) if *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    if out.is_empty() {
        out = if absolute { "/".into() } else { ".".into() };
    }
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// `path.join(...segments)`.
pub fn join<S: AsRef<str>>(segments: &[S]) -> String {
    let joined = segments
        .iter()
        .map(AsRef::as_ref)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    normalize(&joined)
}

/// `path.resolve(...segments)` relative to `cwd`.
pub fn resolve<S: AsRef<str>>(cwd: &str, segments: &[S]) -> String {
    let mut resolved = String::new();
    for segment in segments.iter().rev().map(AsRef::as_ref) {
        if segment.is_empty() {
            continue;
        }
        resolved = if resolved.is_empty() {
            segment.to_owned()
        } else {
            format!("{segment}/{resolved}")
        };
        if segment.starts_with('/') {
            break;
        }
    }
    if !resolved.starts_with('/') {
        resolved = if resolved.is_empty() {
            cwd.to_owned()
        } else {
            format!("{cwd}/{resolved}")
        };
    }
    let normalized = normalize(&resolved);
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_owned()
    } else {
        normalized
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `resolveDshHome()`: `$DSH_HOME` when set and not blank, else `~/.dsh`.
pub fn dsh_home() -> PathBuf {
    let configured = std::env::var("DSH_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let path = match configured {
        Some(path) if path == "~" => home(),
        Some(path) if path.starts_with("~/") => home().join(&path[2..]),
        Some(path) => PathBuf::from(path),
        None => home().join(".dsh"),
    };
    absolute(&path)
}

/// `path.resolve(p)` against the process working directory.
pub fn absolute(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    PathBuf::from(resolve(
        &cwd.to_string_lossy(),
        &[path.to_string_lossy().as_ref()],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_path_rules() {
        assert_eq!(join(&["/a", "b", "../c", ""]), "/a/c");
        assert_eq!(join(&["a", "./b/"]), "a/b/");
        assert_eq!(join::<&str>(&[]), ".");
        assert_eq!(
            resolve("/cwd", &["", "dependencies", "node"]),
            "/cwd/dependencies/node"
        );
        assert_eq!(resolve("/cwd", &["/root", "..", "x"]), "/x");
        assert_eq!(resolve("/cwd", &["a", "/b", "c/"]), "/b/c");
        assert_eq!(resolve("/cwd", &[""]), "/cwd");
        assert_eq!(normalize("/../a/./b/.."), "/a");
        assert_eq!(normalize("../../a"), "../../a");
    }
}
