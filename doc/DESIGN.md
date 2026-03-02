# Oniai — Design Document

Oniai is a pure-Rust regular expression engine compatible with
[Onigmo](https://github.com/k-takata/Onigmo) (the regex library used by Ruby).
It follows the classic *compile-then-execute* pipeline:

```
pattern (str)
    │
    ▼  parser.rs
   AST  (ast.rs)
    │
    ▼  compile.rs
  VM program  (Vec<Inst>)
    │
    ▼  vm.rs
  match result
```

No external dependencies are used.

---

## Source layout

| File | Purpose |
|------|---------|
| `src/lib.rs` | Public API: `Regex`, `Match`, `Captures`, `FindIter`, `CapturesIter` |
| `src/ast.rs` | AST node types produced by the parser |
| `src/parser.rs` | Recursive-descent parser: `&str` → `(Node, named_groups)` |
| `src/compile.rs` | Compiler: `Node` → `Vec<Inst>` + `Vec<CharSet>` |
| `src/vm.rs` | Backtracking executor: `Vec<Inst>` × `&str` → match |
| `src/charset.rs` | Character-property helpers (POSIX, Unicode, shorthands) |
| `src/casefold.rs` | Runtime Unicode full case folding: `case_fold(ch) → CaseFold` |
| `src/casefold_trie.rs` | Compile-time case-fold expansion: `fold_seq_to_trie`, `charset_to_bytetrie` |
| `src/general_category.rs` | Unicode General Category: `get_general_category(ch) → GeneralCategory` |
| `src/bytetrie.rs` | Immutable byte-trie data structure used for case-fold matching |
| `src/error.rs` | `Error` enum (`Parse`, `Compile`) |
| `src/data/casefold_data.rs` | Pre-generated case fold tables (from `data/CaseFolding.txt`) |
| `src/data/general_category_data.rs` | Pre-generated GC range table (from `data/extracted/DerivedGeneralCategory.txt`) |
| `src/bin/oniai.rs` | `grep`-like CLI binary |

### Unicode data files and generator

The `src/data/` tables are pre-generated from the Unicode Character Database
and committed to the repository so that builds require no network access.

| Path | Contents |
|------|----------|
| `data/CaseFolding.txt` | Unicode 17.0.0 case folding data |
| `data/extracted/DerivedGeneralCategory.txt` | Unicode 17.0.0 General Category data |
| `scripts/fetch_unicode_data.sh` | Downloads both files from unicode.org |
| `scripts/gen_unicode_tables/` | Standalone Rust binary; reads `data/` and writes `src/data/` |

To regenerate after a Unicode update:

```sh
sh scripts/fetch_unicode_data.sh [VERSION]
cargo run --manifest-path scripts/gen_unicode_tables/Cargo.toml
```

`build.rs` is intentionally trivial (no generation); the tables are plain Rust
source files that can be read and debugged directly.

---

## Code style

All source code is formatted with **`rustfmt`** (stable defaults) and must
pass **`cargo clippy --tests`** with zero warnings.  Run both before
committing:

```sh
cargo fmt
cargo clippy --tests
```

Notable lint decisions carried in the source:

| Annotation | Location | Reason |
|------------|----------|--------|
| `#[allow(dead_code)]` | `Inst::Ret`, `GroupRef::RelativeFwd`, `NamedCapture::name`, `Compiler::base_flags`, `CompiledProgram::subexp_starts` | Planned for future use; suppressed rather than removed |
| `#[allow(clippy::too_many_arguments)]` | `exec_lookaround`, `check_inner_in_range` | Internal helpers that take many closely-related parameters; refactoring would obscure the algorithm |

---

## Parser (`parser.rs`)

Entry point:

```rust
pub fn parse(pattern: &str) -> Result<(Node, Vec<(String, u32)>), Error>
```

The parser is a hand-written recursive-descent parser.  It walks the pattern
byte-by-byte via a `Parser` struct that holds the input slice and the current
position.

### Grammar overview

```
alternation  = concat ('|' concat)*
concat       = quantified*
quantified   = atom quantifier?
atom         = literal | '.' | '\' escape | '[' charclass ']' | '(' group ')'
```

### Group syntax

| Syntax | Meaning |
|--------|---------|
| `(...)` | Numbered capturing group |
| `(?:...)` | Non-capturing group |
| `(?<name>...)` / `(?'name'...)` | Named capturing group |
| `(?>...)` | Atomic group |
| `(?=...)` / `(?!...)` | Positive/negative lookahead |
| `(?<=...)` / `(?<!...)` | Positive/negative lookbehind |
| `(?~...)` | Absence operator |
| `(?(n)yes\|no)` | Conditional on group *n* |
| `(?imxa-imxa)` / `(?imxa-imxa:...)` | Inline flags |
| `\g<name>` / `\g<n>` / `\g<0>` | Subexpression call |
| `\K` | Keep (reset match start) |

### Inline flags

An isolated flag group `(?i)` (no `:` subexpression) sets flags for the *rest*
of the enclosing group.  The parser detects this by checking whether
`InlineFlags { node: Empty }` was returned, then re-parses the remaining atoms
with the updated `Flags` value and wraps the result in an `InlineFlags` node so
the compiler can apply the correct flags.

### Character classes

`[...]` supports:
- Single chars and ranges (`a-z`)
- POSIX brackets (`[:alpha:]`)
- Shorthands (`\w`, `\d`, `\s`, `\h` and their uppercase negations)
- Unicode properties (`\p{Letter}`)
- Nested classes (`[a[b-z]]`)
- Intersection (`[A&&B]`) — stored in `CharClass.intersections`
- Negation (`[^...]`)

---

## AST (`ast.rs`)

The central type is `Node`, an enum with one variant per construct:

```
Node::Empty | Literal(char) | AnyChar | CharClass(_) | Shorthand(_)
     | UnicodeProp { name, negate } | Anchor(_) | Concat(Vec<Node>)
     | Alternation(Vec<Node>) | Quantifier { node, range, kind }
     | Capture { index, node, flags } | NamedCapture { name, index, node, flags }
     | Group { node, flags } | Atomic(_) | LookAround { dir, pol, node }
     | Keep | BackRef { target, level } | SubexpCall(_) | InlineFlags { flags, node }
     | Absence(_) | Conditional { cond, yes, no }
```

Supporting types: `Flags`, `FlagMod`, `QuantRange`, `QuantKind`, `ClassItem`,
`Shorthand`, `PosixClass`, `CharClass`, `Condition`, `GroupRef`, `AnchorKind`,
`LookDir`, `LookPol`.

---

## Compiler (`compile.rs`)

Entry point:

```rust
pub fn compile(node: &Node, named_groups: Vec<(String, u32)>, opts: CompileOptions)
    -> Result<CompiledProgram, Error>
```

The compiler walks the AST and emits a flat `Vec<Inst>`.  Jump targets that are
not yet known are patched in a second pass via `patch_jump` / `patch_no_jump`.

### Instruction set (`Inst`)

| Instruction | Semantics |
|-------------|-----------|
| `Match` | Report success at current position |
| `Char(c)` | Match literal `c` at `pos`; advance `pos` |
| `AnyChar(dotall)` | Match any char at `pos`; if `!dotall` reject `\n` |
| `Class(idx, ic)` | Match char against `charsets[idx]` at `pos`; if `ic`, fold char before lookup |
| `AltTrie(idx)` | Match one of multiple literal strings via a ByteTrie at `tries[idx]`; advance `pos` |
| `CharBack(c)` | Match `c` ending at `pos`; decrement `pos` (lookbehind) |
| `AnyCharBack(dotall)` | Match any char ending at `pos`; decrement `pos` |
| `ClassBack(idx, ic)` | Charset match ending at `pos`; decrement `pos` |
| `AltTrieBack(idx)` | ByteTrie match ending at `pos`; decrement `pos` (lookbehind) |
| `Anchor(kind, flags)` | Zero-width assertion (`^`, `$`, `\b`, `\A`, `\z`, …) |
| `Jump(pc)` | Unconditional jump |
| `Fork(alt)` | **Greedy** branch: try `pc+1` first, retry at `alt` on failure |
| `ForkNext(alt)` | **Lazy** branch: try `alt` first, retry at `pc+1` on failure |
| `Save(slot)` | Record current position into capture slot |
| `KeepStart` | Reset the effective match start (`\K`) |
| `BackRef(n, ic, level)` | Match same text as group *n* |
| `BackRefRelBack(n, ic)` | *(never emitted)* relative backref resolved at compile time to `BackRef` |
| `Call(pc)` | Push `pc+1` onto call stack, jump to `pc` |
| `Ret` | Pop call stack and jump to saved address |
| `RetIfCalled` | If call stack non-empty: pop and jump; else fall through |
| `AtomicStart(end)` | Push `AtomicBarrier` fence; body runs inline (no sub-exec) |
| `AtomicEnd` | Drain backtrack stack to nearest `AtomicBarrier` (commit) |
| `LookStart { … }` | Execute lookaround body; see below |
| `LookEnd` | Terminates lookaround body |
| `CheckGroup { slot, yes, no }` | Conditional on whether group matched |
| `AbsenceStart(end)` | Absence operator; see below |
| `AbsenceEnd` | Terminates absence inner program |

### Quantifiers

- `*`, `+`, `?` and `{n,m}` in greedy / lazy / possessive flavours.
- Greedy repetition uses `Fork`; lazy uses `ForkNext`; possessive uses
  `AtomicStart`.
- For `{n,m}`: the mandatory `n` copies are emitted inline; the optional
  `m-n` copies use a chain of `Fork` instructions.

### Subexpression calls

`\g<name>` / `\g<n>` emits `Call(target_pc)` where `target_pc` is the PC of
the `Save(slot_open)` instruction for that group.  Because the group may not
have been compiled yet, the target is stored as a *pending call* and
backfilled after the whole program is assembled.

Relative subexpression calls (`\g<-n>`, `\g<+n>`) are resolved at compile
time using the `current_group` counter (1-based index of the innermost
enclosing capture group at the call site).  Relative backreferences
(`\k<-n>`) are resolved the same way and emitted as ordinary `BackRef`
instructions.

Every capturing group ends with `RetIfCalled`: in normal (non-called) flow the
call stack is empty and execution falls through; when the group was entered via
`Call`, `RetIfCalled` pops the return address and resumes at the call site.

### Lookaround

Lookahead bodies are compiled in **forward** mode (normal instructions).
Lookbehind bodies are compiled in **backward** mode using the `*Back`
instruction variants.  In backward mode:

- `Concat` children are emitted in **reverse** order.
- Character-matching instructions use `CharBack` / `AnyCharBack` / etc., which
  read the char *ending* at `pos` and then **decrement** `pos`.
- `Capture` / `NamedCapture` swap the `Save` slot order: `Save(close)` is
  emitted before the body and `Save(open)` after, so that slots are populated
  with the correct (start < end) positions even though execution moves backward.

At run time `exec_lookaround` simply runs the body from the outer `pos`.
For lookahead the body advances `pos`; for lookbehind the body decrements it.
Success is `exec(...).is_some()`.  No position scanning is needed; there is no
fixed-width restriction on lookbehind.

### Absence operator `(?~X)`

`(?~X)` matches the *longest* string at the current position that does **not
contain** X as a substring anywhere within it.

The VM instruction is `AbsenceStart(inner_end_pc)` followed by the inner
program for X, followed by `AbsenceEnd`.  At runtime:

1. All candidate end positions (current position … end-of-text) are enumerated.
2. For each candidate (longest first) `check_inner_in_range` verifies that X
   does not match starting at *any* offset within the candidate range.
3. Valid candidates are tried in order with full backtracking (Fork-style): if
   the outer continuation fails with a longer absence match, the engine retries
   with a shorter one.

---

## VM (`vm.rs`)

### Data structures

```
State {
    slots:      Vec<Option<usize>>,   // capture slot pairs (open/close byte offsets)
    keep_pos:   Option<usize>,        // \K position
    call_stack: Vec<usize>,           // return addresses for \g<...> calls
}

Ctx<'t> {
    prog:         &[Inst],
    charsets:     &[CharSet],
    match_tries:  &[Option<ByteTrie>],  // parallel to prog; precomputed for FoldSeq/Class(ic=true)
    text:         &str,
    search_start: usize,              // for \G anchor
    use_memo:     bool,               // false when pattern has BackRef/CheckGroup
}

Bt (backtrack stack entry, one of):
    Retry    { pc, pos, slots, keep_pos, call_stack }  // saved state to restore on failure
    AtomicBarrier                                       // atomic group fence (see below)
    MemoMark { fork_pc, fork_pos }                     // memoization sentinel (see below)

MemoState {                           // shared across all exec() calls in one find()
    fork_failures: HashMap<u64, usize>,    // (pc,pos) → min atomic_depth of known failure
    look_results:  HashMap<u64, LookCacheEntry>,  // (lk_pc,pos) → cached lookaround outcome
}

LookCacheEntry (one of):
    BodyMatched    { slot_delta: Vec<(usize,Option<usize>)>, keep_pos_delta: Option<Option<usize>> }
    BodyNotMatched
```

### Execution model

`exec(ctx, start_pc, start_pos, state, depth, memo) → Option<usize>`

The function runs a single `'vm: loop` over instructions — no Rust recursion
for ordinary backtracking.  An explicit `bt: Vec<Bt>` stack drives backtracking.
A local `atomic_depth: usize` counter tracks how many atomic-group barriers are
currently live on the backtrack stack:

- **`Fork(alt)`** — if `use_memo`: check `fork_failures` at `(pc, pos)`; if a
  failure entry exists with `stored_depth ≤ atomic_depth`, invoke `fail!()`.
  Otherwise push `Bt::MemoMark` then `Bt::Retry { pc: alt, … }`, advance to `pc+1`.
- **`ForkNext(alt)`** — symmetric lazy version.
- **`fail!()`** macro — call `do_backtrack`, which pops the top `Bt` entry:
  - `Bt::Retry`: restore state, continue.
  - `Bt::MemoMark { fork_pc, fork_pos }`: if `use_memo`, record
    `(fork_pc, fork_pos) → min(stored, atomic_depth)` in `fork_failures`,
    then continue popping.
  - `Bt::AtomicBarrier`: decrement `atomic_depth`, skip (transparent), continue
    popping.
  - Empty stack: return `None`.

Backtracking depth is limited only by heap memory, not by the Rust call stack.

#### Memoization (Algorithms 5–7 of Fujinami & Hasuo 2024)

Implements the memoization framework from:
> Fujinami, H. & Hasuo, I. (2024).  "Efficient Matching with Memoization for
> Regexes with Look-around and Atomic Grouping."  arXiv:2401.12639.

A single `MemoState` is created in `find()` and shared across every `exec()`
invocation — including lookaround sub-executions and different search start
positions.  The state has two tables:

##### Algorithm 5 — Fork-state failure memo (`fork_failures`)

When both alternatives of a `Fork`/`ForkNext` at `(pc, pos)` are exhausted,
the `(pc, pos)` pair is recorded in `fork_failures`.  Future visits
short-circuit immediately, bounding Fork-state work to
**O(|prog| × |text|)**.  This reduces `(a?)^n a^n` from O(2^n) to O(n²)
(~2,600× faster at n=20 in practice).

##### Algorithm 7 — Depth-tagged failures (atomic groups)

A failure recorded at `atomic_depth = j` means "both alternatives fail in an
environment where at least `j` atomic barriers are live."  Failures under more
constraints cannot be reused in less-constrained contexts:

- Failure at depth `j` is reused when current `atomic_depth ≥ j`.
- `fork_failures` stores the **minimum** depth seen for each `(pc, pos)`.
- `AtomicStart` increments `atomic_depth`; `AtomicEnd` (success path) and
  `Bt::AtomicBarrier` skip (failure path) each decrement it.

##### Algorithm 6 — Lookaround result cache (`look_results`)

Without this, the same `LookStart` at `(lk_pc, pos)` could be re-evaluated on
every outer backtracking path, giving exponential time for patterns like
`(a|ε)^n (?=complex_body)`.

`look_results` maps `(lk_pc, pos)` to `LookCacheEntry`:
- **Cache hit**: re-apply the cached outcome immediately.  For `BodyMatched` with
  a positive lookahead, the stored `slot_delta` (index/value pairs that changed)
  is replayed onto `state.slots`.  Only the *delta* is stored — not the full slot
  vector — so re-application is correct even when outer captures differ.
- **Cache miss**: run the sub-execution, record the result (including delta), proceed.

##### When memoization is disabled

`use_memo` is `false` (all memo operations skipped) when the compiled program
contains any of:

| Instruction | Reason |
|-------------|--------|
| `BackRef` / `BackRefRelBack` | Fork outcome depends on the captured text, not just `(pc, pos)` |
| `CheckGroup` | Branches on whether an outer capture group matched; lookaround result depends on outer capture state |

#### Atomic groups

`AtomicStart` increments `atomic_depth`, pushes a `Bt::AtomicBarrier`, and
execution continues inline (no sub-call).  When `AtomicEnd` is reached the VM
**commits** by draining all `Bt` entries back to (and including) the innermost
`AtomicBarrier`, decrementing `atomic_depth` once.  `MemoMark` entries inside
the body are discarded during this drain (body succeeded — no failures to record).

If the body fails before reaching `AtomicEnd`, normal backtracking consumes the
body's internal `Bt` entries one by one until the `Bt::AtomicBarrier` is
encountered; it is silently skipped and `atomic_depth` is decremented, then
backtracking continues to entries that existed before the atomic group started.

#### Lookarounds

`LookStart` first checks `look_results` for a cached outcome.  On a cache miss
it calls `exec_lookaround`, which runs the body in an **isolated sub-execution**
(`exec` with a cloned `State`, depth+1, and the **shared** `MemoState`).

The result (match/no-match) is cached and then used to continue or invoke
`fail!()` in the outer loop.  For positive lookarounds the sub-state (captures)
is merged back into the outer state on success; on a cache hit, only the stored
`slot_delta` is replayed.

`LookEnd` terminates the sub-execution by returning `Some(pos)` from the inner
`exec` call.

#### Absence operator

`AbsenceStart` collects all valid end positions (where the inner pattern does
not appear anywhere in `[start..end]`) via `check_inner_in_range`, then pushes
the shorter alternatives as `Bt::Retry` entries onto the backtrack stack and
jumps to the longest candidate.  If the outer continuation fails, normal
backtracking restores from the next pushed entry — no extra recursion.

`AbsenceEnd` terminates the absence inner-pattern sub-execution (called from
`check_inner_in_range`) by returning `Some(pos)`.

#### Depth limit

`depth` is incremented only for lookaround and absence sub-executions, and is
capped at `MAX_DEPTH = 100`.  It guards against pathological nesting of
lookarounds inside lookarounds, not ordinary backtracking.  The `call_stack`
depth (subexpression call recursion via `\g<...>`) is independently capped at
`MAX_CALL_DEPTH = 200`.

### Case-insensitive matching

#### Compile-time byte trie (fast path)

At `Regex::new()` time, `build_match_tries` constructs a `ByteTrie` for each
`FoldSeq`, `FoldSeqBack`, and case-insensitive `Class`/`ClassBack` instruction:

- **`FoldSeq(chars)`** → `fold_seq_to_trie(&chars)` (from `src/casefold_trie.rs`):
  enumerates all Unicode codepoints whose `case_fold()` produces `chars` (or
  some sequence of codepoints that together produce `chars`), then inserts their
  UTF-8 encodings.  For example, `FoldSeq(['s'])` produces a trie that accepts
  `"s"`, `"S"`, `"ſ"` (U+017F), `"K"` (Kelvin U+212A), etc.
- **`Class(idx, true)`** → `charset_to_bytetrie`: scans `cs.ranges` (the
  compiled inversion list) and inserts all matching codepoints' UTF-8 encodings.
  Since all charsets are now pure inversion lists, there are no "complex" charsets;
  every `Class` can get a ByteTrie for case-insensitive matching.
- The resulting `Vec<Option<ByteTrie>>` is stored in `CompiledRegex::match_tries`
  and passed to `Ctx` as `match_tries: &[Option<ByteTrie>]`.

The trie construction uses the static `SIMPLE_CASE_FOLDS` and `MULTI_CASE_FOLDS`
tables from `src/data/casefold_data.rs` (pre-generated by
`scripts/gen_unicode_tables` from `data/CaseFolding.txt`).  Each table has
≈ 1 400 entries — a >700× reduction compared to scanning all 1.1 M Unicode
codepoints at compile time.

At match time in `exec()`, when `match_tries[pc]` is `Some(trie)` the engine
calls `trie.advance(text.as_bytes(), pos)` — a plain byte-walk with no UTF-8
decoding and no `case_fold()` calls.  The trie returns the end position of the
longest accepted prefix, or `None` to trigger backtracking.

#### Scalar fallback

When no ByteTrie is available (backreference patterns, or the
JIT path), matching falls back to:

- **`Char(c)`** (case-insensitive matching is now handled by `FoldSeq` at compile
  time; `Char` no longer carries an `ic` flag).
- **`FoldSeq(chars)`** (trie absent): `fold_advance(text, pos, chars)` advances
  char-by-char comparing fold outputs — zero allocation, O(match_len).
- **`BackRef`** matching (strings): `caseless_advance(text, pos, pattern)` folds
  both strings one codepoint at a time, handling multi-codepoint folds such as
  `ß` ↔ `ss` and `ﬁ` ↔ `fi`.

#### Start-position scanner

`StartStrategy::CaselessPrefix { folded, non_ascii_first_bytes }` is used when
the pattern begins with a `FoldSeq` instruction.  The scanner:
1. Pre-computes ASCII case variants of `folded[0]` and uses SIMD `str::find`
   for each — fast for ASCII-dominant text.
2. Uses `non_ascii_first_bytes` (derived from the ByteTrie root transitions) to
   scan for non-ASCII starting bytes using raw `&[u8]` byte comparisons —
   avoids any `case_fold()` calls in the scanner hot path.
3. Pre-filters each candidate position with `fold_advance` before launching the
   full NFA.



`CompiledRegex::find(text, start_pos)` applies two compile-time pre-filters
before the main loop:

1. **`required_char`** — if the last mandatory case-sensitive `Char` before
   `Match` is not present anywhere in `text[start_pos..]`, return `None`
   immediately (O(n) `memchr` scan; skipped for `Anchored` patterns).
2. **`StartStrategy`** — choose how to advance through candidate start positions:
   - `Anchored`: try only `start_pos` once.
   - `LiteralPrefix(s)`: use `memchr::memmem::find` to jump to each occurrence.
   - `CaselessPrefix { folded, non_ascii_first_bytes }`: use SIMD `str::find`
     for ASCII variants of `folded[0]`, plus raw byte scans for non-ASCII first
     bytes; pre-filter each candidate with `fold_advance`.
   - `LiteralSet(lits)`: use `memchr::memmem::find` for each literal; take the
     leftmost candidate.
   - `AsciiClassStart { ascii_bits, can_match_non_ascii }`: the first instruction
     is `Class`; use the charset's precomputed 128-bit ASCII bitmap to skip
     positions that can never start a match without calling `exec`.
   - `FirstChars(set)`: use `memchr`/`memchr2`/`memchr3` for 1–3 pure-ASCII
     chars, `str::find(char)` otherwise.
   - `Anywhere`: try every byte-aligned position.

---

## Public API (`lib.rs`)

```rust
Regex::new(pattern) -> Result<Regex, Error>
Regex::is_match(text) -> bool
Regex::find(text) -> Option<Match>
Regex::find_iter(text) -> FindIter          // non-overlapping iterator
Regex::captures(text) -> Option<Captures>
Regex::captures_iter(text) -> CapturesIter  // non-overlapping iterator

Match::as_str() / start() / end() / range()
Captures::get(n)     // 0 = whole match
Captures::name(s)    // named group
```

`FindIter` / `CapturesIter` advance past zero-length matches by stepping one
UTF-8 code point forward to avoid infinite loops.

---

## Limitations and known gaps

- **No NFA / DFA compilation**: the engine is a pure backtracking interpreter;
  exponential worst-case exists for ambiguous patterns on adversarial inputs
  (mitigated for many patterns by the memoization framework).
- **JIT compilation** (optional, behind the `jit` feature flag) is planned to
  replace the interpreter loop with native machine code for eligible patterns;
  see [`doc/JIT.md`](JIT.md) for the full design and implementation plan.
