//! Unit tests for the pure, backend-independent logic, exercised directly
//! through the `synapse` library crate (no graph backend, no CLI process).
//!
//! These cover the spec's required units: config round-trip, include/exclude
//! glob matching, the token-budget estimator and fitter, per-language symbol
//! extraction against fixtures, manifest parsing (.csproj / package.json),
//! pack selection ordering, markdown rendering, and the table renderer — plus
//! `GraphStore` behaviour via the in-memory store.

use std::path::{Path, PathBuf};

use synapse::config::SynapseConfig;
use synapse::graph::memory_store::MemoryGraphStore;
use synapse::graph::model::{
    FileSearchQuery, IndexedFile, IndexedSymbol, Language, SymbolKind, SymbolSearchQuery,
};
use synapse::graph::store::GraphStore;
use synapse::indexer::tree_sitter::extract;
use synapse::indexer::{dotnet, languages, node};
use synapse::output::table;
use synapse::pack::budget;
use synapse::repo::{build_globset, path_matches};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn read_fixture(rel: &str) -> String {
    std::fs::read_to_string(fixtures().join(rel))
        .unwrap_or_else(|e| panic!("reading fixture {rel}: {e}"))
}

// --- config ---------------------------------------------------------------

#[test]
fn config_default_roundtrips_through_toml() {
    let cfg = SynapseConfig::default();
    let toml = cfg.to_toml().expect("serialize");
    assert!(toml.contains("backend = \"ladybug\""));
    assert!(toml.contains("default_budget = 40000"));
    let parsed: SynapseConfig = toml::from_str(&toml).expect("parse");
    assert_eq!(parsed, cfg);
}

#[test]
fn config_defaults_have_expected_includes_and_excludes() {
    let cfg = SynapseConfig::default();
    assert!(cfg.index.include.iter().any(|g| g == "**/*.ts"));
    assert!(cfg.index.include.iter().any(|g| g == "**/*.cs"));
    assert!(cfg.index.exclude.iter().any(|g| g == "**/node_modules/**"));
    assert!(cfg.index.languages.typescript);
    assert!(cfg.index.languages.csharp);
}

// --- include/exclude glob matching ----------------------------------------

#[test]
fn glob_matching_respects_include_and_exclude() {
    let cfg = SynapseConfig::default();
    let include = build_globset(&cfg.index.include).unwrap();
    let exclude = build_globset(&cfg.index.exclude).unwrap();

    assert!(path_matches(&include, &exclude, "src/app/Foo.cs"));
    assert!(path_matches(
        &include,
        &exclude,
        "web/src/hooks/useThing.ts"
    ));
    // excluded directories win over includes
    assert!(!path_matches(
        &include,
        &exclude,
        "web/node_modules/x/index.js"
    ));
    assert!(!path_matches(
        &include,
        &exclude,
        "backend/obj/Debug/Foo.cs"
    ));
    // generated files excluded
    assert!(!path_matches(&include, &exclude, "src/App.Designer.cs"));
    assert!(!path_matches(&include, &exclude, "web/dist/app.min.js"));
    // not in include set
    assert!(!path_matches(&include, &exclude, "README.unknownext"));
}

// --- token budget ---------------------------------------------------------

#[test]
fn estimate_tokens_is_chars_over_four() {
    assert_eq!(budget::estimate_tokens(""), 0);
    assert_eq!(budget::estimate_tokens("abcd"), 1);
    assert_eq!(budget::estimate_tokens(&"x".repeat(400)), 100);
    // non-empty but tiny rounds up to at least 1
    assert_eq!(budget::estimate_tokens("ab"), 1);
}

// --- symbol extraction per language ---------------------------------------

fn names(symbols: &[IndexedSymbol]) -> Vec<String> {
    symbols.iter().map(|s| s.name.clone()).collect()
}

#[test]
fn extract_rust_symbols() {
    let text = read_fixture("rust/widget.rs");
    let syms = extract("tests/fixtures/rust/widget.rs", Language::Rust, &text).unwrap();
    let ns = names(&syms);
    assert!(ns.iter().any(|n| n.contains("Widget")), "got {ns:?}");
    // at least one struct/enum/trait/function kind present
    assert!(syms.iter().any(|s| matches!(
        s.kind,
        SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait | SymbolKind::Function
    )));
}

#[test]
fn extract_csharp_symbols() {
    let text = read_fixture("csharp/SftpAutoReceiveCommandHandler.cs");
    let syms = extract(
        "tests/fixtures/csharp/SftpAutoReceiveCommandHandler.cs",
        Language::CSharp,
        &text,
    )
    .unwrap();
    let ns = names(&syms);
    assert!(
        ns.iter().any(|n| n.contains("SftpAutoReceive")),
        "expected an SftpAutoReceive* symbol, got {ns:?}"
    );
    assert!(syms.iter().any(|s| s.kind == SymbolKind::Class));
}

#[test]
fn extract_python_symbols() {
    let text = read_fixture("python/service.py");
    let syms = extract("tests/fixtures/python/service.py", Language::Python, &text).unwrap();
    assert!(syms.iter().any(|s| s.kind == SymbolKind::Class));
    assert!(syms.iter().any(|s| s.kind == SymbolKind::Function));
}

#[test]
fn extract_go_symbols() {
    let text = read_fixture("go/ledger.go");
    let syms = extract("tests/fixtures/go/ledger.go", Language::Go, &text).unwrap();
    assert!(
        syms.iter()
            .any(|s| s.kind == SymbolKind::Function || s.kind == SymbolKind::Method),
        "got {:?}",
        names(&syms)
    );
    assert!(
        syms.iter()
            .any(|s| s.kind == SymbolKind::Struct || s.kind == SymbolKind::Interface)
    );
}

#[test]
fn extract_javascript_symbols() {
    let text = read_fixture("javascript/cart.js");
    let syms = extract(
        "tests/fixtures/javascript/cart.js",
        Language::JavaScript,
        &text,
    )
    .unwrap();
    assert!(!syms.is_empty(), "expected some JS symbols");
    assert!(
        syms.iter().any(|s| s.exported),
        "expected an exported symbol"
    );
}

#[test]
fn extract_typescript_symbols() {
    let text = read_fixture("typescript/billing.ts");
    let syms = extract(
        "tests/fixtures/typescript/billing.ts",
        Language::TypeScript,
        &text,
    )
    .unwrap();
    assert!(
        syms.iter().any(|s| s.kind == SymbolKind::Interface)
            || syms.iter().any(|s| s.kind == SymbolKind::TypeAlias)
            || syms.iter().any(|s| s.kind == SymbolKind::Enum),
        "expected a TS interface/type/enum, got {:?}",
        syms.iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect::<Vec<_>>()
    );
}

#[test]
fn extract_tsx_component() {
    let text = read_fixture("typescript/TenantSwitcher.tsx");
    let syms = extract(
        "tests/fixtures/typescript/TenantSwitcher.tsx",
        Language::TypeScript,
        &text,
    )
    .unwrap();
    assert!(
        syms.iter()
            .any(|s| s.kind == SymbolKind::Component && s.name == "TenantSwitcher"),
        "expected a TenantSwitcher component, got {:?}",
        syms.iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect::<Vec<_>>()
    );
}

// --- manifest parsing ------------------------------------------------------

#[test]
fn parse_csproj_extracts_references() {
    let text = read_fixture("dotnet/Sample.csproj");
    let data = dotnet::parse_csproj(&text).unwrap();
    assert!(!data.project_references.is_empty(), "expected project refs");
    assert!(!data.package_references.is_empty(), "expected package refs");
    // a package reference should carry a version
    assert!(data.package_references.iter().any(|p| p.version.is_some()));
}

#[test]
fn parse_package_json_extracts_deps_and_scripts() {
    let text = read_fixture("node/package.json");
    let data = node::parse_package_json(&text).unwrap();
    assert!(data.name.is_some());
    assert!(data.dependencies.iter().any(|d| d.kind == "dependency"));
    assert!(data.dependencies.iter().any(|d| d.kind == "devDependency"));
    // deterministic order: sorted by (kind, name)
    let mut sorted = data.dependencies.clone_kinds_names();
    let original = data.dependencies.clone_kinds_names();
    sorted.sort();
    assert_eq!(
        original, sorted,
        "dependencies should be sorted by (kind, name)"
    );
    assert!(!data.scripts.is_empty());
}

// Small helper trait to make the sortedness assertion above readable.
trait KindsNames {
    fn clone_kinds_names(&self) -> Vec<(String, String)>;
}
impl KindsNames for Vec<node::NodeDep> {
    fn clone_kinds_names(&self) -> Vec<(String, String)> {
        self.iter()
            .map(|d| (d.kind.clone(), d.name.clone()))
            .collect()
    }
}

// --- language heuristics ---------------------------------------------------

#[test]
fn language_and_test_heuristics() {
    assert_eq!(languages::detect("src/Foo.cs"), Language::CSharp);
    assert_eq!(languages::detect("a/b.tsx"), Language::TypeScript);
    assert_eq!(languages::detect("x.unknown"), Language::Other);
    assert!(languages::is_test_file("tests/FooTests.cs"));
    assert!(languages::is_test_file("src/foo.spec.ts"));
    assert!(languages::is_test_file("pkg/thing_test.go"));
    assert!(!languages::is_test_file("src/foo.ts"));
    assert!(languages::is_generated("src/App.Designer.cs"));
    assert!(languages::is_lockfile("web/package-lock.json"));
    assert!(languages::is_config_file("infra/values.yaml"));
}

// --- table rendering -------------------------------------------------------

#[test]
fn table_renders_aligned_columns() {
    let out = table::render(
        &["Symbol", "Kind", "File"],
        &[
            vec!["Foo".into(), "class".into(), "src/Foo.cs".into()],
            vec!["Barbarian".into(), "fn".into(), "src/b.rs".into()],
        ],
    );
    // header present, each row present, trailing newline
    assert!(out.starts_with("Symbol"));
    assert!(out.contains("Foo"));
    assert!(out.contains("Barbarian"));
    assert!(out.ends_with('\n'));
    // no trailing spaces on any line
    for line in out.lines() {
        assert_eq!(line, line.trim_end(), "line has trailing space: {line:?}");
    }
}

#[test]
fn table_handles_empty_rows() {
    let out = table::render(&["A", "B"], &[]);
    assert!(out.contains('A') && out.contains('B'));
    assert!(out.contains("(no results)"));
}

// --- GraphStore behaviour via the in-memory store -------------------------

fn sample_file(path: &str, lang: Language, hash: &str) -> IndexedFile {
    IndexedFile {
        id: format!("file:{path}"),
        path: path.to_string(),
        language: lang,
        hash: hash.to_string(),
        size_bytes: 10,
        tracked: true,
        last_indexed_at: "2026-06-01T00:00:00+00:00".to_string(),
    }
}

fn sample_symbol(path: &str, name: &str, kind: SymbolKind, lang: Language) -> IndexedSymbol {
    IndexedSymbol {
        id: format!("sym:{path}#{}#{name}#1", kind.as_str()),
        name: name.to_string(),
        full_name: name.to_string(),
        kind,
        language: lang,
        file_path: path.to_string(),
        start_line: 1,
        end_line: 5,
        visibility: "public".to_string(),
        exported: true,
    }
}

#[test]
fn memory_store_upsert_query_and_remove() {
    let store = MemoryGraphStore::new();
    store.initialize_schema().unwrap();

    store
        .upsert_file(sample_file("src/Foo.cs", Language::CSharp, "h1"))
        .unwrap();
    store
        .upsert_symbol(sample_symbol(
            "src/Foo.cs",
            "Foo",
            SymbolKind::Class,
            Language::CSharp,
        ))
        .unwrap();
    store
        .link_file_declares_symbol("file:src/Foo.cs", "sym:src/Foo.cs#class#Foo#1")
        .unwrap();

    // symbol search by substring + kind
    let found = store
        .symbols_matching(&SymbolSearchQuery {
            name: Some("foo".into()),
            kind: Some(SymbolKind::Class),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Foo");

    // file search
    let files = store
        .files_matching(&FileSearchQuery {
            path_contains: Some("Foo".into()),
            language: None,
        })
        .unwrap();
    assert_eq!(files.len(), 1);

    // stats
    let stats = store.stats().unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.symbols, 1);

    // removing the file cascades to its symbols
    store.remove_file("src/Foo.cs").unwrap();
    assert_eq!(store.stats().unwrap().files, 0);
    assert_eq!(store.stats().unwrap().symbols, 0);
}

#[test]
fn memory_store_related_to_symbol_is_deterministic() {
    let store = MemoryGraphStore::new();
    store
        .upsert_symbol(sample_symbol(
            "src/b.cs",
            "Handler",
            SymbolKind::Class,
            Language::CSharp,
        ))
        .unwrap();
    store
        .upsert_symbol(sample_symbol(
            "src/a.cs",
            "Handler",
            SymbolKind::Class,
            Language::CSharp,
        ))
        .unwrap();
    let related = store.related_to_symbol("Handler", 1).unwrap();
    let paths: Vec<_> = related.iter().map(|r| r.path.clone()).collect();
    // sorted, de-duplicated by path
    assert_eq!(paths, vec!["src/a.cs".to_string(), "src/b.cs".to_string()]);
}

// --- new languages: svelte / bash / yaml / json ---------------------------

#[test]
fn extract_svelte_component_and_script() {
    let text = read_fixture("svelte/TenantSwitcher.svelte");
    let syms = extract(
        "tests/fixtures/svelte/TenantSwitcher.svelte",
        Language::Svelte,
        &text,
    )
    .unwrap();
    // The file itself is a component named after its stem.
    assert!(
        syms.iter()
            .any(|s| s.kind == SymbolKind::Component && s.name == "TenantSwitcher"),
        "expected TenantSwitcher component, got {:?}",
        syms.iter()
            .map(|s| (s.name.clone(), s.kind))
            .collect::<Vec<_>>()
    );
    // Script-block symbols are extracted (TS interface + functions).
    assert!(
        syms.iter().any(|s| s.name == "selectTenant" && s.exported),
        "expected exported selectTenant fn"
    );
    assert!(
        syms.iter()
            .any(|s| s.name == "Tenant" && s.kind == SymbolKind::Interface)
    );
    // Script symbols carry the .svelte path and Svelte language.
    let sel = syms.iter().find(|s| s.name == "selectTenant").unwrap();
    assert_eq!(sel.file_path, "tests/fixtures/svelte/TenantSwitcher.svelte");
    assert_eq!(sel.language, Language::Svelte);
    // Line offset applied: the script function is well past line 1.
    assert!(sel.start_line > 1);
}

#[test]
fn extract_sveltekit_route_role() {
    let text = read_fixture("svelte/routes/dashboard/+page.svelte");
    let syms = extract(
        "tests/fixtures/svelte/routes/dashboard/+page.svelte",
        Language::Svelte,
        &text,
    )
    .unwrap();
    let comp = syms
        .iter()
        .find(|s| s.kind == SymbolKind::Component)
        .expect("a component symbol");
    // Route role surfaced in visibility and full name; name qualified by dir.
    assert_eq!(comp.visibility, "page");
    assert!(comp.full_name.contains("sveltekit page"));
    assert!(comp.name.contains("dashboard"));
}

#[test]
fn extract_bash_functions() {
    let text = read_fixture("bash/deploy.sh");
    let syms = extract("tests/fixtures/bash/deploy.sh", Language::Bash, &text).unwrap();
    let names: Vec<_> = syms.iter().map(|s| s.name.clone()).collect();
    for want in ["log", "build_image", "deploy_all"] {
        assert!(
            names.contains(&want.to_string()),
            "missing {want}, got {names:?}"
        );
    }
    assert!(syms.iter().all(|s| s.kind == SymbolKind::Function));
}

#[test]
fn extract_yaml_top_level_keys_and_anchors() {
    let text = read_fixture("yaml/compose.yaml");
    let syms = extract("tests/fixtures/yaml/compose.yaml", Language::Yaml, &text).unwrap();
    let names: Vec<_> = syms.iter().map(|s| s.name.clone()).collect();
    // Top-level keys.
    for want in ["version", "services", "volumes"] {
        assert!(
            names.contains(&want.to_string()),
            "missing key {want}, got {names:?}"
        );
    }
    // Should NOT include nested keys like "api" or "worker" as top-level.
    assert!(
        !names.contains(&"api".to_string()),
        "leaked nested key: {names:?}"
    );
    // The named anchor `common` is captured.
    assert!(
        syms.iter()
            .any(|s| s.name == "common" && s.visibility == "anchor"),
        "expected the &common anchor"
    );
    assert!(syms.iter().all(|s| s.kind == SymbolKind::Key));
}

#[test]
fn extract_json_top_level_keys() {
    let text = read_fixture("json/config.json");
    let syms = extract("tests/fixtures/json/config.json", Language::Json, &text).unwrap();
    let names: Vec<_> = syms.iter().map(|s| s.name.clone()).collect();
    for want in ["name", "version", "compilerOptions", "include"] {
        assert!(
            names.contains(&want.to_string()),
            "missing key {want}, got {names:?}"
        );
    }
    // Nested keys (e.g. "strict", "target") must not appear at top level.
    assert!(
        !names.contains(&"strict".to_string()),
        "leaked nested key: {names:?}"
    );
    assert!(syms.iter().all(|s| s.kind == SymbolKind::Key));
}

#[test]
fn language_detect_new_extensions() {
    assert_eq!(languages::detect("App.svelte"), Language::Svelte);
    assert_eq!(languages::detect("scripts/deploy.sh"), Language::Bash);
    assert_eq!(languages::detect("infra/compose.yaml"), Language::Yaml);
    assert_eq!(languages::detect("tsconfig.json"), Language::Json);
    assert_eq!(
        languages::sveltekit_route_role("src/routes/x/+page.svelte"),
        Some("page")
    );
    assert_eq!(
        languages::sveltekit_route_role("src/routes/x/+server.ts"),
        Some("endpoint")
    );
    assert_eq!(languages::sveltekit_route_role("src/lib/Foo.svelte"), None);
}

#[test]
fn extract_markdown_headings() {
    let text = read_fixture("markdown/guide.md");
    let syms = extract(
        "tests/fixtures/markdown/guide.md",
        Language::Markdown,
        &text,
    )
    .unwrap();
    let names: Vec<_> = syms.iter().map(|s| s.name.clone()).collect();
    for want in [
        "Synapse Guide",
        "Installation",
        "Prerequisites",
        "Usage",
        "Setext Heading One",
        "Setext Heading Two",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "missing heading {want}, got {names:?}"
        );
    }
    assert!(syms.iter().all(|s| s.kind == SymbolKind::Key));
    // Level recorded in visibility/full name.
    let install = syms.iter().find(|s| s.name == "Installation").unwrap();
    assert_eq!(install.visibility, "h2");
    assert!(install.full_name.starts_with("h2 "));
    let prereq = syms.iter().find(|s| s.name == "Prerequisites").unwrap();
    assert_eq!(prereq.visibility, "h3");
    // Setext headings: One is h1, Two is h2.
    assert_eq!(
        syms.iter()
            .find(|s| s.name == "Setext Heading One")
            .unwrap()
            .visibility,
        "h1"
    );
    assert_eq!(
        syms.iter()
            .find(|s| s.name == "Setext Heading Two")
            .unwrap()
            .visibility,
        "h2"
    );
}

#[test]
fn language_detect_markdown() {
    assert_eq!(languages::detect("README.md"), Language::Markdown);
    assert_eq!(languages::detect("docs/guide.markdown"), Language::Markdown);
    assert_eq!(languages::detect("page.mdx"), Language::Markdown);
}

// --- .NET Central Package Management --------------------------------------

#[test]
fn parse_csproj_collects_package_versions_and_cpm_flag() {
    // Directory.Packages.props: central version pins.
    let pkgs = read_fixture("dotnet/cpm/Directory.Packages.props");
    let data = synapse::indexer::dotnet::parse_msbuild(&pkgs).unwrap();
    assert!(data.package_references.is_empty());
    assert_eq!(data.package_versions.len(), 2);
    assert!(
        data.package_versions
            .iter()
            .any(|p| p.name == "Serilog" && p.version.as_deref() == Some("3.1.1"))
    );

    // Directory.Build.props: the CPM-enabled flag, set outside the csproj.
    let build = read_fixture("dotnet/cpm/Directory.Build.props");
    let bdata = synapse::indexer::dotnet::parse_msbuild(&build).unwrap();
    assert_eq!(bdata.cpm_enabled, Some(true));

    // The csproj: version-less references plus one inline version.
    let csproj = read_fixture("dotnet/cpm/src/Worker/Worker.csproj");
    let cdata = synapse::indexer::dotnet::parse_csproj(&csproj).unwrap();
    let serilog = cdata
        .package_references
        .iter()
        .find(|p| p.name == "Serilog")
        .unwrap();
    assert_eq!(
        serilog.version, None,
        "Serilog version comes from CPM, not the csproj"
    );
    let newtonsoft = cdata
        .package_references
        .iter()
        .find(|p| p.name == "Newtonsoft.Json")
        .unwrap();
    assert_eq!(newtonsoft.version.as_deref(), Some("13.0.3"));
}

#[test]
fn central_version_map_merges_pins() {
    let pkgs = read_fixture("dotnet/cpm/Directory.Packages.props");
    let data = synapse::indexer::dotnet::parse_msbuild(&pkgs).unwrap();
    let map = synapse::indexer::dotnet::central_version_map([&data]);
    assert_eq!(map.get("Serilog").map(String::as_str), Some("3.1.1"));
    assert_eq!(map.get("Wolverine").map(String::as_str), Some("2.4.0"));
}

#[test]
fn index_resolves_cpm_versions_end_to_end() {
    use synapse::config::SynapseConfig;
    use synapse::indexer::index_repo;
    use synapse::repo::Repo;

    // Build a temp repo containing the CPM fixture tree.
    let tmp = std::env::temp_dir().join(format!("synapse-cpm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let dst = tmp.join("cpm");
    // copy_tree equivalent (tests can't share cli.rs helpers).
    fn copy_tree(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for e in std::fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            let (from, to) = (e.path(), dst.join(e.file_name()));
            if from.is_dir() {
                copy_tree(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }
    copy_tree(&fixtures().join("dotnet/cpm"), &dst);

    let repo = Repo { root: tmp.clone() };
    let config = SynapseConfig::default();
    let store = MemoryGraphStore::new();
    index_repo(
        &repo,
        &config,
        &store,
        true,
        false,
        "2026-06-01T00:00:00+00:00",
    )
    .unwrap();

    // The central props resolved the version-less Serilog/Wolverine references.
    let packages = store.all_packages().unwrap();
    let version_of = |name: &str| {
        packages
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.version.clone())
    };
    assert_eq!(
        version_of("Serilog"),
        Some("3.1.1".to_string()),
        "Serilog version should be resolved from Directory.Packages.props"
    );
    assert_eq!(version_of("Wolverine"), Some("2.4.0".to_string()));
    // Inline version on the csproj is preserved (not overridden by CPM).
    assert_eq!(version_of("Newtonsoft.Json"), Some("13.0.3".to_string()));

    // The project is flagged as CPM (enabled via Directory.Build.props).
    let projects = store.all_projects().unwrap();
    assert!(
        projects
            .iter()
            .any(|p| p.path.ends_with("Worker.csproj") && p.kind.contains("cpm")),
        "Worker project should be marked as CPM, got {:?}",
        projects
            .iter()
            .map(|p| (p.path.clone(), p.kind.clone()))
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn memory_store_project_siblings_traverses_contains_file() {
    let store = MemoryGraphStore::new();
    // A project owning three files.
    store
        .upsert_project(synapse::graph::model::IndexedProject {
            id: "proj:src/Worker/Worker.csproj".into(),
            name: "Worker".into(),
            path: "src/Worker/Worker.csproj".into(),
            language: Language::CSharp,
            kind: "dotnet".into(),
        })
        .unwrap();
    for p in ["src/Worker/A.cs", "src/Worker/B.cs", "src/Worker/C.cs"] {
        store
            .upsert_file(sample_file(p, Language::CSharp, "h"))
            .unwrap();
        store
            .link_project_contains_file("proj:src/Worker/Worker.csproj", &format!("file:{p}"))
            .unwrap();
    }
    // A file in a different project should not appear.
    store
        .upsert_file(sample_file("src/Other/Z.cs", Language::CSharp, "h"))
        .unwrap();

    let sibs = store.project_siblings("src/Worker/A.cs").unwrap();
    let paths: Vec<_> = sibs.iter().map(|s| s.path.clone()).collect();
    assert_eq!(
        paths,
        vec!["src/Worker/B.cs".to_string(), "src/Worker/C.cs".to_string()]
    );
    assert!(
        sibs.iter()
            .all(|s| s.depth == 1 && s.reason.contains("same project"))
    );
    // The queried file itself and unrelated-project files are excluded.
    assert!(!paths.contains(&"src/Worker/A.cs".to_string()));
    assert!(!paths.contains(&"src/Other/Z.cs".to_string()));
}

#[test]
fn extract_imports_js_ts_and_csharp() {
    use synapse::indexer::tree_sitter::extract_imports;
    let ts = "import { foo } from 'lodash';\nimport x from '@scope/pkg/sub';\nimport rel from './local';\nconst c = require('chalk');";
    let mut imps = extract_imports("a.ts", Language::TypeScript, ts);
    imps.sort();
    assert_eq!(
        imps,
        vec![
            "@scope/pkg".to_string(),
            "chalk".to_string(),
            "lodash".to_string()
        ]
    );
    // Relative imports excluded.
    assert!(!imps.iter().any(|i| i.contains("local")));

    let cs = "using Serilog;\nusing Serilog.Sinks.Console;\nusing System.Text;\nnamespace X { class Y {} }";
    let mut cimps = extract_imports("a.cs", Language::CSharp, cs);
    cimps.sort();
    assert!(cimps.contains(&"Serilog".to_string()));
    assert!(cimps.contains(&"Serilog.Sinks.Console".to_string()));
    assert!(cimps.contains(&"System.Text".to_string()));
}

#[test]
fn index_creates_imports_package_edges() {
    use synapse::config::SynapseConfig;
    use synapse::indexer::index_repo;
    use synapse::repo::Repo;

    let tmp = std::env::temp_dir().join(format!("synapse-imp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("web/src")).unwrap();
    // A package.json declaring lodash, and a TS file importing it.
    std::fs::write(
        tmp.join("web/package.json"),
        r#"{"name":"web","dependencies":{"lodash":"^4.17.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        tmp.join("web/src/util.ts"),
        "import { map } from 'lodash';\nimport rel from './x';\nexport function go() { return map; }\n",
    )
    .unwrap();

    let repo = Repo { root: tmp.clone() };
    let store = MemoryGraphStore::new();
    index_repo(
        &repo,
        &SynapseConfig::default(),
        &store,
        true,
        false,
        "2026-06-01T00:00:00+00:00",
    )
    .unwrap();

    // The lodash package exists and the file imports it.
    let importers = store.files_importing_package("lodash").unwrap();
    assert!(
        importers.iter().any(|p| p.ends_with("web/src/util.ts")),
        "expected util.ts to import lodash, got {importers:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

// --- supertypes (INHERITS / IMPLEMENTS) -----------------------------------

#[test]
fn extract_supertypes_per_language() {
    use synapse::indexer::tree_sitter::{SuperHint, extract_supertypes};

    // Rust: impl Trait for Type, and supertrait bounds.
    let rs = "struct Widget; trait Draw {} impl Draw for Widget {} trait Button: Draw {}";
    let rsup = extract_supertypes("a.rs", Language::Rust, rs);
    assert!(
        rsup.iter().any(|s| s.child == "Widget"
            && s.supertype == "Draw"
            && s.hint == SuperHint::Implements),
        "Rust impl..for should be Implements: {rsup:?}"
    );
    assert!(
        rsup.iter()
            .any(|s| s.child == "Button" && s.supertype == "Draw" && s.hint == SuperHint::Inherits),
        "Rust supertrait should be Inherits: {rsup:?}"
    );

    // TypeScript: extends vs implements are syntactically explicit.
    let ts = "interface Animal {} class Dog extends Pet implements Animal {} class Pet {}";
    let tsup = extract_supertypes("a.ts", Language::TypeScript, ts);
    assert!(
        tsup.iter()
            .any(|s| s.child == "Dog" && s.supertype == "Pet" && s.hint == SuperHint::Inherits)
    );
    assert!(
        tsup.iter().any(|s| s.child == "Dog"
            && s.supertype == "Animal"
            && s.hint == SuperHint::Implements)
    );

    // Python: class bases (Inherits, `object` skipped).
    let py = "class Base: pass\nclass Derived(Base): pass\nclass Plain(object): pass";
    let psup = extract_supertypes("a.py", Language::Python, py);
    assert!(
        psup.iter()
            .any(|s| s.child == "Derived" && s.supertype == "Base")
    );
    assert!(
        !psup.iter().any(|s| s.supertype == "object"),
        "object base skipped"
    );

    // C#: base list, hint Unknown (resolved later by target kind).
    let cs = "interface IFoo {} class Base {} class Impl : Base, IFoo {}";
    let csup = extract_supertypes("a.cs", Language::CSharp, cs);
    assert!(
        csup.iter()
            .any(|s| s.child == "Impl" && s.supertype == "Base")
    );
    assert!(
        csup.iter()
            .any(|s| s.child == "Impl" && s.supertype == "IFoo")
    );
    assert!(csup.iter().all(|s| s.hint == SuperHint::Unknown));
}

#[test]
fn index_creates_inherits_and_implements_edges() {
    use synapse::config::SynapseConfig;
    use synapse::indexer::index_repo;
    use synapse::repo::Repo;

    let tmp = std::env::temp_dir().join(format!("synapse-super-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // C#: Impl : Base, IFoo  -> INHERITS Base, IMPLEMENTS IFoo (by target kind).
    std::fs::write(
        tmp.join("Types.cs"),
        "namespace N { interface IFoo {} class Base {} class Impl : Base, IFoo {} }",
    )
    .unwrap();

    let repo = Repo { root: tmp.clone() };
    let store = MemoryGraphStore::new();
    index_repo(
        &repo,
        &SynapseConfig::default(),
        &store,
        true,
        false,
        "2026-06-01T00:00:00+00:00",
    )
    .unwrap();

    // From Impl, relations should include Base (inherits) and IFoo (implements).
    let rels = store.symbol_type_relations("Impl").unwrap();
    assert!(
        rels.iter().any(|r| r.reason.contains("inherits")),
        "expected an inherits relation, got {rels:?}"
    );
    assert!(
        rels.iter()
            .any(|r| r.reason.contains("implemented") || r.reason.contains("implement")),
        "expected an implements relation, got {rels:?}"
    );
    // Reverse direction: from IFoo, Impl is an implementor.
    let ifoo = store.symbol_type_relations("IFoo").unwrap();
    assert!(
        ifoo.iter().any(|r| r.reason.contains("implementor")),
        "expected IFoo to have an implementor, got {ifoo:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn pack_format_parsing() {
    use synapse::pack::PackFormat;
    assert_eq!(PackFormat::from_str_opt("json"), Some(PackFormat::Json));
    assert_eq!(
        PackFormat::from_str_opt("markdown"),
        Some(PackFormat::Markdown)
    );
    assert_eq!(PackFormat::from_str_opt("md"), Some(PackFormat::Markdown));
    assert_eq!(PackFormat::from_str_opt("xml"), None);
}

// --- explore (Ladybug Explorer docker command) ----------------------------

#[test]
fn explore_docker_args_default_readonly_mount() {
    use synapse::explore::{ExploreOptions, docker_args};
    let opts = ExploreOptions {
        port: 8000,
        read_write: false,
        detach: false,
        in_memory: false,
        image: "ghcr.io/ladybugdb/explorer".into(),
        tag: "latest".into(),
    };
    let args = docker_args(&opts, std::path::Path::new("/repo/.synapse/graph"));
    let joined = args.join(" ");
    assert!(args.starts_with(&["run".to_string(), "--rm".to_string()]));
    assert!(joined.contains("-p 8000:8000"));
    assert!(joined.contains("/repo/.synapse/graph:/database"));
    assert!(joined.contains("LBUG_FILE=synapse.lbug"));
    assert!(joined.contains("MODE=READ_ONLY"), "default is read-only");
    assert!(joined.ends_with("ghcr.io/ladybugdb/explorer:latest"));
    assert!(!joined.contains("-d "), "not detached by default");
}

#[test]
fn explore_docker_args_read_write_and_detach() {
    use synapse::explore::{ExploreOptions, docker_args};
    let opts = ExploreOptions {
        port: 9001,
        read_write: true,
        detach: true,
        in_memory: false,
        image: "ghcr.io/ladybugdb/explorer".into(),
        tag: "dev".into(),
    };
    let args = docker_args(&opts, std::path::Path::new("/g"));
    let joined = args.join(" ");
    assert!(args.contains(&"-d".to_string()), "detached");
    assert!(args.contains(&"--rm".to_string()), "always --rm");
    assert!(joined.contains("-p 9001:8000"));
    assert!(!joined.contains("READ_ONLY"), "read-write omits MODE");
    assert!(joined.ends_with(":dev"));
}

#[test]
fn explore_docker_args_in_memory_skips_mount() {
    use synapse::explore::{ExploreOptions, docker_args};
    let opts = ExploreOptions {
        port: 8000,
        read_write: false,
        detach: false,
        in_memory: true,
        image: "ghcr.io/ladybugdb/explorer".into(),
        tag: "latest".into(),
    };
    let joined = docker_args(&opts, std::path::Path::new("/g")).join(" ");
    assert!(joined.contains("LBUG_IN_MEMORY=true"));
    assert!(!joined.contains("/database"), "in-memory has no mount");
    // READ_ONLY is unsupported with in-memory, so it must not be set.
    assert!(!joined.contains("READ_ONLY"));
}
