//! Token estimation and greedy budget-fitting for context packs.
//!
//! Estimation is deliberately crude and deterministic: roughly four characters
//! per token, with a small fixed per-file overhead to account for the Markdown
//! heading and code fence that wrap each file. Fitting preserves the incoming
//! candidate order (tier, then path) — that order *is* the priority, so we never
//! re-sort by size.

use crate::pack::{PackRequest, SelectedFile};
use crate::repo::Repo;

/// Fixed per-file token overhead for the `### path` heading and code fence.
const PER_FILE_OVERHEAD: usize = 8;

/// Estimate the number of tokens in `text` as roughly `chars / 4`.
///
/// Saturating: non-empty text always estimates at least one token.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    (text.chars().count() / 4).max(1)
}

/// Greedily fit `candidates` within `req.budget`, preserving their order.
///
/// For each candidate we read its file contents (lossy UTF-8) from
/// `repo.root.join(path)` and estimate its token cost. Files are marked
/// `included` while the running total stays within budget; the first file that
/// would overflow (and every file after it) is marked `included = false` but is
/// still returned so the selection summary can show it as trimmed.
///
/// Returns the updated candidate list and the total tokens of *included* files.
pub fn fit(
    repo: &Repo,
    candidates: &[SelectedFile],
    req: &PackRequest,
) -> (Vec<SelectedFile>, usize) {
    let mut out: Vec<SelectedFile> = Vec::with_capacity(candidates.len());
    let mut running: usize = 0;

    for candidate in candidates {
        let mut sel = candidate.clone();

        let full = repo.root.join(&sel.path);
        let contents = std::fs::read(&full)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();

        // Cost of this file: body estimate plus a small fixed overhead, except
        // an unreadable/empty file contributes only its (zero) body estimate.
        let body = estimate_tokens(&contents);
        sel.estimated_tokens = if body == 0 {
            0
        } else {
            body + PER_FILE_OVERHEAD
        };

        // Greedy fit in tier/path order. Dry-run still computes costs but the
        // renderer omits bodies; we keep the same accounting so the summary is
        // consistent either way.
        let prospective = running.saturating_add(sel.estimated_tokens);
        if prospective <= req.budget {
            sel.included = true;
            running = prospective;
        } else {
            sel.included = false;
        }

        out.push(sel);
    }

    (out, running)
}
