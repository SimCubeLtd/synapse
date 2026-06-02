//! Git metadata via shelling out to `git`. This is the only module that runs
//! git. Every function degrades gracefully (returns `None`/empty) when the repo
//! is not a git repository or git is unavailable — Synapse never requires git.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

/// Snapshot of git facts used by `status` and pack metadata.
#[derive(Debug, Clone, Default)]
pub struct GitInfo {
    pub branch: Option<String>,
    pub commit: Option<String>,
}

/// Run `git <args>` in `root`, returning trimmed stdout on success.
fn run(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Current branch and short commit hash.
pub fn info(root: &Path) -> GitInfo {
    GitInfo {
        branch: run(root, &["branch", "--show-current"]).filter(|s| !s.is_empty()),
        commit: run(root, &["rev-parse", "--short", "HEAD"]).filter(|s| !s.is_empty()),
    }
}

/// The full (40-char) commit SHA of HEAD, or `None` outside a git repo / with no
/// commits. Used as the canonical identity for a shared graph artifact.
pub fn full_commit(root: &Path) -> Option<String> {
    run(root, &["rev-parse", "HEAD"]).filter(|s| !s.is_empty())
}

/// True if `root` is inside a git work tree.
pub fn is_git_repo(root: &Path) -> bool {
    run(root, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s == "true")
        .unwrap_or(false)
}

/// The set of git-tracked files (repo-relative, forward slashes).
pub fn tracked_files(root: &Path) -> HashSet<String> {
    match run(root, &["ls-files"]) {
        Some(s) => s.lines().map(|l| l.replace('\\', "/")).collect(),
        None => HashSet::new(),
    }
}

/// Files that differ from `HEAD` (staged + unstaged) plus untracked files.
///
/// Returns repo-relative, forward-slash paths. Empty when not a git repo.
pub fn changed_files(root: &Path) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    // Tracked changes vs HEAD (staged and working tree).
    if let Some(s) = run(root, &["diff", "--name-only", "HEAD"]) {
        set.extend(s.lines().map(|l| l.replace('\\', "/")));
    }
    // Unstaged changes (covers the case where HEAD diff missed something).
    if let Some(s) = run(root, &["diff", "--name-only"]) {
        set.extend(s.lines().map(|l| l.replace('\\', "/")));
    }
    // Untracked, not ignored.
    if let Some(s) = run(root, &["ls-files", "--others", "--exclude-standard"]) {
        set.extend(s.lines().map(|l| l.replace('\\', "/")));
    }
    let mut out: Vec<String> = set.into_iter().filter(|p| !p.is_empty()).collect();
    out.sort();
    out
}

/// Full working-tree diff against HEAD (for `pack --include-diff`).
pub fn diff(root: &Path) -> Option<String> {
    run(root, &["diff", "HEAD"]).filter(|s| !s.is_empty())
}
