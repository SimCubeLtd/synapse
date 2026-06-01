//! In-memory [`GraphStore`] for tests and small fixtures only.
//!
//! The production CLI path uses LadybugDB; this store exists so unit tests can
//! exercise indexing, querying and packing without building the C++ backend.

use crate::graph::model::{
    FileSearchQuery, IndexStats, IndexedFile, IndexedPackage, IndexedProject, IndexedSymbol,
    RelatedItem, SymbolSearchQuery,
};
use crate::graph::store::GraphStore;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

#[derive(Default)]
struct Inner {
    files: BTreeMap<String, IndexedFile>,
    symbols: BTreeMap<String, IndexedSymbol>,
    projects: BTreeMap<String, IndexedProject>,
    packages: BTreeMap<String, IndexedPackage>,
    /// file_id -> set of symbol ids declared in that file.
    file_symbols: BTreeMap<String, BTreeSet<String>>,
    project_files: BTreeSet<(String, String)>,
    project_refs: BTreeSet<(String, String)>,
    project_pkgs: BTreeSet<(String, String)>,
    file_pkgs: BTreeSet<(String, String)>,
    sym_inherits: BTreeSet<(String, String)>,
    sym_implements: BTreeSet<(String, String)>,
    sym_references: BTreeSet<(String, String)>,
}

/// Simple, thread-safe, fully-functional in-memory store.
#[derive(Default)]
pub struct MemoryGraphStore {
    inner: Mutex<Inner>,
}

impl MemoryGraphStore {
    pub fn new() -> Self {
        Self::default()
    }
}

fn ci_contains(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

impl GraphStore for MemoryGraphStore {
    fn initialize_schema(&self) -> Result<()> {
        Ok(())
    }

    fn upsert_file(&self, file: IndexedFile) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.files.insert(file.path.clone(), file);
        Ok(())
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        let fid = format!("file:{path}");
        g.files.remove(path);
        if let Some(syms) = g.file_symbols.remove(&fid) {
            for sid in syms {
                g.symbols.remove(&sid);
            }
        }
        // Also drop any symbols whose file_path matches (belt and braces).
        g.symbols.retain(|_, s| s.file_path != path);
        // Drop the file's IMPORTS_PACKAGE edges and project membership.
        g.file_pkgs.retain(|(f, _)| f != &fid);
        g.project_files.retain(|(_, f)| f != &fid);
        // Drop symbol-level edges originating from this file's symbols.
        let sym_prefix = format!("sym:{path}#");
        g.sym_inherits
            .retain(|(from, _)| !from.starts_with(&sym_prefix));
        g.sym_implements
            .retain(|(from, _)| !from.starts_with(&sym_prefix));
        g.sym_references
            .retain(|(from, _)| !from.starts_with(&sym_prefix));
        Ok(())
    }

    fn upsert_symbol(&self, symbol: IndexedSymbol) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.symbols.insert(symbol.id.clone(), symbol);
        Ok(())
    }

    fn upsert_project(&self, project: IndexedProject) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.projects.insert(project.id.clone(), project);
        Ok(())
    }

    fn upsert_package(&self, package: IndexedPackage) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.packages.insert(package.id.clone(), package);
        Ok(())
    }

    fn link_project_contains_file(&self, project_id: &str, file_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.project_files
            .insert((project_id.to_string(), file_id.to_string()));
        Ok(())
    }

    fn link_file_declares_symbol(&self, file_id: &str, symbol_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.file_symbols
            .entry(file_id.to_string())
            .or_default()
            .insert(symbol_id.to_string());
        Ok(())
    }

    fn link_project_references_project(
        &self,
        from_project_id: &str,
        to_project_id: &str,
    ) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.project_refs
            .insert((from_project_id.to_string(), to_project_id.to_string()));
        Ok(())
    }

    fn link_project_uses_package(&self, project_id: &str, package_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.project_pkgs
            .insert((project_id.to_string(), package_id.to_string()));
        Ok(())
    }

    fn link_file_imports_package(&self, file_id: &str, package_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.file_pkgs
            .insert((file_id.to_string(), package_id.to_string()));
        Ok(())
    }

    fn link_symbol_inherits(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.sym_inherits
            .insert((from_symbol_id.to_string(), to_symbol_id.to_string()));
        Ok(())
    }

    fn link_symbol_implements(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.sym_implements
            .insert((from_symbol_id.to_string(), to_symbol_id.to_string()));
        Ok(())
    }

    fn link_symbol_references(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()> {
        let mut g = self.inner.lock().unwrap();
        g.sym_references
            .insert((from_symbol_id.to_string(), to_symbol_id.to_string()));
        Ok(())
    }

    fn symbol_references(&self, symbol_name: &str) -> Result<Vec<RelatedItem>> {
        let g = self.inner.lock().unwrap();
        // Symbol ids declaring this name (the reference targets).
        let ids: BTreeSet<&str> = g
            .symbols
            .values()
            .filter(|s| s.name == symbol_name)
            .map(|s| s.id.as_str())
            .collect();
        let reason = format!("references {symbol_name}");
        let mut out: Vec<RelatedItem> = Vec::new();
        for (from, to) in &g.sym_references {
            if ids.contains(to.as_str())
                && let Some(s) = g.symbols.get(from)
            {
                out.push(RelatedItem {
                    path: s.file_path.clone(),
                    reason: reason.clone(),
                    depth: 1,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        Ok(out)
    }

    fn symbol_type_relations(&self, symbol_name: &str) -> Result<Vec<RelatedItem>> {
        let g = self.inner.lock().unwrap();
        // Symbol ids declaring this name.
        let ids: BTreeSet<&str> = g
            .symbols
            .values()
            .filter(|s| s.name == symbol_name)
            .map(|s| s.id.as_str())
            .collect();
        let file_of = |id: &str| g.symbols.get(id).map(|s| s.file_path.clone());

        let mut out: Vec<RelatedItem> = Vec::new();
        let push = |id: &str, reason: &str, out: &mut Vec<RelatedItem>| {
            if let Some(path) = file_of(id) {
                out.push(RelatedItem {
                    path,
                    reason: reason.to_string(),
                    depth: 1,
                });
            }
        };
        for (from, to) in &g.sym_inherits {
            if ids.contains(from.as_str()) {
                push(to, "base type (inherits)", &mut out);
            }
            if ids.contains(to.as_str()) {
                push(from, "subtype (inherits)", &mut out);
            }
        }
        for (from, to) in &g.sym_implements {
            if ids.contains(from.as_str()) {
                push(to, "implemented interface/trait", &mut out);
            }
            if ids.contains(to.as_str()) {
                push(from, "implementor", &mut out);
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.reason.cmp(&b.reason)));
        out.dedup_by(|a, b| a.path == b.path && a.reason == b.reason);
        Ok(out)
    }

    fn files_importing_package(&self, package_name: &str) -> Result<Vec<String>> {
        let g = self.inner.lock().unwrap();
        // Package ids matching this name (across ecosystems).
        let pkg_ids: BTreeSet<&str> = g
            .packages
            .values()
            .filter(|p| p.name == package_name)
            .map(|p| p.id.as_str())
            .collect();
        let mut out: Vec<String> = g
            .file_pkgs
            .iter()
            .filter(|(_, pid)| pkg_ids.contains(pid.as_str()))
            .filter_map(|(fid, _)| fid.strip_prefix("file:").map(|s| s.to_string()))
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn symbols_matching(&self, query: &SymbolSearchQuery) -> Result<Vec<IndexedSymbol>> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<IndexedSymbol> = g
            .symbols
            .values()
            .filter(|s| {
                query
                    .name
                    .as_deref()
                    .is_none_or(|n| ci_contains(&s.name, n))
                    && query.kind.is_none_or(|k| s.kind == k)
                    && query.language.is_none_or(|l| s.language == l)
                    && query.file.as_deref().is_none_or(|f| s.file_path == f)
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.file_path.cmp(&b.file_path)));
        Ok(out)
    }

    fn files_matching(&self, query: &FileSearchQuery) -> Result<Vec<IndexedFile>> {
        let g = self.inner.lock().unwrap();
        let mut out: Vec<IndexedFile> = g
            .files
            .values()
            .filter(|f| {
                query
                    .path_contains
                    .as_deref()
                    .is_none_or(|p| ci_contains(&f.path, p))
                    && query.language.is_none_or(|l| f.language == l)
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn related_to_symbol(&self, symbol: &str, _depth: usize) -> Result<Vec<RelatedItem>> {
        let g = self.inner.lock().unwrap();
        let mut out = Vec::new();
        // Files declaring an exactly-named symbol are depth 0.
        let mut target_ids: BTreeSet<&str> = BTreeSet::new();
        for s in g.symbols.values() {
            if s.name == symbol {
                target_ids.insert(s.id.as_str());
                out.push(RelatedItem {
                    path: s.file_path.clone(),
                    reason: "exact symbol declaration".to_string(),
                    depth: 0,
                });
            }
        }
        // Files that reference the symbol via incoming REFERENCES edges are
        // depth 1 (callers/instantiation sites). This traversal is what makes
        // usages visible to `pack`/`related`. Mirror the lock-free inline form
        // of `symbol_references` to avoid re-locking the mutex.
        let reason = format!("references {symbol}");
        for (from, to) in &g.sym_references {
            if target_ids.contains(to.as_str())
                && let Some(s) = g.symbols.get(from)
            {
                out.push(RelatedItem {
                    path: s.file_path.clone(),
                    reason: reason.clone(),
                    depth: 1,
                });
            }
        }
        // Deterministic; declaration (depth 0) wins over reference (depth 1)
        // when the same file both declares and references the symbol.
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.depth.cmp(&b.depth)));
        out.dedup_by(|a, b| a.path == b.path);
        Ok(out)
    }

    fn related_to_file(&self, path: &str, _depth: usize) -> Result<Vec<RelatedItem>> {
        let g = self.inner.lock().unwrap();
        let mut out = vec![RelatedItem {
            path: path.to_string(),
            reason: "seed file".to_string(),
            depth: 0,
        }];
        // Same-directory neighbours.
        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        for f in g.files.keys() {
            let fdir = f.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            if fdir == dir && f != path {
                out.push(RelatedItem {
                    path: f.clone(),
                    reason: "same directory".to_string(),
                    depth: 1,
                });
            }
        }
        out.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.path.cmp(&b.path)));
        Ok(out)
    }

    fn stats(&self) -> Result<IndexStats> {
        let g = self.inner.lock().unwrap();
        Ok(IndexStats {
            files: g.files.len(),
            symbols: g.symbols.len(),
            projects: g.projects.len(),
            packages: g.packages.len(),
            edges: g.project_refs.len() + g.project_pkgs.len(),
            reference_edges: g.sym_references.len(),
        })
    }

    fn all_files(&self) -> Result<Vec<IndexedFile>> {
        let g = self.inner.lock().unwrap();
        Ok(g.files.values().cloned().collect())
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFile>> {
        let g = self.inner.lock().unwrap();
        Ok(g.files.get(path).cloned())
    }

    fn all_packages(&self) -> Result<Vec<IndexedPackage>> {
        let g = self.inner.lock().unwrap();
        Ok(g.packages.values().cloned().collect())
    }

    fn all_projects(&self) -> Result<Vec<IndexedProject>> {
        let g = self.inner.lock().unwrap();
        Ok(g.projects.values().cloned().collect())
    }

    fn project_siblings(&self, path: &str) -> Result<Vec<RelatedItem>> {
        let g = self.inner.lock().unwrap();
        let file_id = format!("file:{path}");
        // Projects that contain this file.
        let owning: Vec<&String> = g
            .project_files
            .iter()
            .filter(|(_, fid)| fid == &file_id)
            .map(|(pid, _)| pid)
            .collect();

        let mut out = Vec::new();
        for pid in owning {
            let proj_name = g.projects.get(pid).map(|p| p.name.as_str()).unwrap_or(pid);
            for (p, fid) in &g.project_files {
                if p == pid && fid != &file_id {
                    // file_id format is `file:{path}` — recover the path.
                    if let Some(sib) = fid.strip_prefix("file:") {
                        out.push(RelatedItem {
                            path: sib.to_string(),
                            reason: format!("same project ({proj_name})"),
                            depth: 1,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        Ok(out)
    }
}
