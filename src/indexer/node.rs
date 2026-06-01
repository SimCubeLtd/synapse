//! Parsing of `package.json` manifests into the dependency and script facts the
//! indexer cares about.
//!
//! We use [`serde_json::Value`] rather than a typed struct because real-world
//! `package.json` files are full of fields we don't model and occasionally
//! malformed entries (null versions, arrays where objects are expected, etc.).
//! Working with the loose value tree lets us pick out exactly what we need and
//! silently skip anything unexpected.
//!
//! Dependencies from `dependencies`, `devDependencies` and `peerDependencies`
//! are merged into one list, each tagged with its `kind`. The output is sorted
//! by `(kind, name)` for dependencies and by `name` for scripts so that
//! re-indexing an unchanged file always produces identical graph facts.

use anyhow::{Context, Result};
use serde_json::Value;

/// Parsed facts extracted from a single `package.json`.
#[derive(Debug, Default)]
pub struct PackageJsonData {
    /// The `"name"` field, if present and a string.
    pub name: Option<String>,
    /// All dependencies across the dependency maps, sorted by `(kind, name)`.
    pub dependencies: Vec<NodeDep>,
    /// `(scriptName, command)` pairs from `"scripts"`, sorted by name.
    pub scripts: Vec<(String, String)>,
}

/// A single npm dependency edge.
#[derive(Debug)]
pub struct NodeDep {
    /// Package name (the key in the dependency map).
    pub name: String,
    /// Version range/specifier string (the value in the dependency map).
    pub version: String,
    /// One of `"dependency"`, `"devDependency"`, or `"peerDependency"`.
    pub kind: String,
}

/// Parse the textual contents of a `package.json` file.
///
/// Extracts the package `name`, merges the three standard dependency maps into a
/// single sorted list, and collects the `scripts` map. Missing, null, or
/// wrongly-typed fields are tolerated and skipped rather than producing an
/// error.
///
/// Returns an error only when the input is not valid JSON at all.
pub fn parse_package_json(text: &str) -> Result<PackageJsonData> {
    let root: Value = serde_json::from_str(text).context("failed to parse package.json")?;

    // "name": only accept an actual string.
    let name = root
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    // Merge the standard dependency maps, each with its associated kind.
    let mut dependencies = Vec::new();
    for (field, kind) in [
        ("dependencies", "dependency"),
        ("devDependencies", "devDependency"),
        ("peerDependencies", "peerDependency"),
    ] {
        collect_deps(&root, field, kind, &mut dependencies);
    }

    // "scripts": object of name -> command string.
    let mut scripts = Vec::new();
    if let Some(obj) = root.get("scripts").and_then(Value::as_object) {
        for (name, val) in obj {
            if let Some(cmd) = val.as_str() {
                scripts.push((name.clone(), cmd.to_string()));
            }
        }
    }

    // Deterministic ordering, independent of JSON-map iteration order.
    dependencies.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    scripts.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(PackageJsonData {
        name,
        dependencies,
        scripts,
    })
}

/// Pull a single dependency map (`field`) out of `root`, pushing each
/// string-valued entry into `out` tagged with `kind`. A non-object or absent
/// field is skipped; non-string values within the object are skipped.
fn collect_deps(root: &Value, field: &str, kind: &str, out: &mut Vec<NodeDep>) {
    let Some(obj) = root.get(field).and_then(Value::as_object) else {
        return;
    };
    for (name, val) in obj {
        if let Some(version) = val.as_str() {
            out.push(NodeDep {
                name: name.clone(),
                version: version.to_string(),
                kind: kind.to_string(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_deps_and_scripts() {
        let json = r#"{
            "name": "my-pkg",
            "dependencies": { "react": "^18.0.0", "axios": "1.2.3" },
            "devDependencies": { "vitest": "^1.0.0" },
            "peerDependencies": { "react": ">=17" },
            "scripts": { "build": "tsc", "test": "vitest" }
        }"#;
        let d = parse_package_json(json).unwrap();
        assert_eq!(d.name.as_deref(), Some("my-pkg"));

        // Sorted by (kind, name): dependency < devDependency < peerDependency.
        let got: Vec<_> = d
            .dependencies
            .iter()
            .map(|x| (x.kind.as_str(), x.name.as_str(), x.version.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("dependency", "axios", "1.2.3"),
                ("dependency", "react", "^18.0.0"),
                ("devDependency", "vitest", "^1.0.0"),
                ("peerDependency", "react", ">=17"),
            ]
        );

        assert_eq!(
            d.scripts,
            vec![
                ("build".to_string(), "tsc".to_string()),
                ("test".to_string(), "vitest".to_string()),
            ]
        );
    }

    #[test]
    fn tolerates_missing_and_wrong_typed_fields() {
        let json = r#"{
            "name": 42,
            "dependencies": null,
            "devDependencies": ["not", "an", "object"],
            "scripts": { "build": 7, "ok": "echo hi" }
        }"#;
        let d = parse_package_json(json).unwrap();
        assert_eq!(d.name, None);
        assert!(d.dependencies.is_empty());
        assert_eq!(d.scripts, vec![("ok".to_string(), "echo hi".to_string())]);
    }

    #[test]
    fn empty_object_is_ok() {
        let d = parse_package_json("{}").unwrap();
        assert_eq!(d.name, None);
        assert!(d.dependencies.is_empty());
        assert!(d.scripts.is_empty());
    }

    #[test]
    fn invalid_json_errors() {
        assert!(parse_package_json("{ not json").is_err());
    }
}
