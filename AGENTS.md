# Project Instructions

## Code Style

All Rust source must be formatted with **`rustfmt`** and must pass
**`cargo clippy --tests`** with zero warnings before committing.

```sh
cargo fmt           # format
cargo clippy --tests  # lint
```

- `rustfmt` uses stable defaults (no custom `rustfmt.toml`).
- Clippy runs at default lint level; the only in-source suppressions are
  `#[allow(dead_code)]` on planned-but-unused items and
  `#[allow(clippy::too_many_arguments)]` on two internal VM helpers where
  extracting a struct would obscure the algorithm.
- Run `cargo clippy --fix --allow-dirty --tests` to apply auto-fixable
  suggestions automatically.

## Benchmarks

Run the Criterion benchmark suite with:

```sh
cargo bench
```

To run a specific benchmark group or filter by engine, pass a regex filter:

```sh
# Only oniai variants — skips regex/fancy-regex/pcre2 (much faster)
cargo bench -- oniai

# Only one benchmark group (e.g. literal)
cargo bench -- literal

# Only the JIT variant
cargo bench -- oniai/jit

# Advanced-feature groups only
cargo bench -- "lookahead|lookbehind|backreference|atomic"
```

HTML reports are written to `target/criterion/`. To compare against a saved
baseline, save one first then run with `--baseline`:

```sh
cargo bench -- --save-baseline main
# (make your changes)
cargo bench -- --baseline main
```

### Saving benchmark logs

**Always save the full `cargo bench` output to a log file** in the `log/`
directory at the repository root (create it if it does not exist yet).
Never pipe `cargo bench` output through `grep`, `head`, `tail`, or any other
filter — doing so truncates the output and forces an expensive re-run.

Naming convention: `log/bench-<short-description>-<YYYY-MM-DD>.txt`

Example workflow:

```sh
mkdir -p log
cargo bench 2>&1 | tee log/bench-fork-guard-2026-02-28.txt
```

After the run completes, analyse the saved log file instead of re-running
the benchmarks.  The `log/` directory is git-ignored (add it if needed).

## Version Control

This repository uses **Jujutsu (`jj`)** for version control. Use `jj` commands
for day-to-day workflow rather than raw `git` commands.

### Important: Agent Environment

Always use `-m` flags to provide messages inline (never rely on editor prompts):

```bash
jj desc -m "message"      # NOT: jj desc
jj squash -m "message"    # NOT: jj squash (which opens editor)
```

### Core Concepts

- The working directory is always a commit (`@`). Changes are auto-snapshotted.
  There is no staging area and no need to run `jj commit`.
- Commits are mutable: freely modify them with `jj squash` / `jj absorb`.
- Prefer **Change IDs** (stable across rewrites) over Commit IDs when
  referencing commits.

### Essential Workflow

```bash
# Describe intent first, then code
jj desc -m "Add user authentication to login endpoint"
# ... edit files ...
jj st

# New commit
jj new && jj desc -m "Next change"

# View history / diff
jj log
jj diff
jj show <change-id>
```

### Refining Commits

```bash
jj squash            # move changes into parent
jj absorb            # auto-distribute to appropriate ancestors
jj abandon <id>      # remove a commit (descendants rebased)
jj undo              # reverse last operation
jj restore [paths]   # discard changes
```

### Bookmarks (Branches)

```bash
jj bookmark create my-feature -r@
jj bookmark move my-feature --to <change-id>
jj bookmark list
jj git push -b my-feature
```

### Handling Conflicts

Do not use `jj resolve` (interactive). Edit conflicted files directly to remove
conflict markers, then run `jj st` to verify resolution.

### Quick Reference

| Action | Command |
|--------|---------|
| Describe commit | `jj desc -m "message"` |
| View status | `jj st` |
| View log | `jj log` |
| View diff | `jj diff` |
| New commit | `jj new && jj desc -m "message"` |
| Edit commit | `jj edit <id>` |
| Squash to parent | `jj squash` |
| Auto-distribute | `jj absorb` |
| Abandon commit | `jj abandon <id>` |
| Undo last operation | `jj undo` |
| Restore files | `jj restore [paths]` |
| Create bookmark | `jj bookmark create <name>` |
| Push bookmark | `jj git push -b <name>` |

## Project Context

**Oniai** is a pure-Rust regular expression engine compatible with
[Onigmo](https://github.com/k-takata/Onigmo) (the regex library used by Ruby).
The library core (`src/lib.rs` and its modules) has no external dependencies;
the CLI binary (`src/bin/oniai.rs`) uses `clap` for argument parsing.

The `doc/RE` file is the authoritative reference for **Onigmo (Oniguruma-mod)
Regular Expressions v6.1.0** — consult it when working on parser or compiler
features.  The full design is documented in `doc/DESIGN.md`.

### Architecture (compile-then-execute pipeline)

```
pattern ──► parser.rs ──► AST (ast.rs) ──► compile.rs ──► Vec<Inst> ──► vm.rs ──► match
```

| Module | Role |
|--------|------|
| `src/parser.rs` | Recursive-descent parser; entry: `parse(pattern)` |
| `src/ast.rs` | AST node types (`Node`, `Flags`, `CharClass`, …) |
| `src/compile.rs` | AST → `Vec<Inst>` + `Vec<CharSet>`; entry: `compile(node, …)` |
| `src/vm.rs` | Iterative backtracking executor with explicit `Bt` stack |
| `src/charset.rs` | Character-property helpers (POSIX, Unicode, shorthands) |
| `src/error.rs` | `Error` enum (`Parse`, `Compile`) |
| `src/lib.rs` | Public API: `Regex`, `Match`, `Captures`, iterators |

### VM design notes

- **Backtracking is iterative**: `Fork`/`ForkNext` push `Bt::Retry` entries onto
  an explicit `bt: Vec<Bt>` stack — no Rust recursion for ordinary backtracking.
- **Atomic groups** use a `Bt::AtomicBarrier` fence; `AtomicEnd` commits by
  draining the stack to the fence.  A local `atomic_depth` counter tracks live
  barriers for depth-tagged memoization.
- **Lookarounds** run in isolated sub-`exec` calls with a cloned `State`
  (`depth` is incremented, capped at `MAX_DEPTH = 100`).
- **Subexpression calls** (`\g<name>`) use an iterative `call_stack: Vec<usize>`
  inside `State`; every capturing group ends with `RetIfCalled`.
- `MAX_CALL_DEPTH = 200` prevents infinite recursion in recursive patterns.
- **Memoization** (Algorithms 5–7, Fujinami & Hasuo 2024, arXiv:2401.12639):
  - A single `MemoState` is created in `find()` and shared across all `exec()`
    invocations (including lookaround sub-executions and all search-start positions).
  - `fork_failures: HashMap<(pc,pos) → min_atomic_depth>` — prevents redundant
    re-exploration of the same `Fork` state; provides O(|prog|×|text|) bound.
  - `look_results: HashMap<(lk_pc,pos) → LookCacheEntry>` — caches lookaround
    SUCCESS and FAILURE outcomes with a capture slot *delta* (not full state).
  - Depth-tagged failures: failures under atomic groups (high `atomic_depth`)
    are not reused in less-constrained outer contexts.
  - Memoization is disabled (`use_memo = false`) when the program contains
    `BackRef`, `BackRefRelBack`, or `CheckGroup` — these make `(pc, pos)` alone
    an insufficient cache key.
