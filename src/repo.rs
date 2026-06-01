//! Repository discovery and candidate-file enumeration.
//!
//! Finds the repo root (the nearest ancestor containing `.synapse`, falling back
//! to a `.git` directory or the current dir) and walks the tree with the
//! `ignore` crate, honouring `.gitignore` plus the Synapse include/exclude
//! globs from config.

use crate::config::{SYNAPSE_DIR, SynapseConfig};
use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// A repository rooted at a discovered directory.
pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// Discover the repo root by walking up from `start`.
    ///
    /// Prefers a directory containing `.synapse`; otherwise the nearest `.git`;
    /// otherwise `start` itself.
    pub fn discover(start: &Path) -> Result<Repo> {
        let start = std::fs::canonicalize(start)
            .with_context(|| format!("resolving {}", start.display()))?;
        let mut synapse_root = None;
        let mut git_root = None;
        for dir in start.ancestors() {
            if synapse_root.is_none() && dir.join(SYNAPSE_DIR).is_dir() {
                synapse_root = Some(dir.to_path_buf());
            }
            if git_root.is_none() && dir.join(".git").exists() {
                git_root = Some(dir.to_path_buf());
            }
        }
        let root = synapse_root.or(git_root).unwrap_or(start);
        Ok(Repo { root })
    }

    /// True if `synapse init` has run here.
    pub fn is_initialized(&self) -> bool {
        crate::config::config_path(&self.root).is_file()
    }

    /// Enumerate candidate files (repo-relative, forward-slash paths) that match
    /// the config include globs and survive the exclude globs. Honours
    /// `.gitignore` via the `ignore` walker.
    pub fn candidate_files(&self, config: &SynapseConfig) -> Result<Vec<String>> {
        let include = build_globset(&config.index.include)?;
        let exclude = build_globset(&config.index.exclude)?;

        let mut out = Vec::new();
        let walker = WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .parents(true)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = match entry.path().strip_prefix(&self.root) {
                Ok(p) => normalize(p),
                Err(_) => continue,
            };
            // Always skip anything inside the synapse dir.
            if rel.starts_with(&format!("{SYNAPSE_DIR}/")) || rel == SYNAPSE_DIR {
                continue;
            }
            if path_matches(&include, &exclude, &rel) {
                out.push(rel);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// Build a [`GlobSet`] from glob patterns.
pub fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("invalid glob `{p}`"))?);
    }
    b.build().context("building glob set")
}

/// Decide whether a repo-relative path is included and not excluded.
pub fn path_matches(include: &GlobSet, exclude: &GlobSet, rel: &str) -> bool {
    if exclude.is_match(rel) {
        return false;
    }
    include.is_match(rel)
}

/// Normalize a path to forward slashes (for cross-platform deterministic ids).
pub fn normalize(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}
