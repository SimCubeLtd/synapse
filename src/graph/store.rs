//! The [`GraphStore`] contract.
//!
//! All persistence and querying of indexed facts goes through this trait so the
//! storage backend (LadybugDB in production, an in-memory store in tests) stays
//! replaceable. No backend-specific type ever appears in this signature.

use crate::graph::model::{
    FileSearchQuery, IndexStats, IndexedFile, IndexedPackage, IndexedProject, IndexedSymbol,
    RelatedItem, SymbolSearchQuery,
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

    /// Files that belong to the same project(s) as `path`, traversing the
    /// `CONTAINS_FILE` edges (project -> file). Excludes `path` itself. Each
    /// returned item names the owning project in its reason. Deterministic
    /// order. Returns empty when the file has no owning project.
    fn project_siblings(&self, path: &str) -> Result<Vec<RelatedItem>>;
}
