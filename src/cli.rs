//! Command-line argument parsing. This module defines the clap structures only;
//! all behaviour lives in `main.rs` command handlers and the feature modules.

use clap::{Parser, Subcommand};

/// SimCube Synapse — a deterministic local repository context compiler.
#[derive(Debug, Parser)]
#[command(name = "synapse", version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create local config and storage directories.
    Init(InitArgs),
    /// Index the current repository into the graph.
    Index(IndexArgs),
    /// Show whether the index is ready or stale.
    Status(StatusArgs),
    /// Search/list indexed symbols.
    Symbols(SymbolsArgs),
    /// Find files related to a symbol or file.
    Related(RelatedArgs),
    /// List indexed projects and their package dependencies.
    Packages(PackagesArgs),
    /// Emit a compact Markdown context pack for an LLM.
    Pack(PackArgs),
    /// Launch Ladybug Explorer (Docker) to visualize the indexed graph.
    Explore(ExploreArgs),
    /// Remove cached/index/pack data.
    Clean(CleanArgs),
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Overwrite an existing config.
    #[arg(long)]
    pub force: bool,
    /// Set the repository name in the new config.
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct IndexArgs {
    /// Re-index every file, ignoring content hashes.
    #[arg(long)]
    pub force: bool,
    /// Only index files git reports as changed.
    #[arg(long)]
    pub changed: bool,
    /// Print detailed statistics after indexing.
    #[arg(long)]
    pub stats: bool,
    /// Suppress the progress bar (also auto-disabled when stderr isn't a TTY).
    #[arg(long)]
    pub quiet: bool,
}

#[derive(Debug, clap::Args)]
pub struct StatusArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
    /// List the stale files instead of a summary.
    #[arg(long)]
    pub stale: bool,
}

#[derive(Debug, clap::Args)]
pub struct SymbolsArgs {
    /// Case-insensitive substring to match against symbol names.
    pub query: Option<String>,
    /// Filter by symbol kind (e.g. class, function, component).
    #[arg(long)]
    pub kind: Option<String>,
    /// Filter to a specific repo-relative file path.
    #[arg(long)]
    pub file: Option<String>,
    /// Filter by language (e.g. csharp, typescript).
    #[arg(long)]
    pub language: Option<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct RelatedArgs {
    /// Start from this symbol name.
    #[arg(long)]
    pub symbol: Option<String>,
    /// Start from this repo-relative file path.
    #[arg(long)]
    pub file: Option<String>,
    /// Maximum graph/heuristic depth to explore.
    #[arg(long, default_value_t = 1)]
    pub depth: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct PackagesArgs {
    /// Case-insensitive substring to match against package/project names.
    pub query: Option<String>,
    /// Filter by ecosystem (e.g. nuget, npm).
    #[arg(long)]
    pub ecosystem: Option<String>,
    /// List projects instead of packages.
    #[arg(long)]
    pub projects: bool,
    /// List the files that import the named package (impact analysis).
    #[arg(long, value_name = "PACKAGE")]
    pub importers: Option<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct PackArgs {
    /// Include files git reports as changed.
    #[arg(long)]
    pub changed: bool,
    /// Include files under this path.
    #[arg(long)]
    pub path: Option<String>,
    /// Centre the pack on this symbol plus related files.
    #[arg(long)]
    pub symbol: Option<String>,
    /// Case-insensitive text query across paths, symbols and contents.
    #[arg(long)]
    pub query: Option<String>,
    /// Approximate token budget (chars / 4).
    #[arg(long)]
    pub budget: Option<usize>,
    /// Relation depth when expanding from a symbol/path.
    #[arg(long, default_value_t = 1)]
    pub depth: usize,
    /// Write the pack to this file instead of stdout.
    #[arg(long)]
    pub output: Option<String>,
    /// Output format: `markdown` (default) or `json`.
    #[arg(long)]
    pub format: Option<String>,
    /// Include likely test files.
    #[arg(long)]
    pub include_tests: bool,
    /// Include config/registration files.
    #[arg(long)]
    pub include_config: bool,
    /// Include a git diff section.
    #[arg(long)]
    pub include_diff: bool,
    /// Show the selection summary without writing file contents.
    #[arg(long)]
    pub dry_run: bool,
    /// Print the ranking reasons for each selected file.
    #[arg(long)]
    pub explain: bool,
}

#[derive(Debug, clap::Args)]
pub struct ExploreArgs {
    /// Host port to expose Ladybug Explorer on.
    #[arg(long, default_value_t = 8000)]
    pub port: u16,
    /// Allow Explorer to modify the index (default is read-only).
    #[arg(long)]
    pub read_write: bool,
    /// Run the container detached (in the background) instead of foreground.
    #[arg(long)]
    pub detach: bool,
    /// Launch with an empty in-memory database instead of the indexed graph.
    #[arg(long)]
    pub in_memory: bool,
    /// Explorer image (without tag).
    #[arg(long, default_value = "ghcr.io/ladybugdb/explorer")]
    pub image: String,
    /// Explorer image tag (e.g. `latest`, `dev`).
    #[arg(long, default_value = "latest")]
    pub tag: String,
    /// Print the `docker run` command instead of executing it.
    #[arg(long)]
    pub print: bool,
}

#[derive(Debug, clap::Args)]
pub struct CleanArgs {
    /// Remove the cache directory.
    #[arg(long)]
    pub cache: bool,
    /// Remove the graph/index directory.
    #[arg(long)]
    pub index: bool,
    /// Remove generated packs.
    #[arg(long)]
    pub packs: bool,
    /// Remove cache, index and packs.
    #[arg(long)]
    pub all: bool,
}
