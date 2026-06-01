# SimCube Synapse

A deterministic, **local**, **offline** repository context compiler.

`synapse` indexes a source repository into a lightweight graph (files, symbols,
projects, packages, and their relationships), persists it in
[LadybugDB](https://ladybugdb.com), and emits compact, LLM-ready Markdown
context packs you can paste into ChatGPT, Claude, Codex, or any other LLM UI.

It is **not** an AI agent, **not** an MCP server, and requires **no daemon, no
network, and no AI API calls**. It is a pure local CLI built for large
mono-repos and mixed-language engineering repositories.

```text
local repo
  -> indexed graph in LadybugDB
  -> selected relevant files/symbols/relationships
  -> compact LLM-ready Markdown context pack
```

The core value is **selection**: it does not dump your whole repo. It picks
focused context based on changed files, paths, symbols, or queries, ranks it,
fits it to an approximate token budget, and renders portable Markdown.

## Install / build

Requires a recent Rust toolchain and a C++ toolchain (LadybugDB statically
compiles its engine on first build).

```bash
cargo build --release          # binary at target/release/synapse
```

LadybugDB lives behind a default-on cargo feature (`ladybug`). Tests that only
need the in-memory store can run without it:

```bash
cargo test --no-default-features   # fast: no C++ build, no Ladybug
cargo test                         # full suite incl. the Ladybug smoke test
```

## Usage

```bash
synapse init --name my-repo                 # create .synapse/ + config
synapse index --stats                       # build the graph
synapse status                              # ready / stale / counts
synapse symbols GraphStore                  # search symbols
synapse symbols --kind component --language typescript
synapse related --symbol MyHandler --depth 2
synapse pack --symbol MyHandler --output context.md
synapse pack --changed --budget 40000 --output context.md --explain
synapse pack --symbol MyHandler --format json     # structured output for tools
synapse explore                                   # visualize the graph in a browser
synapse clean --all
```

### Visualizing the graph

`synapse explore` launches [Ladybug Explorer](https://docs.ladybugdb.com/visualization/)
— a browser UI for the graph — from its Docker image, mounting the indexed
`.lbug` file **read-only** by default on <http://localhost:8000>:

```bash
synapse explore                 # foreground, read-only, Ctrl-C to stop
synapse explore --read-write    # allow edits in the UI to persist
synapse explore --detach        # run in the background
synapse explore --print         # just print the `docker run` command
```

Requires Docker. If it isn't installed/running, `explore` prints the exact
`docker run` command so you can run it yourself. Note: Explorer's storage format
must match this build's LadybugDB version — if you hit a storage-format error,
try `--tag dev` or a matching image tag.

### Commands

| Command | Purpose |
|---|---|
| `init` | Create `.synapse/{synapse.toml,graph,cache,packs}` (`--force`, `--name`). |
| `index` | Index the repo into LadybugDB (`--force`, `--changed`, `--stats`). Incremental via blake3 content hashes. |
| `status` | Show index readiness, counts, stale + untracked files (`--json`, `--stale`). |
| `symbols <query>` | Search symbols (`--kind`, `--file`, `--language`, `--json`). |
| `related` | Find related files for a `--symbol` or `--file` (`--depth`, `--json`). Traverses the graph — project membership (`CONTAINS_FILE`) for same-project siblings, and type relations (`INHERITS`/`IMPLEMENTS`) for base types, interfaces/traits, subtypes and implementors — alongside same-directory / test / name heuristics. |
| `packages` | List indexed package dependencies (or `--projects`), with versions resolved via .NET Central Package Management (`--ecosystem`, `--json`). Use `--importers <pkg>` for impact analysis: the files that import a package. |
| `explore` | Launch [Ladybug Explorer](https://docs.ladybugdb.com/visualization/) (Docker) to visualize the indexed graph in a browser (`--port`, `--read-write`, `--detach`, `--in-memory`, `--tag`, `--print`). Mounts the index read-only by default; `--print` shows the `docker run` command without executing it. |
| `pack` | Emit a context pack (`--changed`/`--path`/`--symbol`/`--query`, `--budget`, `--include-tests`, `--include-config`, `--include-diff`, `--dry-run`, `--explain`, `--output`). `--format markdown` (default) or `--format json` for programmatic callers. Writes to stdout unless `--output` is given; diagnostics go to stderr. |
| `clean` | Remove `--cache` / `--index` / `--packs` / `--all`. |

## Languages

Symbol extraction (via tree-sitter) covers **C#, Rust, Python, Go, JavaScript,
TypeScript/TSX, Svelte (Svelte 5 / SvelteKit), Bash/sh, YAML, JSON, and
Markdown**:

- **JS/TS**: functions, classes, interfaces, type aliases, enums, exported
  declarations, and obvious React components.
- **Svelte**: each `.svelte` file is a `component` symbol named after its file
  stem, plus the functions/classes/types declared in its `<script>` block
  (parsed as TS by default, `lang="js"` honoured). SvelteKit route files
  (`+page`, `+layout`, `+server`, `+error`) are recognised and tagged with their
  route role.
- **Bash/sh**: function definitions from `.sh`/`.bash`/`.zsh` files.
- **YAML / JSON**: top-level mapping keys (and YAML anchors) as `key` symbols, so
  config sections are searchable and packable by name.
- **Markdown**: every heading (ATX and setext) as a `key` symbol named after the
  heading text, with its level recorded — so docs repos are ingestible and you
  can pack a specific section by name (`pack --query "Installation"`). Covers
  `.md`/`.markdown`/`.mdx`.

`.csproj` files contribute project/package references; `package.json` files
contribute dependencies, dev/peer dependencies, and scripts. Per-language
extraction can be toggled in `[index.languages]` in `synapse.toml`.

**Type relationships** (`INHERITS` / `IMPLEMENTS`) are extracted from explicit
declarations — C# base lists, Rust `impl Trait for Type` and supertrait bounds,
TS `extends`/`implements`, Python class bases, Go embedding — and resolved to
symbols by name (preferring same-file/same-project matches). For C#, where the
syntax doesn't separate a base class from interfaces, the edge kind is decided
from the target symbol's kind. This is deterministic name-based resolution, not
full type inference.

**File → package imports** (`IMPORTS_PACKAGE`) are extracted for JS/TS (`import`/
`require`, reduced to the npm package root) and C# (`using` namespace, matched to
the NuGet package by longest dotted prefix). Edges are only created when an
import resolves to a package the manifests already declared, so
`synapse packages --importers <pkg>` answers "which files use this dependency?".

**.NET Central Package Management** is handled: a `PackageReference` without an
inline version is resolved against the `<PackageVersion>` pins in the nearest
`Directory.Packages.props` (and shared `PackageReference`s in
`Directory.Build.props`) up the directory tree. Projects governed by CPM are
flagged accordingly. Files are also linked to their nearest owning project
(`.csproj`/`package.json`) in the graph.

## Architecture

```text
src/
  main.rs        cli.rs        config.rs     repo.rs      git.rs    errors.rs
  indexer/       { languages, tree_sitter, dotnet, node }
  graph/         { model, store (GraphStore trait), ladybug_store, memory_store }
  pack/          { selector, budget, markdown }
  output/        { table, json }
```

All LadybugDB-specific code is isolated in `src/graph/ladybug_store.rs` behind
the `GraphStore` trait, so the storage backend stays replaceable and testable.
A `MemoryGraphStore` backs unit tests; the production CLI always uses LadybugDB.
