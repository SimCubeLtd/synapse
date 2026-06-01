//! Context-pack generation: select relevant files from the graph, fit them to a
//! token budget, and render portable Markdown. The graph is used purely for
//! *selection*; the emitted pack is boring, self-contained Markdown.

pub mod budget;
pub mod json;
pub mod markdown;
pub mod selector;

use crate::config::SynapseConfig;
use crate::git::GitInfo;
use crate::graph::GraphStore;
use crate::repo::Repo;
use anyhow::Result;
use serde::Serialize;

/// Output format for a context pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackFormat {
    Markdown,
    Json,
}

impl PackFormat {
    /// Parse from a CLI/config string; defaults to Markdown for unknown values.
    pub fn from_str_opt(s: &str) -> Option<PackFormat> {
        match s.to_ascii_lowercase().as_str() {
            "markdown" | "md" => Some(PackFormat::Markdown),
            "json" => Some(PackFormat::Json),
            _ => None,
        }
    }
}

/// How the pack was scoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackMode {
    Changed,
    Path(String),
    Symbol(String),
    Query(String),
}

impl PackMode {
    pub fn label(&self) -> &'static str {
        match self {
            PackMode::Changed => "changed",
            PackMode::Path(_) => "path",
            PackMode::Symbol(_) => "symbol",
            PackMode::Query(_) => "query",
        }
    }

    pub fn query_text(&self) -> String {
        match self {
            PackMode::Changed => "(changed files)".to_string(),
            PackMode::Path(p) => p.clone(),
            PackMode::Symbol(s) => s.clone(),
            PackMode::Query(q) => q.clone(),
        }
    }
}

/// A fully-resolved pack request.
#[derive(Debug, Clone)]
pub struct PackRequest {
    pub mode: PackMode,
    pub budget: usize,
    pub depth: usize,
    pub include_tests: bool,
    pub include_config: bool,
    pub include_diff: bool,
    pub dry_run: bool,
    pub explain: bool,
    pub format: PackFormat,
}

/// A file selected for inclusion, with its reason and estimated cost.
#[derive(Debug, Clone, Serialize)]
pub struct SelectedFile {
    pub path: String,
    pub reason: String,
    /// Ranking tier (lower = higher priority).
    pub tier: u8,
    pub estimated_tokens: usize,
    /// Whether the file's full contents were included (false when budget-trimmed
    /// or in dry-run).
    pub included: bool,
}

/// Result of running a pack: the rendered text (Markdown or JSON) plus the
/// selection table.
#[derive(Debug, Clone)]
pub struct PackResult {
    pub rendered: String,
    pub selection: Vec<SelectedFile>,
    pub total_tokens: usize,
}

/// Build a context pack end to end, rendering in the requested format.
pub fn build_pack(
    repo: &Repo,
    config: &SynapseConfig,
    store: &dyn GraphStore,
    git_info: &GitInfo,
    request: &PackRequest,
) -> Result<PackResult> {
    let candidates = selector::select(repo, config, store, request)?;
    let (selection, total) = budget::fit(repo, &candidates, request);
    let rendered = match request.format {
        PackFormat::Markdown => {
            markdown::render(repo, config, store, git_info, request, &selection)?
        }
        PackFormat::Json => json::render(repo, config, store, git_info, request, &selection)?,
    };
    Ok(PackResult {
        rendered,
        selection,
        total_tokens: total,
    })
}
