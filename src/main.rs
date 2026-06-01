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

    println!("Initialized Synapse workspace at {}", dir.display());
    println!("  config: {}", cfg_path.display());
    println!("  graph:  {}", dir.join("graph").display());
    Ok(())
}

fn cmd_index(cwd: &Path, args: cli::IndexArgs) -> Result<()> {
    let (repo, config) = require_repo(cwd)?;
    let store = open_store(&repo.root, &config)?;

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
            // Colored live stats line + the current file, both below the bar.
            pb.set_message(format!(
                "\x1b[36mfiles\x1b[0m {} \x1b[36msymbols\x1b[0m {} \x1b[36mprojects\x1b[0m {}\n  \x1b[2m{}\x1b[0m",
                p.files_indexed,
                p.symbols,
                p.projects,
                truncate_middle(current, 64),
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
    Ok(())
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
