# Oniai — Design Document

Oniai is a pure-Rust regular expression engine compatible with
[Onigmo](https://github.com/k-takata/Onigmo) (the regex library used by Ruby).
It follows a *compile-then-execute* pipeline with an explicit IR layer:

```
pattern (str)
    │
    ▼  parser.rs
   AST  (ast.rs)
    │
    ▼  ir/build.rs
  IrProgram  ← explicit CFG; basic blocks + terminators  (ir/)
    │
    ├─▶  ir/pass/   ← optimization passes (liveness, DCE, …)
    │
    ├─▶  ir/lower.rs   ─▶  Vec<Inst>  ─▶  vm.rs
    │                                        │
    │                                        ▼  jit/
    │                                     native code (optional)
    │
    └─▶  ir/jit.rs     ─▶  Cranelift IR  ─▶  native code (planned)
```

No external dependencies are used by the library core; the optional `jit`
feature adds Cranelift dependencies.

---

## Source layout

| File | Purpose |
|------|---------|
| `src/lib.rs` | Public API: `Regex`, `Match`, `Captures`, `FindIter`, `CapturesIter` |
| `src/ast.rs` | AST node types produced by the parser |
| `src/parser.rs` | Recursive-descent parser: `&str` → `(Node, named_groups)` |
| `src/ir/mod.rs` | IR types: `IrProgram`, `IrRegion`, `IrBlock`, `IrStmt`, `IrTerminator` |
| `src/ir/build.rs` | IR builder: `Node` → `IrProgram` |
| `src/ir/lower.rs` | IR lowering: `IrProgram` → `Vec<Inst>` + `Vec<CharSet>` (compatibility path) |
| `src/ir/verify.rs` | Debug-mode IR invariant checker |
| `src/ir/pass/` | Optimization passes: DCE, block merge, capture liveness, fork guard, span detection |
| `src/compile.rs` | Legacy compiler entry point: delegates to `ir/build.rs` + `ir/lower.rs` |
| `src/vm.rs` | Backtracking executor: `Vec<Inst>` × `&str` → match |
| `src/charset.rs` | Character-property helpers (POSIX, Unicode, shorthands); binary-searches pre-generated static range tables |
| `src/casefold.rs` | Runtime Unicode full case folding: `case_fold(ch) → CaseFold` |
| `src/casefold_trie.rs` | Compile-time case-fold expansion: `fold_seq_to_trie`, `charset_to_bytetrie` |
| `src/general_category.rs` | Unicode General Category: `get_general_category(ch) → GeneralCategory` |
| `src/bytetrie.rs` | Immutable byte-trie data structure used for case-fold matching |
| `src/error.rs` | `Error` enum (`Parse`, `Compile`) |
| `src/data/casefold_data.rs` | Pre-generated case fold tables (from `data/CaseFolding.txt`) |
| `src/data/general_category_data.rs` | Pre-generated GC range table (from `data/extracted/DerivedGeneralCategory.txt`) |
| `src/data/unicode_prop_ranges_data.rs` | Pre-generated property range tables (from `data/DerivedCoreProperties.txt`, `data/PropList.txt`, `data/extracted/DerivedGeneralCategory.txt`) |
| `src/data/script_data.rs` | Pre-generated Script and Script_Extensions range tables (from `data/Scripts.txt`, `data/ScriptExtensions.txt`, `data/PropertyValueAliases.txt`) |
| `src/bin/oniai.rs` | `grep`-like CLI binary |

### Unicode data files and generator

The `src/data/` tables are pre-generated from the Unicode Character Database
and committed to the repository so that builds require no network access.

| Path | Contents |
|------|----------|
| `data/CaseFolding.txt` | Unicode 17.0.0 case folding data |
| `data/extracted/DerivedGeneralCategory.txt` | Unicode 17.0.0 General Category data |
| `data/DerivedCoreProperties.txt` | Unicode 17.0.0 derived core properties (Alphabetic, Uppercase, Lowercase, Math, …) |
| `data/PropList.txt` | Unicode 17.0.0 property list (White_Space, Hex_Digit, …) |
| `data/Scripts.txt` | Unicode 17.0.0 Script property assignments |
| `data/ScriptExtensions.txt` | Unicode 17.0.0 Script_Extensions property assignments |
| `data/PropertyValueAliases.txt` | Unicode 17.0.0 property value aliases (used to resolve Script abbreviations) |
| `scripts/fetch_unicode_data.sh` | Downloads all seven files from unicode.org |
| `scripts/gen_unicode_tables/` | Standalone Rust binary; reads `data/` and writes `src/data/` |

The `data/` directory is **git-ignored**; regenerate its contents by running:

```sh
sh scripts/fetch_unicode_data.sh [VERSION]
```

To regenerate the `src/data/` source files after updating `data/`:

```sh
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

## IR Builder and Compiler (`ir/build.rs`, `compile.rs`)

The compiler pipeline now passes through the IR layer:

1. `ir/build.rs` — `IrBuilder` walks the AST and produces an `IrProgram` (see
   [`doc/IR_DESIGN.md`](IR_DESIGN.md) for the full IR specification).
2. `ir/pass/` — optimization passes run on the `IrProgram` (capture liveness,
   dead block elimination, fork guard propagation, span detection).
3. `ir/lower.rs` — `IrLower` converts the optimized `IrProgram` back to a flat
   `Vec<Inst>` for the existing interpreter and JIT.

The public entry point (unchanged):

```rust
pub fn compile(node: &Node, named_groups: Vec<(String, u32)>, opts: CompileOptions)
    -> Result<CompiledProgram, Error>
```

Internally `compile` calls `IrBuilder::build`, runs the pass pipeline, then
calls `IrLower::lower`.  Jump targets that are not yet known are patched in a
second pass via `patch_jump` / `patch_no_jump` in the lowering step.

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
| `RepeatInit { slot }` | Zero `counters[slot]` — begins a counter-based exact-repetition loop |
| `RepeatNext { slot, count, body_pc }` | Increment `counters[slot]`; if `< count` jump to `body_pc`; else fall through |
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
- For `{n,m}`: the mandatory `n` copies are emitted inline for small `n`; the
  optional `m-n` copies use a chain of `Fork` instructions.
- **Counter-based loop** (`{n}` or `{n,m}` with `n ≥ 4`): instead of
  duplicating the body `n` times (O(n·|body|) program size), the compiler
  allocates a counter slot and emits:
  ```
  RepeatInit { slot }          ← zeroes counter[slot]
  <body>                       ← body_pc
  RepeatNext { slot, count: n, body_pc }  ← loop until counter reaches n
  ```
  Counter slots are a `Vec<u32>` local to `exec()` (not part of `State`), so
  lookaround sub-executions always start with zeroed counters.

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
    prog:                &[Inst],
    charsets:            &[CharSet],
    match_tries:         &[Option<ByteTrie>],  // parallel to prog; precomputed for FoldSeq/Class(ic=true)
    text:                &str,
    search_start:        usize,               // for \G anchor
    use_memo:            bool,                // false when pattern has BackRef/CheckGroup
    num_repeat_counters: usize,               // number of counter slots for {n} loops
}

Bt (backtrack stack entry, one of):
    Retry    { pc, pos, slots, keep_pos, call_stack }       // saved state to restore on failure
    AtomicBarrier                                            // atomic group fence (see below)
    MemoMark { fork_pc, fork_pos, counters_snapshot }       // memoization sentinel (see below)

MemoState {                           // shared across all exec() calls in one find()
    fork_failures:         HashMap<u64, u8>,             // (pc,pos) → depth bitmask (no-counter fast path)
    fork_failures_counted: HashMap<(u64,Vec<u32>), u8>,  // (pc,pos,counters) → depth bitmask (counter path)
    look_results:          HashMap<u64, LookCacheEntry>, // (lk_pc,pos) → cached lookaround outcome
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

- **`Fork(alt)`** — if `use_memo`:
  - *No-counter path* (`num_repeat_counters == 0`): check `fork_failures` at
    `memo_key(pc, pos)`; on a hit, invoke `fail!()`.  Push `Bt::MemoMark {
    counters_snapshot: [] }` then `Bt::Retry`.
  - *Counter path* (`num_repeat_counters > 0`): check `fork_failures_counted`
    at `(memo_key(pc, pos), counters.clone())`; on a hit, invoke `fail!()`.
    Push `Bt::MemoMark { counters_snapshot: counters.clone() }` then `Bt::Retry`.
  - If `!use_memo`: push only `Bt::Retry`.
- **`ForkNext(alt)`** — symmetric lazy version.
- **`RepeatInit { slot }`** — set `counters[slot] = 0`, advance.
- **`RepeatNext { slot, count, body_pc }`** — increment `counters[slot]`; if
  `counters[slot] < count` jump to `body_pc`; else fall through.
- **`fail!()`** macro — call `do_backtrack`, which pops the top `Bt` entry:
  - `Bt::Retry`: restore state, continue.
  - `Bt::MemoMark { fork_pc, fork_pos, counters_snapshot }`: if `use_memo`,
    record the failure keyed by `counters_snapshot` (empty → `fork_failures`,
    non-empty → `fork_failures_counted`), then continue popping.
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

##### Algorithm 5 — Fork-state failure memo (`fork_failures` / `fork_failures_counted`)

When both alternatives of a `Fork`/`ForkNext` at `(pc, pos)` are exhausted,
the state is recorded so future visits can short-circuit immediately, bounding
Fork-state work to **O(|prog| × |text|)**.  This reduces `(a?)^n a^n` from
O(2^n) to O(n²) (~2,600× faster at n=20 in practice).

The key includes the repeat-counter state when the program has counter-based
`{n}` loops (`num_repeat_counters > 0`), because the same `(pc, pos)` pair
visited under different counter values can have different outcomes.  A
**two-map design** avoids overhead for the common case:

- **`fork_failures: HashMap<u64, u8>`** — used when `num_repeat_counters == 0`.
  Plain `memo_key(pc, pos)` key; no `Vec` allocation anywhere on the fast path.
- **`fork_failures_counted: HashMap<(u64, Vec<u32>), u8>`** — used when
  `num_repeat_counters > 0`.  Key includes a snapshot of the counter vector.

`counters` are a `Vec<u32>` **local to `exec()`** (not part of `State`), so
lookaround sub-executions always start with fresh zeroed counters; `(lk_pc, pos)`
remains a valid cache key for `look_results` regardless of the outer counter state.

##### Algorithm 7 — Depth-tagged failures (atomic groups)

A failure recorded at `atomic_depth = j` means "both alternatives fail in an
environment where at least `j` atomic barriers are live."  Failures under more
constraints cannot be reused in less-constrained contexts:

- Failure at depth `j` is reused when current `atomic_depth ≥ j`.
- Both `fork_failures` and `fork_failures_counted` store a **bitmask** where
  bit `d` is set when a failure was recorded at `atomic_depth = d` (capped at
  bit 7); a hit fires when any bit `0..=atomic_depth` is set.
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
- **JIT compilation** (optional, behind the `jit` feature flag) compiles
  eligible patterns to native code at `Regex::new()` time; see
  [`doc/JIT.md`](JIT.md) for the full design.  Patterns containing
  backreferences, subexpression calls, or the absence operator fall back to the
  interpreter transparently.
- **IR layer**: the compiler now produces an `IrProgram` (explicit CFG) before
  lowering to `Vec<Inst>`; see [`doc/IR_DESIGN.md`](IR_DESIGN.md) for the IR
  specification and planned optimization passes.
