# Copilot Instructions

## Version Control

This repository uses **Jujutsu (`jj`)** for version control. Use `jj` commands for day-to-day workflow (e.g., `jj new`, `jj describe`, `jj squash`) rather than raw `git` commands.

## Project Context

**Aigumo** is a pure-Rust regular expression engine compatible with
[Onigmo](https://github.com/k-takata/Onigmo) (the regex library used by Ruby).
It has no external dependencies.

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
  draining the stack to the fence.
- **Lookarounds** run in isolated sub-`exec` calls (`depth` is incremented here,
  capped at `MAX_DEPTH = 100`).
- **Subexpression calls** (`\g<name>`) use an iterative `call_stack: Vec<usize>`
  inside `State`; every capturing group ends with `RetIfCalled`.
- `MAX_CALL_DEPTH = 200` prevents infinite recursion in recursive patterns.
