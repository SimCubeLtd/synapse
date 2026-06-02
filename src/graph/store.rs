//! The [`GraphStore`] contract.
//!
//! All persistence and querying of indexed facts goes through this trait so the
//! storage backend (LadybugDB in production, an in-memory store in tests) stays
//! replaceable. No backend-specific type ever appears in this signature.

use crate::graph::model::{
    FileSearchQuery, FileWrite, IndexStats, IndexedFile, IndexedPackage, IndexedProject,
    IndexedSymbol, RelatedItem, SymbolSearchQuery,
};
use anyhow::Result;

/// Persistence + query interface over the indexed repository graph.
pub trait GraphStore {
    /// Create node/relationship tables if they do not already exist.
    fn initialize_schema(&self) -> Result<()>;

    fn upsert_file(&self, file: IndexedFile) -> Result<()>;
    /// Remove a file and all symbols it declared.
    fn remove_file(&self, path: &str) -> Result<()>;

    fn upsert_symbol(&self, symbol: IndexedSymbol) -> Result<()>;
    fn upsert_project(&self, project: IndexedProject) -> Result<()>;
    fn upsert_package(&self, package: IndexedPackage) -> Result<()>;

    fn link_project_contains_file(&self, project_id: &str, file_id: &str) -> Result<()>;
    fn link_file_declares_symbol(&self, file_id: &str, symbol_id: &str) -> Result<()>;
    fn link_project_references_project(
        &self,
        from_project_id: &str,
        to_project_id: &str,
    ) -> Result<()>;
    fn link_project_uses_package(&self, project_id: &str, package_id: &str) -> Result<()>;
    /// Record that a file imports/uses a package (the `IMPORTS_PACKAGE` edge).
    fn link_file_imports_package(&self, file_id: &str, package_id: &str) -> Result<()>;
    /// Record that one symbol inherits from another (the `INHERITS` edge).
    fn link_symbol_inherits(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()>;
    /// Record that a symbol implements an interface/trait (the `IMPLEMENTS` edge).
    fn link_symbol_implements(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()>;
    /// Record that one symbol references another — an instantiation, call, or
    /// type use (the `REFERENCES` edge). Direction is referrer -> referenced.
    fn link_symbol_references(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()>;

    /// Create many relationship edges in a single batch. Backends that support
    /// transactions should write the whole batch in one transaction with reused
    /// prepared statements — far faster than one `link_*` call per edge during
    /// the indexer's post-pass. Idempotent (`MERGE`); endpoints must already
    /// exist as nodes. The default implementation falls back to per-edge writes.
    fn link_edges(&self, edges: &[crate::graph::model::GraphEdge]) -> Result<()> {
        use crate::graph::model::GraphEdge;
        for e in edges {
            match e {
                GraphEdge::SymbolReferences { from, to } => {
                    self.link_symbol_references(from, to)?
                }
                GraphEdge::SymbolInherits { from, to } => self.link_symbol_inherits(from, to)?,
                GraphEdge::SymbolImplements { from, to } => {
                    self.link_symbol_implements(from, to)?
                }
                GraphEdge::FileImportsPackage { file, package } => {
                    self.link_file_imports_package(file, package)?
                }
                GraphEdge::ProjectContainsFile { project, file } => {
                    self.link_project_contains_file(project, file)?
                }
            }
        }
        Ok(())
    }

    /// Apply many files' (re)index writes in one batch. For each [`FileWrite`]:
    /// remove the file's existing nodes, upsert the file, upsert every declared
    /// symbol, and create the `DECLARES` edges. Backends that support
    /// transactions should write the whole batch in one transaction with reused
    /// prepared statements — far faster than one auto-committed `upsert_symbol`
    /// per symbol, which dominates indexing time on large repos. The default
    /// implementation falls back to the per-file/per-symbol methods, preserving
    /// the original ordering (remove -> upsert file -> upsert+link each symbol).
    fn write_files_batch(&self, files: &[FileWrite]) -> Result<()> {
        for fw in files {
            self.remove_file(&fw.file.path)?;
            let fid = fw.file.id.clone();
            self.upsert_file(fw.file.clone())?;
            for sym in &fw.symbols {
                let sid = sym.id.clone();
                self.upsert_symbol(sym.clone())?;
                self.link_file_declares_symbol(&fid, &sid)?;
            }
        }
        Ok(())
    }

    fn symbols_matching(&self, query: &SymbolSearchQuery) -> Result<Vec<IndexedSymbol>>;
    fn files_matching(&self, query: &FileSearchQuery) -> Result<Vec<IndexedFile>>;
    fn related_to_symbol(&self, symbol: &str, depth: usize) -> Result<Vec<RelatedItem>>;
    fn related_to_file(&self, path: &str, depth: usize) -> Result<Vec<RelatedItem>>;

    fn stats(&self) -> Result<IndexStats>;

    // --- Convenience accessors used by status/pack (default-free reads) ---

    /// Return every indexed file. Used by `status` (stale detection) and pack
    /// selection. Implementations should return a deterministic order.
    fn all_files(&self) -> Result<Vec<IndexedFile>>;

    /// Look up a single file by repo-relative path.
    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFile>>;

    /// Return every indexed package (dependency), in deterministic order.
    fn all_packages(&self) -> Result<Vec<IndexedPackage>>;

    /// Return every indexed project, in deterministic order.
    fn all_projects(&self) -> Result<Vec<IndexedProject>>;

    /// Repo-relative paths of files that import a package (by package name,
    /// any ecosystem), via the `IMPORTS_PACKAGE` edge. Deterministic order.
    fn files_importing_package(&self, package_name: &str) -> Result<Vec<String>>;

    /// Files reached from a symbol via `INHERITS`/`IMPLEMENTS` edges in either
    /// direction: the symbol's base types/interfaces and its subtypes/
    /// implementors. Each item's reason names the relationship. Used by
    /// `related`. Deterministic order; empty when the symbol has no such edges.
    fn symbol_type_relations(&self, symbol_name: &str) -> Result<Vec<RelatedItem>>;

    /// Files that reference the named symbol via incoming `REFERENCES` edges
    /// (the symbol's callers / instantiation sites). Each item's reason names
    /// the relationship. Used by `related`/`pack`. Deterministic order; empty
    /// when nothing references the symbol.
    fn symbol_references(&self, symbol_name: &str) -> Result<Vec<RelatedItem>>;

    /// Files that belong to the same project(s) as `path`, traversing the
    /// `CONTAINS_FILE` edges (project -> file). Excludes `path` itself. Each
    /// returned item names the owning project in its reason. Deterministic
    /// order. Returns empty when the file has no owning project.
    fn project_siblings(&self, path: &str) -> Result<Vec<RelatedItem>>;
}
