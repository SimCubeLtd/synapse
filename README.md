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
synapse pull                                      # fetch a shared graph from an OCI registry
synapse push --yes                                # publish the graph (restricted; see below)
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
| `status` | Show index readiness, counts (including `referenceEdges`), stale + untracked files (`--json`, `--stale`). The `--json` output lists `referenceLanguages` so reference-edge coverage is explicit. |
| `symbols <query>` | Search symbols (`--kind`, `--file`, `--language`, `--json`). |
| `related` | Find related files for a `--symbol` or `--file` (`--depth`, `--json`). Traverses the graph — project membership (`CONTAINS_FILE`) for same-project siblings, type relations (`INHERITS`/`IMPLEMENTS`) for base types, interfaces/traits, subtypes and implementors, and usage references (`REFERENCES`) for callers / instantiation sites — alongside same-directory / test / name heuristics. |
| `packages` | List indexed package dependencies (or `--projects`), with versions resolved via .NET Central Package Management (`--ecosystem`, `--json`). Use `--importers <pkg>` for impact analysis: the files that import a package. |
| `explore` | Launch [Ladybug Explorer](https://docs.ladybugdb.com/visualization/) (Docker) to visualize the indexed graph in a browser (`--port`, `--read-write`, `--detach`, `--in-memory`, `--tag`, `--print`). Mounts the index read-only by default; `--print` shows the `docker run` command without executing it. |
| `pack` | Emit a context pack (`--changed`/`--path`/`--symbol`/`--query`, `--budget`, `--include-tests`, `--include-config`, `--include-diff`, `--dry-run`, `--explain`, `--output`). `--format markdown` (default) or `--format json` for programmatic callers. Writes to stdout unless `--output` is given; diagnostics go to stderr. |
| `pull` | Fetch a shared graph from the configured OCI registry (`--tag`, `--registry`, `--repository`). Verifies integrity (blake3), writes the graph atomically, and warns if its indexed commit differs from local `HEAD`. |
| `push` | Publish the indexed graph to the configured OCI registry (`--tag`, `--registry`, `--repository`, `--yes`, `--allow-dirty`). Restricted — see [Sharing the graph](#sharing-the-graph-oci-registry). |
| `clean` | Remove `--cache` / `--index` / `--packs` / `--all`. |

## Sharing the graph (OCI registry)

The indexed graph (`synapse.lbug`) is a multi-MB binary — too heavy for git. Instead, share it with your team via any OCI registry (GHCR, ECR, ACR, Harbor, …): CI (or a maintainer) `push`es the graph, teammates `pull` it instead of re-indexing.

```bash
# One-time: point the [share] section at your registry (in .synapse/synapse.toml)
#   [share]
#   registry = "ghcr.io"
#   repository = "myorg/myrepo-synapse-graph"
#   push_enabled = true        # required to allow `push` at all (default false)

synapse pull                   # fetch the current shared graph (tag "latest")
synapse pull --tag a1b2c3d     # fetch the graph for a specific commit
synapse push --yes             # publish (CI-friendly; skips the confirm prompt)
```

- **Credentials are auto-discovered** from your existing `docker login` (`~/.docker/config.json` + OS credential helpers). You do **not** put a token in synapse's config. Public registries pull anonymously with no setup. For headless setups without a docker config, set `SYNAPSE_REGISTRY_USER` + `SYNAPSE_REGISTRY_PASS` (or `SYNAPSE_REGISTRY_TOKEN`).
- **Push is heavily guarded** (a fresh clone / CI can never push by accident): it requires `push_enabled = true` in config **and** a clean working tree (or `--allow-dirty`) **and** interactive type-to-confirm (or `--yes`). The graph is tagged by commit (a per-commit tag plus the moving `latest`), so a tag always describes a known state.
- **Staleness is surfaced, never hidden.** `pull` warns loudly when the graph's indexed commit differs from your `HEAD`, and `synapse status` shows the pulled graph's `Origin:` commit on every run. If it's stale, `synapse index` rebuilds locally.
- **Visibility = the registry's visibility.** The graph encodes file paths, symbol names and dependencies — treat a public registry accordingly.
- Sharing is the `share` Cargo feature (on by default); a `--no-default-features` build without it drops the networking/TLS stack and the `push`/`pull` commands.

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

**Usage references** (`REFERENCES`) connect the symbol a usage sits inside to the
symbol it points at — `new Foo(...)` instantiations, calls, and type uses. So
`related --symbol Foo` and `pack --symbol Foo` now surface the callers and
instantiation sites of `Foo`, not just its declaration. Resolution is the same
deterministic, name-based scheme as type relationships (same-file/same-project
preference; ambiguous names link every candidate rather than guessing; a name
that matches no declared symbol — e.g. a local variable — produces no edge).
Reference extraction currently covers **C#, Rust, and JS/TS**; `status --json`
reports `referenceLanguages` and a `referenceEdges` count so coverage is explicit
rather than silently partial. (Python, Go and Svelte resolve type relationships
and imports but not yet usage references.)

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

## Claude

Get claude to create a memory, that prefers the tool usage over raw grep.

```markdown
Save this as a memory called reference_synapse_cli.md in this project's memory directory, and add a one-line entry for it to MEMORY.md so it shows up in the index. Here's the content:

---
name: reference-synapse-cli
description: "synapse CLI — local offline graph index of the repo; use BEFORE grep/glob/Explore to pull focused, token-budgeted context."
metadata:
  node_type: memory
  type: reference
---

`synapse` is a deterministic, offline Rust CLI that indexes a repo into a local graph (LadybugDB at `.synapse/`) and emits focused, token-budgeted context packs. No network, no AI, no daemon. **Prefer it over reading whole files or running multi-file grep sweeps** when you need to understand or modify code.

## Setup (once per repo)
\```bash
synapse init            # creates .synapse/
synapse index           # build the graph (incremental, blake3-hashed, safe to repeat)
synapse status --json   # confirm: filesIndexed, symbolsIndexed, referenceEdges, referenceLanguages
\```

Re-run `synapse index` after code changes; `status` reports `staleFiles`.

## When to reach for it
- Before editing a known file: **`synapse pack --path <file> --format json --budget 20000`** is the killer move — gives the file + same-directory siblings (events, exceptions, supporting types) in one call. Replaces 5–7 sequential Reads when working in a domain slice.
- "Who emits/calls/instantiates X" — **`synapse related --symbol X --json`** returns true reference sites via the REFERENCES edge (C#/Rust/JS/TS). **Replaces grep for symbol impact analysis on supported languages.**
- Before editing a symbol: `synapse pack --symbol <Name> --format json --budget 20000` pulls the declaration + all reference sites at tier 1, ahead of same-directory noise.
- Before editing the working tree: `synapse pack --changed --format json` to see exactly what's about to be touched plus related context.
- Symbol lookup faster than grep: `synapse symbols <query> --kind class --json` — fast, precise.
- Dependency impact analysis: `synapse packages --importers <pkg> --json` — beats grep for "what would a library upgrade touch?".
- Skip when: you already know the exact file path AND need the full contents (just Read it); you need full-text content search inside strings/comments/log messages (synapse correctly ignores comments — grep for those); the target language is Python/Go/Svelte (no REFERENCES yet — check `status` `referenceLanguages`).

## Contract
- **stdout = data, stderr = diagnostics.** Capture stdout, ignore stderr. Exit 0 = ok, 1 = error.
- Pass `--json` (or `pack --format json`) for machine-parseable output — always prefer this when consuming programmatically.
- Deterministic; all paths repo-relative with forward slashes.
- If a command errors "no Synapse workspace" / "run `synapse index` first" → run `synapse index`. Use `synapse index --changed` on big repos.

## `pack` — the main tool
Pick exactly ONE selection mode: `--symbol <Name>`, `--changed`, `--path <dir|file>`, or `--query <text>`. Useful flags:
- `--format json` (structured: `{request, repository, selection[], symbols[], files[{path,language,contents}], diff?}`)
- `--budget <N>` token cap (default 40000; over-budget files listed as `included:false`)
- `--depth <N>` graph expansion (default 1)
- `--dry-run` selection + symbols only, no contents (cheap preview)
- `--explain` print ranking tier/reason (to stderr)
- `--include-tests` / `--include-config` / `--include-diff` / `--output <file>`

Selection ranking (high→low): changed / exact-symbol / exact-path → graph relations (same project, base types, implementors, imports, **references**) → tests/config/same-dir → docs. Generated files, lockfiles, minified bundles excluded.

## Other commands
- `synapse symbols <query> [--kind <k>] [--language <l>] [--file <path>] [--json]` — kinds: class struct record interface trait enum function method module type component constructor key. Languages: csharp rust python go javascript typescript svelte bash yaml json markdown.
- `synapse related --symbol <Name>` or `--file <path>` `[--depth N] [--json]`
- `synapse packages [--json] [--projects] [--importers <pkg>]` — last form is impact analysis: which files import a package. CPM-resolved for .NET.
- `synapse explore` — Ladybug Explorer UI on :8000 (needs Docker); `--print` to just print the docker command.

## Graph contents
Nodes: Repository, Project, Package, File, Symbol. Populated edges: DECLARES, CONTAINS_FILE, REFERENCES_PROJECT, USES_PACKAGE, IMPORTS_PACKAGE (JS/TS + C#), INHERITS, IMPLEMENTS, **REFERENCES (symbol→symbol: `new`/call/type use; C#/Rust/JS/TS only as of v0.1.5 — check `status.referenceLanguages`).** CALLS is folded into REFERENCES; the legacy edge is empty, don't rely on it. Edge resolution is name-based (no type inference): ambiguous names may link to multiple symbols (prefers same-file/project). `related --symbol`/`pack --symbol` need exact (case-insensitive) name match to seed. **REFERENCES are anchored to the enclosing declaration** — a usage at file/module top-level (outside any function/type) is not captured.

## Symbol extraction by language
C#, Rust, Python, Go, JS/TS (+JSX/TSX), Svelte (component + script symbols + SvelteKit route roles), Bash (functions), YAML/JSON (top-level keys), Markdown (headings as `key` symbols).

## Gotchas (learned from real use)
- **`related --symbol` can explode on overloaded names at depth 1.** A name declared in many files makes "same project" at depth 1 pull the entire enclosing project. The REFERENCES results come first (good) but the same-project noise still bloats the JSON — filter on `reason` containing `"references"` or `"declares"` when you only care about real call sites, or use `--path <file>` for narrow scope.
- **Always budget-cap `pack`.** Output is huge by default (40k token budget). For most domain-slice work, `--budget 20000` is plenty. Use `--dry-run` first when unsure — it shows selection + estimated tokens without the contents.
- **REFERENCES only covers C#/Rust/JS/TS** (v0.1.5). Python, Go, Svelte declarations index fine but their call sites won't show up via `related`. Confirm coverage with `synapse status --json` → `referenceLanguages`.
- **REFERENCES needs an enclosing declaration.** A `new Foo()` at module top-level (outside any function/method/class) is not captured. Rare in C#/Rust, more common in TS/JS init code — grep if you suspect a top-level call site.
- **`symbols`/`--query` match names+paths, not file bodies.** For text inside files (string literals, comments, log messages), use grep. **Comments are correctly ignored** by REFERENCES — a "see `UserRegistered` in XYZ" doc comment is NOT a reference and won't show up.
- **Pipe big JSON through `python3 -c`/`jq`** instead of `head` — the file list is what you want, raw JSON dumps blow up context.

## Verdict (measured head-to-head vs grep)
- **Find related classes**: synapse finds ~4× more relevant classes than naive `grep "class X"` (catches names *containing* X, not just prefix matches).
- **Find usages**: filtered `related --symbol` returns the actionable list; raw `grep -l` returns ~6× more files (substring matches in unrelated names, comment hits, doc references).
- **Get a domain slice**: `pack --path` is 1 call + automatic budget enforcement; the grep equivalent is 1 `ls` + N `Read` calls with no ranking.

Use synapse first. Reach for grep only when (a) you need text inside strings/comments/log messages, (b) the language isn't in `referenceLanguages` yet, or (c) you're hunting a top-level usage outside any declaration.
```
