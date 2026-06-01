//! Smoke test for the production LadybugDB-backed graph store.
//!
//! Only compiled when the `ladybug` feature is enabled (the default), because it
//! links the LadybugDB C++ backend. It opens an on-disk database in a temp
//! directory, initialises the schema, upserts a file + symbol, and queries them
//! back through the `GraphStore` trait.

#![cfg(feature = "ladybug")]

use synapse::graph::ladybug_store::LadybugGraphStore;
use synapse::graph::model::{IndexedFile, IndexedSymbol, Language, SymbolKind, SymbolSearchQuery};
use synapse::graph::store::GraphStore;

fn temp_db_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("synapse-lbug-{}-{}.lbug", name, std::process::id()))
}

#[test]
fn ladybug_roundtrip_file_and_symbol() {
    let path = temp_db_path("roundtrip");
    // Clean any leftover from a previous aborted run.
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);

    let store = LadybugGraphStore::open(&path).expect("open ladybug db");
    store.initialize_schema().expect("init schema");

    store
        .upsert_file(IndexedFile {
            id: "file:src/Foo.cs".into(),
            path: "src/Foo.cs".into(),
            language: Language::CSharp,
            hash: "deadbeef".into(),
            size_bytes: 42,
            tracked: true,
            last_indexed_at: "2026-06-01T00:00:00+00:00".into(),
        })
        .expect("upsert file");

    store
        .upsert_symbol(IndexedSymbol {
            id: "sym:src/Foo.cs#class#Foo#1".into(),
            name: "Foo".into(),
            full_name: "Foo".into(),
            kind: SymbolKind::Class,
            language: Language::CSharp,
            file_path: "src/Foo.cs".into(),
            start_line: 1,
            end_line: 10,
            visibility: "public".into(),
            exported: true,
        })
        .expect("upsert symbol");
    store
        .link_file_declares_symbol("file:src/Foo.cs", "sym:src/Foo.cs#class#Foo#1")
        .expect("link declares");

    // Query the symbol back.
    let found = store
        .symbols_matching(&SymbolSearchQuery {
            name: Some("foo".into()),
            ..Default::default()
        })
        .expect("query symbols");
    assert_eq!(found.len(), 1, "expected exactly one matching symbol");
    assert_eq!(found[0].name, "Foo");
    assert_eq!(found[0].kind, SymbolKind::Class);
    assert_eq!(found[0].start_line, 1);
    assert_eq!(found[0].end_line, 10);
    assert!(found[0].exported);

    // Stats reflect the upserts.
    let stats = store.stats().expect("stats");
    assert_eq!(stats.files, 1);
    assert_eq!(stats.symbols, 1);

    // Removing the file cascades to its symbol.
    store.remove_file("src/Foo.cs").expect("remove file");
    let stats = store.stats().expect("stats after remove");
    assert_eq!(stats.files, 0);
    assert_eq!(stats.symbols, 0);

    // Best-effort cleanup.
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);
}
