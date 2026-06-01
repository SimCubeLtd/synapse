//! Shared data types for the indexed graph.
//!
//! These structs are the lingua franca between the indexer (which produces
//! them), the graph store (which persists/queries them) and the pack/output
//! layers (which render them). They are deliberately plain data with derived
//! `serde` so any store or formatter can round-trip them.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Source language of a file or symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    CSharp,
    Rust,
    Python,
    Go,
    JavaScript,
    TypeScript,
    Svelte,
    Bash,
    Yaml,
    Json,
    Markdown,
    /// Config/markup/data files we index but do not extract symbols from.
    Other,
}

impl Language {
    /// Canonical lowercase name used in output and queries.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::CSharp => "csharp",
            Language::Rust => "rust",
            Language::Python => "python",
            Language::Go => "go",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Svelte => "svelte",
            Language::Bash => "bash",
            Language::Yaml => "yaml",
            Language::Json => "json",
            Language::Markdown => "markdown",
            Language::Other => "other",
        }
    }

    /// Parse a language from its canonical name (used by CLI filters).
    pub fn from_str_opt(s: &str) -> Option<Language> {
        Some(match s.to_ascii_lowercase().as_str() {
            "csharp" | "c#" | "cs" => Language::CSharp,
            "rust" | "rs" => Language::Rust,
            "python" | "py" => Language::Python,
            "go" => Language::Go,
            "javascript" | "js" => Language::JavaScript,
            "typescript" | "ts" => Language::TypeScript,
            "svelte" => Language::Svelte,
            "bash" | "sh" | "shell" => Language::Bash,
            "yaml" | "yml" => Language::Yaml,
            "json" => Language::Json,
            "markdown" | "md" => Language::Markdown,
            "other" => Language::Other,
            _ => return None,
        })
    }

    /// Markdown fence language hint for code blocks.
    pub fn fence(self) -> &'static str {
        match self {
            Language::CSharp => "csharp",
            Language::Rust => "rust",
            Language::Python => "python",
            Language::Go => "go",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            // Svelte's markup has no universal fence; `svelte` is widely
            // recognised by syntax highlighters.
            Language::Svelte => "svelte",
            Language::Bash => "bash",
            Language::Yaml => "yaml",
            Language::Json => "json",
            Language::Markdown => "markdown",
            Language::Other => "text",
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of an extracted symbol. Kept as a small closed set so output and
/// ranking are deterministic across languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Class,
    Struct,
    Record,
    Interface,
    Trait,
    Enum,
    Function,
    Method,
    Module,
    TypeAlias,
    Component,
    Constructor,
    /// A top-level key in a data/config file (YAML/JSON) or a YAML anchor.
    Key,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Record => "record",
            SymbolKind::Interface => "interface",
            SymbolKind::Trait => "trait",
            SymbolKind::Enum => "enum",
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Module => "module",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Component => "component",
            SymbolKind::Constructor => "constructor",
            SymbolKind::Key => "key",
        }
    }

    pub fn from_str_opt(s: &str) -> Option<SymbolKind> {
        Some(match s.to_ascii_lowercase().as_str() {
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "record" => SymbolKind::Record,
            "interface" => SymbolKind::Interface,
            "trait" => SymbolKind::Trait,
            "enum" => SymbolKind::Enum,
            "function" | "func" | "fn" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "module" | "mod" => SymbolKind::Module,
            "type" | "typealias" => SymbolKind::TypeAlias,
            "component" => SymbolKind::Component,
            "constructor" | "ctor" => SymbolKind::Constructor,
            "key" => SymbolKind::Key,
            _ => return None,
        })
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A file recorded in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    /// Deterministic id derived from the repo-relative path.
    pub id: String,
    /// Repo-relative path using forward slashes.
    pub path: String,
    pub language: Language,
    /// blake3 content hash (hex).
    pub hash: String,
    pub size_bytes: u64,
    /// Whether git tracks this file.
    pub tracked: bool,
    /// RFC3339 timestamp of last index.
    pub last_indexed_at: String,
}

/// A symbol declared within a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbol {
    /// Deterministic id derived from `path#kind#name#start_line`.
    pub id: String,
    pub name: String,
    /// Fully-qualified name where cheaply available, else equal to `name`.
    pub full_name: String,
    pub kind: SymbolKind,
    pub language: Language,
    pub file_path: String,
    pub start_line: u32,
    pub end_line: u32,
    /// e.g. "public", "private", "" when unknown.
    pub visibility: String,
    /// Whether the symbol is exported/public (JS/TS export, Rust `pub`, etc.).
    pub exported: bool,
}

/// A buildable project (e.g. a `.csproj` or a JS/TS `package.json` workspace).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedProject {
    pub id: String,
    pub name: String,
    /// Repo-relative path of the project manifest.
    pub path: String,
    pub language: Language,
    /// e.g. "dotnet", "node".
    pub kind: String,
}

/// An external package dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedPackage {
    pub id: String,
    pub name: String,
    pub version: String,
    /// e.g. "nuget", "npm".
    pub ecosystem: String,
    /// e.g. "dependency", "devDependency", "peerDependency", "package".
    pub dependency_kind: String,
}

/// Search filter for symbols.
#[derive(Debug, Clone, Default)]
pub struct SymbolSearchQuery {
    /// Case-insensitive substring match against symbol name. Empty = any.
    pub name: Option<String>,
    pub kind: Option<SymbolKind>,
    pub language: Option<Language>,
    /// Exact repo-relative file path.
    pub file: Option<String>,
}

/// Search filter for files.
#[derive(Debug, Clone, Default)]
pub struct FileSearchQuery {
    /// Case-insensitive substring match against path. Empty = any.
    pub path_contains: Option<String>,
    pub language: Option<Language>,
}

/// An item discovered as related to a starting symbol or file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedItem {
    /// Repo-relative path of the related file.
    pub path: String,
    /// Human-readable reason this item was selected.
    pub reason: String,
    /// Graph distance from the seed (0 = the seed file itself).
    pub depth: usize,
}

/// Aggregate counts for `status`/`index --stats`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStats {
    pub files: usize,
    pub symbols: usize,
    pub projects: usize,
    pub packages: usize,
    /// project/package relationship edge count.
    pub edges: usize,
}
