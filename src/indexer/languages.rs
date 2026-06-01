//! Language detection, test-file heuristics, and per-language tree-sitter query
//! definitions. This module owns the mapping from file extension to
//! [`Language`] and the query strings consumed by [`super::tree_sitter`].

use crate::graph::model::Language;
use std::path::Path;

/// Detect a [`Language`] from a repo-relative path's extension/name.
///
/// Returns [`Language::Other`] for indexable-but-not-parsed files (config,
/// data, manifests) and `None` for paths we never index by extension.
pub fn detect(path: &str) -> Language {
    let lower = path.to_ascii_lowercase();
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "cs" => Language::CSharp,
        "rs" => Language::Rust,
        "py" | "pyi" => Language::Python,
        "go" => Language::Go,
        "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
        "ts" | "tsx" => Language::TypeScript,
        "svelte" => Language::Svelte,
        "sh" | "bash" | "zsh" => Language::Bash,
        "yml" | "yaml" => Language::Yaml,
        "json" => Language::Json,
        "md" | "markdown" | "mdx" => Language::Markdown,
        _ => Language::Other,
    }
}

/// SvelteKit filesystem-routing role for a `.svelte`/route file, if recognised.
///
/// SvelteKit gives special meaning to files named `+page`, `+layout`, `+server`,
/// `+error`, and their `.server`/`.ts` variants. We surface the role as a short
/// label used in the symbol's full name / visibility so route entry points are
/// easy to spot in `symbols`/`pack` output.
pub fn sveltekit_route_role(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let stem = name.split('.').next().unwrap_or(&name);
    match stem {
        "+page" => Some("page"),
        "+layout" => Some("layout"),
        "+server" => Some("endpoint"),
        "+error" => Some("error"),
        _ => None,
    }
}

/// True if the file extension corresponds to a `.tsx`/`.jsx` (JSX-capable) file.
pub fn is_jsx(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".tsx") || l.ends_with(".jsx")
}

/// Heuristic: does this path look like a test file?
pub fn is_test_file(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    let name = l.rsplit('/').next().unwrap_or(&l);
    l.contains("/tests/")
        || l.contains("/test/")
        || l.contains("__tests__/")
        || name.starts_with("test_")
        || name.ends_with("tests.cs")
        || name.ends_with("test.cs")
        || name.ends_with("_test.go")
        || name.ends_with("_test.py")
        || name.ends_with(".test.rs")
        || name.ends_with(".test.ts")
        || name.ends_with(".spec.ts")
        || name.ends_with(".test.tsx")
        || name.ends_with(".spec.tsx")
        || name.ends_with(".test.js")
        || name.ends_with(".spec.js")
        || name.ends_with(".test.jsx")
        || name.ends_with(".spec.jsx")
        || name.contains("test") && name.ends_with(".rs")
}

/// Heuristic: does this path look like a config/registration/wiring file?
pub fn is_config_file(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".yml")
        || l.ends_with(".yaml")
        || l.ends_with(".json")
        || l.ends_with(".toml")
        || l.ends_with(".csproj")
        || l.ends_with(".props")
        || l.ends_with(".targets")
        || l.ends_with("package.json")
}

/// Heuristic: generated/low-value file that should be down-ranked or excluded.
pub fn is_generated(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".designer.cs")
        || l.ends_with(".g.cs")
        || l.ends_with(".generated.cs")
        || l.ends_with(".min.js")
        || l.ends_with(".bundle.js")
        || l.ends_with(".map")
}

/// Heuristic: lockfile that should only be included on explicit request.
pub fn is_lockfile(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    let name = l.rsplit('/').next().unwrap_or(&l);
    name == "package-lock.json"
        || name == "yarn.lock"
        || name == "pnpm-lock.yaml"
        || name == "cargo.lock"
}
