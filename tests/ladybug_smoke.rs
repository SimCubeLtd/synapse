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

#[test]
fn ladybug_reference_edge_roundtrips() {
    let path = temp_db_path("refedge");
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);

    let store = LadybugGraphStore::open(&path).expect("open");
    store.initialize_schema().expect("schema");

    let mk = |id: &str, name: &str, file: &str| IndexedSymbol {
        id: id.into(),
        name: name.into(),
        full_name: name.into(),
        kind: SymbolKind::Class,
        language: Language::CSharp,
        file_path: file.into(),
        start_line: 1,
        end_line: 2,
        visibility: "public".into(),
        exported: true,
    };
    // Target (declared) and referrer (the enclosing decl that uses it).
    store
        .upsert_symbol(mk("sym:a.cs#class#Employee#1", "Employee", "a.cs"))
        .unwrap();
    store
        .upsert_symbol(mk("sym:b.cs#class#Attendance#1", "Attendance", "b.cs"))
        .unwrap();
    store
        .link_symbol_references("sym:b.cs#class#Attendance#1", "sym:a.cs#class#Employee#1")
        .expect("link references");

    // The referrer file must come back via symbol_references on the target name.
    let refs = store
        .symbol_references("Employee")
        .expect("symbol_references");
    assert_eq!(refs.len(), 1, "exactly one referrer expected, got {refs:?}");
    assert_eq!(refs[0].path, "b.cs");
    assert_eq!(store.stats().unwrap().reference_edges, 1);

    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);
}

/// The batched `link_edges` path (one transaction, prepared statements reused)
/// writes the same edges as the per-edge `link_*` methods, across edge kinds.
#[test]
fn ladybug_link_edges_batch_roundtrips() {
    use synapse::graph::model::GraphEdge;

    let path = temp_db_path("batch");
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);

    let store = LadybugGraphStore::open(&path).expect("open");
    store.initialize_schema().expect("schema");

    let mk = |id: &str, name: &str, file: &str| IndexedSymbol {
        id: id.into(),
        name: name.into(),
        full_name: name.into(),
        kind: SymbolKind::Class,
        language: Language::CSharp,
        file_path: file.into(),
        start_line: 1,
        end_line: 2,
        visibility: "public".into(),
        exported: true,
    };
    store
        .upsert_symbol(mk("sym:base.cs#class#Base#1", "Base", "base.cs"))
        .unwrap();
    store
        .upsert_symbol(mk("sym:impl.cs#class#Impl#1", "Impl", "impl.cs"))
        .unwrap();
    store
        .upsert_symbol(mk("sym:user.cs#class#User#1", "User", "user.cs"))
        .unwrap();

    // Two edge kinds written in one batch (one transaction).
    store
        .link_edges(&[
            GraphEdge::SymbolInherits {
                from: "sym:impl.cs#class#Impl#1".into(),
                to: "sym:base.cs#class#Base#1".into(),
            },
            GraphEdge::SymbolReferences {
                from: "sym:user.cs#class#User#1".into(),
                to: "sym:base.cs#class#Base#1".into(),
            },
        ])
        .expect("link_edges batch");

    // Both edges are queryable, and re-running the batch is idempotent (MERGE).
    assert_eq!(store.stats().unwrap().reference_edges, 1);
    let refs = store.symbol_references("Base").unwrap();
    assert!(
        refs.iter().any(|r| r.path == "user.cs"),
        "batched REFERENCES edge must be queryable: {refs:?}"
    );
    let rels = store.symbol_type_relations("Impl").unwrap();
    assert!(
        rels.iter().any(|r| r.reason.contains("inherits")),
        "batched INHERITS edge must be queryable: {rels:?}"
    );

    // Idempotency: the same batch again must not double-count.
    store
        .link_edges(&[GraphEdge::SymbolReferences {
            from: "sym:user.cs#class#User#1".into(),
            to: "sym:base.cs#class#Base#1".into(),
        }])
        .expect("re-link");
    assert_eq!(
        store.stats().unwrap().reference_edges,
        1,
        "MERGE must keep the batch idempotent"
    );

    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::remove_file(&path);
}
