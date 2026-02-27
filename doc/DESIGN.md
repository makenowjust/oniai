# Aigumo — Design Document

Aigumo is a pure-Rust regular expression engine compatible with
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
| `src/error.rs` | `Error` enum (`Parse`, `Compile`) |

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
| `Char(c, ic)` | Match literal `c`; `ic` = ignore-case |
| `AnyChar(dotall)` | Match any char; if `!dotall` reject `\n` |
| `Class(idx, ic)` | Match char against `charsets[idx]` |
| `Shorthand(sh, ar)` | Match `\w`/`\d`/`\s`/`\h`… |
| `Prop(name, neg)` | Match Unicode property |
| `Anchor(kind, flags)` | Zero-width assertion (`^`, `$`, `\b`, `\A`, `\z`, …) |
| `Jump(pc)` | Unconditional jump |
| `Fork(alt)` | **Greedy** branch: try `pc+1` first, retry at `alt` on failure |
| `ForkNext(alt)` | **Lazy** branch: try `alt` first, retry at `pc+1` on failure |
| `Save(slot)` | Record current position into capture slot |
| `KeepStart` | Reset the effective match start (`\K`) |
| `BackRef(n, ic, level)` | Match same text as group *n* |
| `BackRefRelBack(n, ic)` | Relative backward backreference |
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

Every capturing group ends with `RetIfCalled`: in normal (non-called) flow the
call stack is empty and execution falls through; when the group was entered via
`Call`, `RetIfCalled` pops the return address and resumes at the call site.

### Lookaround

At compile time the compiler pre-computes the set of possible widths of a
lookbehind body (`compute_widths`) and stores them in `LookStart.behind_lens`.
At run time the VM tries each stored length.

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
    text:         &str,
    search_start: usize,              // for \G anchor
}

Bt (backtrack stack entry, one of):
    Retry { pc, pos, slots, keep_pos, call_stack }  // saved state to restore on failure
    AtomicBarrier                                    // atomic group fence (see below)
```

### Execution model

`exec(ctx, start_pc, start_pos, state, depth) → Option<usize>`

The function runs a single `'vm: loop` over instructions — no Rust recursion
for ordinary backtracking.  An explicit `bt: Vec<Bt>` stack drives backtracking:

- **`Fork(alt)`** — push `Bt::Retry { pc: alt, … }`, advance to `pc+1`.
- **`ForkNext(alt)`** — push `Bt::Retry { pc: pc+1, … }`, jump to `alt`.
- **`fail!()`** macro — pop the top `Bt` entry and restore state (`Bt::Retry`),
  or skip transparent `Bt::AtomicBarrier` fences, or return `None` if the stack
  is empty.

This means backtracking depth is limited only by heap memory, not by the Rust
call stack.

#### Atomic groups

`AtomicStart` pushes a `Bt::AtomicBarrier` onto the backtrack stack and
execution continues inline (no sub-call).  When `AtomicEnd` is reached the VM
**commits** by draining all `Bt` entries back to (and including) the innermost
`AtomicBarrier` — those entries represented internal alternatives of the body
that are now discarded.  If the body fails before reaching `AtomicEnd`, normal
backtracking consumes the body's internal `Bt::Retry` entries one by one until
the `AtomicBarrier` is encountered; the barrier is silently skipped (treated as
transparent), and backtracking continues to the entries that existed before the
atomic group started.

#### Lookarounds

`LookStart` calls the helper `exec_lookaround`, which runs the body in an
**isolated sub-execution** (`exec` with a cloned `State` and depth+1).  The
result (match/no-match) is used to continue or trigger `fail!()` in the outer
loop; the outer position is unchanged.  For positive lookarounds the sub-state
(captures) is merged back into the outer state on success.

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

### Search loop

`CompiledRegex::find(text, start_pos)` tries `exec` at every byte-aligned
position from `start_pos` to `text.len()` (anchored patterns still try each
start but fail fast on the anchor).  The first success is returned.

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

- **No JIT / NFA compilation**: the engine is a pure backtracking interpreter;
  exponential worst-case exists for ambiguous patterns on adversarial inputs.
- **Lookbehind width**: only patterns with a finite, statically computable set
  of widths are supported (variable-length lookbehind will yield an empty
  `behind_lens` and never match).
- **Relative/forward subexpression calls** (`\g<+n>`, `\g<-n>`) are parsed but
  the VM returns `None` for them (not yet implemented).
- **Unicode case folding**: only single-codepoint lowercasing is used; full
  Unicode case-folding tables are not included.
- **No `ONIG_OPTION_FIND_LONGEST`**: the API always returns the *leftmost*
  match, not the longest.
