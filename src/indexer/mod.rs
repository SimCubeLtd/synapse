//! The indexer: turns repository files into graph facts.
//!
//! Orchestration lives here; per-language extraction lives in submodules. The
//! flow is: enumerate candidates -> hash -> skip unchanged -> parse symbols ->
//! upsert file + symbols -> parse manifests (.csproj / package.json) into
//! projects/packages/edges -> remove deleted files.

pub mod dotnet;
pub mod languages;
pub mod node;
pub mod tree_sitter;

use crate::config::SynapseConfig;
use crate::git;
use crate::graph::GraphStore;
use crate::graph::model::{IndexedFile, IndexedPackage, IndexedProject, Language};
use crate::repo::Repo;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

/// Outcome counts from an indexing run.
#[derive(Debug, Default, Clone)]
pub struct IndexOutcome {
    pub files_indexed: usize,
    pub files_skipped_unchanged: usize,
    pub files_removed: usize,
    pub symbols: usize,
    pub edges: usize,
}

/// Deterministic id for a file from its repo-relative path.
pub fn file_id(path: &str) -> String {
    format!("file:{path}")
}

/// Deterministic id for a symbol.
pub fn symbol_id(path: &str, kind: &str, name: &str, start_line: u32) -> String {
    format!("sym:{path}#{kind}#{name}#{start_line}")
}

/// Deterministic id for a project from its manifest path.
pub fn project_id(path: &str) -> String {
    format!("proj:{path}")
}

/// Deterministic id for a package from ecosystem + name.
pub fn package_id(ecosystem: &str, name: &str) -> String {
    format!("pkg:{ecosystem}:{name}")
}

/// A live snapshot of indexing progress, passed to the [`ProgressFn`].
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexProgress {
    /// Files visited so far (1-based as indexing proceeds).
    pub processed: usize,
    /// Total candidate files.
    pub total: usize,
    /// Files (re)indexed so far this run.
    pub files_indexed: usize,
    /// Symbols extracted so far.
    pub symbols: usize,
    /// Projects discovered so far (from manifests parsed up to this point).
    pub projects: usize,
    /// Packages discovered so far.
    pub packages: usize,
    /// The current post-loop phase, when indexing has moved past the per-file
    /// scan into edge resolution (e.g. "resolving references"). `None` during
    /// the file scan. Lets the UI show what the otherwise-frozen bar is doing.
    pub phase: Option<&'static str>,
}

/// A progress observer invoked once per candidate file as indexing proceeds.
///
/// Receives the current file path and a live [`IndexProgress`] snapshot. Kept as
/// a plain callback so the indexer stays UI-agnostic — the binary wires this to
/// a progress bar; tests can ignore it.
pub type ProgressFn<'a> = dyn Fn(&str, &IndexProgress) + 'a;

/// Index the repository into `store`.
///
/// * `force` re-indexes every file regardless of hash.
/// * `changed_only` restricts to files git reports as changed.
/// * `progress` is invoked per file when `Some` (for a CLI progress bar).
pub fn index_repo(
    repo: &Repo,
    config: &SynapseConfig,
    store: &dyn GraphStore,
    force: bool,
    changed_only: bool,
    now: &str,
    progress: Option<&ProgressFn<'_>>,
) -> Result<IndexOutcome> {
    store.initialize_schema()?;

    let mut outcome = IndexOutcome::default();

    let candidates = repo.candidate_files(config)?;
    let candidate_set: HashSet<&str> = candidates.iter().map(|s| s.as_str()).collect();

    // Project manifests across the whole candidate set, sorted by descending
    // directory depth so the *nearest* owning manifest for a file is found
    // first. Computed up front (independent of --changed) so file ownership is
    // stable regardless of which files were re-indexed this run.
    let mut manifests: Vec<String> = candidates
        .iter()
        .filter(|p| p.ends_with(".csproj") || p.ends_with("package.json"))
        .cloned()
        .collect();
    manifests.sort_by(|a, b| {
        let depth = |p: &str| p.matches('/').count();
        depth(b).cmp(&depth(a)).then_with(|| b.cmp(a))
    });

    // Central Package Management: parse every Directory.Packages.props and
    // Directory.Build.props so .csproj PackageReferences without an inline
    // version can be resolved against the nearest central version pin.
    let central = CentralVersions::collect(repo, &candidates);

    // Existing files in the graph, for stale-skip and deletion detection.
    let existing: Vec<IndexedFile> = store.all_files()?;
    let tracked = git::tracked_files(&repo.root);

    let changed: HashSet<String> = if changed_only {
        git::changed_files(&repo.root).into_iter().collect()
    } else {
        HashSet::new()
    };

    // Imports collected per file during the main loop, resolved to packages in a
    // second pass once every manifest has contributed its package nodes.
    let mut pending_imports: Vec<(String, Vec<String>, Language)> = Vec::new();
    // Supertype relationships collected per file, resolved to INHERITS/IMPLEMENTS
    // edges in a second pass once every symbol exists.
    let mut pending_supertypes: Vec<(String, Vec<tree_sitter::Supertype>)> = Vec::new();
    // Reference relationships collected per file, resolved to REFERENCES edges
    // in a second pass once every symbol (the link targets) exists.
    let mut pending_references: Vec<(String, Vec<tree_sitter::Reference>)> = Vec::new();

    // Live tally for the progress display (projects aren't in `outcome`, which
    // only tracks files/symbols/edges). Package counts need post-pass dedup, so
    // they're shown in the final summary rather than live.
    let mut projects_seen = 0usize;

    let total = candidates.len();
    for (i, rel) in candidates.iter().enumerate() {
        if let Some(cb) = progress {
            let snap = IndexProgress {
                processed: i + 1,
                total,
                files_indexed: outcome.files_indexed,
                symbols: outcome.symbols,
                projects: projects_seen,
                packages: 0,
                phase: None,
            };
            cb(rel, &snap);
        }
        if changed_only && !changed.contains(rel) {
            // Skip files git didn't flag as changed.
            outcome.files_skipped_unchanged += 1;
            continue;
        }

        let abs = repo.root.join(rel);
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let size = bytes.len() as u64;

        // Skip unchanged files (unless forced).
        if !force
            && let Some(prev) = existing.iter().find(|f| &f.path == rel)
            && prev.hash == hash
        {
            outcome.files_skipped_unchanged += 1;
            continue;
        }

        // Changed/new: clear old symbols then re-upsert.
        store.remove_file(rel)?;

        let language = languages::detect(rel);
        let fid = file_id(rel);
        let file = IndexedFile {
            id: fid.clone(),
            path: rel.clone(),
            language,
            hash,
            size_bytes: size,
            tracked: tracked.contains(rel),
            last_indexed_at: now.to_string(),
        };
        store.upsert_file(file)?;
        outcome.files_indexed += 1;

        // Symbol extraction for supported languages.
        if language != Language::Other && language_enabled(config, language) {
            let text = String::from_utf8_lossy(&bytes);
            let symbols = tree_sitter::extract(rel, language, &text).unwrap_or_default();
            for sym in symbols {
                let sid = sym.id.clone();
                store.upsert_symbol(sym)?;
                store.link_file_declares_symbol(&fid, &sid)?;
                outcome.symbols += 1;
            }
            // Collect imports (JS/TS, C#) for later file -> package resolution.
            if matches!(
                language,
                Language::JavaScript | Language::TypeScript | Language::CSharp
            ) {
                let imports = tree_sitter::extract_imports(rel, language, &text);
                if !imports.is_empty() {
                    pending_imports.push((fid.clone(), imports, language));
                }
            }

            // Collect supertype relationships for later INHERITS/IMPLEMENTS edges.
            let supers = tree_sitter::extract_supertypes(rel, language, &text);
            if !supers.is_empty() {
                pending_supertypes.push((rel.clone(), supers));
            }

            // Collect usage references for later REFERENCES edges.
            let refs = tree_sitter::extract_references(rel, language, &text);
            if !refs.is_empty() {
                pending_references.push((rel.clone(), refs));
            }
        }

        // Manifest parsing -> projects/packages/edges.
        if rel.ends_with(".csproj") {
            outcome.edges += index_csproj(rel, &abs, store, &central)?;
            projects_seen += 1;
        } else if rel.ends_with("package.json") {
            outcome.edges += index_package_json(rel, &abs, store)?;
            projects_seen += 1;
        }
    }

    // Post-loop edge-resolution passes. These run after the per-file scan (so
    // their link targets all exist) and can be the bulk of wall-clock on large
    // repos, so each reports a phase to the progress UI — otherwise the bar
    // sits frozen at N/N and looks hung. `report_phase` reuses the same
    // callback as the file scan, emitting a snapshot tagged with the phase.
    // It's a fn (not a closure) taking the live counts so it doesn't hold a
    // borrow of `outcome` across the mutations below.
    let report_phase = |phase: &'static str, files: usize, symbols: usize| {
        if let Some(cb) = progress {
            cb(
                "",
                &IndexProgress {
                    processed: total,
                    total,
                    files_indexed: files,
                    symbols,
                    projects: projects_seen,
                    packages: 0,
                    phase: Some(phase),
                },
            );
        }
    };

    // Resolve collected imports to known packages -> IMPORTS_PACKAGE edges.
    // Done after the main loop so every manifest has registered its packages.
    if !pending_imports.is_empty() {
        report_phase("resolving imports", outcome.files_indexed, outcome.symbols);
        outcome.edges += resolve_imports(store, &pending_imports)?;
    }

    // Resolve supertype relationships -> INHERITS/IMPLEMENTS edges. Done after
    // the main loop so all symbols (the link targets) exist.
    if !pending_supertypes.is_empty() {
        report_phase(
            "resolving type relationships",
            outcome.files_indexed,
            outcome.symbols,
        );
        outcome.edges += resolve_supertypes(store, &pending_supertypes)?;
    }

    // Resolve usage references -> REFERENCES edges. Done after the main loop so
    // all declarations (the link targets) exist — references are cross-file.
    if !pending_references.is_empty() {
        report_phase(
            "resolving references",
            outcome.files_indexed,
            outcome.symbols,
        );
        outcome.edges += resolve_references(store, &pending_references)?;
    }

    // Associate every indexed file with its nearest owning project manifest,
    // creating CONTAINS_FILE edges. We link against the full candidate set (not
    // just files touched this run) so ownership is complete after any index.
    // Batched into one transaction like the resolve passes above.
    report_phase(
        "linking project membership",
        outcome.files_indexed,
        outcome.symbols,
    );
    let contains_edges: Vec<crate::graph::model::GraphEdge> = candidates
        .iter()
        .filter_map(|rel| {
            owning_manifest(rel, &manifests).map(|manifest| {
                crate::graph::model::GraphEdge::ProjectContainsFile {
                    project: project_id(manifest),
                    file: file_id(rel),
                }
            })
        })
        .collect();
    store.link_edges(&contains_edges)?;

    // Remove files that no longer exist as candidates (deleted/now-ignored).
    if !changed_only {
        for f in &existing {
            if !candidate_set.contains(f.path.as_str()) {
                store.remove_file(&f.path)?;
                outcome.files_removed += 1;
            }
        }
    }

    Ok(outcome)
}

/// Resolve collected per-file imports to known package nodes and create
/// `IMPORTS_PACKAGE` edges. Returns the number of edges created.
///
/// Matching is ecosystem-specific:
/// * JS/TS imports match an npm package by exact name.
/// * C# `using` namespaces match a nuget package whose name is a dotted prefix
///   of the namespace (e.g. package `Serilog` matches `Serilog.Sinks.Console`);
///   the longest such prefix wins.
fn resolve_imports(
    store: &dyn GraphStore,
    pending: &[(String, Vec<String>, Language)],
) -> Result<usize> {
    let packages = store.all_packages()?;
    let npm: HashSet<&str> = packages
        .iter()
        .filter(|p| p.ecosystem == "npm")
        .map(|p| p.name.as_str())
        .collect();
    let nuget: Vec<&str> = packages
        .iter()
        .filter(|p| p.ecosystem == "nuget")
        .map(|p| p.name.as_str())
        .collect();

    let mut batch: Vec<crate::graph::model::GraphEdge> = Vec::new();
    for (fid, imports, lang) in pending {
        // De-dup the resolved package ids per file.
        let mut linked: HashSet<String> = HashSet::new();
        for imp in imports {
            let resolved: Option<String> = match lang {
                Language::JavaScript | Language::TypeScript => {
                    npm.get(imp.as_str()).map(|_| package_id("npm", imp))
                }
                Language::CSharp => nuget_prefix_match(imp, &nuget).map(|n| package_id("nuget", n)),
                _ => None,
            };
            if let Some(pkg_id) = resolved
                && linked.insert(pkg_id.clone())
            {
                batch.push(crate::graph::model::GraphEdge::FileImportsPackage {
                    file: fid.clone(),
                    package: pkg_id,
                });
            }
        }
    }
    let edges = batch.len();
    store.link_edges(&batch)?;
    Ok(edges)
}

/// Whether `candidate_path` lives in the same project directory as a file whose
/// parent directory is `project_dir`. Segment-safe: `src` does not match
/// `src2/foo` — the match must fall on a `/` boundary (or be the dir itself).
fn same_project_dir(candidate_path: &str, project_dir: &str) -> bool {
    !project_dir.is_empty()
        && candidate_path
            .strip_prefix(project_dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Resolve collected supertype relationships into `INHERITS`/`IMPLEMENTS` edges.
/// Returns the number of edges created.
///
/// For each `(declaring_file, [child -> supertype])`:
/// * The child symbol is the declaration in that file with the matching name.
/// * Candidate supertype symbols are all symbols with the matching name. If any
///   candidate is in the same file or same project, only those are linked;
///   otherwise every candidate is linked (per the ambiguity policy).
/// * The edge kind comes from the syntactic hint, or — when unknown (e.g. C#
///   base lists) — from the target symbol's kind (interface/trait => IMPLEMENTS,
///   else INHERITS).
fn resolve_supertypes(
    store: &dyn GraphStore,
    pending: &[(String, Vec<tree_sitter::Supertype>)],
) -> Result<usize> {
    use crate::graph::model::{GraphEdge, SymbolKind, SymbolSearchQuery};

    let mut batch: Vec<GraphEdge> = Vec::new();
    for (file, supers) in pending {
        let project_prefix = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for st in supers {
            // The child symbol must be declared in this file.
            let child_candidates = store.symbols_matching(&SymbolSearchQuery {
                name: Some(st.child.clone()),
                file: Some(file.clone()),
                ..Default::default()
            })?;
            let Some(child) = child_candidates.into_iter().find(|s| s.name == st.child) else {
                continue;
            };

            // Candidate supertype symbols (exact name match, any file).
            let mut targets: Vec<_> = store
                .symbols_matching(&SymbolSearchQuery {
                    name: Some(st.supertype.clone()),
                    ..Default::default()
                })?
                .into_iter()
                .filter(|s| s.name == st.supertype && s.id != child.id)
                .collect();
            if targets.is_empty() {
                continue;
            }

            // Ambiguity policy: prefer same-file, then same-project (directory
            // prefix); only fall back to all candidates if neither matches.
            let same_file: Vec<_> = targets
                .iter()
                .filter(|s| s.file_path == *file)
                .cloned()
                .collect();
            if !same_file.is_empty() {
                targets = same_file;
            } else {
                let same_proj: Vec<_> = targets
                    .iter()
                    .filter(|s| same_project_dir(&s.file_path, project_prefix))
                    .cloned()
                    .collect();
                if !same_proj.is_empty() {
                    targets = same_proj;
                }
            }

            for target in targets {
                let implements = match st.hint {
                    tree_sitter::SuperHint::Implements => true,
                    tree_sitter::SuperHint::Inherits => false,
                    // Unknown (C# base lists): decide from the target's kind.
                    tree_sitter::SuperHint::Unknown => {
                        matches!(target.kind, SymbolKind::Interface | SymbolKind::Trait)
                    }
                };
                batch.push(if implements {
                    GraphEdge::SymbolImplements {
                        from: child.id.clone(),
                        to: target.id.clone(),
                    }
                } else {
                    GraphEdge::SymbolInherits {
                        from: child.id.clone(),
                        to: target.id.clone(),
                    }
                });
            }
        }
    }
    let edges = batch.len();
    store.link_edges(&batch)?;
    Ok(edges)
}

/// Resolve collected usage references into `REFERENCES` edges. Returns the
/// number of edges created.
///
/// For each `(file, [from -> to])`:
/// * `from` (the enclosing declaration) must be a symbol declared in this file.
///   References with no enclosing declaration (top-level/module-scope usages)
///   are skipped — the schema's `REFERENCES` edge requires a Symbol on both
///   ends, and there is no file-level pseudo-symbol to anchor to.
/// * `to` candidates are all symbols with the matching name. The same ambiguity
///   policy as `resolve_supertypes` applies: prefer same-file, then same-project
///   (directory prefix), else link every candidate. Ambiguity yields multiple
///   edges, never a guess.
/// * A `to` that matches no declared symbol yields no edge — this is the guard
///   against local variables shadowing a global name.
fn resolve_references(
    store: &dyn GraphStore,
    pending: &[(String, Vec<tree_sitter::Reference>)],
) -> Result<usize> {
    use crate::graph::model::{IndexedSymbol, SymbolSearchQuery};
    use std::collections::HashMap;

    // Cache of target candidates keyed by referenced name, shared across every
    // file — references to the same type recur constantly, so this turns an
    // O(#refs) query pattern into O(#distinct names). Each entry holds the full
    // exact-name candidate set (any file); same-file/same-project narrowing is
    // applied per reference below.
    let mut to_cache: HashMap<String, Vec<IndexedSymbol>> = HashMap::new();

    // Accumulate edges and write them in one batch (one transaction) at the end
    // rather than one DB statement per edge — this is what removes the stall.
    let mut batch: Vec<crate::graph::model::GraphEdge> = Vec::new();
    for (file, refs) in pending {
        let project_prefix = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

        // Preload every declaration in this file once, indexed by name, for the
        // `from` (enclosing-declaration) lookup — one query per file instead of
        // one per reference. Candidates are sorted so selection is deterministic
        // when a file has multiple same-named declarations (overloads, etc.).
        let mut from_by_name: HashMap<&str, Vec<&IndexedSymbol>> = HashMap::new();
        let file_symbols = store.symbols_matching(&SymbolSearchQuery {
            file: Some(file.clone()),
            ..Default::default()
        })?;
        for sym in &file_symbols {
            from_by_name.entry(sym.name.as_str()).or_default().push(sym);
        }
        for list in from_by_name.values_mut() {
            list.sort_by(|a, b| {
                a.start_line
                    .cmp(&b.start_line)
                    .then(a.end_line.cmp(&b.end_line))
                    .then(a.id.cmp(&b.id))
            });
        }

        for r in refs {
            // The referring symbol must be a declaration in this file.
            if r.from.is_empty() {
                continue;
            }
            let Some(from) = from_by_name.get(r.from.as_str()).and_then(|v| v.first()) else {
                continue;
            };

            // Candidate target symbols (exact name match, any file), cached
            // across files. An empty set means the name isn't a declared symbol
            // (e.g. a local var) — no edge, the false-positive guard.
            if !to_cache.contains_key(&r.to) {
                let candidates = store
                    .symbols_matching(&SymbolSearchQuery {
                        name: Some(r.to.clone()),
                        ..Default::default()
                    })?
                    .into_iter()
                    .filter(|s| s.name == r.to)
                    .collect();
                to_cache.insert(r.to.clone(), candidates);
            }
            let mut targets: Vec<&IndexedSymbol> =
                to_cache[&r.to].iter().filter(|s| s.id != from.id).collect();
            if targets.is_empty() {
                continue;
            }

            // Ambiguity policy mirrors resolve_supertypes: prefer same-file,
            // then same-project; only fall back to all candidates if neither.
            let same_file: Vec<_> = targets
                .iter()
                .filter(|s| s.file_path == *file)
                .copied()
                .collect();
            if !same_file.is_empty() {
                targets = same_file;
            } else {
                let same_proj: Vec<_> = targets
                    .iter()
                    .filter(|s| same_project_dir(&s.file_path, project_prefix))
                    .copied()
                    .collect();
                if !same_proj.is_empty() {
                    targets = same_proj;
                }
            }

            // Deterministic order when an ambiguous name produces multiple edges.
            targets.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.id.cmp(&b.id)));
            for target in targets {
                batch.push(crate::graph::model::GraphEdge::SymbolReferences {
                    from: from.id.clone(),
                    to: target.id.clone(),
                });
            }
        }
    }
    let edges = batch.len();
    store.link_edges(&batch)?;
    Ok(edges)
}

/// Among `packages`, return the one whose name is the longest dotted prefix of
/// the C# namespace `ns` (matching on `.` segment boundaries).
fn nuget_prefix_match<'a>(ns: &str, packages: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    for &pkg in packages {
        let is_prefix = ns == pkg
            || (ns.len() > pkg.len()
                && ns.starts_with(pkg)
                && ns.as_bytes().get(pkg.len()) == Some(&b'.'));
        if is_prefix && best.is_none_or(|b| pkg.len() > b.len()) {
            best = Some(pkg);
        }
    }
    best
}

/// Find the nearest project manifest that owns `path` — the manifest whose
/// directory is the deepest ancestor of `path`. `manifests` must be pre-sorted
/// by descending directory depth (as done in [`index_repo`]). A manifest never
/// owns itself.
fn owning_manifest<'a>(path: &str, manifests: &'a [String]) -> Option<&'a str> {
    manifests
        .iter()
        .find(|m| m.as_str() != path && manifest_owns(m, path))
        .map(|m| m.as_str())
}

/// True if the directory containing manifest `m` is an ancestor of (or equal to
/// the directory of) `path`.
fn manifest_owns(manifest: &str, path: &str) -> bool {
    let dir = manifest.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    if dir.is_empty() {
        // Root manifest owns everything.
        return true;
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Central Package Management state gathered from every `Directory.Packages.props`
/// and `Directory.Build.props` in the repo.
///
/// MSBuild resolves a `PackageReference` without an inline `Version` against the
/// `<PackageVersion>` pin in the nearest `Directory.Packages.props` walking up
/// from the project. We mirror that: each entry records the props file's
/// directory, its package→version pins, and any `ManagedPackageVersionsCentrally`
/// flag. Lookups for a given `.csproj` merge all ancestor props farthest-first,
/// so the nearest pin wins.
struct CentralVersions {
    /// `(dir, version_pins, cpm_flag)` per props file, sorted by ascending
    /// directory depth (farthest ancestors first) for nearest-wins merging.
    entries: Vec<(
        String,
        std::collections::HashMap<String, String>,
        Option<bool>,
    )>,
}

impl CentralVersions {
    /// Parse all `Directory.Packages.props` / `Directory.Build.props` among the
    /// candidate files into a resolvable structure.
    fn collect(repo: &Repo, candidates: &[String]) -> CentralVersions {
        let mut entries = Vec::new();
        for rel in candidates {
            let name = rel.rsplit('/').next().unwrap_or(rel).to_ascii_lowercase();
            if name != "directory.packages.props" && name != "directory.build.props" {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(repo.root.join(rel)) else {
                continue;
            };
            let Ok(parsed) = dotnet::parse_msbuild(&text) else {
                continue;
            };
            let dir = rel
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or("")
                .to_string();
            let map = dotnet::central_version_map([&parsed]);
            entries.push((dir, map, parsed.cpm_enabled));
        }
        // Farthest ancestors first so nearer props overwrite during merge.
        entries.sort_by(|a, b| {
            let depth = |p: &str| {
                if p.is_empty() {
                    0
                } else {
                    p.matches('/').count() + 1
                }
            };
            depth(&a.0).cmp(&depth(&b.0)).then_with(|| a.0.cmp(&b.0))
        });
        CentralVersions { entries }
    }

    /// Whether a props directory is an ancestor of (or equal to) `csproj`'s dir.
    fn applies(dir: &str, csproj: &str) -> bool {
        if dir.is_empty() {
            return true;
        }
        csproj
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Resolve the central version for `package`, as seen from `csproj`.
    fn version_for(&self, csproj: &str, package: &str) -> Option<String> {
        let mut found = None;
        for (dir, map, _) in &self.entries {
            if Self::applies(dir, csproj)
                && let Some(v) = map.get(package)
            {
                found = Some(v.clone()); // later (nearer) entries overwrite
            }
        }
        found
    }

    /// Whether CPM is enabled for `csproj` (nearest flag up the tree wins).
    fn enabled_for(&self, csproj: &str) -> bool {
        let mut enabled = false;
        for (dir, _, flag) in &self.entries {
            if Self::applies(dir, csproj)
                && let Some(f) = flag
            {
                enabled = *f;
            }
        }
        enabled
    }
}

fn language_enabled(config: &SynapseConfig, lang: Language) -> bool {
    let l = &config.index.languages;
    match lang {
        Language::CSharp => l.csharp,
        Language::Rust => l.rust,
        Language::Python => l.python,
        Language::Go => l.go,
        Language::JavaScript => l.javascript,
        Language::TypeScript => l.typescript,
        Language::Svelte => l.svelte,
        Language::Bash => l.bash,
        Language::Yaml => l.yaml,
        Language::Json => l.json,
        Language::Markdown => l.markdown,
        Language::Other => false,
    }
}

/// Parse a `.csproj` and upsert the project + its references/packages.
/// Returns the number of edges created. Package versions missing from the
/// `.csproj` are resolved against Central Package Management via `central`.
fn index_csproj(
    rel: &str,
    abs: &Path,
    store: &dyn GraphStore,
    central: &CentralVersions,
) -> Result<usize> {
    let text =
        std::fs::read_to_string(abs).with_context(|| format!("reading {}", abs.display()))?;
    let parsed = dotnet::parse_csproj(&text)?;

    let name = Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
        .to_string();
    let pid = project_id(rel);
    // Note CPM in the project kind so it's visible in the graph/output.
    let kind = if central.enabled_for(rel) || !parsed.package_versions.is_empty() {
        "dotnet (cpm)"
    } else {
        "dotnet"
    };
    store.upsert_project(IndexedProject {
        id: pid.clone(),
        name,
        path: rel.to_string(),
        language: Language::CSharp,
        kind: kind.to_string(),
    })?;

    let mut edges = 0;
    // Resolve project references relative to this csproj's directory.
    let dir = Path::new(rel).parent();
    for proj_ref in &parsed.project_references {
        let target = resolve_rel(dir, proj_ref);
        store.link_project_references_project(&pid, &project_id(&target))?;
        edges += 1;
    }
    for pkg in &parsed.package_references {
        // Prefer the inline version; fall back to the central pin (CPM).
        let version = pkg
            .version
            .clone()
            .or_else(|| central.version_for(rel, &pkg.name))
            .unwrap_or_default();
        let pkg_id = package_id("nuget", &pkg.name);
        store.upsert_package(IndexedPackage {
            id: pkg_id.clone(),
            name: pkg.name.clone(),
            version,
            ecosystem: "nuget".to_string(),
            dependency_kind: "package".to_string(),
        })?;
        store.link_project_uses_package(&pid, &pkg_id)?;
        edges += 1;
    }
    Ok(edges)
}

/// Parse a `package.json` and upsert the project + its dependencies.
/// Returns the number of edges created.
fn index_package_json(rel: &str, abs: &Path, store: &dyn GraphStore) -> Result<usize> {
    let text =
        std::fs::read_to_string(abs).with_context(|| format!("reading {}", abs.display()))?;
    let parsed = node::parse_package_json(&text)?;

    let name = parsed.name.clone().unwrap_or_else(|| {
        Path::new(rel)
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("package")
            .to_string()
    });
    let pid = project_id(rel);
    store.upsert_project(IndexedProject {
        id: pid.clone(),
        name,
        path: rel.to_string(),
        language: Language::JavaScript,
        kind: "node".to_string(),
    })?;

    let mut edges = 0;
    for dep in &parsed.dependencies {
        let pkg_id = package_id("npm", &dep.name);
        store.upsert_package(IndexedPackage {
            id: pkg_id.clone(),
            name: dep.name.clone(),
            version: dep.version.clone(),
            ecosystem: "npm".to_string(),
            dependency_kind: dep.kind.clone(),
        })?;
        store.link_project_uses_package(&pid, &pkg_id)?;
        edges += 1;
    }
    Ok(edges)
}

/// Resolve a relative manifest reference (possibly using `\`) against a base dir
/// into a normalized repo-relative path.
fn resolve_rel(base: Option<&Path>, reference: &str) -> String {
    let reference = reference.replace('\\', "/");
    let joined = match base {
        Some(b) => b.join(&reference),
        None => Path::new(&reference).to_path_buf(),
    };
    normalize_components(&joined)
}

/// Collapse `.`/`..` components into a clean forward-slash path.
fn normalize_components(p: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let normalized = p.to_string_lossy().replace('\\', "/");
    for comp in normalized.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}
