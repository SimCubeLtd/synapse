//! End-to-end integration tests that drive the compiled `synapse` binary.
//!
//! Because `synapse` is a *binary* crate, its internal modules (parsers, the
//! budget fitter, the table/markdown renderers) are not importable from an
//! integration test. So these tests exercise the whole pipeline through the
//! public CLI surface instead: they build a throwaway repo in a unique temp
//! directory, copy in language fixtures, run `init`/`index`/`symbols`/`status`/
//! `pack`/`clean`, and assert on stdout, stderr and on-disk output.
//!
//! Determinism: each test uses its own temp directory derived from the test
//! name, initialises an empty git repo so tracked/changed logic has something
//! to read, and removes the directory on success. No network, no shared state.
//!
//! Note on assertions: integration tests only link against the crate's binary
//! plus dev-dependencies, *not* the crate's normal dependencies, so we cannot
//! use `serde_json` here. All assertions are plain substring checks.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Absolute path to the compiled `synapse` binary under test.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_synapse")
}

/// Absolute path to the `tests/fixtures` directory shipped with the crate.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Create a unique, empty temp directory for a single test. The name is
/// derived from the (unique) test name plus the process id so concurrent test
/// runs never collide.
fn make_temp_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("synapse-it-{}-{}", test_name, std::process::id()));
    // Start from a clean slate even if a previous aborted run left junk behind.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

/// Recursively copy a directory tree.
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("creating dst dir");
    for entry in std::fs::read_dir(src).expect("reading src dir") {
        let entry = entry.expect("dir entry");
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).expect("copying file");
        }
    }
}

/// Copy one fixture language directory (e.g. `rust`) flat into the repo root.
fn copy_fixture(repo: &Path, lang: &str) {
    let src = fixtures_dir().join(lang);
    copy_tree(&src, &repo.join(lang));
}

/// Initialise an empty git repo so the `git` heuristics have a HEAD to read.
/// Tolerate git being absent: the CLI degrades gracefully without it.
fn git_init(repo: &Path) {
    let _ = Command::new("git").arg("init").current_dir(repo).output();
    // A user identity keeps `git` happy on machines without a global config.
    let _ = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(repo)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(repo)
        .output();
    let _ = Command::new("git")
        .args(["add", "-A"])
        .current_dir(repo)
        .output();
    let _ = Command::new("git")
        .args(["-c", "commit.gpgsign=false", "commit", "-m", "fixtures"])
        .current_dir(repo)
        .output();
}

/// Run the binary with `args` inside `repo` and return the captured output.
fn run(repo: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawning synapse binary")
}

/// Run the binary and assert it exited successfully, surfacing stderr on
/// failure so test diagnostics are actionable.
fn run_ok(repo: &Path, args: &[&str]) -> Output {
    let out = run(repo, args);
    assert!(
        out.status.success(),
        "command {:?} failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        args,
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Standard setup shared by most tests: temp repo + git + chosen fixtures +
/// `synapse init` + `synapse index`.
fn setup_indexed(test_name: &str, langs: &[&str]) -> PathBuf {
    let repo = make_temp_dir(test_name);
    for lang in langs {
        copy_fixture(&repo, lang);
    }
    git_init(&repo);
    run_ok(&repo, &["init"]);
    run_ok(&repo, &["index"]);
    repo
}

#[test]
fn test_init_creates_config() {
    let repo = make_temp_dir("init");
    git_init(&repo);

    let out = run_ok(&repo, &["init"]);
    assert!(
        stdout(&out).contains("Initialized Synapse workspace"),
        "init should print a confirmation, got:\n{}",
        stdout(&out)
    );

    let cfg = repo.join(".synapse").join("synapse.toml");
    assert!(
        cfg.is_file(),
        "config file should exist at {}",
        cfg.display()
    );

    let text = std::fs::read_to_string(&cfg).expect("reading config");
    assert!(
        text.contains("backend = \"ladybug\""),
        "config should select the ladybug backend, got:\n{text}"
    );

    // Storage subdirectories should be created.
    assert!(repo.join(".synapse").join("graph").is_dir());
    assert!(repo.join(".synapse").join("cache").is_dir());
    assert!(repo.join(".synapse").join("packs").is_dir());

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn test_index_and_symbols() {
    let repo = setup_indexed("symbols", &["rust", "typescript", "csharp"]);

    // C# class.
    let out = run_ok(&repo, &["symbols", "SftpAutoReceiveCommandHandler"]);
    let text = stdout(&out);
    assert!(
        text.contains("SftpAutoReceiveCommandHandler"),
        "should find the C# handler, got:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("class"),
        "C# handler should be reported as a class, got:\n{text}"
    );

    // Rust struct.
    let out = run_ok(&repo, &["symbols", "WidgetRenderer"]);
    let text = stdout(&out);
    assert!(
        text.contains("WidgetRenderer"),
        "should find the Rust struct, got:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("struct"),
        "WidgetRenderer should be reported as a struct, got:\n{text}"
    );

    // TypeScript interface.
    let out = run_ok(&repo, &["symbols", "Invoice"]);
    let text = stdout(&out);
    assert!(
        text.contains("Invoice"),
        "should find the TS Invoice interface, got:\n{text}"
    );

    // JSON output flag is accepted and produces the symbol name.
    let out = run_ok(&repo, &["symbols", "WidgetRenderer", "--json"]);
    let text = stdout(&out);
    assert!(
        text.contains("WidgetRenderer"),
        "json symbols output should mention the symbol, got:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn test_status_json() {
    let repo = setup_indexed("status", &["rust"]);

    let out = run_ok(&repo, &["status", "--json"]);
    let text = stdout(&out);
    assert!(
        text.contains("\"graphBackend\""),
        "status --json should report the graph backend, got:\n{text}"
    );
    assert!(
        text.contains("\"filesIndexed\""),
        "status --json should report files indexed, got:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn test_pack_symbol_output() {
    let repo = setup_indexed("packout", &["rust"]);

    run_ok(
        &repo,
        &["pack", "--symbol", "WidgetRenderer", "--output", "ctx.md"],
    );

    let ctx = repo.join("ctx.md");
    assert!(ctx.is_file(), "pack should write ctx.md");

    let md = std::fs::read_to_string(&ctx).expect("reading pack output");
    assert!(
        md.contains("# SimCube Synapse Context Pack"),
        "pack should have the title heading, got:\n{md}"
    );
    assert!(
        md.contains("## Selection Summary"),
        "pack should have a selection summary heading, got:\n{md}"
    );
    assert!(
        md.contains("widget.rs"),
        "pack should reference the seeded fixture file path, got:\n{md}"
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn test_pack_dry_run() {
    let repo = setup_indexed("packdry", &["rust"]);

    let out = run_ok(&repo, &["pack", "--symbol", "WidgetRenderer", "--dry-run"]);
    // The rendered markdown goes to stdout; the selection breakdown goes to
    // stderr (see cmd_pack). Either way the summary section must appear, and
    // the file body (a fenced code block carrying the actual source) must not.
    let combined = format!("{}\n{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("Selection Summary") || combined.contains("Selection ("),
        "dry-run should show a selection summary, got:\n{combined}"
    );
    assert!(
        !stdout(&out).contains("pub struct WidgetRenderer"),
        "dry-run must not emit the file's source body, got:\n{}",
        stdout(&out)
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn test_clean_index() {
    let repo = setup_indexed("clean", &["rust"]);

    let graph_dir = repo.join(".synapse").join("graph");
    // After indexing the graph dir should hold the backing store.
    let before: Vec<_> = std::fs::read_dir(&graph_dir)
        .expect("reading graph dir")
        .filter_map(Result::ok)
        .collect();
    assert!(
        !before.is_empty(),
        "graph dir should contain the index after `index`"
    );

    run_ok(&repo, &["clean", "--index"]);

    // `clean` recreates an empty directory; assert it is now empty.
    assert!(graph_dir.is_dir(), "graph dir should be recreated empty");
    let after: Vec<_> = std::fs::read_dir(&graph_dir)
        .expect("reading graph dir after clean")
        .filter_map(Result::ok)
        .collect();
    assert!(
        after.is_empty(),
        "graph dir should be emptied by `clean --index`, still has: {:?}",
        after.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&repo);
}

#[test]
fn test_packages_resolves_cpm_versions() {
    let dir = make_temp_dir("packages_cpm");
    // The cpm fixture: csproj with version-less refs + Directory.Packages.props.
    copy_tree(&fixtures_dir().join("dotnet/cpm"), &dir);
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "cpm"]);
    run_ok(&dir, &["index"]);

    let out = stdout(&run_ok(&dir, &["packages"]));
    // Versions defined only in Directory.Packages.props are resolved.
    assert!(out.contains("Serilog"), "missing Serilog: {out}");
    assert!(
        out.contains("3.1.1"),
        "Serilog version not resolved from CPM: {out}"
    );
    assert!(
        out.contains("Wolverine") && out.contains("2.4.0"),
        "Wolverine CPM version: {out}"
    );

    let projects = stdout(&run_ok(&dir, &["packages", "--projects"]));
    assert!(
        projects.contains("cpm"),
        "project should be flagged CPM: {projects}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_packages_importers_impact_analysis() {
    let dir = make_temp_dir("importers");
    std::fs::create_dir_all(dir.join("web/src")).unwrap();
    std::fs::write(
        dir.join("web/package.json"),
        r#"{"name":"web","dependencies":{"lodash":"^4.17.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("web/src/a.ts"),
        "import _ from 'lodash';\nexport const a = _;\n",
    )
    .unwrap();
    std::fs::write(dir.join("web/src/b.ts"), "export const b = 1;\n").unwrap();
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "imp"]);
    run_ok(&dir, &["index"]);

    let out = stdout(&run_ok(&dir, &["packages", "--importers", "lodash"]));
    assert!(out.contains("web/src/a.ts"), "a.ts imports lodash: {out}");
    assert!(
        !out.contains("web/src/b.ts"),
        "b.ts does not import lodash: {out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_pack_json_format() {
    let dir = make_temp_dir("pack_json");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "pub struct Foo { pub x: i32 }\npub fn bar() {}\n",
    )
    .unwrap();
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "j"]);
    run_ok(&dir, &["index"]);

    let out = stdout(&run_ok(
        &dir,
        &["pack", "--symbol", "Foo", "--format", "json"],
    ));
    // Structurally a JSON object with the expected top-level keys and content.
    assert!(out.trim_start().starts_with('{'), "should be JSON: {out}");
    assert!(out.contains("\"tool\""), "has tool field");
    assert!(out.contains("\"selection\"") && out.contains("\"files\""));
    assert!(out.contains("src/lib.rs"));
    assert!(out.contains("\"name\": \"Foo\""), "symbol Foo present");
    // No Markdown heading leaked into JSON output.
    assert!(!out.contains("# SimCube Synapse Context Pack"));

    // dry-run JSON has no file contents.
    let dry = stdout(&run_ok(
        &dir,
        &["pack", "--symbol", "Foo", "--format", "json", "--dry-run"],
    ));
    assert!(dry.contains("\"selection\""), "dry-run still has selection");
    assert!(dry.contains("\"files\": []"), "dry-run files empty: {dry}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// End-to-end on the production (LadybugDB) backend: a `new Foo()` in a
/// separate file from the declaration must appear in `pack --symbol Foo`'s
/// selection, and `status --json` must report a non-zero `referenceEdges`.
/// This is the user-visible payoff of the REFERENCES edges — without the graph
/// traversal in `related_to_symbol`, the caller file would be invisible here.
#[test]
fn test_pack_symbol_includes_reference_sites() {
    let dir = make_temp_dir("pack_refs");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // Declaration and caller in different files (and different leaf dirs) so a
    // match can only come from a REFERENCES edge, not directory-neighbour noise.
    std::fs::create_dir_all(dir.join("src/model")).unwrap();
    std::fs::create_dir_all(dir.join("src/handlers")).unwrap();
    std::fs::write(
        dir.join("src/model/widget.rs"),
        "pub struct Widget; impl Widget { pub fn new() -> Widget { Widget } }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/handlers/app.rs"),
        "fn run() { let _w = crate::model::widget::Widget::new(); }\n",
    )
    .unwrap();
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "r"]);
    run_ok(&dir, &["index"]);

    let out = stdout(&run_ok(
        &dir,
        &[
            "pack",
            "--symbol",
            "Widget",
            "--format",
            "json",
            "--dry-run",
        ],
    ));
    assert!(
        out.contains("src/handlers/app.rs"),
        "caller file must appear in pack selection via REFERENCES edge: {out}"
    );
    assert!(
        out.contains("references Widget"),
        "reference reason should be present: {out}"
    );

    // status --json exposes the new edge count, > 0 here.
    let status = stdout(&run_ok(&dir, &["status", "--json"]));
    assert!(
        status.contains("\"referenceEdges\""),
        "status has referenceEdges: {status}"
    );
    assert!(
        status.contains("\"referenceLanguages\""),
        "status names covered languages: {status}"
    );
    assert!(
        !status.contains("\"referenceEdges\": 0"),
        "referenceEdges should be non-zero after indexing a reference: {status}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Regression: in a single-project repo (or same directory) every reference is
/// ALSO a same-project / same-directory sibling. The `related` de-duplication
/// must keep the specific "references" reason rather than letting the generic
/// membership reason mask it — otherwise reference sites are silently invisible
/// even though the edges exist.
#[test]
fn test_related_reference_not_masked_by_same_project() {
    let dir = make_temp_dir("ref_mask");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    // Declaration and caller in the SAME directory: the caller is a
    // same-directory + same-project sibling AND a reference site.
    std::fs::write(
        dir.join("src/widget.rs"),
        "pub struct Widget; impl Widget { pub fn new() -> Widget { Widget } }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/app.rs"),
        "fn run() { let _w = crate::widget::Widget::new(); }\n",
    )
    .unwrap();
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "r"]);
    run_ok(&dir, &["index"]);

    let out = stdout(&run_ok(&dir, &["related", "--symbol", "Widget", "--json"]));
    // The caller appears with the specific reference reason, not just "same ...".
    assert!(
        out.contains("references Widget"),
        "reference reason must survive de-dup against same-project/same-dir: {out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_explore_print_command() {
    let dir = make_temp_dir("explore_print");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn x() {}\n").unwrap();
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "x"]);
    run_ok(&dir, &["index"]);

    // --print emits the docker command without needing Docker.
    let out = stdout(&run_ok(&dir, &["explore", "--print"]));
    assert!(out.contains("docker run --rm"), "got: {out}");
    assert!(out.contains("-e LBUG_FILE=synapse.lbug"));
    assert!(out.contains("MODE=READ_ONLY"), "default read-only");
    assert!(out.contains("ghcr.io/ladybugdb/explorer:latest"));

    // Before indexing, explore errors clearly.
    let dir2 = make_temp_dir("explore_noindex");
    git_init(&dir2);
    run_ok(&dir2, &["init", "--name", "y"]);
    let out2 = run(&dir2, &["explore", "--print"]);
    assert!(!out2.status.success(), "should fail without an index");
    assert!(
        String::from_utf8_lossy(&out2.stderr).contains("run `synapse index`"),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

// --- share (push/pull) guard tests ------------------------------------------
// These verify the safety gates with NO network: a fresh/default workspace must
// never push, and misconfiguration must fail loudly. They assume the binary was
// built with the default `share` feature.

/// A git repo with one committed file, so HEAD resolves to a real commit (the
/// share tests need a commit to tag the graph by). `git_init` alone makes an
/// empty repo with no HEAD when there are no fixtures to commit.
fn git_repo_with_commit(name: &str) -> PathBuf {
    let dir = make_temp_dir(name);
    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    git_init(&dir);
    dir
}

/// Enable push + set a (fake) registry/repository in an initialized workspace.
fn enable_push_config(dir: &Path) {
    let cfg = dir.join(".synapse/synapse.toml");
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("push_enabled = false", "push_enabled = true")
        .replace("registry = \"\"", "registry = \"localhost:5000\"")
        .replace("repository = \"\"", "repository = \"team/graph\"");
    std::fs::write(&cfg, text).unwrap();
}

/// `synapse push` on a default workspace must refuse: push is disabled.
#[test]
fn test_push_disabled_by_default() {
    let dir = make_temp_dir("push_disabled");
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "p"]);
    let out = run(&dir, &["push", "--yes"]);
    assert!(!out.status.success(), "push must fail when disabled");
    assert!(
        stderr(&out).contains("push is disabled"),
        "stderr: {}",
        stderr(&out)
    );
}

/// With push enabled but no registry/repository, push must report the
/// misconfiguration rather than attempting any network call.
#[test]
fn test_push_enabled_but_unconfigured() {
    let dir = make_temp_dir("push_unconfigured");
    git_init(&dir);
    run_ok(&dir, &["init", "--name", "p"]);
    // Enable push in the config (registry/repository left empty).
    let cfg = dir.join(".synapse/synapse.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text = text.replace("push_enabled = false", "push_enabled = true");
    std::fs::write(&cfg, text).unwrap();

    let out = run(&dir, &["push", "--yes"]);
    assert!(!out.status.success(), "push must fail when unconfigured");
    assert!(
        stderr(&out).contains("share target not configured"),
        "stderr: {}",
        stderr(&out)
    );
}

/// Push from a dirty working tree must refuse without --allow-dirty.
#[test]
fn test_push_refuses_dirty_tree() {
    let dir = git_repo_with_commit("push_dirty");
    run_ok(&dir, &["init", "--name", "p"]);
    enable_push_config(&dir);
    // Make the tree dirty with a new file, and index so the graph exists.
    std::fs::write(dir.join("dirty.txt"), "uncommitted").unwrap();
    run_ok(&dir, &["index"]);

    let out = run(&dir, &["push", "--yes"]);
    assert!(!out.status.success(), "push must fail on a dirty tree");
    assert!(
        stderr(&out).contains("uncommitted changes"),
        "stderr: {}",
        stderr(&out)
    );
}

/// Non-interactive push without --yes must refuse rather than hang (the test
/// harness has no TTY on stdin).
#[test]
fn test_push_non_interactive_without_yes() {
    let dir = git_repo_with_commit("push_no_tty");
    run_ok(&dir, &["init", "--name", "p"]);
    enable_push_config(&dir);
    run_ok(&dir, &["index"]);

    // No --yes, stdin is not a TTY → must refuse (not block). Pass
    // --allow-dirty so we reach the confirmation guard (the untracked
    // .synapse/ dir would otherwise trip the dirty-tree guard first).
    let out = run(&dir, &["push", "--allow-dirty"]);
    assert!(
        !out.status.success(),
        "push must refuse without confirmation"
    );
    assert!(
        stderr(&out).contains("not confirmed"),
        "stderr: {}",
        stderr(&out)
    );
}

/// `synapse pull` with no configured registry must report misconfiguration.
#[test]
fn test_pull_unconfigured() {
    let dir = git_repo_with_commit("pull_unconfigured");
    run_ok(&dir, &["init", "--name", "p"]);
    let out = run(&dir, &["pull"]);
    assert!(!out.status.success(), "pull must fail when unconfigured");
    assert!(
        stderr(&out).contains("share target not configured"),
        "stderr: {}",
        stderr(&out)
    );
}

/// `synapse init` adds the graph dir to an existing root .gitignore (so the
/// multi-MB graph isn't committed), but leaves a repo without one untouched.
#[test]
fn test_init_gitignores_graph_dir() {
    // With an existing .gitignore: the graph dir gets appended.
    let dir = git_repo_with_commit("init_gitignore");
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    run_ok(&dir, &["init", "--name", "p"]);
    let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(
        gi.contains(".synapse/graph/"),
        "graph dir should be ignored: {gi}"
    );
    // synapse.toml stays committable (not ignored).
    assert!(
        !gi.contains("synapse.toml"),
        "config must stay committable: {gi}"
    );

    // Idempotent: a second init doesn't duplicate the entry.
    run_ok(&dir, &["init", "--force", "--name", "p"]);
    let gi2 = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert_eq!(
        gi2.matches(".synapse/graph/").count(),
        1,
        "entry must not be duplicated: {gi2}"
    );

    // No .gitignore present → init does not create one.
    let dir2 = git_repo_with_commit("init_no_gitignore");
    run_ok(&dir2, &["init", "--name", "p"]);
    assert!(
        !dir2.join(".gitignore").exists(),
        "init must not create a .gitignore where none existed"
    );
}
