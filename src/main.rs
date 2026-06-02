//! SimCube Synapse — a deterministic, local, offline repository context
//! compiler. Indexes a repo into a graph and emits compact LLM-ready Markdown
//! context packs. No network, no AI calls, no daemon.

use anyhow::{Context, Result};
use clap::Parser;
use std::io::IsTerminal;
use std::path::Path;
use synapse::cli::{self, Cli, Command};
use synapse::config::{self, SynapseConfig};
use synapse::graph::{self, GraphStore};
use synapse::repo::Repo;
use synapse::{errors, explore, git, indexer, output, pack};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .without_time()
        .init();

    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().context("getting current directory")?;

    match cli.command {
        Command::Init(args) => cmd_init(&cwd, args),
        Command::Index(args) => cmd_index(&cwd, args),
        Command::Status(args) => cmd_status(&cwd, args),
        Command::Symbols(args) => cmd_symbols(&cwd, args),
        Command::Related(args) => cmd_related(&cwd, args),
        Command::Packages(args) => cmd_packages(&cwd, args),
        Command::Pack(args) => cmd_pack(&cwd, args),
        Command::Explore(args) => cmd_explore(&cwd, args),
        #[cfg(feature = "share")]
        Command::Push(args) => cmd_push(&cwd, args),
        #[cfg(feature = "share")]
        Command::Pull(args) => cmd_pull(&cwd, args),
        #[cfg(not(feature = "share"))]
        Command::Push(_) | Command::Pull(_) => anyhow::bail!(
            "this build was compiled without the `share` feature; rebuild with `--features share` to push/pull graphs"
        ),
        Command::Clean(args) => cmd_clean(&cwd, args),
    }
}

/// Open the graph store for a repo. Uses LadybugDB in production; the in-memory
/// store is test-only and never used here.
fn open_store(root: &Path, config: &SynapseConfig) -> Result<Box<dyn GraphStore>> {
    let graph_path = root.join(&config.graph.path).join("synapse.lbug");
    #[cfg(feature = "ladybug")]
    {
        let store = graph::ladybug_store::LadybugGraphStore::open(&graph_path)?;
        Ok(Box::new(store))
    }
    #[cfg(not(feature = "ladybug"))]
    {
        let _ = graph_path;
        anyhow::bail!(
            "this build was compiled without the `ladybug` feature; no production graph backend is available"
        )
    }
}

fn now_rfc3339() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

fn require_repo(cwd: &Path) -> Result<(Repo, SynapseConfig)> {
    let repo = Repo::discover(cwd)?;
    if !repo.is_initialized() {
        return Err(errors::SynapseError::NotInitialized.into());
    }
    let config = SynapseConfig::load(&repo.root)?;
    Ok((repo, config))
}

fn cmd_init(cwd: &Path, args: cli::InitArgs) -> Result<()> {
    let repo = Repo::discover(cwd)?;
    let cfg_path = config::config_path(&repo.root);
    if cfg_path.is_file() && !args.force {
        return Err(errors::SynapseError::ConfigExists(cfg_path).into());
    }

    // Create directory structure.
    let dir = config::synapse_dir(&repo.root);
    for sub in ["graph", "cache", "packs"] {
        std::fs::create_dir_all(dir.join(sub))
            .with_context(|| format!("creating {}", dir.join(sub).display()))?;
    }

    let mut config = SynapseConfig::default();
    if let Some(name) = args.name {
        config.repo.name = name;
    }
    config.save(&repo.root)?;

    // If the repo has a root .gitignore, make sure the local graph working state
    // is ignored — the graph is a multi-MB binary that should be shared via
    // `synapse push`, never committed. We deliberately keep `synapse.toml`
    // committable (it's shared team config), so we ignore the working subdirs,
    // not the whole `.synapse/`.
    if let Some(added) = ensure_gitignored(&repo.root, &config)? {
        println!("Added {added} to .gitignore");
    }

    println!("Initialized Synapse workspace at {}", dir.display());
    println!("  config: {}", cfg_path.display());
    println!("  graph:  {}", dir.join("graph").display());
    Ok(())
}

/// If a root `.gitignore` exists, ensure the synapse graph working state is
/// ignored. Returns the entry added, or `None` if nothing changed (no
/// `.gitignore`, or already covered). Only the graph/cache/packs working dirs
/// are ignored — `synapse.toml` stays committable as shared config.
fn ensure_gitignored(root: &Path, config: &SynapseConfig) -> Result<Option<String>> {
    let gitignore = root.join(".gitignore");
    if !gitignore.is_file() {
        // Respect a repo that deliberately has no .gitignore.
        return Ok(None);
    }
    let text = std::fs::read_to_string(&gitignore)
        .with_context(|| format!("reading {}", gitignore.display()))?;

    // Derive the graph dir relative to the repo root (handles a custom
    // `graph.path`); fall back to the default working subdirs of `.synapse`.
    let graph_rel = config.graph.path.trim_end_matches('/');
    let entry = format!("{graph_rel}/");

    // Already covered? Accept the exact entry, or a broader ignore of the
    // synapse dir / a bare graph glob.
    let already = text.lines().map(str::trim).any(|l| {
        l == entry
            || l == graph_rel
            || l == ".synapse/"
            || l == ".synapse"
            || l == "**/synapse.lbug"
    });
    if already {
        return Ok(None);
    }

    let mut new_text = text;
    if !new_text.is_empty() && !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    new_text.push_str("\n# SimCube Synapse local graph (share via `synapse push`, don't commit)\n");
    new_text.push_str(&entry);
    new_text.push('\n');
    std::fs::write(&gitignore, new_text)
        .with_context(|| format!("updating {}", gitignore.display()))?;
    Ok(Some(entry))
}

fn cmd_index(cwd: &Path, args: cli::IndexArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;

    // Re-indexing makes the graph locally-derived, so any registry-pull
    // provenance marker no longer applies — remove it (best effort).
    let _ = std::fs::remove_file(graph_origin_path(&repo.root, &config));

    // Count candidates for the summary (walked-and-eligible files).
    let candidates = repo.candidate_files(&config)?;
    let now = now_rfc3339();

    // Progress: a colored multi-line block (bar + live stats + current file),
    // shown only on an interactive stderr and when not --quiet, so piped/CI/
    // agent runs stay clean. `indicatif` draws to stderr and clears itself on
    // completion, leaving the stdout summary untouched.
    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let bar = if show_progress {
        let pb = indicatif::ProgressBar::new(candidates.len() as u64);
        pb.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner:.green} indexing {bar:28.green/dim} {pos:>6}/{len:<6} \
                 {elapsed:.dim} {msg}",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        // Redraw a few times a second so the spinner/ETA stay lively even when
        // many files are skipped quickly.
        pb.enable_steady_tick(std::time::Duration::from_millis(120));
        Some(pb)
    } else {
        None
    };

    let progress = bar.as_ref().map(|pb| {
        move |current: &str, p: &indexer::IndexProgress| {
            pb.set_length(p.total as u64);
            pb.set_position(p.processed as u64);
            // Bottom line: the current file during the scan, or the active
            // post-loop phase (e.g. "resolving references 412/1559…") once the
            // per-file scan is done — so the bar shows work rather than looking
            // hung while the graph is written and edges resolved.
            let bottom = match p.phase {
                Some(phase) => match p.phase_progress {
                    Some((done, total)) => {
                        format!("\x1b[2m{phase} {done}/{total}…\x1b[0m")
                    }
                    None => format!("\x1b[2m{phase}…\x1b[0m"),
                },
                None => format!("\x1b[2m{}\x1b[0m", truncate_middle(current, 64)),
            };
            pb.set_message(format!(
                "\x1b[36mfiles\x1b[0m {} \x1b[36msymbols\x1b[0m {} \x1b[36mprojects\x1b[0m {}\n  {}",
                p.files_indexed, p.symbols, p.projects, bottom,
            ));
        }
    });
    let progress_ref: Option<&indexer::ProgressFn<'_>> = match &progress {
        Some(f) => Some(f),
        None => None,
    };

    let outcome = indexer::index_repo(
        &repo,
        &config,
        store.as_ref(),
        args.force,
        args.changed,
        &now,
        progress_ref,
    )?;
    if let Some(pb) = &bar {
        pb.finish_and_clear();
    }
    let stats = store.stats()?;

    println!("Indexed {} files", fmt_num(outcome.files_indexed));
    println!("Found {} symbols", fmt_num(stats.symbols));
    println!("Found {} project/package edges", fmt_num(stats.edges));
    if outcome.files_skipped_unchanged > 0 {
        println!(
            "Skipped {} unchanged files",
            fmt_num(outcome.files_skipped_unchanged)
        );
    }
    if outcome.files_removed > 0 {
        println!("Removed {} deleted files", fmt_num(outcome.files_removed));
    }
    println!(
        "Graph stored at {}",
        repo.root.join(&config.graph.path).display()
    );

    if args.stats {
        println!();
        println!("Stats:");
        println!("  files:    {}", fmt_num(stats.files));
        println!("  symbols:  {}", fmt_num(stats.symbols));
        println!("  projects: {}", fmt_num(stats.projects));
        println!("  packages: {}", fmt_num(stats.packages));
        println!("  edges:    {}", fmt_num(stats.edges));
        println!("  candidates considered: {}", fmt_num(candidates.len()));
    }
    Ok(())
}

fn cmd_status(cwd: &Path, args: cli::StatusArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;
    let stats = store.stats()?;

    // Stale = candidate file whose current hash differs from the indexed hash.
    let candidates = repo.candidate_files(&config)?;
    let indexed = store.all_files()?;
    let tracked = git::tracked_files(&repo.root);
    let mut stale = Vec::new();
    let mut untracked = Vec::new();
    for rel in &candidates {
        if !tracked.contains(rel) {
            untracked.push(rel.clone());
        }
        if let Some(prev) = indexed.iter().find(|f| &f.path == rel) {
            if let Ok(bytes) = std::fs::read(repo.root.join(rel)) {
                let h = blake3::hash(&bytes).to_hex().to_string();
                if h != prev.hash {
                    stale.push(rel.clone());
                }
            }
        } else {
            stale.push(rel.clone());
        }
    }
    stale.sort();
    untracked.sort();

    let info = git::info(&repo.root);
    let last_index = indexed
        .iter()
        .map(|f| f.last_indexed_at.clone())
        .max()
        .unwrap_or_default();
    let ready = !indexed.is_empty();

    // Provenance: if this graph was pulled from a registry, surface its origin
    // commit and whether it matches local HEAD (staleness, even days later).
    let origin = read_graph_origin(&repo.root, &config);
    let origin_commit = origin
        .as_ref()
        .and_then(|o| o.get("commit").and_then(|c| c.as_str()))
        .map(|s| s.to_string());
    let origin_stale = match (&origin_commit, git::full_commit(&repo.root)) {
        (Some(g), Some(h)) => Some(!g.starts_with(&h) && !h.starts_with(g.as_str())),
        _ => None,
    };

    if args.stale {
        for s in &stale {
            println!("{s}");
        }
        return Ok(());
    }

    if args.json {
        let obj = serde_json::json!({
            "repo": if config.repo.name.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(config.repo.name.clone()) },
            "index": if ready { "ready" } else { "empty" },
            "graphBackend": config.graph.backend,
            "graphPath": config.graph.path,
            "filesIndexed": stats.files,
            "symbolsIndexed": stats.symbols,
            "projects": stats.projects,
            "packages": stats.packages,
            "referenceEdges": stats.reference_edges,
            // Languages for which usage/reference edges are extracted today.
            // Surfaced so partial coverage is visible rather than silent.
            "referenceLanguages": ["csharp", "rust", "typescript", "javascript"],
            "staleFiles": stale.len(),
            "untrackedFiles": untracked.len(),
            "branch": info.branch,
            "commit": info.commit,
            "lastIndex": last_index,
            "origin": origin,
            "originStale": origin_stale,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    let name = if config.repo.name.is_empty() {
        "(unnamed)"
    } else {
        &config.repo.name
    };
    println!("Repo: {name}");
    println!("Index: {}", if ready { "ready" } else { "empty" });
    println!("Graph backend: {}", config.graph.backend);
    println!("Graph path: {}", config.graph.path);
    println!("Files indexed: {}", fmt_num(stats.files));
    println!("Symbols indexed: {}", fmt_num(stats.symbols));
    println!("Reference edges: {}", fmt_num(stats.reference_edges));
    println!("Stale files: {}", fmt_num(stale.len()));
    println!("Untracked files: {}", fmt_num(untracked.len()));
    if !last_index.is_empty() {
        println!("Last index: {last_index}");
    }
    if let Some(o) = &origin {
        let reg = o.get("registry").and_then(|v| v.as_str()).unwrap_or("?");
        let repo_s = o.get("repository").and_then(|v| v.as_str()).unwrap_or("?");
        let tag = o.get("tag").and_then(|v| v.as_str()).unwrap_or("?");
        let commit = origin_commit.as_deref().unwrap_or("?");
        println!("Origin: {reg}/{repo_s}:{tag} (commit {commit})");
        if origin_stale == Some(true) {
            // Loud staleness note to stderr (data stays on stdout).
            eprintln!(
                "warning: pulled graph was indexed at commit {commit}, but local HEAD differs; run `synapse index` to refresh"
            );
        }
    }
    Ok(())
}

/// Path to the graph provenance sidecar written by `synapse pull`.
fn graph_origin_path(root: &Path, config: &SynapseConfig) -> std::path::PathBuf {
    root.join(&config.graph.path).join("origin.json")
}

/// Read the provenance sidecar (`origin.json`) if present; `None` otherwise.
fn read_graph_origin(root: &Path, config: &SynapseConfig) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(graph_origin_path(root, config)).ok()?;
    serde_json::from_str(&text).ok()
}

fn cmd_symbols(cwd: &Path, args: cli::SymbolsArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;

    let mut query = graph::model::SymbolSearchQuery {
        name: args.query.clone(),
        ..Default::default()
    };
    if let Some(k) = &args.kind {
        query.kind = graph::model::SymbolKind::from_str_opt(k);
    }
    if let Some(l) = &args.language {
        query.language = graph::model::Language::from_str_opt(l);
    }
    query.file = args.file.clone();

    let symbols = store.symbols_matching(&query)?;

    if args.json {
        println!("{}", output::json::to_string(&symbols)?);
        return Ok(());
    }

    let rows: Vec<Vec<String>> = symbols
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                s.kind.to_string(),
                s.language.to_string(),
                s.file_path.clone(),
            ]
        })
        .collect();
    print!(
        "{}",
        output::table::render(&["Symbol", "Kind", "Language", "File"], &rows)
    );
    Ok(())
}

fn cmd_related(cwd: &Path, args: cli::RelatedArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;

    let items = if let Some(sym) = &args.symbol {
        pack::selector::related_for_symbol(&repo, &config, store.as_ref(), sym, args.depth)?
    } else if let Some(file) = &args.file {
        pack::selector::related_for_file(&repo, &config, store.as_ref(), file, args.depth)?
    } else {
        anyhow::bail!("provide --symbol <name> or --file <path>");
    };

    if args.json {
        println!("{}", output::json::to_string(&items)?);
        return Ok(());
    }

    let seed = args
        .symbol
        .clone()
        .or(args.file.clone())
        .unwrap_or_default();
    println!("Related to {seed}\n");
    let mut last_depth = usize::MAX;
    for item in &items {
        if item.depth != last_depth {
            let header = if item.depth == 0 {
                "Direct:"
            } else {
                "Related:"
            };
            if last_depth != usize::MAX {
                println!();
            }
            println!("{header}");
            last_depth = item.depth;
        }
        println!("  {}", item.path);
        println!("    reason: {}", item.reason);
    }
    Ok(())
}

fn cmd_packages(cwd: &Path, args: cli::PackagesArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;

    // Impact analysis: which files import a given package.
    if let Some(pkg) = &args.importers {
        let files = store.files_importing_package(pkg)?;
        if args.json {
            println!("{}", output::json::to_string(&files)?);
            return Ok(());
        }
        if files.is_empty() {
            println!("No indexed files import `{pkg}`.");
        } else {
            println!("Files importing `{pkg}`:");
            for f in &files {
                println!("  {f}");
            }
        }
        return Ok(());
    }

    let matches_query = |name: &str| {
        args.query
            .as_deref()
            .is_none_or(|q| name.to_ascii_lowercase().contains(&q.to_ascii_lowercase()))
    };

    if args.projects {
        let mut projects = store.all_projects()?;
        projects.retain(|p| matches_query(&p.name) || matches_query(&p.path));
        projects.sort_by(|a, b| a.path.cmp(&b.path));
        if args.json {
            println!("{}", output::json::to_string(&projects)?);
            return Ok(());
        }
        let rows: Vec<Vec<String>> = projects
            .iter()
            .map(|p| {
                vec![
                    p.name.clone(),
                    p.language.to_string(),
                    p.kind.clone(),
                    p.path.clone(),
                ]
            })
            .collect();
        print!(
            "{}",
            output::table::render(&["Project", "Language", "Kind", "Path"], &rows)
        );
        return Ok(());
    }

    let mut packages = store.all_packages()?;
    packages.retain(|p| {
        matches_query(&p.name)
            && args
                .ecosystem
                .as_deref()
                .is_none_or(|e| p.ecosystem.eq_ignore_ascii_case(e))
    });
    packages.sort_by(|a, b| a.name.cmp(&b.name).then(a.version.cmp(&b.version)));

    if args.json {
        println!("{}", output::json::to_string(&packages)?);
        return Ok(());
    }
    let rows: Vec<Vec<String>> = packages
        .iter()
        .map(|p| {
            vec![
                p.name.clone(),
                if p.version.is_empty() {
                    "(unversioned)".to_string()
                } else {
                    p.version.clone()
                },
                p.ecosystem.clone(),
                p.dependency_kind.clone(),
            ]
        })
        .collect();
    print!(
        "{}",
        output::table::render(&["Package", "Version", "Ecosystem", "Kind"], &rows)
    );
    Ok(())
}

fn cmd_pack(cwd: &Path, args: cli::PackArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;

    let mode = if args.changed {
        pack::PackMode::Changed
    } else if let Some(p) = &args.path {
        pack::PackMode::Path(p.clone())
    } else if let Some(s) = &args.symbol {
        pack::PackMode::Symbol(s.clone())
    } else if let Some(q) = &args.query {
        pack::PackMode::Query(q.clone())
    } else {
        anyhow::bail!("provide one of --changed, --path, --symbol or --query");
    };

    // Format: explicit --format wins, else the config default, else Markdown.
    let format_str = args
        .format
        .clone()
        .unwrap_or_else(|| config.pack.default_format.clone());
    let format = pack::PackFormat::from_str_opt(&format_str).ok_or_else(|| {
        anyhow::anyhow!("unknown pack format `{format_str}` (use markdown or json)")
    })?;

    let request = pack::PackRequest {
        mode,
        budget: args.budget.unwrap_or(config.pack.default_budget),
        depth: args.depth,
        include_tests: args.include_tests,
        include_config: args.include_config,
        include_diff: args.include_diff,
        dry_run: args.dry_run,
        explain: args.explain,
        format,
    };

    let info = git::info(&repo.root);
    let result = pack::build_pack(&repo, &config, store.as_ref(), &info, &request)?;

    if args.explain || args.dry_run {
        eprintln!("Selection ({} tokens):", result.total_tokens);
        for f in &result.selection {
            eprintln!(
                "  [{}] {} (~{} tok){}",
                f.tier,
                f.path,
                f.estimated_tokens,
                if f.included { "" } else { " [trimmed]" }
            );
            eprintln!("       {}", f.reason);
        }
    }

    if let Some(out) = &args.output {
        let path = repo.root.join(out);
        std::fs::write(&path, &result.rendered)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "Wrote {} ({} files, ~{} tokens)",
            path.display(),
            result.selection.iter().filter(|f| f.included).count(),
            result.total_tokens
        );
    } else {
        print!("{}", result.rendered);
    }
    Ok(())
}

fn cmd_explore(cwd: &Path, args: cli::ExploreArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;

    let opts = explore::ExploreOptions {
        port: args.port,
        read_write: args.read_write,
        detach: args.detach,
        in_memory: args.in_memory,
        image: args.image,
        tag: args.tag,
    };

    // The graph dir is required unless launching an empty in-memory instance.
    let dir = if opts.in_memory {
        repo.root.join(&config.graph.path)
    } else {
        explore::graph_dir(&repo, &config)?
    };

    // --print: just show the command and exit (no Docker needed).
    if args.print {
        println!("{}", explore::docker_command_string(&opts, &dir));
        return Ok(());
    }

    if !explore::docker_available() {
        eprintln!(
            "error: Docker is not available (is it installed and running?).\n\
             Install Docker from https://docs.docker.com/get-docker/, or run Explorer manually:\n\n    {}\n",
            explore::docker_command_string(&opts, &dir)
        );
        std::process::exit(1);
    }

    let url = format!("http://localhost:{}", opts.port);
    if opts.detach {
        eprintln!("Starting Ladybug Explorer (detached) on {url} …");
    } else {
        eprintln!("Launching Ladybug Explorer on {url} (Ctrl-C to stop) …");
        if !opts.read_write && !opts.in_memory {
            eprintln!("Mounting the index read-only. Pass --read-write to allow edits.");
        }
        eprintln!(
            "Note: if you see a storage-format error, the Explorer image may not match \
             this build's LadybugDB; try `--tag dev` or a matching version."
        );
    }

    explore::launch(&opts, &dir)?;

    if opts.detach {
        eprintln!("Started. Open {url} — stop it with `docker ps` + `docker stop <id>`.");
    }
    Ok(())
}

/// Resolve registry + repository from config with per-invocation overrides.
/// Errors if neither config nor override supplies both.
#[cfg(feature = "share")]
fn resolve_share_coords(
    config: &SynapseConfig,
    registry_override: Option<String>,
    repository_override: Option<String>,
) -> Result<(String, String)> {
    let registry = registry_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| config.share.registry.clone());
    let repository = repository_override
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| config.share.repository.clone());
    if registry.is_empty() || repository.is_empty() {
        return Err(errors::SynapseError::ShareNotConfigured.into());
    }
    Ok((registry, repository))
}

#[cfg(feature = "share")]
fn cmd_pull(cwd: &Path, args: cli::PullArgs) -> Result<()> {
    use synapse::share;

    let (repo, config) = require_repo(cwd)?;
    let (registry, repository) =
        resolve_share_coords(&config, args.registry.clone(), args.repository.clone())?;

    let tag = share::resolve_pull_tag(args.tag.as_deref(), &config.share.moving_tag);
    let target = share::ShareTarget {
        registry,
        repository,
        tag,
    };

    eprintln!("Pulling graph: {}", target.display());
    let pulled = share::pull_graph(&config.share, &target)?;

    // Atomic write: temp file + rename, so an interrupted pull never leaves a
    // half-written graph in place.
    let graph_dir = repo.root.join(&config.graph.path);
    std::fs::create_dir_all(&graph_dir)
        .with_context(|| format!("creating {}", graph_dir.display()))?;
    let final_path = graph_dir.join("synapse.lbug");
    if final_path.exists() {
        eprintln!(
            "warning: overwriting existing local graph at {}",
            final_path.display()
        );
    }
    let tmp_path = graph_dir.join("synapse.lbug.tmp");
    std::fs::write(&tmp_path, &pulled.bytes)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("replacing {}", final_path.display()))?;

    // Provenance sidecar.
    let origin = serde_json::json!({
        "source": "registry",
        "registry": target.registry,
        "repository": target.repository,
        "tag": target.tag,
        "commit": pulled.meta.commit,
        "branch": pulled.meta.branch,
        "synapseVersion": pulled.meta.synapse_version,
        "blake3": pulled.meta.blob_blake3,
        "pulledAt": now_rfc3339(),
    });
    let _ = std::fs::write(
        graph_origin_path(&repo.root, &config),
        serde_json::to_string_pretty(&origin).unwrap_or_default(),
    );

    // Staleness: warn loudly if the graph's commit differs from local HEAD.
    let head_full = git::full_commit(&repo.root);
    match share::compare_commit(pulled.meta.commit.as_deref(), head_full.as_deref()) {
        share::GraphFreshness::Mismatch {
            graph_commit,
            head_commit,
        } => {
            eprintln!(
                "warning: pulled graph was indexed at commit {graph_commit}, but local HEAD is {head_commit}; \
                 symbols/edges may not reflect your working tree — run `synapse index` to refresh"
            );
        }
        share::GraphFreshness::Match => {
            eprintln!("Graph matches local HEAD commit.");
        }
        share::GraphFreshness::Unknown => {}
    }

    println!(
        "Pulled {} bytes to {} (tag {})",
        fmt_num(pulled.bytes.len()),
        final_path.display(),
        target.tag,
    );
    Ok(())
}

#[cfg(feature = "share")]
fn cmd_push(cwd: &Path, args: cli::PushArgs) -> Result<()> {
    use synapse::share;

    let (repo, config) = require_repo(cwd)?;

    // Guard 1: push must be explicitly enabled in config.
    if !config.share.push_enabled {
        return Err(errors::SynapseError::PushDisabled.into());
    }
    // Guard 2: registry + repository configured.
    let (registry, repository) =
        resolve_share_coords(&config, args.registry.clone(), args.repository.clone())?;

    // Guard 3: the graph must exist.
    let graph_path = repo.root.join(&config.graph.path).join("synapse.lbug");
    let bytes = std::fs::read(&graph_path).map_err(|_| {
        anyhow::anyhow!(
            "no graph at {} — run `synapse index` first",
            graph_path.display()
        )
    })?;

    // Guard 4: clean working tree (unless --allow-dirty), so the commit tag
    // actually describes what's in the graph.
    if !args.allow_dirty {
        let changed = git::changed_files(&repo.root);
        if !changed.is_empty() {
            return Err(errors::SynapseError::DirtyTree(changed.len()).into());
        }
    }

    // Guard 5: a real commit to tag by.
    let full_commit = git::full_commit(&repo.root).ok_or_else(|| {
        anyhow::anyhow!("cannot determine HEAD commit; not a git repo with commits")
    })?;
    let short_commit = git::info(&repo.root).commit;
    let tags = share::push_tags(
        args.tag.as_deref(),
        short_commit.as_deref(),
        &config.share.moving_tag,
    );

    // Guard 6: interactive type-to-confirm (unless --yes). Refuse on a
    // non-interactive stdin rather than hang.
    if !args.yes {
        if !std::io::stdin().is_terminal() {
            return Err(errors::SynapseError::PushNotConfirmed.into());
        }
        eprintln!("About to push the graph to:");
        for t in &tags {
            eprintln!("  {registry}/{repository}:{t}");
        }
        eprint!("Type the repository ({repository}) to confirm: ");
        use std::io::Write;
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("reading confirmation")?;
        if line.trim() != repository {
            return Err(errors::SynapseError::PushNotConfirmed.into());
        }
    }

    let info = git::info(&repo.root);
    let meta = share::GraphArtifactMeta {
        commit: Some(full_commit),
        branch: info.branch,
        repo_name: if config.repo.name.is_empty() {
            None
        } else {
            Some(config.repo.name.clone())
        },
        synapse_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        blob_blake3: Some(blake3::hash(&bytes).to_hex().to_string()),
        created_at: Some(now_rfc3339()),
    };

    eprintln!("Pushing graph ({} bytes)…", fmt_num(bytes.len()));
    let outcome = share::push_graph(&config.share, &registry, &repository, &tags, bytes, &meta)?;

    println!(
        "Pushed {} to {}/{} (tags: {})",
        outcome.digest,
        registry,
        repository,
        outcome.references.join(", "),
    );
    Ok(())
}

fn cmd_clean(cwd: &Path, args: cli::CleanArgs) -> Result<()> {
    let (repo, _config) = require_repo(cwd)?;
    let dir = config::synapse_dir(&repo.root);

    let do_cache = args.cache || args.all;
    let do_index = args.index || args.all;
    let do_packs = args.packs || args.all;
    if !(do_cache || do_index || do_packs) {
        anyhow::bail!("specify what to clean: --cache, --index, --packs, or --all");
    }

    let remove = |sub: &str| -> Result<()> {
        let target = dir.join(sub);
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("removing {}", target.display()))?;
            std::fs::create_dir_all(&target).ok();
            println!("Cleaned {}", target.display());
        }
        Ok(())
    };
    if do_cache {
        remove("cache")?;
    }
    if do_index {
        remove("graph")?;
    }
    if do_packs {
        remove("packs")?;
    }
    Ok(())
}

/// Format an integer with thousands separators (deterministic, locale-free).
fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Shorten `s` to at most `max` chars, eliding the middle with `…` so both the
/// leading directory and the file name stay visible. Operates on chars to stay
/// UTF-8 safe.
fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let keep = max - 1;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let head_s: String = chars[..head].iter().collect();
    let tail_s: String = chars[chars.len() - tail..].iter().collect();
    format!("{head_s}…{tail_s}")
}
