//! npm's range syntax (node-semver `satisfies` with `includePrerelease`),
//! enough for `peerDependencies`: `||`, space-separated comparators, `=` `<`
//! `<=` `>` `>=`, `^`, `~`, x-ranges (`1.x`, `1.2.*`, `*`) and hyphen ranges.

use semver::{Prerelease, Version};

#[derive(Clone, Copy)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

type Comparator = (Op, Version);

/// A partial version: missing or `x` components are `None`.
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Prerelease,
}

fn parse_partial(text: &str) -> Option<Partial> {
    let text = text.trim_start_matches(['v', '=']).trim();
    if text.is_empty() || text == "*" || text.eq_ignore_ascii_case("x") {
        return Some(Partial {
            major: None,
            minor: None,
            patch: None,
            pre: Prerelease::EMPTY,
        });
    }
    let (core, pre) = match text.split_once('-') {
        Some((core, pre)) => (core, Prerelease::new(pre.split('+').next()?).ok()?),
        None => (text.split('+').next()?, Prerelease::EMPTY),
    };
    let mut parts = core.split('.');
    let mut next = || -> Option<Option<u64>> {
        match parts.next() {
            None => Some(None),
            Some(p) if p == "*" || p.eq_ignore_ascii_case("x") => Some(None),
            Some(p) => p.parse().ok().map(Some),
        }
    };
    let major = next()?;
    let minor = next()?;
    let patch = next()?;
    Some(Partial {
        major,
        minor: major.and(minor),
        patch: major.and(minor).and(patch),
        pre,
    })
}

fn v(major: u64, minor: u64, patch: u64) -> Version {
    Version::new(major, minor, patch)
}

/// `x.y.z-0`: the lowest version of that release, prereleases included.
fn floor(major: u64, minor: u64, patch: u64) -> Version {
    let mut version = v(major, minor, patch);
    version.pre = Prerelease::new("0").unwrap();
    version
}

fn exact(p: &Partial) -> Version {
    let mut version = v(
        p.major.unwrap_or(0),
        p.minor.unwrap_or(0),
        p.patch.unwrap_or(0),
    );
    version.pre = p.pre.clone();
    version
}

/// The comparators of one x-range, `~` or `^` term.
fn desugar(op: &str, p: &Partial) -> Vec<Comparator> {
    let (Some(major), minor, patch) = (p.major, p.minor, p.patch) else {
        // `*` and friends: anything, except `<*` / `>*` which match nothing.
        return match op {
            "<" | ">" => vec![(Op::Lt, v(0, 0, 0)), (Op::Gt, v(0, 0, 0))],
            _ => Vec::new(),
        };
    };
    match op {
        "^" => {
            let upper = match (major, minor, patch) {
                (0, Some(0), Some(patch)) => floor(0, 0, patch + 1),
                (0, Some(minor), _) => floor(0, minor + 1, 0),
                (major, _, _) => floor(major + 1, 0, 0),
            };
            let upper = if major == 0 && minor.is_none() { floor(1, 0, 0) } else { upper };
            vec![(Op::Ge, exact(p)), (Op::Lt, upper)]
        }
        "~" => {
            let upper = match minor {
                Some(minor) => floor(major, minor + 1, 0),
                None => floor(major + 1, 0, 0),
            };
            vec![(Op::Ge, exact(p)), (Op::Lt, upper)]
        }
        "" | "=" => match (minor, patch) {
            (Some(_), Some(_)) => vec![(Op::Eq, exact(p))],
            (Some(minor), None) => vec![(Op::Ge, v(major, minor, 0)), (Op::Lt, floor(major, minor + 1, 0))],
            _ => vec![(Op::Ge, v(major, 0, 0)), (Op::Lt, floor(major + 1, 0, 0))],
        },
        ">=" => vec![(Op::Ge, exact(p))],
        "<" => vec![(Op::Lt, if patch.is_some() { exact(p) } else { floor(major, minor.unwrap_or(0), 0) })],
        ">" => vec![match (minor, patch) {
            (Some(_), Some(_)) => (Op::Gt, exact(p)),
            (Some(minor), None) => (Op::Ge, floor(major, minor + 1, 0)),
            _ => (Op::Ge, floor(major + 1, 0, 0)),
        }],
        _ /* <= */ => vec![match (minor, patch) {
            (Some(_), Some(_)) => (Op::Le, exact(p)),
            (Some(minor), None) => (Op::Lt, floor(major, minor + 1, 0)),
            _ => (Op::Lt, floor(major + 1, 0, 0)),
        }],
    }
}

fn parse_set(text: &str) -> Option<Vec<Comparator>> {
    let text = text.trim();
    if let Some((low, high)) = text.split_once(" - ") {
        let low = parse_partial(low)?;
        let high = parse_partial(high)?;
        let mut out = desugar(
            ">=",
            &Partial {
                pre: low.pre.clone(),
                ..low_or_zero(&low)
            },
        );
        out.extend(desugar("<=", &high));
        return Some(out);
    }
    // Operators may be separated from their version by spaces.
    let mut tokens: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        match tokens.last_mut() {
            Some(last) if matches!(last.as_str(), "<" | "<=" | ">" | ">=" | "=" | "^" | "~") => {
                last.push_str(token)
            }
            _ => tokens.push(token.to_owned()),
        }
    }
    let mut out = Vec::new();
    for token in tokens {
        let op = ["<=", ">=", "<", ">", "=", "^", "~>", "~"]
            .into_iter()
            .find(|op| token.starts_with(op))
            .unwrap_or("");
        let partial = parse_partial(&token[op.len()..])?;
        out.extend(desugar(if op == "~>" { "~" } else { op }, &partial));
    }
    Some(out)
}

fn low_or_zero(p: &Partial) -> Partial {
    Partial {
        major: Some(p.major.unwrap_or(0)),
        minor: Some(p.minor.unwrap_or(0)),
        patch: Some(p.patch.unwrap_or(0)),
        pre: p.pre.clone(),
    }
}

/// Whether `version` satisfies the npm `range`, prereleases included. An
/// unparsable range is not satisfied.
pub fn satisfies(version: &Version, range: &str) -> bool {
    range.split("||").any(|set| {
        parse_set(set).is_some_and(|comparators| {
            comparators.iter().all(|(op, bound)| match op {
                Op::Lt => version < bound,
                Op::Le => version <= bound,
                Op::Gt => version > bound,
                Op::Ge => version >= bound,
                Op::Eq => version == bound,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(version: &str, range: &str) -> bool {
        satisfies(&Version::parse(version).unwrap(), range)
    }

    #[test]
    fn npm_ranges() {
        let cases = [
            ("0.2.0-rc.2", "0.2.0-rc.2", true),
            ("0.2.0-rc.2", "^0.2.0-rc.1", true),
            ("0.2.0-rc.2", "^0.2.0", false),
            ("0.2.0", "^0.2.0-rc.1", true),
            ("0.3.0-rc.1", "^0.2.0", false),
            ("1.4.0", "^1.2", true),
            ("2.0.0-rc.1", "^1.2", false),
            ("1.2.9", "~1.2.3", true),
            ("1.3.0", "~1.2.3", false),
            ("1.9.9", "1.x", true),
            ("2.0.0", "1.x || >=3", false),
            ("3.0.0", "1.x || >=3", true),
            ("1.5.0", "1.2.3 - 2.3", true),
            ("2.4.0", "1.2.3 - 2.3", false),
            ("1.0.0", ">= 1.0.0 < 2", true),
            ("0.0.4", "^0.0.3", false),
            ("5.0.0", "*", true),
            ("1.2.0", "<1.2", false),
            ("1.2.5", "<=1.2", true),
            ("1.3.0", ">1.2", true),
            ("1.2.9", ">1.2", false),
        ];
        for (version, range, expected) in cases {
            assert_eq!(ok(version, range), expected, "{version} in {range}");
        }
    }
}
