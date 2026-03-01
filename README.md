# Oniai

A pure-Rust regular expression engine compatible with
[Onigmo](https://github.com/k-takata/Onigmo) — the regex library used by Ruby.

Oniai implements the full Onigmo syntax including look-around (lookahead and
variable-length lookbehind), atomic groups, backreferences, named captures,
subexpression calls (`\g<name>`), the absence operator `(?~...)`, and more.
Case-insensitive matching uses full Unicode case folding (via static tables
generated from the official Unicode data files), including multi-codepoint folds
such as `ß` ↔ `ss`.
It is backed by a memoizing backtracking VM that provides near-linear time
behaviour on a broad class of patterns (see [Performance](#performance)).

---

## Table of contents

- [Installation](#installation)
- [CLI usage](#cli-usage)
- [Library usage](#library-usage)
- [Supported syntax](#supported-syntax)
- [Performance](#performance)
- [Project layout](#project-layout)
- [Development](#development)
- [References](#references)

---

## Installation

### As a command-line tool

```sh
cargo install --path .
```

This installs the `oniai` binary into `~/.cargo/bin/`.

### As a library

Add to `Cargo.toml`:

```toml
[dependencies]
oniai = { path = "/path/to/oniai" }
```

---

## CLI usage

`oniai` is a `grep`-like search tool that uses Onigmo regular expressions.

```
oniai [OPTIONS] PATTERN [FILE]...
```

When no `FILE` arguments are given, standard input is read.

### Options

| Flag | Long form | Description |
|------|-----------|-------------|
| `-i` | `--ignore-case` | Case-insensitive matching |
| `-v` | `--invert-match` | Print lines that do **not** match |
| `-n` | `--line-number` | Prefix each line with its line number |
| `-c` | `--count` | Print only the count of matching lines per file |
| `-l` | `--list-files` / `--files-with-matches` | Print only filenames that contain a match |
| `-o` | `--only-matching` | Print only the matched portion of each line |
| `-r` | `--recursive` | Search directories recursively |
|      | `--color=WHEN` | Colorize output: `auto` (default), `always`, or `never` |
| `-h` | `--help` | Print help |
| `-V` | `--version` | Print version |

### Exit status

| Code | Meaning |
|------|---------|
| `0` | At least one match was found |
| `1` | No match found |
| `2` | Error (bad pattern, unreadable file, …) |

### Examples

```sh
# Find all function definitions in Rust files
oniai 'fn \w+' src/*.rs

# Case-insensitive search with line numbers
oniai -in 'error' server.log

# Print only the matched portion (like grep -o)
oniai -o '\b[A-Z][a-z]+\b' README.md

# Count matches per file
oniai -c 'TODO' src/**/*.rs

# List files containing a pattern
oniai -rl 'unsafe' src/

# Variable-length lookbehind (not supported by many engines)
oniai '(?<=\d{4}-\d{2}-)\d{2}' dates.txt

# Absence operator: match C-style comments
oniai '/\*(?~\*/)\*/' source.c
```

---

## Library usage

```rust
use oniai::Regex;

// Simple match test
let re = Regex::new(r"\d+").unwrap();
assert!(re.is_match("abc 123"));

// Find the first match
let m = re.find("price: 42").unwrap();
assert_eq!(m.as_str(), "42");
assert_eq!(m.start(), 7);

// Iterate over all non-overlapping matches
let words: Vec<_> = Regex::new(r"\w+").unwrap()
    .find_iter("one two three")
    .map(|m| m.as_str())
    .collect();
assert_eq!(words, ["one", "two", "three"]);

// Named capture groups
let re = Regex::new(r"(?<year>\d{4})-(?<month>\d{2})-(?<day>\d{2})").unwrap();
let caps = re.captures("today is 2024-01-15").unwrap();
assert_eq!(caps.name("year").unwrap().as_str(),  "2024");
assert_eq!(caps.name("month").unwrap().as_str(), "01");
assert_eq!(caps.name("day").unwrap().as_str(),   "15");

// Iterate over all capture matches
for caps in re.captures_iter("2024-01-15 and 2025-06-30") {
    println!("{}", caps.get(0).unwrap().as_str());
}
```

### API reference

```rust
// Compilation
Regex::new(pattern: &str) -> Result<Regex, Error>

// Matching
re.is_match(text: &str) -> bool
re.find(text: &str) -> Option<Match>
re.find_iter(text: &str) -> FindIter          // yields Match
re.captures(text: &str) -> Option<Captures>
re.captures_iter(text: &str) -> CapturesIter  // yields Captures

// Match
m.as_str() -> &str
m.start()  -> usize   // byte offset
m.end()    -> usize   // byte offset (exclusive)
m.range()  -> Range<usize>

// Captures
caps.get(i: usize) -> Option<Match>   // 0 = whole match, 1.. = groups
caps.name(name: &str) -> Option<Match>
caps.len() -> usize
```

---

## Supported syntax

Oniai supports the Onigmo v6.1.0 regex syntax.  A complete reference is in
[`doc/RE`](doc/RE).  Key features:

### Literals and character classes

| Syntax | Description |
|--------|-------------|
| `.` | Any character except newline (with `(?m)`, matches newline too) |
| `[abc]`, `[a-z]` | Character class |
| `[^abc]` | Negated character class |
| `[a&&b]` | Intersection of character classes |
| `\d`, `\w`, `\s` | Digit, word, space (and `\D`, `\W`, `\S`) |
| `\h` | Hex digit |
| `\p{Alpha}`, `\p{Lu}` | Unicode property |
| `[:alpha:]` | POSIX class (inside `[...]`) |

### Anchors

| Syntax | Description |
|--------|-------------|
| `^`, `$` | Start/end of line (always multiline in Ruby semantics) |
| `\A`, `\z` | Start/end of string |
| `\Z` | End of string (or before final `\n`) |
| `\b`, `\B` | Word boundary / non-boundary |
| `\G` | Start of current search position |
| `\K` | Reset match start (keep) |

### Quantifiers

| Syntax | Description |
|--------|-------------|
| `*`, `+`, `?` | Greedy (0+, 1+, 0 or 1) |
| `*?`, `+?`, `??` | Lazy (reluctant) |
| `*+`, `++`, `?+` | Possessive |
| `{n}`, `{n,}`, `{n,m}` | Counted (greedy); add `?` for lazy |

### Groups

| Syntax | Description |
|--------|-------------|
| `(...)` | Capturing group |
| `(?:...)` | Non-capturing group |
| `(?<name>...)`, `(?'name'...)` | Named capturing group |
| `(?>...)` | Atomic group (no backtrack) |
| `(?~...)` | Absence operator |
| `(?i)`, `(?m)`, `(?x)`, `(?a)` | Inline flags (case, dot-all, extended, ASCII) |
| `(?#...)` | Comment |

### Look-around

| Syntax | Description |
|--------|-------------|
| `(?=...)` | Positive lookahead |
| `(?!...)` | Negative lookahead |
| `(?<=...)` | Positive lookbehind (variable-length supported) |
| `(?<!...)` | Negative lookbehind (variable-length supported) |

### Backreferences and calls

| Syntax | Description |
|--------|-------------|
| `\1` … `\9`, `\k<name>` | Backreference |
| `\k<-1>` | Relative backreference |
| `\g<name>`, `\g<1>` | Subexpression call (recursive patterns) |

### Conditional groups

| Syntax | Description |
|--------|-------------|
| `(?(n)yes\|no)` | Branch on whether group `n` matched |
| `(?(name)yes\|no)` | Branch by named group |

---

## Performance

Oniai implements the memoization framework from:

> Fujinami, H. & Hasuo, I. (2024).  *Efficient Matching with Memoization for
> Regexes with Look-around and Atomic Grouping.*  arXiv:2401.12639.

This gives near-linear matching time for a broad class of patterns.  Key properties:

- **Fork-state memo** (Algorithm 5): each `(pc, pos)` fork state is visited at
  most once, bounding total work to O(|pattern| × |text|).  This eliminates
  exponential blowup for patterns like `(a?)^n a^n`.
- **Lookaround result cache** (Algorithm 6): the success/failure of a
  lookaround body at a given position is cached and reused on subsequent
  backtracking paths.
- **Depth-tagged atomic groups** (Algorithm 7): memoized failures are tagged
  with the atomic-group nesting depth so they are not incorrectly reused outside
  the group.

Memoization is automatically disabled for patterns containing backreferences or
conditional groups, where `(pc, pos)` alone does not determine the outcome.

See [`doc/BENCHMARKS.md`](doc/BENCHMARKS.md) for measured results.

---

## Project layout

```
src/
  lib.rs        Public API (Regex, Match, Captures, iterators)
  ast.rs        AST node types
  parser.rs     Recursive-descent parser
  compile.rs    AST → VM bytecode compiler
  vm.rs         Memoizing backtracking VM executor
  charset.rs    Character property helpers (Unicode, POSIX)
  casefold.rs   Unicode full case folding (binary search on static tables)
  casefold_trie.rs  Compile-time case-fold → ByteTrie expansion
  general_category.rs  Unicode General Category lookup (binary search on static ranges)
  bytetrie.rs   Immutable UTF-8 byte trie for case-fold matching
  error.rs      Error type
  data/
    casefold_data.rs          Pre-generated case fold tables (from data/CaseFolding.txt)
    general_category_data.rs  Pre-generated GC range table (from data/DerivedGeneralCategory.txt)
  bin/
    oniai.rs   grep-like CLI binary
data/
  CaseFolding.txt                        Unicode 17.0.0 case folding data
  extracted/DerivedGeneralCategory.txt   Unicode 17.0.0 general category data
doc/
  RE            Onigmo v6.1.0 syntax reference
  DESIGN.md     Architecture and implementation notes
  BENCHMARKS.md Benchmark methodology and results
scripts/
  fetch_unicode_data.sh     Download Unicode data files from unicode.org
  gen_unicode_tables/       Standalone Rust binary: regenerates src/data/*.rs
tests/
  integration_test.rs  Integration tests
benches/
  regex.rs      Criterion benchmarks
```

---

## Development

### Prerequisites

- Rust 1.85+ (edition 2024)

### Build

```sh
cargo build          # debug
cargo build --release
```

### Test

```sh
cargo test
```

### Lint and format

```sh
cargo fmt
cargo clippy --tests
```

### Unicode data tables

The files `src/data/casefold_data.rs` and `src/data/general_category_data.rs`
are pre-generated from the Unicode Character Database and committed to the
repository so that builds require no network access or extra tooling.

To update to a new Unicode version:

```sh
sh scripts/fetch_unicode_data.sh 17.0.0   # downloads data/ files
cargo run --manifest-path scripts/gen_unicode_tables/Cargo.toml
```

Then commit the updated `data/` and `src/data/` files together.

### Benchmarks

```sh
cargo bench
```

The full suite compares five engines (oniai/jit, oniai/interp, regex, fancy-regex, pcre2)
and takes several minutes.  Use Criterion's filter argument to narrow the run:

```sh
# Only oniai variants (fast — skips comparison libraries)
cargo bench -- oniai

# Only one benchmark group
cargo bench -- literal

# Only the JIT variant across all groups
cargo bench -- oniai/jit

# Advanced-feature groups only (lookahead, lookbehind, …)
cargo bench -- "lookahead|lookbehind|backreference|atomic"
```

Results are written to `target/criterion/`.

### Version control

This repository uses [Jujutsu (`jj`)](https://github.com/jj-vcs/jj) for version
control.  Use `jj` commands (`jj new`, `jj describe`, `jj log`, …) rather than
raw `git` commands.

---

## References

- [Onigmo](https://github.com/k-takata/Onigmo) — the regex library Oniai is compatible with
- Fujinami, H. & Hasuo, I. (2024).  *Efficient Matching with Memoization for Regexes with Look-around and Atomic Grouping.*  [arXiv:2401.12639](https://arxiv.org/abs/2401.12639)
- [`doc/DESIGN.md`](doc/DESIGN.md) — detailed architecture document
- [`doc/BENCHMARKS.md`](doc/BENCHMARKS.md) — benchmark results and analysis
