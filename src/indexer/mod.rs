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
use rayon::prelude::*;
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
    /// Progress *within* the current `phase`, as `(done, total)` — e.g. files
    /// resolved so far in the references pass. `None` when a phase has no
    /// meaningful sub-count (or during the file scan). Lets the UI show that a
    /// long phase is advancing rather than hung.
    pub phase_progress: Option<(usize, usize)>,
}

/// A progress observer invoked as indexing proceeds.
///
/// Receives the current file path and a live [`IndexProgress`] snapshot. Kept as
/// a plain callback so the indexer stays UI-agnostic — the binary wires this to
/// a progress bar; tests can ignore it. `Sync` so a scoped observer thread can
/// drive it while the parallel parse runs (see `index_repo`).
pub type ProgressFn<'a> = dyn Fn(&str, &IndexProgress) + Sync + 'a;

/// The result of parsing one candidate file off the main thread — everything
/// needed to write it to the store, with zero store access. The parallel parse
/// stage produces one of these per candidate (order-aligned with `candidates`);
/// the sequential drain consumes them in order, so symbol-insertion and
/// pending-vec ordering are byte-identical to the old single-threaded loop.
enum FileWork {
    /// File was filtered (`--changed`) or unchanged — counted as "skipped
    /// unchanged" in the outcome, matching the old loop's two `continue`s that
    /// bumped `files_skipped_unchanged`.
    SkipCounted,
    /// File was unreadable — silently skipped with no counter bump, matching the
    /// old loop's bare `continue` on a read error.
    SkipUnreadable,
    /// A changed/new file to (re)index.
    Indexed(Box<IndexedFileWork>),
}

/// Parsed facts for one changed file, ready to drain into the store in order.
struct IndexedFileWork {
    rel: String,
    fid: String,
    file: IndexedFile,
    /// Extracted symbols (empty for unsupported/disabled languages).
    symbols: Vec<crate::graph::model::IndexedSymbol>,
    /// `(imports, language)` for later package resolution, when non-empty.
    imports: Option<(Vec<String>, Language)>,
    /// Supertype relationships discovered in this file, when non-empty.
    supers: Vec<tree_sitter::Supertype>,
    /// Usage references discovered in this file, when non-empty.
    references: Vec<tree_sitter::Reference>,
    /// Parsed manifest (`.csproj`/`package.json`) ready to write, when this file
    /// is a manifest that parsed successfully.
    manifest: Option<ManifestWrite>,
}

/// Read-only inputs shared by every [`parse_file`] call, gathered once before
/// the parallel parse so each rayon worker borrows them immutably. None of
/// these are touched by the store, so sharing across threads is safe.
struct ParseContext<'a> {
    repo: &'a Repo,
    config: &'a SynapseConfig,
    force: bool,
    changed_only: bool,
    changed: &'a HashSet<String>,
    existing: &'a [IndexedFile],
    tracked: &'a HashSet<String>,
    central: &'a CentralVersions,
    now: &'a str,
}

/// Parse a single candidate file into a [`FileWork`], doing only pure work:
/// read + hash + skip-decision + language detection + tree-sitter extraction +
/// manifest parse. No store access — safe to call from a rayon worker thread.
fn parse_file(ctx: &ParseContext<'_>, rel: &str) -> Result<FileWork> {
    if ctx.changed_only && !ctx.changed.contains(rel) {
        // Skip files git didn't flag as changed.
        return Ok(FileWork::SkipCounted);
    }

    let abs = ctx.repo.root.join(rel);
    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        Err(_) => return Ok(FileWork::SkipUnreadable),
    };
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let size = bytes.len() as u64;

    // Skip unchanged files (unless forced).
    if !ctx.force
        && let Some(prev) = ctx.existing.iter().find(|f| f.path == rel)
        && prev.hash == hash
    {
        return Ok(FileWork::SkipCounted);
    }

    let language = languages::detect(rel);
    let fid = file_id(rel);
    let file = IndexedFile {
        id: fid.clone(),
        path: rel.to_string(),
        language,
        hash,
        size_bytes: size,
        tracked: ctx.tracked.contains(rel),
        last_indexed_at: ctx.now.to_string(),
    };

    let mut symbols = Vec::new();
    let mut imports = None;
    let mut supers = Vec::new();
    let mut references = Vec::new();

    // Symbol extraction for supported languages.
    if language != Language::Other && language_enabled(ctx.config, language) {
        let text = String::from_utf8_lossy(&bytes);
        symbols = tree_sitter::extract(rel, language, &text).unwrap_or_default();

        // Collect imports (JS/TS, C#) for later file -> package resolution.
        if matches!(
            language,
            Language::JavaScript | Language::TypeScript | Language::CSharp
        ) {
            let imps = tree_sitter::extract_imports(rel, language, &text);
            if !imps.is_empty() {
                imports = Some((imps, language));
            }
        }

        // Collect supertype relationships for later INHERITS/IMPLEMENTS edges.
        supers = tree_sitter::extract_supertypes(rel, language, &text);

        // Collect usage references for later REFERENCES edges.
        references = tree_sitter::extract_references(rel, language, &text);
    }

    // Manifest parsing -> projects/packages/edges (pure parse only). A parse
    // error propagates out of `parse_file`; the sequential drain surfaces it via
    // `work?` and aborts the run *before* the batched write stage executes. This
    // is a more atomic failure mode than the old write-as-you-go loop, which
    // could leave earlier files already committed when a later manifest failed.
    let manifest = if rel.ends_with(".csproj") {
        Some(parse_csproj_manifest(rel, &abs, ctx.central)?)
    } else if rel.ends_with("package.json") {
        Some(parse_package_json_manifest(rel, &abs)?)
    } else {
        None
    };

    Ok(FileWork::Indexed(Box::new(IndexedFileWork {
        rel: rel.to_string(),
        fid,
        file,
        symbols,
        imports,
        supers,
        references,
        manifest,
    })))
}

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

    // Stage 1 — parallel parse. Read + hash + detect + tree-sitter extract +
    // manifest parse is pure, `Send`, owned-data work (no store access), so it
    // runs across rayon's thread pool. `par_iter().map().collect()` preserves
    // input order, so the resulting `Vec` is index-aligned with `candidates` —
    // the sequential drain below then writes in candidate order, making symbol
    // insertion and `pending_*` ordering byte-identical to the old loop.
    //
    // The parse is where the real per-file wall-clock now is, but `ProgressFn`
    // isn't `Sync`, so rayon workers can't call it. Instead each worker bumps a
    // shared atomic, and a scoped observer thread on the side polls that counter
    // and drives the progress callback (which stays on this thread) — so the bar
    // climbs smoothly during the parse instead of jumping 0 -> N.
    let parse_ctx = ParseContext {
        repo,
        config,
        force,
        changed_only,
        changed: &changed,
        existing: &existing,
        tracked: &tracked,
        central: &central,
        now,
    };
    let parsed_count = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicBool::new(false);
    let parsed: Vec<Result<FileWork>> = std::thread::scope(|scope| {
        // Observer: report parse progress until the parse finishes. Only spawned
        // when there's a progress sink; it borrows `progress`/`parsed_count` and
        // is joined before the scope ends (so the borrows are sound).
        if let Some(cb) = progress {
            let parsed_count = &parsed_count;
            let done = &done;
            scope.spawn(move || {
                let emit = |n: usize| {
                    cb(
                        "",
                        &IndexProgress {
                            processed: n,
                            total,
                            phase: Some("parsing files"),
                            phase_progress: Some((n, total)),
                            ..IndexProgress::default()
                        },
                    );
                };
                loop {
                    // Observe `done` first, then read the counter. `done` is
                    // stored (Release) only after `par_iter().collect()` has
                    // joined every worker, so all the `fetch_add(Release)`
                    // increments happen-before it. Reading `done` via Acquire
                    // therefore guarantees the following Acquire load sees the
                    // final count — emit it and stop, so the last update always
                    // reflects the true total rather than a stale lower value.
                    let finished = done.load(std::sync::atomic::Ordering::Acquire);
                    emit(parsed_count.load(std::sync::atomic::Ordering::Acquire));
                    if finished {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
            });
        }
        let out: Vec<Result<FileWork>> = candidates
            .par_iter()
            .map(|rel| {
                let w = parse_file(&parse_ctx, rel);
                parsed_count.fetch_add(1, std::sync::atomic::Ordering::Release);
                w
            })
            .collect();
        done.store(true, std::sync::atomic::Ordering::Release);
        out
    });

    // Stage 2 — sequential drain. Walk the parsed results in candidate order,
    // accumulating each changed file's node payload into `file_writes` and its
    // manifest into `manifest_writes`; the pending_* vecs are filled in order
    // too. No per-symbol store call here — the file/symbol nodes are written in
    // ONE batched transaction below (the dominant cost otherwise), and manifests
    // (few, cheap) right after. The store mutex is never touched from a rayon
    // worker, and write order stays candidate-ordered so output is unchanged.
    let mut file_writes: Vec<crate::graph::model::FileWrite> = Vec::new();
    let mut manifest_writes: Vec<ManifestWrite> = Vec::new();
    for (_rel, work) in candidates.iter().zip(parsed) {
        let work = work?;
        let work = match work {
            FileWork::SkipCounted => {
                outcome.files_skipped_unchanged += 1;
                continue;
            }
            FileWork::SkipUnreadable => continue,
            FileWork::Indexed(w) => w,
        };

        outcome.files_indexed += 1;
        outcome.symbols += work.symbols.len();
        file_writes.push(crate::graph::model::FileWrite {
            file: work.file,
            symbols: work.symbols,
        });

        if let Some((imports, language)) = work.imports {
            pending_imports.push((work.fid.clone(), imports, language));
        }
        if !work.supers.is_empty() {
            pending_supertypes.push((work.rel.clone(), work.supers));
        }
        if !work.references.is_empty() {
            pending_references.push((work.rel.clone(), work.references));
        }
        if let Some(manifest) = work.manifest {
            manifest_writes.push(manifest);
            projects_seen += 1;
        }
    }

    // Post-loop store work runs after the per-file scan (so link targets all
    // exist) and can be the bulk of wall-clock on large repos, so each step
    // reports a phase to the progress UI — otherwise the bar sits frozen at N/N
    // and looks hung. `report_phase` emits a snapshot tagged with the phase and
    // an optional `(done, total)` sub-count so a long phase visibly advances.
    let report_phase =
        |phase: &'static str, files: usize, symbols: usize, sub: Option<(usize, usize)>| {
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
                        phase_progress: sub,
                    },
                );
            }
        };

    // Write every file + its symbols + DECLARES edges in one transaction, then
    // the manifests (projects/packages + their edges), in candidate order.
    report_phase(
        "writing graph",
        outcome.files_indexed,
        outcome.symbols,
        None,
    );
    store.write_files_batch(&file_writes)?;
    for manifest in &manifest_writes {
        outcome.edges += write_manifest(store, manifest)?;
    }

    // Resolve collected imports to known packages -> IMPORTS_PACKAGE edges.
    // Done after the main loop so every manifest has registered its packages.
    if !pending_imports.is_empty() {
        report_phase(
            "resolving imports",
            outcome.files_indexed,
            outcome.symbols,
            None,
        );
        outcome.edges += resolve_imports(store, &pending_imports)?;
    }

    // Resolve supertype/reference relationships -> INHERITS/IMPLEMENTS/REFERENCES
    // edges. Both passes look symbols up by name and file; rather than issuing
    // one unindexed `symbols_matching` scan per lookup (the resolve-phase
    // bottleneck on large repos), build one in-memory index from a single full
    // scan and resolve against it. Done after the main loop so every symbol
    // (the link targets) exists. Skip building it when there's nothing to
    // resolve. Each pass reports its per-file progress so the bar advances.
    if !pending_supertypes.is_empty() || !pending_references.is_empty() {
        let symbol_index = SymbolIndex::build(store)?;

        if !pending_supertypes.is_empty() {
            let n = pending_supertypes.len();
            report_phase(
                "resolving type relationships",
                outcome.files_indexed,
                outcome.symbols,
                Some((0, n)),
            );
            outcome.edges +=
                resolve_supertypes(store, &symbol_index, &pending_supertypes, &|done| {
                    report_phase(
                        "resolving type relationships",
                        outcome.files_indexed,
                        outcome.symbols,
                        Some((done, n)),
                    );
                })?;
        }

        // References are cross-file, so all declarations must already exist.
        if !pending_references.is_empty() {
            let n = pending_references.len();
            report_phase(
                "resolving references",
                outcome.files_indexed,
                outcome.symbols,
                Some((0, n)),
            );
            outcome.edges +=
                resolve_references(store, &symbol_index, &pending_references, &|done| {
                    report_phase(
                        "resolving references",
                        outcome.files_indexed,
                        outcome.symbols,
                        Some((done, n)),
                    );
                })?;
        }
    }

    // Associate every indexed file with its nearest owning project manifest,
    // creating CONTAINS_FILE edges. We link against the full candidate set (not
    // just files touched this run) so ownership is complete after any index.
    // Batched into one transaction like the resolve passes above.
    report_phase(
        "linking project membership",
        outcome.files_indexed,
        outcome.symbols,
        None,
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

/// In-memory index over every symbol in the graph, built once from a single
/// full scan so the resolve passes can look symbols up by name/file in O(1)
/// instead of issuing one unindexed `symbols_matching` scan per lookup.
///
/// Buckets are sorted deterministically on build (by `start_line`, `end_line`,
/// `id`) so `from`/`child` selection — which takes the first element — is
/// stable when a file declares several same-named symbols (overloads, etc.).
struct SymbolIndex {
    /// All symbols sharing a case-insensitive name, keyed by lowercased name.
    by_name_ci: std::collections::HashMap<String, Vec<crate::graph::model::IndexedSymbol>>,
    /// Declarations per file, keyed by file path then exact symbol name.
    by_file: std::collections::HashMap<
        String,
        std::collections::HashMap<String, Vec<crate::graph::model::IndexedSymbol>>,
    >,
}

impl SymbolIndex {
    /// Build the index from one full-table symbol scan (empty query => no
    /// `WHERE`, so a single pass over the Symbol table).
    fn build(store: &dyn GraphStore) -> Result<SymbolIndex> {
        use crate::graph::model::SymbolSearchQuery;
        use std::collections::HashMap;

        let all = store.symbols_matching(&SymbolSearchQuery::default())?;
        let mut by_name_ci: HashMap<String, Vec<_>> = HashMap::new();
        let mut by_file: HashMap<String, HashMap<String, Vec<_>>> = HashMap::new();
        for sym in all {
            by_name_ci
                .entry(sym.name.to_ascii_lowercase())
                .or_default()
                .push(sym.clone());
            by_file
                .entry(sym.file_path.clone())
                .or_default()
                .entry(sym.name.clone())
                .or_default()
                .push(sym);
        }
        let sort = |list: &mut Vec<crate::graph::model::IndexedSymbol>| {
            list.sort_by(|a, b| {
                a.start_line
                    .cmp(&b.start_line)
                    .then(a.end_line.cmp(&b.end_line))
                    .then(a.id.cmp(&b.id))
            });
        };
        for list in by_name_ci.values_mut() {
            sort(list);
        }
        for names in by_file.values_mut() {
            for list in names.values_mut() {
                sort(list);
            }
        }
        Ok(SymbolIndex {
            by_name_ci,
            by_file,
        })
    }

    /// The first declaration named `name` in `file` (deterministic by build
    /// sort), or `None` if the file declares no such symbol.
    fn decl_in_file(&self, file: &str, name: &str) -> Option<&crate::graph::model::IndexedSymbol> {
        self.by_file.get(file)?.get(name)?.first()
    }

    /// All symbols whose name equals `name` (case-insensitive lookup, then an
    /// exact-name filter to match the prior `name == …` post-filter semantics).
    fn by_name<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a crate::graph::model::IndexedSymbol> + 'a {
        self.by_name_ci
            .get(&name.to_ascii_lowercase())
            .into_iter()
            .flat_map(|v| v.iter())
            .filter(move |s| s.name == name)
    }
}

/// Report resolve progress at a coarse cadence — at the start and roughly every
/// 1% of `total` — so the progress bar advances without the callback dominating
/// the (now cheap) per-file work on large repos.
fn maybe_report(report: &dyn Fn(usize), i: usize, total: usize) {
    let step = (total / 100).max(1);
    if i.is_multiple_of(step) {
        report(i);
    }
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
    index: &SymbolIndex,
    pending: &[(String, Vec<tree_sitter::Supertype>)],
    report: &dyn Fn(usize),
) -> Result<usize> {
    use crate::graph::model::{GraphEdge, SymbolKind};

    let mut batch: Vec<GraphEdge> = Vec::new();
    for (i, (file, supers)) in pending.iter().enumerate() {
        maybe_report(report, i, pending.len());
        let project_prefix = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for st in supers {
            // The child symbol must be declared in this file.
            let Some(child) = index.decl_in_file(file, &st.child) else {
                continue;
            };

            // Candidate supertype symbols (exact name match, any file).
            let mut targets: Vec<_> = index
                .by_name(&st.supertype)
                .filter(|s| s.id != child.id)
                .cloned()
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
    index: &SymbolIndex,
    pending: &[(String, Vec<tree_sitter::Reference>)],
    report: &dyn Fn(usize),
) -> Result<usize> {
    use crate::graph::model::IndexedSymbol;

    // Accumulate edges and write them in one batch (one transaction) at the end
    // rather than one DB statement per edge — this is what removes the stall.
    let mut batch: Vec<crate::graph::model::GraphEdge> = Vec::new();
    for (i, (file, refs)) in pending.iter().enumerate() {
        maybe_report(report, i, pending.len());
        let project_prefix = file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

        for r in refs {
            // The referring symbol must be a declaration in this file.
            if r.from.is_empty() {
                continue;
            }
            // The `from` (enclosing-declaration) lookup: the first declaration
            // in this file named `r.from` (deterministic by the index's build
            // sort when a file has multiple same-named declarations).
            let Some(from) = index.decl_in_file(file, &r.from) else {
                continue;
            };

            // Candidate target symbols (exact name match, any file). An empty
            // set means the name isn't a declared symbol (e.g. a local var) —
            // no edge, the false-positive guard.
            let mut targets: Vec<&IndexedSymbol> =
                index.by_name(&r.to).filter(|s| s.id != from.id).collect();
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

/// A manifest (`.csproj` / `package.json`) parsed into the exact, ordered set of
/// store operations needed to record it — the project node plus its reference
/// and package edges in document order. Produced by the pure `parse_*_manifest`
/// functions (safe to run off the main thread) and replayed against the store
/// by [`write_manifest`] in the sequential drain. Splitting parse from write
/// lets the CPU-heavy parse run in parallel while keeping every store write
/// (and thus edge ordering) on the single indexing thread.
struct ManifestWrite {
    project: IndexedProject,
    ops: Vec<ManifestOp>,
}

/// One ordered store operation contributed by a manifest, after its project
/// node is upserted. Each op corresponds to exactly one edge.
enum ManifestOp {
    /// `link_project_references_project(project, target_project)`.
    ProjectRef { target: String },
    /// `upsert_package` then `link_project_uses_package(project, package)`.
    Package(IndexedPackage),
}

/// Parse a `.csproj` into a [`ManifestWrite`] (pure: no store access). Package
/// versions missing from the `.csproj` are resolved against Central Package
/// Management via `central`.
fn parse_csproj_manifest(
    rel: &str,
    abs: &Path,
    central: &CentralVersions,
) -> Result<ManifestWrite> {
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
    let project = IndexedProject {
        id: pid,
        name,
        path: rel.to_string(),
        language: Language::CSharp,
        kind: kind.to_string(),
    };

    let mut ops = Vec::new();
    // Resolve project references relative to this csproj's directory.
    let dir = Path::new(rel).parent();
    for proj_ref in &parsed.project_references {
        let target = resolve_rel(dir, proj_ref);
        ops.push(ManifestOp::ProjectRef {
            target: project_id(&target),
        });
    }
    for pkg in &parsed.package_references {
        // Prefer the inline version; fall back to the central pin (CPM).
        let version = pkg
            .version
            .clone()
            .or_else(|| central.version_for(rel, &pkg.name))
            .unwrap_or_default();
        ops.push(ManifestOp::Package(IndexedPackage {
            id: package_id("nuget", &pkg.name),
            name: pkg.name.clone(),
            version,
            ecosystem: "nuget".to_string(),
            dependency_kind: "package".to_string(),
        }));
    }
    Ok(ManifestWrite { project, ops })
}

/// Parse a `package.json` into a [`ManifestWrite`] (pure: no store access).
fn parse_package_json_manifest(rel: &str, abs: &Path) -> Result<ManifestWrite> {
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
    let project = IndexedProject {
        id: project_id(rel),
        name,
        path: rel.to_string(),
        language: Language::JavaScript,
        kind: "node".to_string(),
    };

    let mut ops = Vec::new();
    for dep in &parsed.dependencies {
        ops.push(ManifestOp::Package(IndexedPackage {
            id: package_id("npm", &dep.name),
            name: dep.name.clone(),
            version: dep.version.clone(),
            ecosystem: "npm".to_string(),
            dependency_kind: dep.kind.clone(),
        }));
    }
    Ok(ManifestWrite { project, ops })
}

/// Replay a parsed [`ManifestWrite`] against the store, in document order.
/// Returns the number of edges created (one per op), matching the previous
/// `index_csproj`/`index_package_json` return value exactly.
fn write_manifest(store: &dyn GraphStore, manifest: &ManifestWrite) -> Result<usize> {
    let pid = manifest.project.id.clone();
    store.upsert_project(manifest.project.clone())?;
    for op in &manifest.ops {
        match op {
            ManifestOp::ProjectRef { target } => {
                store.link_project_references_project(&pid, target)?;
            }
            ManifestOp::Package(pkg) => {
                store.upsert_package(pkg.clone())?;
                store.link_project_uses_package(&pid, &pkg.id)?;
            }
        }
    }
    Ok(manifest.ops.len())
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
