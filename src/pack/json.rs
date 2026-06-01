//! Renders a selected pack as structured JSON.
//!
//! This is the machine-readable counterpart to [`super::markdown`]: same
//! selection, metadata and symbols, plus full file contents (omitted in
//! `--dry-run`). Intended for programmatic callers (agents, scripts) that want
//! to consume a pack without re-parsing Markdown.

use crate::config::SynapseConfig;
use crate::git::{self, GitInfo};
use crate::graph::GraphStore;
use crate::graph::model::SymbolSearchQuery;
use crate::indexer::languages;
use crate::pack::{PackMode, PackRequest, SelectedFile};
use crate::repo::Repo;
use anyhow::Result;
use serde_json::{Value, json};

/// Render the selected files (and supporting metadata) as a JSON context pack.
/// Deterministic across runs for identical inputs.
pub fn render(
    repo: &Repo,
    config: &SynapseConfig,
    store: &dyn GraphStore,
    git: &GitInfo,
    req: &PackRequest,
    sel: &[SelectedFile],
) -> Result<String> {
    // ---- Selection (every candidate, trimmed ones included with a flag) ----
    let selection: Vec<Value> = sel
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "reason": f.reason,
                "tier": f.tier,
                "estimatedTokens": f.estimated_tokens,
                "included": f.included,
            })
        })
        .collect();

    // ---- Symbols declared in the included files ---------------------------
    let included_paths: Vec<&str> = sel
        .iter()
        .filter(|f| f.included)
        .map(|f| f.path.as_str())
        .collect();

    let mut symbols = Vec::new();
    for path in &included_paths {
        let query = SymbolSearchQuery {
            file: Some((*path).to_string()),
            ..Default::default()
        };
        symbols.extend(store.symbols_matching(&query)?);
    }
    symbols.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.name.cmp(&b.name))
            .then(a.id.cmp(&b.id))
    });
    let symbols_json: Vec<Value> = symbols
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "fullName": s.full_name,
                "kind": s.kind.as_str(),
                "language": s.language.as_str(),
                "file": s.file_path,
                "startLine": s.start_line,
                "endLine": s.end_line,
                "exported": s.exported,
            })
        })
        .collect();

    // ---- Files (full contents, unless dry-run) ----------------------------
    let files_json: Vec<Value> = if req.dry_run {
        Vec::new()
    } else {
        sel.iter()
            .filter(|f| f.included)
            .map(|f| {
                let full = repo.root.join(&f.path);
                let contents = std::fs::read(&full)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
                json!({
                    "path": f.path,
                    "reason": f.reason,
                    "language": languages::detect(&f.path).as_str(),
                    "contents": contents,
                })
            })
            .collect()
    };

    // ---- Assemble the document --------------------------------------------
    let repo_name = if config.repo.name.is_empty() {
        Value::Null
    } else {
        Value::String(config.repo.name.clone())
    };

    let mut doc = json!({
        "tool": "synapse",
        "request": {
            "mode": req.mode.label(),
            "query": req.mode.query_text(),
            "budget": req.budget,
            "dryRun": req.dry_run,
        },
        "repository": {
            "name": repo_name,
            "branch": git.branch,
            "commit": git.commit,
            "changedFilesIncluded": req.mode == PackMode::Changed,
            "graphBackend": "LadybugDB",
        },
        "selection": selection,
        "symbols": symbols_json,
        "files": files_json,
    });

    // ---- Diff (optional) --------------------------------------------------
    if req.include_diff
        && let Some(diff_text) = git::diff(&repo.root)
    {
        doc["diff"] = Value::String(diff_text);
    }

    Ok(serde_json::to_string_pretty(&doc)?)
}
