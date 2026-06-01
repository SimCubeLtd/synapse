//! Selection: turn a [`PackRequest`] into a ranked, de-duplicated list of files
//! to include in a context pack.
//!
//! Selection is the only place the graph is consulted. We combine graph facts
//! (declared symbols, related-to edges) with cheap, deterministic path
//! heuristics (same-directory neighbours, test/config detection, name
//! substring matches) to gather candidates, tag each with a human-readable
//! reason and a ranking tier (lower = higher priority), then sort stably.
//!
//! Tiers:
//! * 0 — exact matches: changed files, symbol-declaration files, exact paths.
//! * 1 — direct relationships: graph edges, `--path` prefix, query matches.
//! * 2 — neighbours: tests, config/registration files, same-directory files.
//! * 3 — low value: documentation, transitive items.

use crate::config::SynapseConfig;
use crate::graph::GraphStore;
use crate::graph::model::{FileSearchQuery, IndexedFile, RelatedItem, SymbolSearchQuery};
use crate::indexer::languages;
use crate::pack::{PackMode, PackRequest, SelectedFile};
use crate::repo::Repo;
use anyhow::Result;
use std::collections::HashMap;

/// An internal candidate before de-duplication and flag filtering.
struct Candidate {
    path: String,
    reason: String,
    tier: u8,
    /// True when the file was matched directly by an explicit path/query (used
    /// to override the config/lockfile drop rules).
    explicit: bool,
}

/// Gather candidate files for the request, rank them, and return a stable,
/// de-duplicated selection. `estimated_tokens` is left at 0 (filled by
/// `budget::fit`) and every returned file is marked `included = true`.
pub fn select(
    repo: &Repo,
    _config: &SynapseConfig,
    store: &dyn GraphStore,
    req: &PackRequest,
) -> Result<Vec<SelectedFile>> {
    // The set of indexed files is the universe we may emit from.
    let indexed = store.all_files()?;
    let indexed_paths: std::collections::HashSet<&str> =
        indexed.iter().map(|f| f.path.as_str()).collect();

    let mut candidates: Vec<Candidate> = Vec::new();

    match &req.mode {
        PackMode::Changed => {
            for path in crate::git::changed_files(&repo.root) {
                candidates.push(Candidate {
                    path,
                    reason: "changed file".to_string(),
                    tier: 0,
                    explicit: false,
                });
            }
        }
        PackMode::Path(prefix) => {
            collect_path(prefix, &indexed, &mut candidates);
        }
        PackMode::Symbol(symbol) => {
            collect_symbol(store, symbol, req.depth, &mut candidates)?;
        }
        PackMode::Query(query) => {
            collect_query(store, query, &indexed, &mut candidates)?;
        }
    }

    // Neighbours of every seed gathered so far: same-directory files (tier 2),
    // plus tests/config detection is applied during filtering below.
    add_directory_neighbours(&indexed, &mut candidates);

    // Filter to indexed files, apply flag rules, then de-dup and sort.
    let mut best: HashMap<String, Candidate> = HashMap::new();
    for cand in candidates {
        if !indexed_paths.contains(cand.path.as_str()) {
            continue;
        }
        if !keep_candidate(&cand, req) {
            continue;
        }
        match best.get(&cand.path) {
            Some(existing) if existing.tier <= cand.tier => {}
            _ => {
                best.insert(cand.path.clone(), cand);
            }
        }
    }

    let mut out: Vec<SelectedFile> = best
        .into_values()
        .map(|c| SelectedFile {
            path: c.path,
            reason: c.reason,
            tier: c.tier,
            estimated_tokens: 0,
            included: true,
        })
        .collect();

    out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.path.cmp(&b.path)));
    Ok(out)
}

/// Decide whether a candidate survives the flag rules and exclusion heuristics.
fn keep_candidate(cand: &Candidate, req: &PackRequest) -> bool {
    let path = cand.path.as_str();

    // Always exclude generated files.
    if languages::is_generated(path) {
        return false;
    }

    // Lockfiles only survive when explicitly matched by path/query.
    if languages::is_lockfile(path) && !cand.explicit {
        return false;
    }

    // Test files: drop tier-2 tests unless --include-tests. Explicitly matched
    // tests (path/query/symbol) are kept regardless.
    if languages::is_test_file(path) && !cand.explicit && !req.include_tests {
        return false;
    }

    // Config files: drop unless --include-config, except directly-matched ones.
    if languages::is_config_file(path) && !cand.explicit && !req.include_config {
        return false;
    }

    true
}

/// `--path` mode: an exact path match is tier 0; everything under the prefix is
/// tier 1.
fn collect_path(prefix: &str, indexed: &[IndexedFile], out: &mut Vec<Candidate>) {
    let prefix = prefix.trim_end_matches('/');
    for f in indexed {
        if f.path == prefix {
            out.push(Candidate {
                path: f.path.clone(),
                reason: "exact path match".to_string(),
                tier: 0,
                explicit: true,
            });
        } else if path_under_prefix(&f.path, prefix) {
            out.push(Candidate {
                path: f.path.clone(),
                reason: format!("under path `{prefix}`"),
                tier: 1,
                explicit: true,
            });
        }
    }
}

/// True if `path` lives under directory `prefix` (or `prefix` is a path segment
/// boundary of it).
fn path_under_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// `--symbol` mode: the file(s) declaring the symbol are tier 0; graph-related
/// files are tier 1.
fn collect_symbol(
    store: &dyn GraphStore,
    symbol: &str,
    depth: usize,
    out: &mut Vec<Candidate>,
) -> Result<()> {
    // Exact declarations from the symbol index.
    let query = SymbolSearchQuery {
        name: Some(symbol.to_string()),
        ..Default::default()
    };
    for sym in store.symbols_matching(&query)? {
        // Require an exact (case-insensitive) name match for the tier-0 seed.
        if sym.name.eq_ignore_ascii_case(symbol) {
            out.push(Candidate {
                path: sym.file_path.clone(),
                reason: format!("declares `{symbol}`"),
                tier: 0,
                explicit: true,
            });
        }
    }

    // Graph relationships (callers, same module, references, ...).
    for item in store.related_to_symbol(symbol, depth)? {
        let tier = if item.depth == 0 { 0 } else { 1 };
        out.push(Candidate {
            path: item.path,
            reason: item.reason,
            tier,
            explicit: item.depth == 0,
        });
    }
    Ok(())
}

/// `--query` mode: case-insensitive substring matches over file paths and
/// symbol names. All matches are tier 1.
fn collect_query(
    store: &dyn GraphStore,
    query: &str,
    indexed: &[IndexedFile],
    out: &mut Vec<Candidate>,
) -> Result<()> {
    let needle = query.to_ascii_lowercase();

    // Path matches.
    for f in indexed {
        if f.path.to_ascii_lowercase().contains(&needle) {
            out.push(Candidate {
                path: f.path.clone(),
                reason: format!("path matches `{query}`"),
                tier: 1,
                explicit: true,
            });
        }
    }

    // Symbol-name matches -> include the declaring files.
    let sq = SymbolSearchQuery {
        name: Some(query.to_string()),
        ..Default::default()
    };
    for sym in store.symbols_matching(&sq)? {
        out.push(Candidate {
            path: sym.file_path.clone(),
            reason: format!("declares symbol matching `{query}`"),
            tier: 1,
            explicit: true,
        });
    }

    // A FileSearchQuery path match is equivalent to the above path scan but kept
    // for symmetry with stores that index differently.
    let fq = FileSearchQuery {
        path_contains: Some(query.to_string()),
        ..Default::default()
    };
    for f in store.files_matching(&fq)? {
        out.push(Candidate {
            path: f.path,
            reason: format!("path matches `{query}`"),
            tier: 1,
            explicit: true,
        });
    }
    Ok(())
}

/// For each seed candidate gathered so far, add the other files that live in the
/// same directory as tier-2 neighbours.
fn add_directory_neighbours(indexed: &[IndexedFile], out: &mut Vec<Candidate>) {
    // Collect the seed directories first (avoid borrowing `out` while pushing).
    let mut seed_dirs: Vec<String> = out.iter().map(|c| parent_dir(&c.path)).collect();
    seed_dirs.sort();
    seed_dirs.dedup();

    for dir in seed_dirs {
        for f in indexed {
            if parent_dir(&f.path) == dir {
                out.push(Candidate {
                    path: f.path.clone(),
                    reason: "same directory as a seed file".to_string(),
                    tier: 2,
                    explicit: false,
                });
            }
        }
    }
}

/// The parent directory of a repo-relative path (`""` for top-level files).
fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(idx) => path[..idx].to_string(),
        None => String::new(),
    }
}

/// Related files for a symbol seed, combining graph edges with path heuristics.
///
/// Depth conventions: the declaring file is depth 0; same-directory files,
/// likely tests, area config, and name-substring matches are depth 1. `depth`
/// acts as a ceiling — items deeper than it are dropped, but depth-0 items are
/// always kept.
pub fn related_for_symbol(
    _repo: &Repo,
    _config: &SynapseConfig,
    store: &dyn GraphStore,
    symbol: &str,
    depth: usize,
) -> Result<Vec<RelatedItem>> {
    let indexed = store.all_files()?;
    let indexed_paths: std::collections::HashSet<&str> =
        indexed.iter().map(|f| f.path.as_str()).collect();

    let mut items: Vec<RelatedItem> = Vec::new();

    // Declaring files (depth 0) and their directories (seeds for neighbours).
    let mut seed_dirs: Vec<String> = Vec::new();
    let mut seed_files: Vec<String> = Vec::new();
    let sq = SymbolSearchQuery {
        name: Some(symbol.to_string()),
        ..Default::default()
    };
    for sym in store.symbols_matching(&sq)? {
        if sym.name.eq_ignore_ascii_case(symbol) {
            seed_dirs.push(parent_dir(&sym.file_path));
            seed_files.push(sym.file_path.clone());
            items.push(RelatedItem {
                path: sym.file_path.clone(),
                reason: format!("declares `{symbol}`"),
                depth: 0,
            });
        }
    }

    // Project siblings of each declaring file via CONTAINS_FILE (depth 1).
    seed_files.sort();
    seed_files.dedup();
    for sf in &seed_files {
        for item in store.project_siblings(sf)? {
            items.push(item);
        }
    }

    // Type relations: base types, interfaces/traits, subtypes, implementors
    // (via INHERITS/IMPLEMENTS edges).
    for item in store.symbol_type_relations(symbol)? {
        items.push(item);
    }

    // Graph relationships from the store.
    for item in store.related_to_symbol(symbol, depth)? {
        if item.depth == 0 {
            seed_dirs.push(parent_dir(&item.path));
        }
        items.push(item);
    }

    seed_dirs.sort();
    seed_dirs.dedup();

    // Same-directory files, area tests, and area config (all depth 1).
    for dir in &seed_dirs {
        for f in &indexed {
            if parent_dir(&f.path) != *dir {
                continue;
            }
            let reason = if languages::is_test_file(&f.path) {
                "test file in the same directory"
            } else if languages::is_config_file(&f.path) {
                "config/registration file in the area"
            } else {
                "same directory as a declaring file"
            };
            items.push(RelatedItem {
                path: f.path.clone(),
                reason: reason.to_string(),
                depth: 1,
            });
        }
    }

    // Files whose path contains the symbol name (depth 1).
    let needle = symbol.to_ascii_lowercase();
    if !needle.is_empty() {
        for f in &indexed {
            if f.path.to_ascii_lowercase().contains(&needle) {
                let reason = if languages::is_test_file(&f.path) {
                    format!("likely test for `{symbol}`")
                } else {
                    format!("path contains `{symbol}`")
                };
                items.push(RelatedItem {
                    path: f.path.clone(),
                    reason,
                    depth: 1,
                });
            }
        }
    }

    Ok(finalize_related(items, depth, &indexed_paths))
}

/// Related files for a file seed: the seed (depth 0), same-directory files and
/// matching tests (depth 1), combined with graph edges from the store.
pub fn related_for_file(
    _repo: &Repo,
    _config: &SynapseConfig,
    store: &dyn GraphStore,
    file: &str,
    depth: usize,
) -> Result<Vec<RelatedItem>> {
    let indexed = store.all_files()?;
    let indexed_paths: std::collections::HashSet<&str> =
        indexed.iter().map(|f| f.path.as_str()).collect();

    let mut items: Vec<RelatedItem> = Vec::new();

    // The seed itself, if indexed.
    if indexed_paths.contains(file) {
        items.push(RelatedItem {
            path: file.to_string(),
            reason: "seed file".to_string(),
            depth: 0,
        });
    }

    // Graph relationships from the store.
    for item in store.related_to_file(file, depth)? {
        items.push(item);
    }

    // Project siblings via the CONTAINS_FILE graph edge (depth 1).
    for item in store.project_siblings(file)? {
        items.push(item);
    }

    // Same-directory files and matching tests (depth 1).
    let dir = parent_dir(file);
    let stem = file_stem(file);
    for f in &indexed {
        if f.path == file {
            continue;
        }
        if parent_dir(&f.path) == dir {
            let reason = if languages::is_test_file(&f.path) {
                "test file in the same directory"
            } else {
                "same directory as the seed file"
            };
            items.push(RelatedItem {
                path: f.path.clone(),
                reason: reason.to_string(),
                depth: 1,
            });
        } else if !stem.is_empty()
            && languages::is_test_file(&f.path)
            && f.path.to_ascii_lowercase().contains(&stem)
        {
            items.push(RelatedItem {
                path: f.path.clone(),
                reason: "likely test for the seed file".to_string(),
                depth: 1,
            });
        }
    }

    Ok(finalize_related(items, depth, &indexed_paths))
}

/// The lowercase file stem (filename without extension) of a path.
fn file_stem(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.split('.').next().unwrap_or(name);
    stem.to_ascii_lowercase()
}

/// Specificity of a relation reason — higher wins when two items for the same
/// path share a depth. An explicit edge ("references", "inherits", an importer)
/// is more informative than membership heuristics ("same project", "same
/// directory"), so it must survive de-duplication. Without this, in a
/// single-project repo every reference is also a same-project sibling and the
/// generic reason would always mask the specific one.
fn reason_specificity(reason: &str) -> u8 {
    if reason.starts_with("declares") {
        4
    } else if reason.starts_with("references")
        || reason.contains("inherits")
        || reason.contains("implement")
        || reason.contains("base type")
        || reason.contains("subtype")
    {
        3
    } else if reason.contains("test") || reason.starts_with("path contains") {
        2
    } else {
        // "same project", "same directory", "config/registration", ...
        1
    }
}

/// De-duplicate related items by path (keeping the smallest depth, then the
/// most specific reason), drop items deeper than the ceiling (except depth 0),
/// restrict to indexed files, and sort by `(depth, path)`.
fn finalize_related(
    items: Vec<RelatedItem>,
    depth: usize,
    indexed_paths: &std::collections::HashSet<&str>,
) -> Vec<RelatedItem> {
    let mut best: HashMap<String, RelatedItem> = HashMap::new();
    for item in items {
        if !indexed_paths.contains(item.path.as_str()) {
            continue;
        }
        // Depth ceiling: always keep depth 0, drop anything beyond `depth`.
        if item.depth != 0 && item.depth > depth {
            continue;
        }
        match best.get(&item.path) {
            // Keep the existing entry only if it's strictly shallower, or at the
            // same depth with an equal-or-higher specificity reason.
            Some(existing)
                if existing.depth < item.depth
                    || (existing.depth == item.depth
                        && reason_specificity(&existing.reason)
                            >= reason_specificity(&item.reason)) => {}
            _ => {
                best.insert(item.path.clone(), item);
            }
        }
    }

    let mut out: Vec<RelatedItem> = best.into_values().collect();
    out.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.path.cmp(&b.path)));
    out
}
