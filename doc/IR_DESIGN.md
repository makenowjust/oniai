# Oniai — IR Design

This document describes the design of Oniai's **Intermediate Representation
(IR)** — an explicit control-flow graph that sits between the parsed AST and
the final execution backends (interpreted `Vec<Inst>` and JIT-compiled native
code).

---

## 1. Motivation

The current pipeline goes directly from `AST → Vec<Inst>`.  The flat
instruction stream has served well, but it creates friction for several
important optimizations:

| Problem | Consequence |
|---------|-------------|
| Control flow is implicit (encoded as numeric jump targets) | Predecessor/successor computation requires a scan of the whole `Vec<Inst>`; each optimization pass reconstructs the CFG independently |
| Capture snapshots at `Fork` copy *all* slots unconditionally | Slots that are never read after a backtrack point are saved and restored unnecessarily, dominating allocation cost for complex patterns |
| Lookaround and atomic bodies are inlined into the flat stream | The JIT must locate their boundaries by scanning for sentinel instructions (`LookStart`/`LookEnd`, etc.) |
| Optimization passes (`compute_fork_guards`, `spanify_greedy_loops`) operate on a flat array with ad-hoc pattern matching | Adding new passes requires careful index arithmetic; errors are hard to detect |

The IR solves all of these by providing an explicit CFG with typed
**basic blocks**, **terminators**, and **sub-program regions** on which
standard algorithms (liveness analysis, dead-code elimination, block merging)
operate directly.

---

## 2. Architecture Overview

```
pattern (str)
    │
    ▼  parser.rs
   AST  (ast.rs)
    │
    ▼  ir/build.rs
  IrProgram  ← explicit CFG; basic blocks + terminators
    │
    ├─▶  ir/pass/   ← optimization passes (liveness, DCE, block merge, …)
    │
    ├─▶  ir/lower.rs   ─▶  Vec<Inst>  ─▶  vm.rs / jit/ (compatibility path)
    │
    └─▶  ir/jit.rs     ─▶  Cranelift IR  ─▶  native code (direct IR-JIT path)
```

The compatibility lowering path (`ir/lower.rs → Vec<Inst>`) lets every
optimization developed on the IR feed the existing interpreter and JIT unchanged
during the transition.  Once the IR is stable the interpreter will be updated
to dispatch on `IrBlock` directly.

---

## 3. Types

### 3.1 IDs

```rust
pub type BlockId  = usize;   // index into IrRegion::blocks
pub type RegionId = usize;   // index into IrProgram::regions
```

### 3.2 `IrProgram`

```rust
pub struct IrProgram {
    /// regions[0] is always the main pattern.
    /// Subsequent entries are sub-programs (lookaround bodies, atomic bodies,
    /// absence inner programs, subroutine bodies).
    pub regions:         Vec<IrRegion>,
    pub charsets:        Vec<CharSet>,
    pub alt_tries:       Vec<ByteTrie>,
    pub num_captures:    usize,   // total slot count = 2 × num_groups
    pub num_counters:    usize,   // repeat-counter slots
    pub num_null_checks: usize,   // null-check slots
    pub use_memo:        bool,    // false when pattern contains BackRef/CheckGroup
    pub named_groups:    Vec<(String, u32)>,
}
```

### 3.3 `IrRegion`

A region is a self-contained sub-CFG.  Each region has a typed `kind` that
determines its execution context (outer-caller semantics, position direction,
etc.).

```rust
pub struct IrRegion {
    pub blocks: Vec<IrBlock>,
    pub entry:  BlockId,
    pub kind:   RegionKind,
}

pub enum RegionKind {
    /// Top-level matching program.  Terminates with `IrTerminator::Match`.
    Main,
    /// Lookahead body (forward direction).
    LookAhead { positive: bool },
    /// Lookbehind body (backward direction — uses `*Back` statements).
    LookBehind { positive: bool },
    /// Atomic group body.  On success the parent commits (drains bt stack).
    Atomic,
    /// Absence operator inner program.  Parent checks for non-match.
    Absence,
    /// Named subexpression body — callable via `IrTerminator::Call`.
    Subroutine { group: u32 },
}
```

### 3.4 `IrBlock`

A basic block is a straight-line sequence of `IrStmt`s followed by exactly
one `IrTerminator`.

```rust
pub struct IrBlock {
    pub stmts: Vec<IrStmt>,
    pub term:  IrTerminator,
}
```

### 3.5 `IrStmt` — non-branching instructions

Statements either advance `pos` (matching), check a zero-width assertion, or
write to execution state (capture slots, counters).  Any statement may fail;
on failure control is transferred to the most recent retry point established
by a `Fork` terminator — no explicit failure edges are needed in the block
body.

```rust
pub enum IrStmt {
    // ── Forward matching ────────────────────────────────────────────────────
    MatchChar(char),
    MatchAnyChar     { dotall: bool },
    MatchClass       { id: usize, ignore_case: bool },

    // ── Backward matching (lookbehind bodies) ────────────────────────────────
    MatchCharBack(char),
    MatchAnyCharBack { dotall: bool },
    MatchClassBack   { id: usize, ignore_case: bool },

    // ── Case-fold sequences ─────────────────────────────────────────────────
    MatchFoldSeq    (Vec<char>),   // forward: match chars whose case fold = folded
    MatchFoldSeqBack(Vec<char>),   // backward

    // ── Trie-based literal alternation ──────────────────────────────────────
    /// Match the longest string in `alt_tries[id]` at the current position.
    /// Replaces a Fork chain of plain-string alternatives (O(len) instead of
    /// O(len×k)).  Only emitted when the alternative set is prefix-free.
    MatchAltTrie    (usize),
    MatchAltTrieBack(usize),

    // ── Zero-width assertions ───────────────────────────────────────────────
    CheckAnchor(AnchorKind, Flags),

    // ── Backreferences ──────────────────────────────────────────────────────
    CheckBackRef { group: u32, ignore_case: bool, level: Option<i32> },

    // ── State side-effects (automatically rolled back on backtrack) ──────────
    /// Write current `pos` into capture slot `slot`.
    SaveCapture(usize),
    /// Reset effective match start to `pos` (`\K`).
    KeepStart,
    /// Zero repeat-counter slot `slot`.  Paired with `IrTerminator::CounterNext`.
    CounterInit(usize),
    /// Record `(pos, bt_depth)` in null-check slot `slot`.
    /// Must appear before the `Fork` whose retry entry it gates.
    NullCheckBegin(usize),
}
```

### 3.6 `IrForkCandidate` and `IrGuard` — fork candidates and guards

A fork candidate pairs a **guard** (a zero-width or peek condition) with a
target block.  Guards are purely observational: they check a condition at the
current execution state without permanently advancing `pos` or mutating
captures.  If the guard passes, the candidate's block is tried; if it fails,
the next candidate is tested instead.

```rust
pub struct IrForkCandidate {
    pub guard: IrGuard,
    pub block: BlockId,
}

pub enum IrGuard {
    // ── Single-character peek guards (forward) ──────────────────────────────
    /// True iff `text[pos] == c`.  Does not advance `pos`; the candidate
    /// block is expected to begin with `MatchChar(c)` (lowered to `CharFast`).
    Char(char),
    /// True iff `text[pos]` is any character (respecting `dotall`).
    AnyChar    { dotall: bool },
    /// True iff `charsets[id].matches(text[pos])`.
    Class      { id: usize, ignore_case: bool },

    // ── Single-character peek guards (backward) ─────────────────────────────
    CharBack(char),
    AnyCharBack { dotall: bool },
    ClassBack   { id: usize, ignore_case: bool },

    // ── Zero-width state guards ─────────────────────────────────────────────
    /// True iff the anchor condition holds at `pos`.
    Anchor(AnchorKind, Flags),
    /// True iff capture group `slot / 2 + 1` has matched.
    /// Absorbs `IrTerminator::CheckGroup` — conditional groups compile to a
    /// `Fork` with a `GroupMatched` guard and an `Always` default candidate.
    GroupMatched(usize),
    /// Run the `body` region as a zero-width side-execution (isolated State,
    /// depth+1, shared MemoState).
    ///   Positive: guard is true iff body matches; on success apply the
    ///             capture delta to the outer state.
    ///   Negative: guard is true iff body fails; no state change.
    /// Absorbs `IrTerminator::Look` — lookarounds become guard conditions
    /// on Fork candidates and compose naturally with other guards.
    LookAround { pol: LookPol, dir: LookDir, body: RegionId },

    // ── Unconditional ────────────────────────────────────────────────────────
    /// Always evaluates to true.  Used as the last (default) candidate.
    Always,
}
```

**Guard disjointness** is the critical invariant for optimization.  Two guards
`g1` and `g2` are *disjoint* if no input position can satisfy both.  When the
Guard Analysis pass (Pass 4, §7) proves that all candidates in a `Fork` are
mutually exclusive — including the `Always` default if it is the unique
survivor — the `Fork` is annotated `disjoint: true` and the interpreter/JIT
skips pushing a `Bt::Retry` entry entirely, making the fork possessive.

Examples of provably disjoint guard sets:

| Pattern | Guards | Disjoint? |
|---|---|---|
| `CheckGroup` | `GroupMatched(s)`, `Always` | ✓ always |
| `[0-9]` vs `[a-z]` | `Class(digits)`, `Class(alpha)` | ✓ (disjoint char sets) |
| `a` vs `b` | `Char('a')`, `Char('b')` | ✓ |
| greedy `a*` | `Char('a')`, `Always` | ✗ (Always is a catch-all, not exclusive after a char) |

---

### 3.7 `IrTerminator` — control flow

Each basic block ends with exactly one terminator.  The terminator is the
*only* place where control flow diverges.

```rust
pub enum IrTerminator {
    // ── Final outcomes ──────────────────────────────────────────────────────
    /// The overall pattern matched (main region only).
    Match,
    /// This sub-program succeeded; return to the invoking terminator.
    RegionEnd,

    // ── Unconditional branch ─────────────────────────────────────────────────
    /// Convenience alias for `Fork { candidates: [{ Always, b }], disjoint: true }`.
    /// Kept as a separate variant for lowering efficiency.
    Branch(BlockId),

    // ── N-way fork ───────────────────────────────────────────────────────────
    /// Candidates are evaluated in order.  For each candidate:
    ///   1. Evaluate the guard (peek, zero-width).
    ///   2. Guard passes:
    ///      a. If `disjoint` is false and there are remaining candidates:
    ///         push `Bt::Retry` for those remaining candidates at `pos`.
    ///      b. Jump to `candidate.block`.
    ///   3. Guard fails: try the next candidate immediately (no bt push).
    ///   4. All guards failed: fail (backtrack to previous retry point).
    ///
    /// `disjoint: true` — set by Pass 4 (Guard Analysis) when all guards are
    /// mutually exclusive.  Skips bt push in step 2a entirely; the fork is
    /// possessive.  Subsumes the old `CheckGroup` (always disjoint) and enables
    /// disjoint-alternation optimizations beyond what the old two-way `Fork`
    /// could express.
    ///
    /// `live_slots` — annotated by Pass 3 (Capture Liveness) with the set of
    /// capture slots that must be included in each `Bt::Retry` snapshot.
    ///
    /// Encodes:
    ///   Old `Fork { now, retry, first_char: Some(c) }`:
    ///     Fork { candidates: [{ Char(c), now }, { Always, retry }], disjoint: false }
    ///   Old `CheckGroup { slot, yes, no }`:
    ///     Fork { candidates: [{ GroupMatched(slot), yes }, { Always, no }], disjoint: true }
    ///   Old `Look { pol, dir, body, next }`:
    ///     Fork { candidates: [{ LookAround { pol, dir, body }, next }], disjoint: true }
    ///   Greedy `a*`  → Fork { candidates: [{ Char('a'), body }, { Always, exit }], disjoint: false }
    ///   Lazy   `a*?` → Fork { candidates: [{ Always, exit }, { Char('a'), body }], disjoint: false }
    Fork {
        candidates: Vec<IrForkCandidate>,
        disjoint:   bool,      // set by Guard Analysis pass
        live_slots: BitSet,    // set by Capture Liveness pass
    },

    // ── Possessive spans (loop-based; not guard-based branching) ─────────────
    /// Advance `pos` while `text[pos] == c`; then jump to `exit`.
    /// Possessive — no bt retry point pushed.  Correct because `c` is proved
    /// disjoint from the continuation's first-character set (Pass 5).
    SpanChar  { c: char, exit: BlockId },
    /// Same as `SpanChar` but matches `charsets[id]` at each step.
    SpanClass { id: usize, exit: BlockId },

    // ── Operational terminators ───────────────────────────────────────────────
    // These are kept as explicit variants because their branching decision is
    // inseparable from a structural mutation of the execution model (bt stack,
    // counter slots, or call stack) that goes beyond a simple guard + bt push.

    /// If `pos == saved_pos` (empty match since `NullCheckBegin`): truncate
    /// the bt stack to the saved depth (committing current captures), then
    /// jump to `exit`.  Otherwise jump to `cont`.
    NullCheckEnd { slot: usize, exit: BlockId, cont: BlockId },

    /// Increment `counters[slot]`; if the new value `< count` jump to `body`;
    /// otherwise fall through to `exit`.
    CounterNext { slot: usize, count: u32, body: BlockId, exit: BlockId },

    /// Push `ret` onto the call stack; jump to `target`.
    Call { target: BlockId, ret: BlockId },
    /// If the call stack is non-empty, pop and jump to the saved address.
    /// Otherwise fall through to `fallthrough`.
    RetIfCalled { fallthrough: BlockId },

    /// Run the `body` region.  On success: drain the bt stack to the innermost
    /// `AtomicBarrier` (committing the body's captures) then continue to `next`.
    /// On failure: fail as a unit.
    Atomic { body: RegionId, next: BlockId },
    /// Run the `inner` region for every sub-range of the candidate absence
    /// window; if inner never matches, continue to `next`; otherwise fail.
    Absence { inner: RegionId, next: BlockId },
}
```

---

## 4. Semantic Contract

> **Within a basic block, all statements are assumed to succeed sequentially.
> If any statement fails, the *entire block* fails and the VM backtracks to the
> most recent retry point created by a `Fork` terminator.**

Corollaries:

1. **No intra-block failure edges**: the backtrack stack implicitly handles
   failure propagation; there is no need to model failure edges between
   statements.
2. **Side-effects are automatically rolled back**: because `Bt::Retry` snapshots
   the full `State` (or, in the JIT, an undo log), any `SaveCapture` /
   `CounterInit` / `NullCheckBegin` executed within a failed block is reversed
   without special IR support.
3. **The only way to create a new retry point is a `Fork` terminator**: this
   invariant is checkable by the IR verifier and makes the retry-point structure
   explicit and analysable.  When `Fork.disjoint` is `true`, no retry point is
   pushed at all — the fork is possessive.
4. **`CharFast` is not an IR concept**: the current flat-VM `CharFast`
   instruction is purely a lowering artifact.  In the IR, a character guard
   (`IrGuard::Char(c)`) on a Fork candidate signals that the candidate block
   starts with `MatchChar(c)` that is already gated by the guard; the lowering
   emits `CharFast(c)` instead of `Char(c)` to skip the redundant bounds+match
   check.
5. **Lookarounds are guards, not terminators**: `IrGuard::LookAround` expresses
   a lookaround as a zero-width condition on a fork candidate.  This enables
   lookaheads and character guards to be composed in the same candidate list and
   analysed for disjointness together.

---

## 5. CFG Structure

### 5.1 Successor edges

The successor relation for each terminator (within a single region):

| Terminator | Successors |
|------------|------------|
| `Match` / `RegionEnd` | ∅ (terminal) |
| `Branch(b)` | `{b}` |
| `Fork { candidates, .. }` | `{ c.block \| c ∈ candidates }` — the union of all candidate blocks |
| `SpanChar { exit, .. }` | `{exit}` |
| `SpanClass { exit, .. }` | `{exit}` |
| `NullCheckEnd { exit, cont, .. }` | `{exit, cont}` |
| `CounterNext { body, exit, .. }` | `{body, exit}` |
| `Call { target, .. }` | `{target}` (`ret` is a return-address, not a CFG edge) |
| `RetIfCalled { fallthrough }` | `{fallthrough}` + dynamic call-site returns |
| `Atomic { next, .. }` | `{next}` (body region analysed separately) |
| `Absence { next, .. }` | `{next}` (inner region analysed separately) |

`IrGuard::LookAround { body, .. }` references a region but is zero-width — the
`body` region is a *guard sub-execution*, not a successor block of the current
region.  It is analysed separately (and benefits from the lookaround result
cache in `MemoState`).

### 5.2 Why `Fork` has N successor edges

Every candidate block in a `Fork` is executed with the full execution state as it
was *at the Fork point*.  Any capture slot that is live in candidate `cᵢ.block`
must therefore also be live at the Fork point.

By treating every candidate block as a plain CFG successor, standard backward
liveness analysis captures this requirement without modification:

```
live_in[Fork] = ∪ { live_in[cᵢ.block] | cᵢ ∈ candidates }
```

When `disjoint: true` is set, no `Bt::Retry` snapshot is taken, so `live_slots`
is irrelevant at runtime — but the union is still computed conservatively.

---

## 6. Liveness Analysis for Captures

### Definition

Capture slot `s` is **live** at program point `p` iff there exists a
path from `p` to a `CheckBackRef` that reads `s`, or to `Match` /
`RegionEnd` where `s` will be reported to the caller, without any
`SaveCapture(s)` occurring on that path first.

### Algorithm

Standard backward data-flow over the CFG with all `Fork` candidates as successors:

```
live_out[B] = ∪ { live_in[S] | S ∈ successors(B) }
live_in[B]  = (live_out[B] − kill[B]) ∪ gen[B]

where:
  gen[B]  = { s | SaveCapture(s) in B that is preceded by a read of s,
                  or CheckBackRef(g) with slots for g }
  kill[B] = { s | SaveCapture(s) in B with no prior read of s in B }
```

Iterate to fixpoint (the CFG is finite; this terminates).

### Result

A `SaveCapture(s)` instruction is a **dead store** — and can be
eliminated — if slot `s` is not in `live_out` of its block (or is killed
by a later `SaveCapture(s)` in the same block before any read).

**Live-slot Fork tagging**: the live set at each `Fork` terminator is
exactly the set of slots that must be included in the `Bt::Retry` snapshot
(relevant only when `disjoint: false`).  Annotating the `Fork` with
`live_slots: BitSet` — computed as the union of `live_in` over all candidate
blocks — allows the interpreter and JIT to clone only those slots, shrinking
the per-retry `SmallSlots` copy.

---

## 7. Optimization Passes

Passes run on `IrProgram` in the following order:

### Pass 1 — Dead block elimination

Remove any `IrBlock` that has no predecessors in the CFG (unreachable code).
Repeat until a fixed point.

### Pass 2 — Block merging

If block A has exactly one successor B, and B has exactly one predecessor A,
and A's terminator is `Branch(B)`, merge B's stmts and terminator into A.
Repeat until a fixed point.

### Pass 3 — Capture liveness

1. Compute `live_in` / `live_out` for every block (§6).
2. Remove `SaveCapture(s)` instructions that are dead stores.
3. Annotate each `Fork { candidates, .. }` with `live_slots` =
   `∪ { live_in[c.block] | c ∈ candidates }`.  The interpreter and JIT
   use this bitset to minimize `SmallSlots` cloning at retry points
   (only when `disjoint: false`).

### Pass 4 — Guard Analysis

For each `Fork { candidates, .. }`:

1. **Infer guards from first statements**: if any candidate has `guard:
   IrGuard::Always` and its block begins with `MatchChar(c)`,
   `MatchClass(id)`, `MatchFoldSeq([c])`, or a backward variant, promote the
   guard to `IrGuard::Char(c)` / `IrGuard::Class(id)` etc., and remove the
   corresponding statement from the block (since the guard now acts as the
   peek).  This replaces the current `compute_fork_guards` post-pass.

2. **Disjointness analysis**: test whether all guard conditions are pairwise
   mutually exclusive (e.g. disjoint `Char`/`Class` sets, or `GroupMatched`
   vs the remaining `Always` default).  If yes, set `disjoint: true`.  A
   possessive fork needs no `Bt::Retry` push, which also enables Pass 5 span
   detection.

The `LookAround` guard is **never** disjoint from `Always` in general
(the lookaround may pass, the candidate block may still fail, and we may
need to try a later candidate on backtrack) — `disjoint` is only set when
all guards are character-based and cover non-overlapping sets, or when the
fork is a conditional-group pattern (`GroupMatched` vs `Always`).

### Pass 5 — Fork guard propagation (CharFast hint)

For each `Fork` whose guards were inferred in Pass 4, emit a `CharFast(c)`
hint in the lowering metadata so that the flat-VM lowering can emit
`CharFast` instead of `Char` for the now-peeked-at character, skipping the
redundant bounds+match check.

### Pass 6 — Span detection

Detect greedy-loop patterns and replace them with possessive spans:

1. Find a `Fork { candidates: [{ guard: g_body, block: body }, { guard: Always, block: exit }], disjoint: false }` where:
   - `body` has no statements (all matching done by guard `g_body`) and
     terminates with `Branch(fork_block)` back to the `Fork` block.
   - `g_body` is `Char(c)` or `Class(id)`.
   - Exit block's guard (from a future iteration) or first statement uses a
     **disjoint** character set.
2. Replace the `Fork` with `SpanChar { c, exit }` or `SpanClass { id, exit }`.
3. Remove the now-dead `body` block (Pass 1 cleans it up).

This replaces the current `spanify_greedy_loops` post-pass.

### Pass 7 — Counter liveness

If a `CounterInit(s)` / `CounterNext { slot: s, .. }` pair is the only user of
counter slot `s` and the exact repetition count is 1 (i.e., the body executes
exactly once), eliminate the counter and replace with a simple `Branch` to the
body.

---

## 8. Lowering Pipeline

### Phase 1 — IR → `Vec<Inst>` (compatibility)

`ir/lower.rs` converts each `IrRegion` + `IrBlock` back to a flat `Vec<Inst>`,
preserving the existing interpreter and JIT paths unchanged.  This is the
initial deployment path: land the IR builder, verify output is bit-identical to
the existing compiler, then enable passes one at a time.

Lowering decisions:
- `Fork { candidates: [{ guard: Char(c), block: B }, ..], .. }` → `Fork(alt, Some(c))` at
  the Fork site; first statement of B lowered as `CharFast(c)` instead of `Char(c)`
  (the guard already verified `text[pos] == c`).
- `Fork { candidates: [{ guard: GroupMatched(s), block: yes }, { guard: Always, block: no }] }` →
  `CheckGroup { slot: s, yes_pc, no_pc }`.
- `Fork` with `IrGuard::LookAround { pol, dir, body }` → `LookStart { positive, end_pc }` +
  body region blocks + `LookEnd`.
- `IrTerminator::RegionEnd` → `Inst::LookEnd` (for lookaround bodies) or
  `Inst::AbsenceEnd` (for absence bodies) or `Inst::AtomicEnd` (for atomic bodies).
- Sub-program region blocks are appended sequentially after the main program
  blocks, with `LookStart` / `AtomicStart` / `AbsenceStart` pointing to them.

### Phase 2 — IR → Cranelift directly

`ir/jit.rs` compiles each `IrRegion` to a separate Cranelift function.  This
eliminates the current JIT's implicit CFG reconstruction step (one `Block` per
PC) and replaces it with a direct mapping:

| IR construct | Cranelift IR |
|---|---|
| `IrBlock` | Cranelift `Block` |
| `IrTerminator::Fork { candidates, .. }` | One Cranelift block per candidate; guard evaluation branching inlined; `disjoint: true` forks skip bt-snapshot code |
| `IrGuard::LookAround { body, .. }` | `call` to the body region's Cranelift function (guard result as bool) |
| `IrTerminator::Atomic { body, .. }` | `call` to body; on return, inline bt-drain |
| `Fork { live_slots, .. }` annotation | Only the annotated slots are saved/restored in the snapshot (when `disjoint: false`) |

### Phase 3 — IR interpreter

Eventually replace the flat-array `'vm: loop` in `vm.rs` with a block-based
interpreter that dispatches on `IrBlock` directly, eliminating the lowering
step entirely.

---

## 9. Relationship to the Current JIT

The current JIT (`src/jit/`) takes `Vec<Inst>` as input and reconstructs a
CFG (one Cranelift `Block` per PC) by scanning for `Fork`/`Jump` instructions.
This works but has two limitations:

1. **Eligibility gaps**: instructions like `RepeatInit`/`RepeatNext` and
   `Call`/`RetIfCalled` are ineligible because mapping their semantics from the
   flat stream to Cranelift is complex.  With an explicit IR the structure is
   already known, and these can be compiled directly.
2. **No capture optimization**: all capture slots are snapshotted at every Fork
   because there is no liveness information.  The IR's `live_slots` annotation
   on `Fork` terminators directly feeds the JIT's snapshot routine (when
   `disjoint: false`; possessive forks take no snapshot at all).

The IR-based JIT (`ir/jit.rs`) supersedes the current JIT once the IR builder
and passes are stable.  The existing `src/jit/` is kept during the transition.

---

## 10. IR Invariants (verifier)

The debug-mode verifier (`ir/verify.rs`) checks:

1. Every `IrBlock` ends with exactly one `IrTerminator`.
2. Every `BlockId` referenced by a terminator is a valid index in the same
   `IrRegion::blocks`.
3. Every `RegionId` referenced by an `IrGuard::LookAround`, `Atomic`, or
   `Absence` terminator is a valid index in `IrProgram::regions`.
4. `IrProgram::regions[0]` has `RegionKind::Main`.
5. The entry block of each region has no intra-region predecessors (it is only
   reachable from a sub-program invocation terminator or from the start of
   execution).
6. Every `Fork` candidate whose `guard` is `Char(c)` or `Class(id)` is
   consistent with the first `IrStmt` of `c.block` (which must be the
   corresponding `MatchChar(c)` or `MatchClass(id)` after Guard Analysis);
   if the Guard pass has not yet run, guards are `Always` by default.
7. No `SaveCapture(s)` is marked dead while `s` is read by a subsequent `CheckBackRef`
   or final `Match`.

---

## 11. File Layout

```
src/
  ir/
    mod.rs         — IrProgram, IrRegion, IrBlock, IrStmt, IrTerminator;
                     BlockId, RegionId; RegionKind
    build.rs       — IrBuilder: AST + CompileOptions → IrProgram
    lower.rs       — IrProgram → Vec<Inst>  (compatibility lowering)
    verify.rs      — debug-mode invariant checker
    pass/
      mod.rs       — pass pipeline entry point
      dce.rs       — dead block elimination
      merge.rs     — block merging
      liveness.rs  — capture liveness + dead SaveCapture elimination
      guard.rs     — guard analysis (guard inference + disjointness + CharFast hint)
      span.rs      — span detection
      counter.rs   — counter liveness
  jit/
    ir_jit.rs      — IrProgram → Cranelift IR (Phase 2 JIT path)
    …              — existing flat-VM JIT (maintained during transition)
doc/
  IR_DESIGN.md     — this file
  DESIGN.md        — overall architecture; references IR_DESIGN.md
  JIT.md           — JIT compilation design; references IR_DESIGN.md
```

---

## 12. References

- Aho, A.V., Lam, M.S., Sethi, R., Ullman, J.D. (2006). *Compilers: Principles,
  Techniques, and Tools* (2nd ed.).  Chapter 9 (data-flow analysis) covers the
  backward liveness algorithm used in Pass 3.
- Fujinami, H. & Hasuo, I. (2024). "Efficient Matching with Memoization for
  Regexes with Look-around and Atomic Grouping." arXiv:2401.12639.  The
  memoization algorithms (5–7) are preserved through the IR layer.
- Cranelift reference manual:
  <https://github.com/bytecodealliance/wasmtime/tree/main/cranelift>
