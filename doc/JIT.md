# Oniai — JIT Compilation Design

This document describes the design and implementation of the JIT (Just-In-Time)
compilation layer in Oniai.  The JIT is an optional acceleration layer
(enabled with `--features jit`) that compiles a `Vec<Inst>` VM program to
native machine code at regex construction time, replacing the interpreter loop
in `vm.rs` for eligible patterns.

---

## 1. Motivation

Oniai's current execution model is an **interpreted backtracking VM**.  Each
call to `exec()` steps through `Vec<Inst>` in a `loop`, dispatching on the
variant of each `Inst` enum.  Despite memoisation and pre-filters, the dispatch
overhead is non-trivial for short patterns on long texts:

- Every instruction incurs a branch mispredict and pointer-chase into the
  `Inst` enum.
- Cache-sensitive data structures (`bt` stack, `slots` vec) are accessed
  through extra indirection.
- Common sequences like a run of `Char` instructions cannot be fused by the
  Rust compiler because the loop body is a single `match`.

A JIT that compiles the `Vec<Inst>` to native code removes the dispatch
overhead entirely, fuses adjacent character tests into tight register-resident
loops, and enables the CPU branch predictor to work on the actual pattern
structure.

PCRE2's JIT (by Zoltan Herczeg, using Sljit) reports 2–10× improvements for
character-intensive patterns.  We expect similar gains for the subset of Oniai
patterns that are JIT-eligible.

---

## 2. Design Goals and Non-Goals

### Goals

| # | Goal |
|---|------|
| G1 | Transparent fallback: patterns that are not JIT-eligible fall back to the interpreter without any API change. |
| G2 | Feature-flag isolation: the JIT backend is behind a `jit` Cargo feature; the library core remains dependency-free. |
| G3 | Correct memoization: the same algorithms (5–7 of Fujinami & Hasuo 2024) are preserved in JIT-compiled code. |
| G4 | All public API behaviour is identical between interpreter and JIT paths. |
| G5 | x86-64 and AArch64 support (the two primary CI targets). |

### Non-Goals

| # | Non-goal |
|---|----------|
| N1 | DFA or NFA simulation — the engine remains backtracking; the JIT accelerates the *same* algorithm. |
| N2 | Profile-guided or adaptive re-compilation (no tiered JIT). |
| N3 | JIT compilation of patterns with back-references or subexpression calls (`\g<…>`) — interpreter fallback is used. |
| N4 | Windows support. |

---

## 3. Backend: Cranelift

The JIT backend uses **[Cranelift](https://github.com/bytecodealliance/wasmtime/tree/main/cranelift)**
(`cranelift-jit` + `cranelift-codegen`), the pure-Rust code-generation
framework from the Bytecode Alliance.

Rationale:

- Pure Rust — no C/C++ toolchain dependency.
- Produces correct, well-optimised code for both x86-64 and AArch64.
- `cranelift-jit` provides a ready-made module that allocates executable
  memory, applies relocations, and hands back a raw function pointer.
- Active upstream; used in production by Wasmtime and Lucet.

Alternatives considered:

| Alternative | Reason rejected |
|-------------|-----------------|
| `dynasm-rs` (x86-64 only) | Would require separate AArch64 implementation; macro-based API makes the code harder to read. |
| LLVM via `inkwell` | Heavy C++ dependency; slow compile-time; linking complexity. |
| `libgccjit` | Requires GCC at runtime. |
| Hand-coded assembly | Not portable; maintenance burden. |

### Cargo feature

```toml
[features]
jit = ["dep:cranelift-jit", "dep:cranelift-codegen", "dep:cranelift-frontend"]

[dependencies]
cranelift-jit      = { version = "0.113", optional = true }
cranelift-codegen  = { version = "0.113", optional = true }
cranelift-frontend = { version = "0.113", optional = true }
```

---

## 4. Compilation Model

### 4.1 Overview

A JIT-compiled program is a single native function with the Cranelift signature:

```text
fn jit_exec(ctx_ptr: i64, start_pos: i64) -> i64
```

`ctx_ptr` points to a `JitExecCtx` struct (see §4.6) which holds the text,
all mutable execution state (capture slots, backtrack stack, memo array), and
references to the immutable program data.  The return value is the match end
position (≥ 0) on success, or −1 on no-match.

### 4.2 Instruction-to-block mapping

The compiler assigns one **Cranelift basic block** (`Block`) per VM program
counter.  Edges between blocks correspond exactly to the control-flow edges
implied by the `Inst` enum:

| VM instruction | Cranelift CFG edge |
|----------------|-------------------|
| `Match` | Return success value |
| `Char` / `AnyChar` / `Shorthand` / `Class` / `Prop` / `FoldSeq` | Fall-through on success; jump to `bt_resume_block` on fail |
| `Anchor` | Same as character match |
| `Jump(t)` | Unconditional `jump` to `block[t]` |
| `Fork(alt)` | Check dense memo; push `MemoMark + Retry` onto bt stack; `jump` to `block[pc+1]` |
| `ForkNext(alt)` | Check dense memo; push `MemoMark + Retry` onto bt stack; `jump` to `block[alt]` |
| `Save(s)` | Push `SaveUndo` entry; store `pos` into `slots[s]`; fall through |
| `KeepStart` | Store `pos` into `ctx.keep_pos`; fall through |
| `CheckGroup` | Branch on `slots[slot] != u64::MAX` |
| `LookStart` / `LookEnd` | Call `jit_lookaround` helper (see §4.5) |
| `AtomicStart` / `AtomicEnd` | Call `jit_atomic_start` / `jit_atomic_end` helper |
| `AbsenceStart` / `AbsenceEnd` | Ineligible — interpreter fallback |
| `BackRef` / `BackRefRelBack` | Ineligible — interpreter fallback |
| `Call` / `Ret` / `RetIfCalled` | Ineligible — interpreter fallback |

### 4.3 Inlining strategy

Common instructions are emitted as inline Cranelift IR to avoid `extern "C"`
call overhead:

| Instruction | Inline IR |
|-------------|-----------|
| `Char(ch)` (ASCII) | `uload8` + `icmp` + conditional branch |
| `AnyChar(dotall)` | bounds check + UTF-8 leading-byte length + optional newline test |
| `Shorthand(sh)` (ASCII) | bounds check + ASCII range checks; non-ASCII falls back to helper |
| `Class` / `ClassBack` (ASCII charsets) | inline range/char checks; POSIX always-ASCII classes inlined |
| `ShorthandBack` (ASCII) | same as Shorthand |
| `Save(slot)` | `store pos → slots_ptr[slot]` (with prior `SaveUndo` push) |
| `KeepStart` | `store pos → ctx.keep_pos` |
| `Anchor(StringStart\|StringEnd)` | `icmp pos == 0` / `icmp pos == text_len` |
| `Jump(t)` | direct block jump |
| Fork memo-check | dense array bounds check + `uload8` + bitmask test (inline) |
| Fork memo-record (MemoMark pop) | dense array bounds check + `uload8` + OR-store (inline) |

Everything else calls the corresponding `jit_*` `extern "C"` helper (§4.5).

### 4.4 Backtracking in native code

The backtrack stack (`Vec<BtJit>`) is **not** compiled away.  It is held in
`ExecScratch` (see §4.6) and passed to the JIT function via raw pointer fields
in `JitExecCtx`.  The JIT function and all helpers maintain the raw-parts
triple `(bt_data_ptr, bt_len, bt_cap)` consistently.

`BtJit` is a flat 24-byte `repr(C)` struct with a stable layout so that both
Rust helpers and inline Cranelift IR can access fields at known byte offsets:

```
offset  0 : tag  (u32) — 0=Retry, 1=SaveUndo, 2=AtomicBarrier, 3=MemoMark
offset  4 : a    (u32) — block_id (Retry) / slot (SaveUndo) / fork_idx (MemoMark)
offset  8 : b    (u64) — pos (Retry) / old_value (SaveUndo) / fork_pos (MemoMark)
offset 16 : c    (u64) — keep_pos (Retry only); zero otherwise
```

When the JIT code needs to backtrack (fail path), control jumps to
`bt_resume_block`, which:

1. Pops entries from the bt stack inline:
   - `MemoMark` entries record failures into the dense memo array (inline
     OR-store if in-bounds; `jit_fork_memo_record` helper if growth needed).
   - `SaveUndo` entries restore the saved slot value.
   - `AtomicBarrier` entries call `jit_atomic_end_fail` to drain the group.
   - The first `Retry` entry restores `pos`/`keep_pos` and dispatches to the
     saved `block_id` via `br_table`.
2. If the stack is empty, returns −1.

This loop runs entirely in Cranelift IR for the common case where no growth or
atomic drain is needed.

```
         ┌──── native block for pc=7  (a Char instruction) ──────────────────┐
         │  uload8 text[pos]                                                  │
         │  icmp == 'a'                                                       │
         │  brnz → block[8]         ← success path                           │
         │  jump → bt_resume_block  ← failure path                           │
         └────────────────────────────────────────────────────────────────────┘
```

### 4.5 Capture-delta undo log

To keep bt-stack pushes allocation-free, `Save` instructions use an
**undo-log** approach:

- **Before each slot write**, the inlined `Save` block pushes
  `BtJit::SaveUndo { slot, old_value }` onto the bt stack (~16 bytes used,
  `c` field zero).
- **`BtJit::Retry` no longer contains a `slots` snapshot.**  It stores only
  `{ block_id, pos, keep_pos }` — 24 bytes, no heap allocation.
- **On backtrack**, `SaveUndo` entries restore their slot, `AtomicBarrier`
  entries call the drain helper, `MemoMark` entries record failures, and the
  first `Retry` restores `pos`/`keep_pos` and jumps to the saved block.
- **`jit_atomic_end`** (committing an atomic group) drains `SaveUndo` entries
  up to the `AtomicBarrier` — the slots already hold the committed values.

### 4.6 `JitExecCtx` — the per-call context block

All mutable state for one `exec_jit` invocation is packed into a single
`#[repr(C)]` struct so that both Rust helpers and inline Cranelift IR can
access fields at stable, compile-time-known byte offsets.

```rust
#[repr(C)]
pub(crate) struct JitExecCtx {
    // immutable for this exec call
    text_ptr:           *const u8,  // offset   0
    text_len:           u64,        // offset   8
    search_start:       u64,        // offset  16
    use_memo:           u64,        // offset  24  (0 or 1)
    charsets_ptr:       *const (),  // offset  32
    charsets_len:       u64,        // offset  40
    prog_ptr:           *const (),  // offset  48  (for lookaround sub-exec)
    prog_len:           u64,        // offset  56
    num_groups:         u64,        // offset  64
    // mutable capture state
    slots_ptr:          *mut u64,   // offset  72  (u64::MAX = None)
    slots_len:          u64,        // offset  80
    keep_pos:           u64,        // offset  88  (u64::MAX = None)
    // backtrack stack (raw parts of Vec<BtJit>)
    bt_data_ptr:        *mut BtJit, // offset  96
    bt_len:             u64,        // offset 104
    bt_cap:             u64,        // offset 112
    // memoization
    memo_ptr:           *mut (),    // offset 120  (*mut MemoState)
    memo_has_failures:  u64,        // offset 128  (1 = dense array has data)
    // atomic group depth
    atomic_depth:       u64,        // offset 136
    bt_retry_count:     u64,        // offset 144  (# Retry entries on bt stack)
    // dense fork-failure memo array (raw parts owned by ExecScratch)
    fork_memo_data_ptr: *mut u8,    // offset 152
    fork_memo_len:      u64,        // offset 160
    fork_memo_cap:      u64,        // offset 168
}
```

### 4.7 Rust helpers for complex instructions

The following instructions call back into Rust `extern "C"` helpers:

| Instruction group | Rust helper | Notes |
|-------------------|-------------|-------|
| `AtomicStart` | `jit_atomic_start` | Increments `atomic_depth`; pushes `AtomicBarrier` |
| `AtomicEnd` | `jit_atomic_end` | Drains `SaveUndo` entries up to barrier; decrements depth |
| `LookStart` / `LookEnd` | `jit_lookaround` | Runs sub-execution via interpreter; caches result in `look_results` |
| `Prop` / `PropBack` | `jit_prop` / `jit_prop_back` | Unicode property check via `charset` module |
| `FoldSeq` / `FoldSeqBack` | `jit_fold_seq` / `jit_fold_seq_back` | Multi-codepoint case-fold advance/retreat |
| Fork slow path | `jit_fork` / `jit_fork_next` | Used when `memo_has_failures == 1` and dense array check misses |
| Memo growth | `jit_fork_memo_record` | Grows the dense array and records a failure |
| Char non-ASCII | `jit_match_char` | Full Unicode codepoint comparison |

All helpers are `unsafe extern "C"` functions declared in `src/jit/helpers.rs`
and registered as named symbols in the `JITBuilder`.

### 4.8 Memoization in JIT code

All three algorithms from Fujinami & Hasuo 2024 (arXiv:2401.12639) are
preserved in JIT-compiled code:

**Algorithm 5 (fork failures — dense array)**: Each `Fork`/`ForkNext`
instruction has a compact `fork_idx` (0-based index among all forks in the
program).  Failures are stored in a dense `Vec<u8>` indexed by
`fork_idx × (text_len + 1) + pos`; each byte is a bitmask where bit `d` means
"failed at atomic depth `d`".

- **Failure check (inline)**: at each Fork, if `memo_has_failures == 1`, the
  inline IR reads `data[fork_idx × stride + pos]`, applies the depth bitmask,
  and jumps directly to `bt_resume_block` on a cache hit — no helper call.
- **Failure record (inline)**: when `bt_resume_block` pops a `MemoMark` entry,
  it OR-stores the depth bit into `data[fork_idx × stride + fork_pos]` inline,
  calling `jit_fork_memo_record` only if the array needs to grow.
- When `use_memo == false` (pattern contains back-references), all memo checks
  are skipped.

**Algorithm 7 (depth-tagged failures)**: `atomic_depth` is maintained as a
field in `JitExecCtx`, incremented/decremented by the atomic helpers.  The
depth bitmask in the dense array ensures failures inside atomic groups are not
reused in less-constrained outer contexts.

**Algorithm 6 (lookaround cache)**: Handled entirely inside the
`jit_lookaround` Rust helper, which uses the same `look_results` HashMap as
the interpreter.

---

## 5. Eligibility and Fallback

Not all `Vec<Inst>` programs are JIT-compiled.  The eligibility check runs
immediately after `compile()` returns:

```
JIT-ineligible if any instruction is:
  • BackRef / BackRefRelBack     → memo disabled anyway; outcome depends on captured text
  • Call / Ret / RetIfCalled     → subexpression calls (recursive patterns)
  • AbsenceStart / AbsenceEnd    → absence operator
```

All other instructions — including `LookStart`/`LookEnd`, `FoldSeq`/
`FoldSeqBack`, `AtomicStart`/`AtomicEnd`, `CheckGroup`, `Prop`, etc. — are
JIT-eligible.

In all ineligible cases `Regex::new` stores the `CompiledProgram` without a
`JitModule` and `find` uses the existing interpreter path.  No API change.

---

## 6. Lookaround Sub-Execution Strategy

Lookaround bodies (`LookStart`/`LookEnd`) are kept in the **interpreter** even
after the outer program is JIT-compiled.  This is because lookaround bodies
are small, executed infrequently relative to the outer loop, and have complex
recursion structure (depth tracking, isolated `State`).

The `jit_lookaround` helper:

1. Extracts the current `slots` / `keep_pos` into a Rust-owned `State`.
2. Calls the existing `exec_lookaround(ctx, lk_pc, pos, &mut state, depth,
   memo)`.
3. On success, writes the updated `State` back through the pointer arguments
   and returns the post-lookaround `pos`.
4. On failure, returns −1 and the JIT failure path jumps to `bt_resume_block`.

This keeps the correctness proof of lookarounds entirely within the existing
interpreter code, at the cost of a function call per lookaround evaluation (no
change from the interpreter baseline).

---

## 7. `JitModule` and Lifetime

`JitModule` wraps the Cranelift `JITModule` and holds:

```rust
pub(crate) struct JitModule {
    _module:  cranelift_jit::JITModule,   // owns the executable memory
    func_ptr: unsafe extern "C" fn(i64, i64) -> i64,
}

// SAFETY: the compiled function is stateless (all mutable state is accessed
// through the ctx_ptr argument) and the JITModule memory is immutable after
// finalize_definitions().
unsafe impl Send for JitModule {}
unsafe impl Sync for JitModule {}
```

`JitModule` is stored inside `CompiledProgram` as `Option<Arc<JitModule>>`.
`Arc` allows `Regex` to be `Clone` without recompiling.  `Send + Sync` impls
make `Regex: Send + Sync` even with the `jit` feature enabled.

---

## 8. Integration with the Public API

No public API changes are required.  The selection between JIT and interpreter
is internal to `CompiledRegex`.

### `find_with_scratch`

The core execution method is:

```rust
pub fn find_with_scratch(
    &self,
    text: &str,
    start_pos: usize,
    scratch: &mut ExecScratch,
) -> Option<(usize, usize, Vec<Option<usize>>)>;
```

It accepts a caller-owned `ExecScratch` so that the backtrack stack and
fork-memo array can be **reused across successive calls** without
re-allocation.

`find()` is a thin wrapper that allocates a temporary `ExecScratch`:

```rust
pub fn find(&self, text: &str, start_pos: usize) -> Option<(…)> {
    let mut scratch = ExecScratch::new();
    self.find_with_scratch(text, start_pos, &mut scratch)
}
```

### Persistent `ExecScratch` in iterators

`FindIter` and `CapturesIter` hold a persistent `ExecScratch` field, allocated
once when the iterator is created and reused for every `find_with_scratch`
call.  This amortises the cost of the dense fork-memo array (which can be up
to `fork_count × (text_len + 1)` bytes) across the entire scan, rather than
allocating and zeroing it on every `find()` call.

```rust
// lib.rs
pub struct FindIter<'r, 't> {
    regex:   &'r Regex,
    text:    &'t str,
    pos:     usize,
    scratch: vm::ExecScratch,  // persists across next() calls
}
```

### `ExecScratch` layout

```rust
pub(crate) struct ExecScratch {
    pub bt:             Vec<BtJit>, // JIT backtrack stack; reused across calls
    pub slots:          Vec<u64>,   // capture slots buffer; resized once
    // Dense fork-failure memo array stored as raw parts to avoid Vec
    // take/forget/reconstruct overhead on every exec_jit call.
    pub fork_memo_ptr:  *mut u8,
    pub fork_memo_len:  usize,
    pub fork_memo_cap:  usize,
}
```

The `fork_memo` allocation is lazily initialised on the first backtracking
failure that triggers `jit_fork_memo_record`.  All subsequent calls reuse the
same allocation; entries already recorded remain valid because fork-failure
facts are position-based and independent of the search start.

When the `jit` feature is disabled, `ExecScratch` is a zero-sized stub.

---

## 9. Implementation History

The JIT was implemented incrementally in seven phases.

### Phase 1 — Core JIT infrastructure

Established the Cranelift backend, `JitModule`, eligibility check, and
`extern "C"` helpers for all instructions.  `BtJit::Retry` stored a full
`slots` snapshot; every `Fork` allocated on the heap.

### Phase 2 — Character instruction inlining

Inlined `Char` (ASCII), `AnyChar`, `Shorthand` (ASCII fast-path), `Save`,
and `Anchor(StringStart|StringEnd)` as Cranelift IR, eliminating the
corresponding `extern "C"` calls.

### Phase 3 — `CharClass` inlining

Inlined `CharClass`/`ClassBack` for charsets whose items are all ASCII `Char`
or `Range` values (no negation, no Unicode escapes).

### Phase 4 — POSIX and `ShorthandBack` inlining

Extended `CharClass`/`ClassBack` inline to POSIX always-ASCII classes and
ASCII-safe `Shorthand` items; inlined `ShorthandBack`.

### Phase 5 — Capture-delta undo log

Replaced per-`Fork` `slots` snapshots with an **undo-log**: `Save` pushes a
`BtJit::SaveUndo` entry before each slot write, and `BtJit::Retry` shrinks
from holding a heap-allocated `Vec<Option<usize>>` to three scalar fields
(24 bytes total).  This eliminated the largest remaining per-fork allocation.

### Phase 6 — Inline fork push and bt-pop fast path

Inlined the `Fork`/`ForkNext` fast path (push `MemoMark + Retry` directly in
Cranelift IR; no helper call for the common case).  Inlined the bt-pop loop
for `Retry` peek-and-pop.  Hoisted the `Vec<BtJit>` allocation into
`ExecScratch` so it is reused across `exec_jit` calls.  Introduced the 24-byte
`repr(C)` `BtJit` layout with zero padding waste.

### Phase 7 — Dense fork-memo array; `FoldSeq` eligibility; persistent scratch

**Dense array**: replaced the `HashMap<u64, u8>` fork-failure cache with a
`Vec<u8>` bitmask indexed by `fork_idx × (text_len + 1) + pos`.  Both the
failure check (at every Fork) and the failure record (when a `MemoMark` is
popped) are now fully inlined in Cranelift IR — zero `extern "C"` calls for
the steady-state pathological-pattern case.

**`FoldSeq`/`FoldSeqBack` eligibility**: added `jit_fold_seq` and
`jit_fold_seq_back` helpers and removed these instructions from the ineligible
list.  Case-insensitive patterns are now JIT-compiled.

**`KeepStart` inline**: emitted as a single store in Cranelift IR.

**Persistent `ExecScratch`**: `FindIter` and `CapturesIter` hold a persistent
`ExecScratch` that survives across successive `find()` calls, amortising the
fork-memo array allocation across the entire scan.

---

## 10. Measured Performance

The table below gives measured median times (x86-64, Apple Silicon M-series via
Rosetta, Criterion 0.5).  See `doc/BENCHMARKS.md` for the full table and
per-phase breakdown.

| Benchmark | Interpreter | JIT Phase 7 | Speedup |
|-----------|-------------|-------------|---------|
| `quantifier/greedy_match_500` | 9.17 µs | 1.83 µs | **5.01×** |
| `pathological/10` | 4.39 µs | 1.88 µs | **2.33×** |
| `pathological/15` | 9.94 µs | 3.97 µs | **2.50×** |
| `pathological/20` | 17.7 µs | 6.93 µs | **2.55×** |
| `charclass/alpha_iter` | 31.2 µs | 6.96 µs | **4.48×** |
| `charclass/posix_digit_iter` | 24.4 µs | 8.99 µs | **2.71×** |
| `find_iter_scale/1000` | 26.7 µs | 16.5 µs | **1.62×** |
| `email/find_all` | 3.25 µs | 1.79 µs | **1.82×** |
| `real_world/title_name` | 139.5 µs | 135 µs | **1.03×** |
| `case_insensitive/match` | 14.0 µs | 14.2 µs | ≈1.00× |

Patterns containing `BackRef` or subexpression calls are unaffected
(interpreter used).

---

## 11. File Layout

```
src/
  jit/
    mod.rs       — JitModule; is_eligible(); try_compile(); exec_jit()
    builder.rs   — Cranelift IR construction (one Block per PC)
    helpers.rs   — extern "C" Rust helpers; register_symbols()
  vm.rs          — ExecScratch; BtJit; JitExecCtx; find_with_scratch()
  lib.rs         — FindIter/CapturesIter with persistent ExecScratch
doc/
  JIT.md         — this file
  DESIGN.md      — overall architecture; cross-references JIT.md
  BENCHMARKS.md  — per-phase benchmark comparison tables
```

---

## 12. References

- Herczeg, Z. (2012). *Fast Regular Expression Matching Using JIT Compiler*.
  PCRE2 JIT design notes.
- Cranelift reference manual: <https://github.com/bytecodealliance/wasmtime/tree/main/cranelift>
- Fujinami, H. & Hasuo, I. (2024). "Efficient Matching with Memoization for
  Regexes with Look-around and Atomic Grouping." arXiv:2401.12639.
- PCRE2 `sljit` backend source:
  <https://github.com/PCRE2Project/pcre2/tree/master/src/sljit>
