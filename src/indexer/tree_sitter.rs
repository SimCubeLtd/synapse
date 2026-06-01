//! Tree-sitter backed symbol extraction.
//!
//! For each supported [`Language`] we run a small set of tree-sitter queries
//! that capture a definition node plus its name node, then map those into
//! [`IndexedSymbol`]s. Extraction is best-effort: a grammar that fails to load
//! or a query that fails to compile yields an empty result rather than aborting
//! the whole index. Output is sorted and de-duplicated for determinism.

use crate::graph::model::{IndexedSymbol, Language, SymbolKind};
use crate::indexer::{languages, symbol_id};
use anyhow::{Result, anyhow};
use tree_sitter::{Language as TsLanguage, Node, Parser, Query, QueryCursor, StreamingIterator};

/// Extract symbols from `text` (the contents of `path`) for the given `lang`.
///
/// Returns an empty vec (never an error) when the grammar/query cannot be set
/// up, so a single malformed file does not break a whole index run.
pub fn extract(path: &str, lang: Language, text: &str) -> Result<Vec<IndexedSymbol>> {
    let mut symbols = match lang {
        Language::Rust => extract_rust(path, text),
        Language::CSharp => extract_csharp(path, text),
        Language::Python => extract_python(path, text),
        Language::Go => extract_go(path, text),
        Language::JavaScript | Language::TypeScript => extract_js_ts(path, lang, text),
        Language::Svelte => extract_svelte(path, text),
        Language::Bash => extract_bash(path, text),
        Language::Yaml => extract_yaml(path, text),
        Language::Json => extract_json(path, text),
        Language::Markdown => extract_markdown(path, text),
        Language::Other => Ok(Vec::new()),
    }
    .unwrap_or_else(|e| {
        tracing::debug!("symbol extraction failed for {path}: {e}");
        Vec::new()
    });

    // Deterministic ordering: by (start_line, name, kind).
    symbols.sort_by(|a, b| {
        a.start_line
            .cmp(&b.start_line)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });
    // De-duplicate identical (name, start_line, kind).
    symbols.dedup_by(|a, b| a.name == b.name && a.start_line == b.start_line && a.kind == b.kind);

    Ok(symbols)
}

/// Parse `text` with `lang`, returning the root node's tree.
fn parse(lang: &TsLanguage, text: &str) -> Result<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(lang)?;
    parser
        .parse(text, None)
        .ok_or_else(|| anyhow!("parse failed"))
}

/// Locate the definition + name capture indices within a query match.
///
/// `def_cap` and `name_cap` are the literal capture names (without `@`).
fn find_captures<'a>(
    query: &Query,
    m: &tree_sitter::QueryMatch<'a, 'a>,
    def_cap: &str,
    name_cap: &str,
) -> Option<(Node<'a>, Node<'a>)> {
    let names = query.capture_names();
    let mut def: Option<Node> = None;
    let mut name: Option<Node> = None;
    for cap in m.captures {
        let cname = names[cap.index as usize];
        if cname == def_cap {
            def = Some(cap.node);
        } else if cname == name_cap {
            name = Some(cap.node);
        }
    }
    match (def, name) {
        (Some(d), Some(n)) => Some((d, n)),
        _ => None,
    }
}

/// True if any ancestor (including self) of `node` has the given `kind`.
fn has_ancestor(node: Node, kind: &str) -> bool {
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.kind() == kind {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// True if `node` (or a child within it) carries a `pub` visibility modifier.
fn rust_is_pub(node: Node, src: &[u8]) -> bool {
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if child.kind() == "visibility_modifier" {
            if let Ok(t) = child.utf8_text(src) {
                return t.starts_with("pub");
            }
            return true;
        }
    }
    false
}

/// First word starts with an uppercase ASCII letter.
fn starts_upper(name: &str) -> bool {
    name.chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}

/// Build an [`IndexedSymbol`] from the common pieces.
#[allow(clippy::too_many_arguments)]
fn make_symbol(
    path: &str,
    lang: Language,
    name: &str,
    kind: SymbolKind,
    def: Node,
    visibility: &str,
    exported: bool,
) -> IndexedSymbol {
    let start_line = def.start_position().row as u32 + 1;
    let end_line = def.end_position().row as u32 + 1;
    IndexedSymbol {
        id: symbol_id(path, kind.as_str(), name, start_line),
        name: name.to_string(),
        full_name: name.to_string(),
        kind,
        language: lang,
        file_path: path.to_string(),
        start_line,
        end_line,
        visibility: visibility.to_string(),
        exported,
    }
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

const RUST_QUERY: &str = r#"
(struct_item   name: (type_identifier) @name) @def
(enum_item     name: (type_identifier) @name) @def
(trait_item    name: (type_identifier) @name) @def
(function_item name: (identifier)      @name) @def
(mod_item      name: (identifier)      @name) @def
"#;

fn rust_kind(node_kind: &str) -> Option<SymbolKind> {
    Some(match node_kind {
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "trait_item" => SymbolKind::Trait,
        "function_item" => SymbolKind::Function,
        "mod_item" => SymbolKind::Module,
        _ => return None,
    })
}

fn extract_rust(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_rust::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let query = Query::new(&lang, RUST_QUERY)?;
    let src = text.as_bytes();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let Some((def, name_node)) = find_captures(&query, m, "def", "name") else {
            continue;
        };
        let Some(kind) = rust_kind(def.kind()) else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(src) else {
            continue;
        };
        let is_pub = rust_is_pub(def, src);
        out.push(make_symbol(
            path,
            Language::Rust,
            name,
            kind,
            def,
            if is_pub { "pub" } else { "" },
            is_pub,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

const CSHARP_QUERY: &str = r#"
(class_declaration       name: (identifier) @name) @def
(record_declaration      name: (identifier) @name) @def
(struct_declaration      name: (identifier) @name) @def
(interface_declaration   name: (identifier) @name) @def
(enum_declaration        name: (identifier) @name) @def
(method_declaration      name: (identifier) @name) @def
(constructor_declaration name: (identifier) @name) @def
"#;

fn csharp_kind(node_kind: &str) -> Option<SymbolKind> {
    Some(match node_kind {
        "class_declaration" => SymbolKind::Class,
        "record_declaration" => SymbolKind::Record,
        "struct_declaration" => SymbolKind::Struct,
        "interface_declaration" => SymbolKind::Interface,
        "enum_declaration" => SymbolKind::Enum,
        "method_declaration" => SymbolKind::Method,
        "constructor_declaration" => SymbolKind::Constructor,
        _ => return None,
    })
}

/// Extract the access modifier (public/private/internal/protected) from a C#
/// declaration's `modifier` children. Defaults to "" when none present.
fn csharp_visibility(node: Node, src: &[u8]) -> String {
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if child.kind() == "modifier"
            && let Ok(t) = child.utf8_text(src)
        {
            match t {
                "public" | "private" | "internal" | "protected" => return t.to_string(),
                _ => {}
            }
        }
    }
    String::new()
}

fn extract_csharp(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_c_sharp::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let query = Query::new(&lang, CSHARP_QUERY)?;
    let src = text.as_bytes();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let Some((def, name_node)) = find_captures(&query, m, "def", "name") else {
            continue;
        };
        let Some(kind) = csharp_kind(def.kind()) else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(src) else {
            continue;
        };
        let vis = csharp_visibility(def, src);
        let exported = vis == "public";
        out.push(make_symbol(
            path,
            Language::CSharp,
            name,
            kind,
            def,
            &vis,
            exported,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

const PYTHON_QUERY: &str = r#"
(class_definition    name: (identifier) @name) @def
(function_definition name: (identifier) @name) @def
"#;

fn extract_python(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_python::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let query = Query::new(&lang, PYTHON_QUERY)?;
    let src = text.as_bytes();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let Some((def, name_node)) = find_captures(&query, m, "def", "name") else {
            continue;
        };
        let kind = match def.kind() {
            "class_definition" => SymbolKind::Class,
            "function_definition" => SymbolKind::Function,
            _ => continue,
        };
        let Ok(name) = name_node.utf8_text(src) else {
            continue;
        };
        let exported = !name.starts_with('_');
        out.push(make_symbol(
            path,
            Language::Python,
            name,
            kind,
            def,
            "",
            exported,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

const GO_QUERY: &str = r#"
(function_declaration name: (identifier)       @name) @def
(method_declaration   name: (field_identifier) @name) @def
(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (struct_type))) @def
(type_declaration
  (type_spec
    name: (type_identifier) @name
    type: (interface_type))) @def
"#;

fn extract_go(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_go::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let query = Query::new(&lang, GO_QUERY)?;
    let src = text.as_bytes();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let Some((def, name_node)) = find_captures(&query, m, "def", "name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(src) else {
            continue;
        };
        let kind = match def.kind() {
            "function_declaration" => Some(SymbolKind::Function),
            "method_declaration" => Some(SymbolKind::Method),
            "type_declaration" => {
                // Distinguish struct vs interface by the spec's type child.
                go_type_kind(def, src)
            }
            _ => continue,
        };
        let Some(kind) = kind else { continue };
        let exported = starts_upper(name);
        out.push(make_symbol(
            path,
            Language::Go,
            name,
            kind,
            def,
            "",
            exported,
        ));
    }
    Ok(out)
}

/// Inspect a `type_declaration`'s spec to decide Struct vs Interface.
fn go_type_kind(node: Node, _src: &[u8]) -> Option<SymbolKind> {
    let mut c = node.walk();
    for spec in node.children(&mut c) {
        if spec.kind() == "type_spec"
            && let Some(ty) = spec.child_by_field_name("type")
        {
            return match ty.kind() {
                "struct_type" => Some(SymbolKind::Struct),
                "interface_type" => Some(SymbolKind::Interface),
                _ => None,
            };
        }
    }
    None
}

// ---------------------------------------------------------------------------
// JavaScript / TypeScript
// ---------------------------------------------------------------------------

const JS_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def
(class_declaration    name: (identifier) @name) @def
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @def
(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @def
"#;

const TS_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def
(class_declaration    name: (type_identifier) @name) @def
(interface_declaration name: (type_identifier) @name) @def
(type_alias_declaration name: (type_identifier) @name) @def
(enum_declaration     name: (identifier) @name) @def
(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @def
(variable_declaration
  (variable_declarator
    name: (identifier) @name
    value: [(arrow_function) (function_expression)])) @def
"#;

/// Pick the grammar for a JS/TS file based on language + JSX extension.
fn js_ts_language(path: &str, lang: Language) -> TsLanguage {
    match lang {
        Language::TypeScript => {
            if languages::is_jsx(path) {
                tree_sitter_typescript::LANGUAGE_TSX.into()
            } else {
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
            }
        }
        // JavaScript always uses the JS grammar, even for `.jsx`.
        _ => tree_sitter_javascript::LANGUAGE.into(),
    }
}

fn js_ts_kind(node_kind: &str) -> Option<SymbolKind> {
    Some(match node_kind {
        "function_declaration" => SymbolKind::Function,
        "class_declaration" => SymbolKind::Class,
        "interface_declaration" => SymbolKind::Interface,
        "type_alias_declaration" => SymbolKind::TypeAlias,
        "enum_declaration" => SymbolKind::Enum,
        // arrow/function expressions captured via the declaration wrapper.
        "lexical_declaration" | "variable_declaration" => SymbolKind::Function,
        _ => return None,
    })
}

fn extract_js_ts(path: &str, lang: Language, text: &str) -> Result<Vec<IndexedSymbol>> {
    extract_js_ts_core(path, lang, text, 0, false)
}

/// Core JS/TS extraction, parameterised so embedded scripts (e.g. a Svelte
/// `<script>` block) can reuse it: `line_offset` is added to every symbol's
/// line numbers, and when `treat_jsx` is true the React-component heuristic is
/// applied even though the host file isn't `.jsx`/`.tsx`.
fn extract_js_ts_core(
    path: &str,
    lang: Language,
    text: &str,
    line_offset: u32,
    treat_jsx: bool,
) -> Result<Vec<IndexedSymbol>> {
    let ts_lang = js_ts_language(path, lang);
    let tree = parse(&ts_lang, text)?;
    let query_str = if lang == Language::TypeScript {
        TS_QUERY
    } else {
        JS_QUERY
    };
    let query = Query::new(&ts_lang, query_str)?;
    let src = text.as_bytes();
    let jsx = treat_jsx || languages::is_jsx(path);

    // Names re-exported via a standalone clause, e.g. `export { Foo, Bar }`.
    // These declarations aren't nested under an `export_statement`, so we
    // collect the clause names up front and treat membership as "exported".
    let reexported = collect_reexported_names(tree.root_node(), src);

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let Some((def, name_node)) = find_captures(&query, m, "def", "name") else {
            continue;
        };
        let Some(mut kind) = js_ts_kind(def.kind()) else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(src) else {
            continue;
        };

        // React component heuristic: PascalCase function/class in a JSX file.
        if jsx && starts_upper(name) && matches!(kind, SymbolKind::Function | SymbolKind::Class) {
            kind = SymbolKind::Component;
        }

        // Exported if declared under `export ...` or named in an `export { }`.
        let exported = has_ancestor(def, "export_statement") || reexported.contains(name);
        let visibility = if exported { "export" } else { "" };
        let mut sym = make_symbol(path, lang, name, kind, def, visibility, exported);
        if line_offset > 0 {
            sym.start_line += line_offset;
            sym.end_line += line_offset;
            // Recompute the id so it stays stable against the host-file line.
            sym.id = symbol_id(path, sym.kind.as_str(), &sym.name, sym.start_line);
        }
        out.push(sym);
    }
    Ok(out)
}

/// Collect the identifiers named in `export { ... }` clauses anywhere in the
/// file (the local name, not the optional alias).
fn collect_reexported_names(root: Node, src: &[u8]) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "export_specifier" {
            // `name: (identifier) @local` is the exported local symbol.
            if let Some(n) = node.child_by_field_name("name")
                && let Ok(text) = n.utf8_text(src)
            {
                names.insert(text.to_string());
            }
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Svelte (Svelte 5 / SvelteKit)
// ---------------------------------------------------------------------------

/// Extract symbols from a `.svelte` file.
///
/// Each component file yields a [`SymbolKind::Component`] named after the file
/// stem, plus every function/class/type declared in its `<script>` block (run
/// through the JS/TS extractor, with `lang="ts"` honoured). SvelteKit route
/// files (`+page`, `+layout`, `+server`, `+error`) carry their role in the
/// component symbol's full name and visibility.
fn extract_svelte(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let svelte_lang: TsLanguage = tree_sitter_svelte_ng::LANGUAGE.into();
    let tree = parse(&svelte_lang, text)?;
    let src = text.as_bytes();
    let root = tree.root_node();

    let mut out = Vec::new();

    // 1) The component itself, named after the file stem.
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .strip_suffix(".svelte")
        .unwrap_or(path);
    let role = languages::sveltekit_route_role(path);
    let component_name = match role {
        // Route files share generic stems (+page, +layout); qualify by parent
        // directory so they're distinguishable in `symbols` output.
        Some(_) => {
            let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let parent = dir.rsplit('/').next().unwrap_or(dir);
            if parent.is_empty() {
                stem.to_string()
            } else {
                format!("{parent}/{stem}")
            }
        }
        None => stem.to_string(),
    };
    let full_name = match role {
        Some(r) => format!("{component_name} (sveltekit {r})"),
        None => component_name.clone(),
    };
    let end_line = root.end_position().row as u32 + 1;
    out.push(IndexedSymbol {
        id: symbol_id(path, "component", &component_name, 1),
        name: component_name,
        full_name,
        kind: SymbolKind::Component,
        language: Language::Svelte,
        file_path: path.to_string(),
        start_line: 1,
        end_line,
        visibility: role.unwrap_or("component").to_string(),
        exported: true,
    });

    // 2) Symbols declared inside each <script> block.
    for script in find_script_elements(root) {
        let Some(raw) = script_raw_text(script) else {
            continue;
        };
        let Ok(code) = raw.utf8_text(src) else {
            continue;
        };
        // Svelte scripts are TS by default in modern projects; honour an
        // explicit lang attribute but default to TypeScript (a superset that
        // also parses plain JS).
        let lang = match script_lang(script, src) {
            Some(l) if l.eq_ignore_ascii_case("js") || l.eq_ignore_ascii_case("javascript") => {
                Language::JavaScript
            }
            _ => Language::TypeScript,
        };
        let offset = raw.start_position().row as u32; // 0-based row of the script text
        // The script source is parsed standalone; use a `.ts`/`.js` pseudo-path
        // so the JS/TS grammar selection is correct, but keep the real path on
        // the emitted symbols.
        let pseudo = if lang == Language::TypeScript {
            format!("{path}.script.ts")
        } else {
            format!("{path}.script.js")
        };
        if let Ok(mut script_syms) = extract_js_ts_core(&pseudo, lang, code, offset, true) {
            for s in &mut script_syms {
                s.language = Language::Svelte;
                s.file_path = path.to_string();
                s.id = symbol_id(path, s.kind.as_str(), &s.name, s.start_line);
            }
            out.append(&mut script_syms);
        }
    }

    Ok(out)
}

/// Find all `script_element` nodes anywhere in the tree.
fn find_script_elements(root: Node) -> Vec<Node> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "script_element" {
            found.push(node);
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
    }
    found
}

/// The `raw_text` child of a `script_element` (the embedded JS/TS source).
fn script_raw_text(script: Node) -> Option<Node> {
    let mut c = script.walk();
    script
        .children(&mut c)
        .find(|n| n.kind() == "raw_text" || n.kind() == "svelte_raw_text")
}

/// The value of the script's `lang` attribute, if present (e.g. `ts`).
fn script_lang<'a>(script: Node, src: &'a [u8]) -> Option<&'a str> {
    let mut c = script.walk();
    let start_tag = script.children(&mut c).find(|n| n.kind() == "start_tag")?;
    let mut tc = start_tag.walk();
    for attr in start_tag.children(&mut tc) {
        if attr.kind() != "attribute" {
            continue;
        }
        let mut ac = attr.walk();
        let mut is_lang = false;
        let mut value: Option<&str> = None;
        for part in attr.children(&mut ac) {
            match part.kind() {
                "attribute_name" => {
                    is_lang = part.utf8_text(src).map(|t| t == "lang").unwrap_or(false);
                }
                "quoted_attribute_value" | "attribute_value" => {
                    value = part
                        .utf8_text(src)
                        .ok()
                        .map(|t| t.trim_matches(['"', '\'']));
                }
                _ => {}
            }
        }
        if is_lang {
            return value;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Bash / shell
// ---------------------------------------------------------------------------

const BASH_QUERY: &str = r#"
(function_definition name: (word) @name) @def
"#;

fn extract_bash(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_bash::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let query = Query::new(&lang, BASH_QUERY)?;
    let src = text.as_bytes();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        let Some((def, name_node)) = find_captures(&query, m, "def", "name") else {
            continue;
        };
        let Ok(name) = name_node.utf8_text(src) else {
            continue;
        };
        // Shell functions are all callable; treat them as exported/public.
        out.push(make_symbol(
            path,
            Language::Bash,
            name,
            SymbolKind::Function,
            def,
            "",
            true,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// YAML
// ---------------------------------------------------------------------------

/// Extract top-level mapping keys (and named anchors) from a YAML document.
fn extract_yaml(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_yaml::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let root = tree.root_node();

    let mut out = Vec::new();

    // Top-level keys: stream -> document -> block_node -> block_mapping ->
    // block_mapping_pair(key:). We descend to the first block_mapping and take
    // its direct pair keys, which are the document's top-level keys.
    if let Some(mapping) = find_first_kind(root, "block_mapping") {
        let mut c = mapping.walk();
        for pair in mapping.children(&mut c) {
            if pair.kind() != "block_mapping_pair" {
                continue;
            }
            if let Some(key) = pair.child_by_field_name("key")
                && let Ok(name) = key.utf8_text(src)
            {
                let name = name.trim();
                if !name.is_empty() {
                    out.push(make_symbol(
                        path,
                        Language::Yaml,
                        name,
                        SymbolKind::Key,
                        pair,
                        "",
                        false,
                    ));
                }
            }
        }
    }

    // Named anchors (&name) anywhere in the document.
    for anchor in find_all_kind(root, "anchor_name") {
        if let Ok(name) = anchor.utf8_text(src) {
            let name = name.trim();
            if !name.is_empty() {
                out.push(make_symbol(
                    path,
                    Language::Yaml,
                    name,
                    SymbolKind::Key,
                    anchor,
                    "anchor",
                    false,
                ));
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Extract the keys of the top-level JSON object.
fn extract_json(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_json::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let root = tree.root_node();

    let mut out = Vec::new();
    // document -> object -> pair(key: (string)). Only the top-level object's
    // pairs are emitted, to keep the symbol set focused.
    if let Some(object) = find_first_kind(root, "object") {
        let mut c = object.walk();
        for pair in object.children(&mut c) {
            if pair.kind() != "pair" {
                continue;
            }
            if let Some(key) = pair.child_by_field_name("key")
                && let Ok(raw) = key.utf8_text(src)
            {
                let name = raw.trim().trim_matches('"');
                if !name.is_empty() {
                    out.push(make_symbol(
                        path,
                        Language::Json,
                        name,
                        SymbolKind::Key,
                        pair,
                        "",
                        false,
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// Depth-first search for the first node of `kind`.
fn find_first_kind<'a>(root: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == kind {
            return Some(node);
        }
        let mut c = node.walk();
        // Push children in reverse so we visit them in source order.
        let children: Vec<Node> = node.children(&mut c).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}

/// Collect every node of `kind` in the tree.
fn find_all_kind<'a>(root: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == kind {
            found.push(node);
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

/// Extract headings from a Markdown document as [`SymbolKind::Key`] symbols.
///
/// Both ATX (`## Title`) and setext (underlined) headings are captured. The
/// heading level is recorded in the symbol's full name (e.g. `h2 Title`) and
/// visibility (`h2`), so docs sections are searchable and packable by name.
fn extract_markdown(path: &str, text: &str) -> Result<Vec<IndexedSymbol>> {
    let lang: TsLanguage = tree_sitter_md::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let root = tree.root_node();

    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "atx_heading" | "setext_heading" => {
                if let Some((level, title)) = markdown_heading(node, src) {
                    out.push(IndexedSymbol {
                        id: symbol_id(path, "key", &title, node.start_position().row as u32 + 1),
                        name: title.clone(),
                        full_name: format!("h{level} {title}"),
                        kind: SymbolKind::Key,
                        language: Language::Markdown,
                        file_path: path.to_string(),
                        start_line: node.start_position().row as u32 + 1,
                        end_line: node.end_position().row as u32 + 1,
                        visibility: format!("h{level}"),
                        exported: false,
                    });
                }
            }
            _ => {}
        }
        let mut c = node.walk();
        for child in node.children(&mut c) {
            stack.push(child);
        }
    }

    Ok(out)
}

/// Return `(level, title)` for a heading node, or `None` if it has no title.
fn markdown_heading(node: Node, src: &[u8]) -> Option<(u8, String)> {
    let mut level = 1u8;
    let mut c = node.walk();
    for child in node.children(&mut c) {
        match child.kind() {
            "atx_h1_marker" | "setext_h1_underline" => level = 1,
            "atx_h2_marker" | "setext_h2_underline" => level = 2,
            "atx_h3_marker" => level = 3,
            "atx_h4_marker" => level = 4,
            "atx_h5_marker" => level = 5,
            "atx_h6_marker" => level = 6,
            _ => {}
        }
    }
    // The title text lives in the `heading_content` field (an inline node).
    let content = node.child_by_field_name("heading_content")?;
    let raw = content.utf8_text(src).ok()?;
    let title = clean_heading_text(raw);
    if title.is_empty() {
        None
    } else {
        Some((level, title))
    }
}

/// Normalise heading text: strip surrounding whitespace, trailing `#` (closed
/// ATX headings), and collapse internal whitespace to single spaces.
fn clean_heading_text(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('#').trim();
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Imports (for file -> package edges)
// ---------------------------------------------------------------------------

/// Extract the imported module/namespace names from a file, normalised so they
/// can be matched against package names. Only JS/TS and C# are supported (the
/// languages with a clean import->package mapping); other languages return an
/// empty list.
///
/// * JS/TS: the module specifier of each `import ... from '<spec>'` (and
///   `require('<spec>')`), reduced to its package root — `@scope/pkg/sub` ->
///   `@scope/pkg`, `pkg/sub` -> `pkg`. Relative imports (`./`, `../`) are
///   dropped.
/// * C#: the namespace of each `using <Namespace>;` directive, kept whole (the
///   indexer matches it against package names by longest-prefix).
pub fn extract_imports(path: &str, lang: Language, text: &str) -> Vec<String> {
    let result = match lang {
        Language::JavaScript | Language::TypeScript => extract_js_ts_imports(path, lang, text),
        Language::CSharp => extract_csharp_imports(text),
        _ => Ok(Vec::new()),
    };
    let mut out = result.unwrap_or_else(|e| {
        tracing::debug!("import extraction failed for {path}: {e}");
        Vec::new()
    });
    out.sort();
    out.dedup();
    out
}

/// Reduce a JS/TS module specifier to its npm package root, or `None` if it is
/// a relative/absolute path import (not a package).
fn npm_package_root(spec: &str) -> Option<String> {
    if spec.starts_with('.') || spec.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = spec.split('/').collect();
    let root = if spec.starts_with('@') {
        // Scoped: keep `@scope/name`.
        if parts.len() >= 2 {
            format!("{}/{}", parts[0], parts[1])
        } else {
            parts[0].to_string()
        }
    } else {
        parts[0].to_string()
    };
    if root.is_empty() { None } else { Some(root) }
}

const JS_IMPORT_QUERY: &str = r#"
(import_statement source: (string) @src)
(call_expression
  function: (identifier) @fn
  arguments: (arguments (string) @reqsrc))
"#;

fn extract_js_ts_imports(path: &str, lang: Language, text: &str) -> Result<Vec<String>> {
    let ts_lang = js_ts_language(path, lang);
    let tree = parse(&ts_lang, text)?;
    let query = Query::new(&ts_lang, JS_IMPORT_QUERY)?;
    let src = text.as_bytes();
    let names = query.capture_names();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        // For require(...) only count it when the callee is `require`.
        let mut require_ok = true;
        let mut src_node: Option<Node> = None;
        let mut is_require = false;
        for cap in m.captures {
            match names[cap.index as usize] {
                "src" => src_node = Some(cap.node),
                "reqsrc" => {
                    src_node = Some(cap.node);
                    is_require = true;
                }
                "fn" => {
                    require_ok = cap
                        .node
                        .utf8_text(src)
                        .map(|t| t == "require")
                        .unwrap_or(false)
                }
                _ => {}
            }
        }
        if is_require && !require_ok {
            continue;
        }
        if let Some(n) = src_node
            && let Ok(raw) = n.utf8_text(src)
        {
            let spec = raw.trim_matches(['"', '\'', '`']);
            if let Some(pkg) = npm_package_root(spec) {
                out.push(pkg);
            }
        }
    }
    Ok(out)
}

const CSHARP_IMPORT_QUERY: &str = r#"
(using_directive (qualified_name) @ns)
(using_directive (identifier) @ns)
"#;

fn extract_csharp_imports(text: &str) -> Result<Vec<String>> {
    let lang: TsLanguage = tree_sitter_c_sharp::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let query = Query::new(&lang, CSHARP_IMPORT_QUERY)?;
    let src = text.as_bytes();
    let names = query.capture_names();

    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&query, tree.root_node(), src);
    while let Some(m) = it.next() {
        for cap in m.captures {
            if names[cap.index as usize] == "ns"
                && let Ok(ns) = cap.node.utf8_text(src)
            {
                let ns = ns.trim();
                if !ns.is_empty() {
                    out.push(ns.to_string());
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Supertypes (for INHERITS / IMPLEMENTS edges)
// ---------------------------------------------------------------------------

/// A "child declares super" relationship discovered syntactically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supertype {
    /// Name of the declaring type (subclass / implementing type / sub-trait).
    pub child: String,
    /// Name of the base type, interface or trait (bare identifier, generics
    /// stripped).
    pub supertype: String,
    /// Syntactic hint about the edge kind.
    pub hint: SuperHint,
}

/// Whether the syntax says this is an inheritance or interface relationship.
/// `Unknown` means the language doesn't distinguish at the syntax level (e.g.
/// C# base lists) — the indexer resolves it from the target symbol's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperHint {
    Inherits,
    Implements,
    Unknown,
}

/// Extract base-type / interface / trait relationships from a file.
///
/// Best-effort and name-based: generics and namespaces are stripped to a bare
/// identifier. Returns an empty list for unsupported languages.
pub fn extract_supertypes(path: &str, lang: Language, text: &str) -> Vec<Supertype> {
    let result = match lang {
        Language::CSharp => csharp_supertypes(text),
        Language::Rust => rust_supertypes(text),
        Language::TypeScript | Language::JavaScript => ts_supertypes(path, lang, text),
        Language::Python => python_supertypes(text),
        Language::Go => go_supertypes(text),
        _ => Ok(Vec::new()),
    };
    let mut out = result.unwrap_or_else(|e| {
        tracing::debug!("supertype extraction failed for {path}: {e}");
        Vec::new()
    });
    out.sort_by(|a, b| {
        a.child
            .cmp(&b.child)
            .then_with(|| a.supertype.cmp(&b.supertype))
    });
    out.dedup();
    out
}

/// Reduce a possibly-qualified, possibly-generic type reference to a bare
/// identifier: `Foo.Bar<T>` -> `Bar`, `std::fmt::Display` -> `Display`.
fn bare_type_name(raw: &str) -> String {
    let no_generics = raw.split(['<', '(']).next().unwrap_or(raw);
    let last = no_generics.rsplit(['.', ':']).next().unwrap_or(no_generics);
    last.trim().to_string()
}

fn csharp_supertypes(text: &str) -> Result<Vec<Supertype>> {
    let lang: TsLanguage = tree_sitter_c_sharp::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    // class/struct/record declarations carrying a base_list.
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let is_type_decl = matches!(
            node.kind(),
            "class_declaration" | "struct_declaration" | "record_declaration"
        );
        if is_type_decl
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(child) = name_node.utf8_text(src)
        {
            let mut c = node.walk();
            for sub in node.children(&mut c) {
                if sub.kind() == "base_list" {
                    let mut bc = sub.walk();
                    for b in sub.children(&mut bc) {
                        // The base entries are `type` (or primary ctor base).
                        let is_type_node = matches!(
                            b.kind(),
                            "type" | "identifier" | "qualified_name" | "generic_name"
                        );
                        if is_type_node && let Ok(t) = b.utf8_text(src) {
                            let supertype = bare_type_name(t);
                            if !supertype.is_empty() {
                                out.push(Supertype {
                                    child: child.to_string(),
                                    supertype,
                                    hint: SuperHint::Unknown,
                                });
                            }
                        }
                    }
                }
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

fn rust_supertypes(text: &str) -> Result<Vec<Supertype>> {
    let lang: TsLanguage = tree_sitter_rust::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // `impl Trait for Type` -> Type implements Trait.
            "impl_item" => {
                if let (Some(tr), Some(ty)) = (
                    node.child_by_field_name("trait"),
                    node.child_by_field_name("type"),
                ) && let (Ok(t), Ok(y)) = (tr.utf8_text(src), ty.utf8_text(src))
                {
                    out.push(Supertype {
                        child: bare_type_name(y),
                        supertype: bare_type_name(t),
                        hint: SuperHint::Implements,
                    });
                }
            }
            // `trait Sub: Super` -> Sub inherits Super (supertrait bounds).
            "trait_item" => {
                if let (Some(name), Some(bounds)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("bounds"),
                ) && let Ok(child) = name.utf8_text(src)
                {
                    let mut bc = bounds.walk();
                    for b in bounds.children(&mut bc) {
                        if matches!(b.kind(), "type_identifier" | "scoped_type_identifier")
                            && let Ok(t) = b.utf8_text(src)
                        {
                            out.push(Supertype {
                                child: child.to_string(),
                                supertype: bare_type_name(t),
                                hint: SuperHint::Inherits,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

fn ts_supertypes(path: &str, lang: Language, text: &str) -> Result<Vec<Supertype>> {
    let ts_lang = js_ts_language(path, lang);
    let tree = parse(&ts_lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "class_declaration" | "interface_declaration")
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(child) = name_node.utf8_text(src)
        {
            let mut c = node.walk();
            for sub in node.children(&mut c) {
                match sub.kind() {
                    // class extends X / interface extends X
                    "class_heritage" => {
                        let mut hc = sub.walk();
                        for clause in sub.children(&mut hc) {
                            let hint = match clause.kind() {
                                "extends_clause" => SuperHint::Inherits,
                                "implements_clause" => SuperHint::Implements,
                                _ => continue,
                            };
                            collect_ts_clause_types(clause, src, child, hint, &mut out);
                        }
                    }
                    "extends_type_clause" => {
                        collect_ts_clause_types(sub, src, child, SuperHint::Inherits, &mut out);
                    }
                    _ => {}
                }
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

/// Pull type names out of a TS heritage clause and push edges.
fn collect_ts_clause_types(
    clause: Node,
    src: &[u8],
    child: &str,
    hint: SuperHint,
    out: &mut Vec<Supertype>,
) {
    let mut c = clause.walk();
    for n in clause.children(&mut c) {
        if matches!(
            n.kind(),
            "identifier"
                | "type_identifier"
                | "member_expression"
                | "generic_type"
                | "nested_type_identifier"
        ) && let Ok(t) = n.utf8_text(src)
        {
            let supertype = bare_type_name(t);
            if !supertype.is_empty() {
                out.push(Supertype {
                    child: child.to_string(),
                    supertype,
                    hint,
                });
            }
        }
    }
}

fn python_supertypes(text: &str) -> Result<Vec<Supertype>> {
    let lang: TsLanguage = tree_sitter_python::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_definition"
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(child) = name_node.utf8_text(src)
            && let Some(supers) = node.child_by_field_name("superclasses")
        {
            let mut c = supers.walk();
            for arg in supers.children(&mut c) {
                if matches!(arg.kind(), "identifier" | "attribute")
                    && let Ok(t) = arg.utf8_text(src)
                {
                    let supertype = bare_type_name(t);
                    if !supertype.is_empty() && supertype != "object" {
                        out.push(Supertype {
                            child: child.to_string(),
                            supertype,
                            hint: SuperHint::Inherits,
                        });
                    }
                }
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

fn go_supertypes(text: &str) -> Result<Vec<Supertype>> {
    let lang: TsLanguage = tree_sitter_go::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    // `type_spec` with name; struct embedding (field_declaration with no name)
    // or interface embedding (type_elem) -> the embedded type is a supertype.
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "type_spec"
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(child) = name_node.utf8_text(src)
            && let Some(ty) = node.child_by_field_name("type")
        {
            match ty.kind() {
                "struct_type" => {
                    // Embedded fields: a field_declaration whose `type` is set
                    // but `name` is absent.
                    for fld in descendants_of_kind(ty, "field_declaration") {
                        if fld.child_by_field_name("name").is_none()
                            && let Some(ft) = fld.child_by_field_name("type")
                            && let Ok(t) = ft.utf8_text(src)
                        {
                            let supertype = bare_type_name(t);
                            if !supertype.is_empty() {
                                out.push(Supertype {
                                    child: child.to_string(),
                                    supertype,
                                    hint: SuperHint::Inherits,
                                });
                            }
                        }
                    }
                }
                "interface_type" => {
                    for te in descendants_of_kind(ty, "type_elem") {
                        if let Ok(t) = te.utf8_text(src) {
                            let supertype = bare_type_name(t);
                            if !supertype.is_empty() {
                                out.push(Supertype {
                                    child: child.to_string(),
                                    supertype,
                                    hint: SuperHint::Inherits,
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

/// Collect descendant nodes of a given kind (shallow DFS).
fn descendants_of_kind<'a>(root: Node<'a>, kind: &str) -> Vec<Node<'a>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == kind {
            found.push(n);
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Reference extraction: which declared symbol does a usage site point at, and
// which declaration encloses that usage? Mirrors the supertype pipeline above
// (extract -> pending list -> resolve in the indexer's second pass). Like
// supertypes, this is name-based and best-effort; cross-file name resolution
// and the false-positive guard for unmatched names happen in the resolve pass.
// ---------------------------------------------------------------------------

/// A "symbol X is referenced from inside declaration Y" relationship discovered
/// syntactically. Both fields are bare identifiers; resolution to symbol ids
/// (and dropping references whose target isn't a declared symbol) happens later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Name of the enclosing declaration the usage appears inside (the smallest
    /// declaration containing the usage). Empty if no enclosing declaration.
    pub from: String,
    /// Name of the referenced symbol (bare identifier, generics/qualifiers
    /// stripped).
    pub to: String,
}

/// Extract symbol→symbol reference relationships from a file.
///
/// Best-effort and name-based. Returns an empty list for languages without a
/// reference extractor yet (Python, Go, Svelte — see `status`'s
/// `referenceLanguages`).
pub fn extract_references(path: &str, lang: Language, text: &str) -> Vec<Reference> {
    let result = match lang {
        Language::CSharp => csharp_references(text),
        Language::Rust => rust_references(text),
        Language::TypeScript | Language::JavaScript => ts_references(path, lang, text),
        _ => Ok(Vec::new()),
    };
    let mut out = result.unwrap_or_else(|e| {
        tracing::debug!("reference extraction failed for {path}: {e}");
        Vec::new()
    });
    out.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    out.dedup();
    out
}

/// Name of the nearest ancestor declaration of `node` whose kind is in
/// `decl_kinds`, read from its `name` field. Empty when none is found (e.g. a
/// top-level / module-scope usage).
fn enclosing_decl_name(node: Node, src: &[u8], decl_kinds: &[&str]) -> String {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if decl_kinds.contains(&n.kind())
            && let Some(name_node) = n.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(src)
        {
            return name.to_string();
        }
        cur = n.parent();
    }
    String::new()
}

/// C#: `new Foo(...)`, `Foo.Bar()` / `Bar()` invocations, generic type
/// arguments (`List<Foo>`), parameter types (`void M(Foo f)`), and property /
/// field types (`Foo Bar { get; }`, `Foo _bar;`). Enclosing declaration is the
/// method/ctor/type the usage sits in.
fn csharp_references(text: &str) -> Result<Vec<Reference>> {
    const DECLS: &[&str] = &[
        "method_declaration",
        "constructor_declaration",
        "class_declaration",
        "record_declaration",
        "struct_declaration",
    ];
    let lang: TsLanguage = tree_sitter_c_sharp::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        // A node may contribute one referenced-type/method node.
        let target: Option<Node> = match node.kind() {
            // `new Foo(...)` / `new Foo { ... }` — the constructed type.
            "object_creation_expression" => node.child_by_field_name("type"),
            // `Foo()` or `recv.Foo()` — the invoked function/method.
            "invocation_expression" => node.child_by_field_name("function"),
            // `void M(Foo f)` — the parameter's declared type. The first child
            // is the type, the rest the name/modifiers.
            "parameter" => node.child_by_field_name("type"),
            // `public Foo Bar { get; set; }` — the property's declared type.
            "property_declaration" => node.child_by_field_name("type"),
            // `private Foo bar;` — the field's type lives on the `type` field
            // of its inner (unnamed) `variable_declaration` child. (Method-local
            // `var x = ...` declarations are `local_declaration_statement`, so
            // this only fires for class/struct/record fields.)
            "field_declaration" => {
                let mut c = node.walk();
                node.children(&mut c)
                    .find(|ch| ch.kind() == "variable_declaration")
                    .and_then(|d| d.child_by_field_name("type"))
            }
            _ => None,
        };
        if let Some(t) = target
            && let Ok(raw) = invocation_or_type_name(t, src)
        {
            let to = bare_type_name(&raw);
            if !to.is_empty() {
                out.push(Reference {
                    from: enclosing_decl_name(node, src, DECLS),
                    to,
                });
            }
        }
        // Generic type arguments: `List<Foo, Bar>` -> Foo, Bar. The argument
        // list holds bare type nodes (identifier / qualified / generic). We
        // can't use a `type` field here, so read each non-punctuation child.
        if node.kind() == "type_argument_list" {
            let from = enclosing_decl_name(node, src, DECLS);
            let mut c = node.walk();
            for arg in node.children(&mut c) {
                if matches!(
                    arg.kind(),
                    "identifier" | "qualified_name" | "generic_name" | "type"
                ) && let Ok(raw) = arg.utf8_text(src)
                {
                    let to = bare_type_name(raw);
                    if !to.is_empty() {
                        out.push(Reference {
                            from: from.clone(),
                            to,
                        });
                    }
                }
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

/// For an `invocation_expression`'s function child, return the rightmost name
/// (`recv.Foo` -> `Foo`, `Foo` -> `Foo`). `bare_type_name` strips the receiver.
fn invocation_or_type_name<'a>(node: Node<'a>, src: &'a [u8]) -> Result<String> {
    node.utf8_text(src)
        .map(|s| s.to_string())
        .map_err(|e| anyhow!("utf8: {e}"))
}

/// Rust: `foo()` / `Foo::new()` calls and `Foo { .. }` struct literals. The
/// resolve pass keeps only references whose target is a declared symbol, so the
/// rightmost path segment is what matters (`Foo::new` -> `new`? no — we want
/// the type, so for a scoped call we take the type segment, see below).
fn rust_references(text: &str) -> Result<Vec<Reference>> {
    const DECLS: &[&str] = &["function_item", "impl_item", "struct_item", "enum_item"];
    let lang: TsLanguage = tree_sitter_rust::LANGUAGE.into();
    let tree = parse(&lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let mut targets: Vec<String> = Vec::new();
        match node.kind() {
            "call_expression" => {
                if let Some(func) = node.child_by_field_name("function") {
                    // `Foo::new()` -> the `Foo` type segment; `foo()` -> `foo`.
                    targets.extend(rust_call_targets(func, src));
                }
            }
            // `Foo { .. }` struct literal -> reference to `Foo`.
            "struct_expression" => {
                if let Some(name) = node.child_by_field_name("name")
                    && let Ok(t) = name.utf8_text(src)
                {
                    targets.push(bare_type_name(t));
                }
            }
            _ => {}
        }
        for to in targets {
            if !to.is_empty() {
                out.push(Reference {
                    from: enclosing_decl_name(node, src, DECLS),
                    to,
                });
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}

/// Names a Rust call expression references. For `Foo::new` we emit both the
/// type segment (`Foo`) and the function segment (`new`); the resolve pass
/// drops whichever doesn't match a declared symbol. For a bare `foo` we emit
/// `foo`. For `recv.method()` (field_expression) we emit the method name.
fn rust_call_targets(func: Node, src: &[u8]) -> Vec<String> {
    match func.kind() {
        "identifier" => func
            .utf8_text(src)
            .ok()
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
        // `Foo::new` / `module::func`
        "scoped_identifier" => {
            let mut out = Vec::new();
            if let Some(path) = func.child_by_field_name("path")
                && let Ok(t) = path.utf8_text(src)
            {
                out.push(bare_type_name(t));
            }
            if let Some(name) = func.child_by_field_name("name")
                && let Ok(t) = name.utf8_text(src)
            {
                out.push(t.to_string());
            }
            out
        }
        // `recv.method` — the method name is the field.
        "field_expression" => func
            .child_by_field_name("field")
            .and_then(|f| f.utf8_text(src).ok())
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// TS/JS: `new Foo()`, `foo()` / `recv.foo()` calls, and `type_identifier`s in
/// type positions. Enclosing declaration is the function/method/class.
fn ts_references(path: &str, lang: Language, text: &str) -> Result<Vec<Reference>> {
    const DECLS: &[&str] = &[
        "function_declaration",
        "method_definition",
        "class_declaration",
        "variable_declarator",
    ];
    let ts_lang = js_ts_language(path, lang);
    let tree = parse(&ts_lang, text)?;
    let src = text.as_bytes();
    let mut out = Vec::new();

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        let target: Option<Node> = match node.kind() {
            // `new Foo(...)` — the constructed class.
            "new_expression" => node.child_by_field_name("constructor"),
            // `foo()` / `recv.foo()` — the called function.
            "call_expression" => node.child_by_field_name("function"),
            // `: Foo` / `<Foo>` type positions.
            "type_identifier" => Some(node),
            _ => None,
        };
        if let Some(t) = target
            && let Ok(raw) = t.utf8_text(src)
        {
            let to = bare_type_name(raw);
            if !to.is_empty() {
                out.push(Reference {
                    from: enclosing_decl_name(node, src, DECLS),
                    to,
                });
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            stack.push(ch);
        }
    }
    Ok(out)
}
