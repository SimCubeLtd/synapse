# synapse — quick reference for AI agents

`synapse` is a deterministic, offline CLI that indexes a repo into a local graph
(LadybugDB) and emits focused, token-budgeted context. No network, no AI calls,
no daemon. Use it to pull *relevant* code/context instead of reading whole files.

## Contract (read this first)
- **stdout = data, stderr = diagnostics.** `pack`/`symbols`/`related`/`packages`/
  `status` write their result to stdout; progress/notes/errors go to stderr.
  Capture stdout, ignore stderr. Exit code `0` = success, `1` = error.
- **`--json`** on `symbols`/`related`/`packages`/`status`, and **`pack --format json`**,
  give machine-parseable output. Prefer these when consuming programmatically.
- **Deterministic**: same repo state → same output (stable sort order).
- **All paths are repo-relative, forward-slash.**

## Setup (once per repo)
```bash
synapse init            # creates .synapse/ (idempotent-ish; --force to overwrite)
synapse index           # build/refresh the graph. Re-run anytime: it's incremental
                        #   (blake3 hash skip), removes deleted files, safe to repeat.
synapse index --changed # only git-changed files (faster on big repos)
synapse status --json   # {index:"ready"|"empty", filesIndexed, symbolsIndexed, referenceEdges, referenceLanguages, ...}
```
If a command errors with "no Synapse workspace"/"run `synapse index` first", run
init/index first.

## The main tool: `pack` (get context for a task)
Emits a self-contained context pack. Pick ONE selection mode:
```bash
synapse pack --symbol <Name>        # the symbol's file + graph-related files
synapse pack --changed              # current git changes (great before editing)
synapse pack --path <dir/or/file>   # everything under a path
synapse pack --query <text>         # case-insensitive match over paths + symbols
```
Useful flags:
- `--format json` — structured `{request, repository, selection[], symbols[], files[{path,language,contents}], diff?}`. Default is Markdown.
- `--budget <N>` — approx token cap (chars/4; default 40000). Over budget → lowest-priority files dropped (still listed as `included:false`).
- `--depth <N>` — how far to expand graph relations (default 1).
- `--dry-run` — selection + symbols only, NO file contents (cheap preview of what would be included).
- `--explain` — print ranking tier + reason per file (to stderr).
- `--include-tests` / `--include-config` — pull in test/config files (excluded by default unless directly matched).
- `--include-diff` — append the git diff.
- `--output <file>` — write to a file instead of stdout.

Selection ranking (high→low): changed/exact-symbol/exact-path → graph relations
(same project, base types, implementors, imports) → tests/config/same-dir → docs.
Generated files, lockfiles, minified/bundles are excluded.

Typical agent flow: `synapse pack --symbol Foo --format json` → read `files[]` →
edit. Or `synapse pack --changed --format json` to review what you're about to touch.

## Discovery commands
```bash
synapse symbols <query>                 # substring match on symbol names
synapse symbols --kind class --language typescript --json
synapse symbols --file src/Foo.cs       # symbols in one file
# kinds: class struct record interface trait enum function method module type component constructor key
# languages: csharp rust python go javascript typescript svelte bash yaml json markdown

synapse related --symbol <Name> --depth 2 --json   # files related to a symbol
synapse related --file <path> --json               # files related to a file
# relations: same project (CONTAINS_FILE), base type (INHERITS), interface/trait
#   (IMPLEMENTS), subtype/implementor (reverse), callers/instantiation sites
#   (REFERENCES, C#/Rust/JS/TS), same directory, likely tests.

synapse packages [--json]               # dependencies w/ versions (CPM-resolved for .NET)
synapse packages --projects             # projects (kind shows "dotnet (cpm)" etc.)
synapse packages --importers <pkg>      # IMPACT ANALYSIS: which files import a package
```

## What's in the graph
Nodes: Repository, Project, Package, File, Symbol.
Edges (populated): DECLARES (file→symbol), CONTAINS_FILE (project→file),
REFERENCES_PROJECT, USES_PACKAGE, IMPORTS_PACKAGE (file→package, JS/TS + C#),
INHERITS, IMPLEMENTS (symbol→symbol), REFERENCES (symbol→symbol: `new`/call/type
use; C#/Rust/JS/TS only — Python/Go/Svelte not yet, see `status` referenceLanguages).
Edges (schema only, empty): CALLS — folded into REFERENCES; don't rely on it.

Languages with symbol extraction: C#, Rust, Python, Go, JS/TS(+JSX/TSX), Svelte
(component + script symbols + SvelteKit route roles), Bash (functions), YAML/JSON
(top-level keys), Markdown (headings as `key` symbols).

## Visualize (optional, needs Docker)
```bash
synapse explore            # Ladybug Explorer UI on http://localhost:8000 (read-only)
synapse explore --print    # just print the docker command (no Docker needed)
```

## Gotchas
- Re-run `synapse index` after code changes; stale results otherwise (`status` shows `staleFiles`).
- `pack` needs exactly one of `--changed/--path/--symbol/--query`.
- `--query`/`symbols` match is on names+paths (case-insensitive substring), not full-text file contents.
- `related`/`pack` for a symbol use exact (case-insensitive) name match for the seed; partial names won't seed.
- Edge resolution is name-based (no type inference): an ambiguous name may link to multiple symbols (prefers same-file/project).
- REFERENCES are anchored to the enclosing declaration: a usage at file/module top-level (outside any function/type) is not captured.
