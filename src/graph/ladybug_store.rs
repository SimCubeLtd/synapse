//! LadybugDB-backed [`GraphStore`] — the production storage backend.
//!
//! ALL `lbug` (LadybugDB) types and Cypher live in this file. Nothing
//! Ladybug-specific leaks elsewhere, so the backend stays replaceable.
//!
//! Schema: node tables Repository/Project/Package/File/Symbol and rel tables for
//! every relationship in the model. We use `MERGE` for idempotent upserts and
//! parameterized statements (`$name`) to avoid Cypher injection / escaping bugs.

use crate::graph::model::{
    FileSearchQuery, IndexStats, IndexedFile, IndexedPackage, IndexedProject, IndexedSymbol,
    Language, RelatedItem, SymbolKind, SymbolSearchQuery,
};
use crate::graph::store::GraphStore;
use anyhow::{Result, anyhow};
use lbug::{Connection, Database, SystemConfig, Value};
use std::path::Path;
use std::sync::Mutex;

/// Production graph store backed by an on-disk LadybugDB database.
pub struct LadybugGraphStore {
    db: Database,
    /// `lbug` connections take `&mut` for execute; we serialize access so the
    /// store is `Sync` and safe to share behind `&dyn GraphStore`.
    lock: Mutex<()>,
}

impl LadybugGraphStore {
    /// Open (or create) a LadybugDB database at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow!("opening LadybugDB at {}: {e}", path.display()))?;
        Ok(LadybugGraphStore {
            db,
            lock: Mutex::new(()),
        })
    }

    fn conn(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db).map_err(|e| anyhow!("opening connection: {e}"))
    }

    /// Run a parameterized write/read, ignoring the result rows.
    fn exec(&self, conn: &Connection<'_>, cypher: &str, params: Vec<(&str, Value)>) -> Result<()> {
        let mut stmt = conn
            .prepare(cypher)
            .map_err(|e| anyhow!("preparing `{cypher}`: {e}"))?;
        conn.execute(&mut stmt, params)
            .map_err(|e| anyhow!("executing `{cypher}`: {e}"))?;
        Ok(())
    }

    /// Run plain DDL/queries with no parameters.
    fn run(&self, conn: &Connection<'_>, cypher: &str) -> Result<()> {
        conn.query(cypher)
            .map(|_| ())
            .map_err(|e| anyhow!("running `{cypher}`: {e}"))
    }
}

/// Extract a String column from a value (Ladybug returns String values via
/// their `Display`/variant; we match the String variant, else stringify).
fn val_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn val_i64(v: &Value) -> i64 {
    match v {
        Value::Int64(n) => *n,
        Value::Int32(n) => *n as i64,
        Value::UInt64(n) => *n as i64,
        Value::UInt32(n) => *n as i64,
        other => other.to_string().parse().unwrap_or(0),
    }
}

fn val_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => other.to_string() == "true",
    }
}

fn lang_to_str(l: Language) -> String {
    l.as_str().to_string()
}

fn parse_lang(s: &str) -> Language {
    Language::from_str_opt(s).unwrap_or(Language::Other)
}

fn parse_kind(s: &str) -> SymbolKind {
    SymbolKind::from_str_opt(s).unwrap_or(SymbolKind::Function)
}

impl GraphStore for LadybugGraphStore {
    fn initialize_schema(&self) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // Node tables. `IF NOT EXISTS` keeps init idempotent.
        let nodes = [
            "CREATE NODE TABLE IF NOT EXISTS Repository(id STRING PRIMARY KEY, name STRING, root STRING)",
            "CREATE NODE TABLE IF NOT EXISTS Project(id STRING PRIMARY KEY, name STRING, path STRING, language STRING, kind STRING)",
            "CREATE NODE TABLE IF NOT EXISTS Package(id STRING PRIMARY KEY, name STRING, version STRING, ecosystem STRING, dependencyKind STRING)",
            "CREATE NODE TABLE IF NOT EXISTS File(id STRING PRIMARY KEY, path STRING, language STRING, hash STRING, sizeBytes INT64, tracked BOOL, lastIndexedAt STRING)",
            "CREATE NODE TABLE IF NOT EXISTS Symbol(id STRING PRIMARY KEY, name STRING, fullName STRING, kind STRING, language STRING, filePath STRING, startLine INT64, endLine INT64, visibility STRING, exported BOOL)",
        ];
        for ddl in nodes {
            self.run(&conn, ddl)?;
        }
        // Relationship tables (schema for all; some unpopulated in MVP).
        let rels = [
            "CREATE REL TABLE IF NOT EXISTS CONTAINS_PROJECT(FROM Repository TO Project)",
            "CREATE REL TABLE IF NOT EXISTS CONTAINS_FILE(FROM Project TO File)",
            "CREATE REL TABLE IF NOT EXISTS DECLARES(FROM File TO Symbol)",
            "CREATE REL TABLE IF NOT EXISTS REFERENCES_PROJECT(FROM Project TO Project)",
            "CREATE REL TABLE IF NOT EXISTS USES_PACKAGE(FROM Project TO Package)",
            "CREATE REL TABLE IF NOT EXISTS IMPORTS_PACKAGE(FROM File TO Package)",
            "CREATE REL TABLE IF NOT EXISTS REFERENCES(FROM Symbol TO Symbol)",
            "CREATE REL TABLE IF NOT EXISTS IMPLEMENTS(FROM Symbol TO Symbol)",
            "CREATE REL TABLE IF NOT EXISTS INHERITS(FROM Symbol TO Symbol)",
            "CREATE REL TABLE IF NOT EXISTS CALLS(FROM Symbol TO Symbol)",
        ];
        for ddl in rels {
            self.run(&conn, ddl)?;
        }
        Ok(())
    }

    fn upsert_file(&self, file: IndexedFile) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MERGE (f:File {id: $id}) \
             SET f.path = $path, f.language = $language, f.hash = $hash, \
                 f.sizeBytes = $size, f.tracked = $tracked, f.lastIndexedAt = $ts",
            vec![
                ("id", Value::String(file.id)),
                ("path", Value::String(file.path)),
                ("language", Value::String(lang_to_str(file.language))),
                ("hash", Value::String(file.hash)),
                ("size", Value::Int64(file.size_bytes as i64)),
                ("tracked", Value::Bool(file.tracked)),
                ("ts", Value::String(file.last_indexed_at)),
            ],
        )
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // Detach-delete the file's declared symbols, then the file node.
        self.exec(
            &conn,
            "MATCH (f:File {path: $path})-[:DECLARES]->(s:Symbol) DETACH DELETE s",
            vec![("path", Value::String(path.to_string()))],
        )
        .ok();
        // Symbols may also be matched by filePath property if the edge is missing.
        self.exec(
            &conn,
            "MATCH (s:Symbol {filePath: $path}) DETACH DELETE s",
            vec![("path", Value::String(path.to_string()))],
        )
        .ok();
        self.exec(
            &conn,
            "MATCH (f:File {path: $path}) DETACH DELETE f",
            vec![("path", Value::String(path.to_string()))],
        )
    }

    fn upsert_symbol(&self, symbol: IndexedSymbol) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            // NB: `$start`/`$end` would collide with Cypher reserved words
            // (END is reserved), so parameters use non-reserved names.
            "MERGE (s:Symbol {id: $id}) \
             SET s.name = $name, s.fullName = $full, s.kind = $kind, s.language = $language, \
                 s.filePath = $file, s.startLine = $startln, s.endLine = $endln, \
                 s.visibility = $vis, s.exported = $exported",
            vec![
                ("id", Value::String(symbol.id)),
                ("name", Value::String(symbol.name)),
                ("full", Value::String(symbol.full_name)),
                ("kind", Value::String(symbol.kind.as_str().to_string())),
                ("language", Value::String(lang_to_str(symbol.language))),
                ("file", Value::String(symbol.file_path)),
                ("startln", Value::Int64(symbol.start_line as i64)),
                ("endln", Value::Int64(symbol.end_line as i64)),
                ("vis", Value::String(symbol.visibility)),
                ("exported", Value::Bool(symbol.exported)),
            ],
        )
    }

    fn upsert_project(&self, project: IndexedProject) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MERGE (p:Project {id: $id}) \
             SET p.name = $name, p.path = $path, p.language = $language, p.kind = $kind",
            vec![
                ("id", Value::String(project.id)),
                ("name", Value::String(project.name)),
                ("path", Value::String(project.path)),
                ("language", Value::String(lang_to_str(project.language))),
                ("kind", Value::String(project.kind)),
            ],
        )
    }

    fn upsert_package(&self, package: IndexedPackage) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MERGE (p:Package {id: $id}) \
             SET p.name = $name, p.version = $version, p.ecosystem = $eco, \
                 p.dependencyKind = $kind",
            vec![
                ("id", Value::String(package.id)),
                ("name", Value::String(package.name)),
                ("version", Value::String(package.version)),
                ("eco", Value::String(package.ecosystem)),
                ("kind", Value::String(package.dependency_kind)),
            ],
        )
    }

    fn link_project_contains_file(&self, project_id: &str, file_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (p:Project {id: $p}), (f:File {id: $f}) MERGE (p)-[:CONTAINS_FILE]->(f)",
            vec![
                ("p", Value::String(project_id.to_string())),
                ("f", Value::String(file_id.to_string())),
            ],
        )
    }

    fn link_file_declares_symbol(&self, file_id: &str, symbol_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (f:File {id: $f}), (s:Symbol {id: $s}) MERGE (f)-[:DECLARES]->(s)",
            vec![
                ("f", Value::String(file_id.to_string())),
                ("s", Value::String(symbol_id.to_string())),
            ],
        )
    }

    fn link_project_references_project(
        &self,
        from_project_id: &str,
        to_project_id: &str,
    ) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // The target project may not be indexed yet; create a stub node so the
        // edge can exist (MERGE on the node by id).
        self.exec(
            &conn,
            "MERGE (t:Project {id: $t})",
            vec![("t", Value::String(to_project_id.to_string()))],
        )
        .ok();
        self.exec(
            &conn,
            "MATCH (a:Project {id: $a}), (b:Project {id: $b}) MERGE (a)-[:REFERENCES_PROJECT]->(b)",
            vec![
                ("a", Value::String(from_project_id.to_string())),
                ("b", Value::String(to_project_id.to_string())),
            ],
        )
    }

    fn link_project_uses_package(&self, project_id: &str, package_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (p:Project {id: $p}), (k:Package {id: $k}) MERGE (p)-[:USES_PACKAGE]->(k)",
            vec![
                ("p", Value::String(project_id.to_string())),
                ("k", Value::String(package_id.to_string())),
            ],
        )
    }

    fn link_file_imports_package(&self, file_id: &str, package_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (f:File {id: $f}), (k:Package {id: $k}) MERGE (f)-[:IMPORTS_PACKAGE]->(k)",
            vec![
                ("f", Value::String(file_id.to_string())),
                ("k", Value::String(package_id.to_string())),
            ],
        )
    }

    fn link_symbol_inherits(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (a:Symbol {id: $a}), (b:Symbol {id: $b}) MERGE (a)-[:INHERITS]->(b)",
            vec![
                ("a", Value::String(from_symbol_id.to_string())),
                ("b", Value::String(to_symbol_id.to_string())),
            ],
        )
    }

    fn link_symbol_implements(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (a:Symbol {id: $a}), (b:Symbol {id: $b}) MERGE (a)-[:IMPLEMENTS]->(b)",
            vec![
                ("a", Value::String(from_symbol_id.to_string())),
                ("b", Value::String(to_symbol_id.to_string())),
            ],
        )
    }

    fn link_symbol_references(&self, from_symbol_id: &str, to_symbol_id: &str) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        self.exec(
            &conn,
            "MATCH (a:Symbol {id: $a}), (b:Symbol {id: $b}) MERGE (a)-[:REFERENCES]->(b)",
            vec![
                ("a", Value::String(from_symbol_id.to_string())),
                ("b", Value::String(to_symbol_id.to_string())),
            ],
        )
    }

    fn symbol_type_relations(&self, symbol_name: &str) -> Result<Vec<RelatedItem>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // Four directed traversals; each returns (file_path, reason). Name
        // matching is case-insensitive (lowercased param vs `lower(name)`) to
        // align with the case-insensitive seed used by pack/related.
        let name_lc = symbol_name.to_ascii_lowercase();
        let queries = [
            (
                "MATCH (s:Symbol)-[:INHERITS]->(t:Symbol) WHERE lower(s.name) = $n \
                 RETURN DISTINCT t.filePath",
                "base type (inherits)",
            ),
            (
                "MATCH (s:Symbol)-[:INHERITS]->(t:Symbol) WHERE lower(t.name) = $n \
                 RETURN DISTINCT s.filePath",
                "subtype (inherits)",
            ),
            (
                "MATCH (s:Symbol)-[:IMPLEMENTS]->(t:Symbol) WHERE lower(s.name) = $n \
                 RETURN DISTINCT t.filePath",
                "implemented interface/trait",
            ),
            (
                "MATCH (s:Symbol)-[:IMPLEMENTS]->(t:Symbol) WHERE lower(t.name) = $n \
                 RETURN DISTINCT s.filePath",
                "implementor",
            ),
        ];
        let mut out = Vec::new();
        for (cypher, reason) in queries {
            let mut stmt = conn
                .prepare(cypher)
                .map_err(|e| anyhow!("preparing symbol_type_relations: {e}"))?;
            let result = conn
                .execute(&mut stmt, vec![("n", Value::String(name_lc.clone()))])
                .map_err(|e| anyhow!("executing symbol_type_relations: {e}"))?;
            for row in result {
                out.push(RelatedItem {
                    path: val_string(&row[0]),
                    reason: reason.to_string(),
                    depth: 1,
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.reason.cmp(&b.reason)));
        out.dedup_by(|a, b| a.path == b.path && a.reason == b.reason);
        Ok(out)
    }

    fn symbol_references(&self, symbol_name: &str) -> Result<Vec<RelatedItem>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // Incoming REFERENCES edges = files that reference this symbol. Name
        // matching is case-insensitive to align with the seed lookup.
        let cypher = "MATCH (s:Symbol)-[:REFERENCES]->(t:Symbol) WHERE lower(t.name) = $n \
                      RETURN DISTINCT s.filePath";
        let mut stmt = conn
            .prepare(cypher)
            .map_err(|e| anyhow!("preparing symbol_references: {e}"))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("n", Value::String(symbol_name.to_ascii_lowercase()))],
            )
            .map_err(|e| anyhow!("executing symbol_references: {e}"))?;
        let reason = format!("references {symbol_name}");
        let mut out = Vec::new();
        for row in result {
            out.push(RelatedItem {
                path: val_string(&row[0]),
                reason: reason.clone(),
                depth: 1,
            });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        Ok(out)
    }

    fn files_importing_package(&self, package_name: &str) -> Result<Vec<String>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        let cypher = "MATCH (f:File)-[:IMPORTS_PACKAGE]->(k:Package {name: $name}) \
                      RETURN DISTINCT f.path ORDER BY f.path";
        let mut stmt = conn
            .prepare(cypher)
            .map_err(|e| anyhow!("preparing files_importing_package: {e}"))?;
        let result = conn
            .execute(
                &mut stmt,
                vec![("name", Value::String(package_name.to_string()))],
            )
            .map_err(|e| anyhow!("executing files_importing_package: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            out.push(val_string(&row[0]));
        }
        Ok(out)
    }

    fn symbols_matching(&self, query: &SymbolSearchQuery) -> Result<Vec<IndexedSymbol>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // Build a WHERE clause; use parameters for safety.
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<(&str, Value)> = Vec::new();
        let name_lc;
        if let Some(n) = &query.name {
            name_lc = n.to_ascii_lowercase();
            clauses.push("contains(lower(s.name), $name)");
            params.push(("name", Value::String(name_lc)));
        }
        if let Some(k) = query.kind {
            clauses.push("s.kind = $kind");
            params.push(("kind", Value::String(k.as_str().to_string())));
        }
        if let Some(l) = query.language {
            clauses.push("s.language = $language");
            params.push(("language", Value::String(lang_to_str(l))));
        }
        if let Some(f) = &query.file {
            clauses.push("s.filePath = $file");
            params.push(("file", Value::String(f.clone())));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let cypher = format!(
            "MATCH (s:Symbol) {where_clause} \
             RETURN s.id, s.name, s.fullName, s.kind, s.language, s.filePath, \
                    s.startLine, s.endLine, s.visibility, s.exported \
             ORDER BY s.name, s.filePath"
        );
        let mut stmt = conn
            .prepare(&cypher)
            .map_err(|e| anyhow!("preparing symbol query: {e}"))?;
        let result = conn
            .execute(&mut stmt, params)
            .map_err(|e| anyhow!("executing symbol query: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            out.push(IndexedSymbol {
                id: val_string(&row[0]),
                name: val_string(&row[1]),
                full_name: val_string(&row[2]),
                kind: parse_kind(&val_string(&row[3])),
                language: parse_lang(&val_string(&row[4])),
                file_path: val_string(&row[5]),
                start_line: val_i64(&row[6]) as u32,
                end_line: val_i64(&row[7]) as u32,
                visibility: val_string(&row[8]),
                exported: val_bool(&row[9]),
            });
        }
        Ok(out)
    }

    fn files_matching(&self, query: &FileSearchQuery) -> Result<Vec<IndexedFile>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<(&str, Value)> = Vec::new();
        let path_lc;
        if let Some(p) = &query.path_contains {
            path_lc = p.to_ascii_lowercase();
            clauses.push("contains(lower(f.path), $path)");
            params.push(("path", Value::String(path_lc)));
        }
        if let Some(l) = query.language {
            clauses.push("f.language = $language");
            params.push(("language", Value::String(lang_to_str(l))));
        }
        let where_clause = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        let cypher = format!(
            "MATCH (f:File) {where_clause} \
             RETURN f.id, f.path, f.language, f.hash, f.sizeBytes, f.tracked, f.lastIndexedAt \
             ORDER BY f.path"
        );
        let mut stmt = conn
            .prepare(&cypher)
            .map_err(|e| anyhow!("preparing file query: {e}"))?;
        let result = conn
            .execute(&mut stmt, params)
            .map_err(|e| anyhow!("executing file query: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            out.push(IndexedFile {
                id: val_string(&row[0]),
                path: val_string(&row[1]),
                language: parse_lang(&val_string(&row[2])),
                hash: val_string(&row[3]),
                size_bytes: val_i64(&row[4]) as u64,
                tracked: val_bool(&row[5]),
                last_indexed_at: val_string(&row[6]),
            });
        }
        Ok(out)
    }

    fn related_to_symbol(&self, symbol: &str, _depth: usize) -> Result<Vec<RelatedItem>> {
        // Declaring files (depth 0) plus files that reference the symbol via
        // incoming REFERENCES edges (depth 1 = callers/instantiation sites).
        // The reference traversal is what lets `pack`/`related` surface usages;
        // without it the graph edges would be populated but invisible.
        let mut out = Vec::new();
        {
            let _guard = self.lock.lock().unwrap();
            let conn = self.conn()?;
            // Case-insensitive to align with the seed lookup.
            let cypher = "MATCH (s:Symbol) WHERE lower(s.name) = $name \
                          RETURN DISTINCT s.filePath ORDER BY s.filePath";
            let mut stmt = conn
                .prepare(cypher)
                .map_err(|e| anyhow!("preparing related query: {e}"))?;
            let result = conn
                .execute(
                    &mut stmt,
                    vec![("name", Value::String(symbol.to_ascii_lowercase()))],
                )
                .map_err(|e| anyhow!("executing related query: {e}"))?;
            for row in result {
                out.push(RelatedItem {
                    path: val_string(&row[0]),
                    reason: "exact symbol declaration".to_string(),
                    depth: 0,
                });
            }
        }
        // symbol_references takes its own lock, so it is called outside the
        // block above. Declaring files (depth 0) take precedence on dedup.
        for item in self.symbol_references(symbol)? {
            if !out.iter().any(|o| o.path == item.path) {
                out.push(item);
            }
        }
        Ok(out)
    }

    fn related_to_file(&self, path: &str, _depth: usize) -> Result<Vec<RelatedItem>> {
        // The richer same-directory / heuristic relation logic lives in the
        // pack/related command layer which combines store reads with file-path
        // heuristics. Here we just return the seed.
        Ok(vec![RelatedItem {
            path: path.to_string(),
            reason: "seed file".to_string(),
            depth: 0,
        }])
    }

    fn stats(&self) -> Result<IndexStats> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        let count = |label: &str| -> Result<usize> {
            let q = format!("MATCH (n:{label}) RETURN count(n)");
            let r = conn
                .query(&q)
                .map_err(|e| anyhow!("counting {label}: {e}"))?;
            let mut c = 0usize;
            for row in r {
                c = val_i64(&row[0]) as usize;
            }
            Ok(c)
        };
        let count_rel = |label: &str| -> Result<usize> {
            let q = format!("MATCH ()-[r:{label}]->() RETURN count(r)");
            let r = conn
                .query(&q)
                .map_err(|e| anyhow!("counting rel {label}: {e}"))?;
            let mut c = 0usize;
            for row in r {
                c = val_i64(&row[0]) as usize;
            }
            Ok(c)
        };
        Ok(IndexStats {
            files: count("File")?,
            symbols: count("Symbol")?,
            projects: count("Project")?,
            packages: count("Package")?,
            edges: count_rel("REFERENCES_PROJECT")? + count_rel("USES_PACKAGE")?,
            reference_edges: count_rel("REFERENCES")?,
        })
    }

    fn all_files(&self) -> Result<Vec<IndexedFile>> {
        self.files_matching(&FileSearchQuery::default())
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFile>> {
        // Exact-path lookup: reuse files_matching (substring) then filter, to
        // keep one code path. (The candidate set per lookup is small.)
        let q = FileSearchQuery {
            path_contains: Some(path.to_string()),
            ..Default::default()
        };
        let files = self.files_matching(&q)?;
        Ok(files.into_iter().find(|f| f.path == path))
    }

    fn all_packages(&self) -> Result<Vec<IndexedPackage>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        let result = conn
            .query(
                "MATCH (p:Package) \
                 RETURN p.id, p.name, p.version, p.ecosystem, p.dependencyKind \
                 ORDER BY p.name, p.version",
            )
            .map_err(|e| anyhow!("listing packages: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            out.push(IndexedPackage {
                id: val_string(&row[0]),
                name: val_string(&row[1]),
                version: val_string(&row[2]),
                ecosystem: val_string(&row[3]),
                dependency_kind: val_string(&row[4]),
            });
        }
        Ok(out)
    }

    fn all_projects(&self) -> Result<Vec<IndexedProject>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        let result = conn
            .query(
                "MATCH (p:Project) \
                 RETURN p.id, p.name, p.path, p.language, p.kind \
                 ORDER BY p.path",
            )
            .map_err(|e| anyhow!("listing projects: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            out.push(IndexedProject {
                id: val_string(&row[0]),
                name: val_string(&row[1]),
                path: val_string(&row[2]),
                language: parse_lang(&val_string(&row[3])),
                kind: val_string(&row[4]),
            });
        }
        Ok(out)
    }

    fn project_siblings(&self, path: &str) -> Result<Vec<RelatedItem>> {
        let _guard = self.lock.lock().unwrap();
        let conn = self.conn()?;
        // Find the project(s) owning `path`, then their other files. The DISTINCT
        // + ORDER BY keeps output deterministic.
        let cypher = "MATCH (p:Project)-[:CONTAINS_FILE]->(seed:File {path: $path}) \
                      MATCH (p)-[:CONTAINS_FILE]->(sib:File) \
                      WHERE sib.path <> $path \
                      RETURN DISTINCT sib.path, p.name ORDER BY sib.path";
        let mut stmt = conn
            .prepare(cypher)
            .map_err(|e| anyhow!("preparing project_siblings: {e}"))?;
        let result = conn
            .execute(&mut stmt, vec![("path", Value::String(path.to_string()))])
            .map_err(|e| anyhow!("executing project_siblings: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            let sib = val_string(&row[0]);
            let proj = val_string(&row[1]);
            out.push(RelatedItem {
                path: sib,
                reason: format!("same project ({proj})"),
                depth: 1,
            });
        }
        Ok(out)
    }
}
