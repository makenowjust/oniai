# Aigumo — JIT Compilation Design

This document describes the design and phased implementation plan for adding
JIT (Just-In-Time) compilation to Aigumo.  The JIT is an optional acceleration
layer that compiles a `Vec<Inst>` VM program to native machine code at regex
construction time, replacing the interpreter loop in `vm.rs` for eligible
patterns.

---

## 1. Motivation

Aigumo's current execution model is an **interpreted backtracking VM**.  Each
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
character-intensive patterns.  We expect similar gains for the subset of Aigumo
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
| N3 | JIT compilation of patterns with back-references or `\g<…>` calls in the first release (interpreter fallback is used). |
| N4 | Windows support in the initial release. |

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

A JIT-compiled program is a single native function with the signature:

```rust
// Conceptual Rust equivalent of the compiled function's ABI:
//
//   fn jit_exec(
//       ctx:        *const JitCtx,   // immutable per-search context
//       bt_stack:   *mut BtStack,    // explicit backtrack stack (same semantics as Bt)
//       memo:       *mut MemoState,  // shared memoisation state
//       pos:        usize,           // current text position
//       slots:      *mut SlotBuf,    // capture slot array
//       keep_pos:   *mut Option<usize>,
//       call_stack: *mut CallStack,  // for \g<…> return addresses
//   ) -> isize;                      // -1 = no match; ≥ 0 = match end position
```

`JitCtx` is a thin struct holding the text pointer/length, `search_start`, and
`use_memo`.  All other state is passed explicitly so backtrack resumptions can
restore it precisely.

### 4.2 Instruction-to-block mapping

The compiler assigns one **Cranelift basic block** (`Block`) per VM program
counter.  Edges between blocks correspond exactly to the control-flow edges
already implied by the `Inst` enum:

| VM instruction | Cranelift CFG edge |
|----------------|-------------------|
| `Match` | Return success value |
| `Char` / `AnyChar` / `Class` / `Shorthand` / `Prop` / `FoldSeq` | Fall-through on success; call `bt_pop` helper on fail |
| `Anchor` | Same as character match |
| `Jump(t)` | Unconditional `jump` to `block[t]` |
| `Fork(alt)` | Push `(block[alt], snapshot)` onto `bt_stack`; `jump` to `block[pc+1]` |
| `ForkNext(alt)` | Push `(block[pc+1], snapshot)` onto `bt_stack`; `jump` to `block[alt]` |
| `Save(s)` | Store `pos` into `slots[s]`; fall through |
| `KeepStart` | Store `pos` into `*keep_pos`; fall through |
| `Call(t)` | Push `(block[pc+1])` onto `call_stack`; `jump` to `block[t]` |
| `RetIfCalled` | Branch: if `call_stack` non-empty, pop and `jump`; else fall through |
| `AtomicStart` / `AtomicEnd` | Call into Rust helper (see §4.5) |
| `LookStart` / `LookEnd` | Call into Rust helper (see §4.5) |
| `CheckGroup` | Branch on `slots[slot].is_some()` |
| `AbsenceStart` / `AbsenceEnd` | Call into Rust helper (see §4.5) |
| `BackRef` | Call into Rust helper |

### 4.3 Backtracking in native code

The backtrack stack (`BtStack`) is **not** compiled away.  It retains the same
`Bt` enum entries used by the interpreter, but instead of holding a `pc:
usize` in `Bt::Retry`, the JIT variant stores a **raw block address** (a
`*const u8` into the compiled function) together with a snapshot of `slots`,
`keep_pos`, and `call_stack`.

When the JIT code needs to backtrack (`fail` path), it calls a small Rust
helper `bt_pop(bt_stack, slots, keep_pos, call_stack) -> *const u8` which:

1. Pops entries off the bt stack (processing `AtomicBarrier` and `MemoMark`
   exactly as the interpreter's `do_backtrack` does).
2. Restores `slots`, `keep_pos`, and `call_stack` from the `Bt::Retry` payload.
3. Returns the saved block address, or `null` on empty stack.

Back in native code the JIT checks for `null` (return -1) and otherwise uses
an **indirect tail-jump** through the returned address to resume execution.
Cranelift supports indirect branches via `br_table` / `call_indirect`; we use
a `call_indirect` with `tail` convention so no extra stack frame is created.

```
         ┌──── native block for pc=7  (a Char instruction) ──────────────────┐
         │  load byte at text[pos]                                            │
         │  cmp with 'a'                                                      │
         │  je  → block[8]           ← success path                          │
         │                                                                    │
         │  call bt_pop(...)         ← failure path                          │
         │  test rax, rax                                                     │
         │  je  → return −1                                                   │
         │  jmp rax                  ← tail-jump to saved block address       │
         └────────────────────────────────────────────────────────────────────┘
```

### 4.4 Capture-delta undo log

To keep bt stack pushes allocation-free, `Save` instructions use an
**undo-log** approach (the same strategy used by Onigmo and Oniguruma):

- **Before each slot write** the inlined `Save` block calls
  `jit_push_save_undo(ctx, slot, old_value)`, which pushes a tiny
  `BtJit::SaveUndo { slot: u32, old_value: u64 }` entry onto the bt stack
  (~16 bytes, no heap allocation).
- **`BtJit::Retry` no longer contains a `slots` field.**  It stores only
  `{ block_id: u32, pos: u64, keep_pos: u64 }` — 20 bytes, entirely on the
  stack.
- **On backtrack** (`jit_bt_pop`), entries are popped one-by-one: `SaveUndo`
  entries restore their slot, `AtomicBarrier` entries adjust `atomic_depth`,
  `MemoMark` entries record failures, and the first `Retry` entry restores
  `pos`/`keep_pos` and returns control to the saved block.
- **`jit_atomic_end`** (committing an atomic group) discards `SaveUndo`
  entries in the drain loop — the slots already hold the committed values.

### 4.5 Rust helpers for complex instructions

The following instructions are **not** compiled to inline Cranelift IR but
instead call back into Rust:

| Instruction group | Rust helper | Notes |
|-------------------|------------|-------|
| `AtomicStart` / `AtomicEnd` | `jit_atomic_start`, `jit_atomic_end` | Same drain logic as interpreter |
| `LookStart` / `LookEnd` | `jit_lookaround` | Calls `exec_lookaround`; sub-execution runs in interpreter (see §6) |
| `AbsenceStart` / `AbsenceEnd` | `jit_absence` | Calls `check_inner_in_range`; candidates pushed as `Bt::Retry` |
| `BackRef` | `jit_backref` | Case-fold comparison; returns new `pos` or -1 |
| `Prop` / `PropBack` | `jit_prop` | Unicode property check via `charset` module |
| `FoldSeq` / `FoldSeqBack` | `jit_fold_seq` | Multi-codepoint fold advance/retreat |

All helpers are `extern "C"` functions whose addresses are embedded into the
Cranelift module as external function symbols.

### 4.6 Memoization in JIT code

Memoization (Algorithms 5–7) is preserved:

- **Algorithm 5 (fork failures)**: At each `Fork`/`ForkNext` block, before
  pushing the bt entry, the JIT emits a call to `memo_check_fork(memo, pc,
  pos, atomic_depth) -> bool`; if `true`, jump to the fail path directly
  without pushing.  When `MemoMark` is consumed during `bt_pop`, the helper
  calls `memo_record_fork(memo, fork_pc, fork_pos, atomic_depth)`.
- **Algorithm 7 (depth-tagged failures)**: `atomic_depth` is maintained as a
  native integer register variable; incremented/decremented by the atomic
  helpers.
- **Algorithm 6 (lookaround cache)**: Handled entirely inside the
  `jit_lookaround` Rust helper, which uses the same `look_results` HashMap.

When `use_memo = false` (pattern contains backrefs / `CheckGroup`), all memo
calls are skipped — `memo_check_fork` returns `false` unconditionally and
`memo_record_fork` is a no-op.

---

## 5. Eligibility and Fallback

Not all `Vec<Inst>` programs are JIT-compiled.  The eligibility check runs
immediately after `compile()` returns a `CompiledProgram`:

```
JIT-ineligible if any of:
  • program contains BackRef, BackRefRelBack    → interpreter (memo disabled anyway)
  • program contains AbsenceStart / AbsenceEnd  → Phase 1: interpreter; Phase 2: JIT
  • program contains LookStart / LookEnd        → Phase 1: interpreter for sub-exec;
                                                   Phase 2: JIT outer + interpreter sub-exec
  • cranelift-jit is unavailable (feature gate) → interpreter
```

In all ineligible cases `Regex::new` stores the `CompiledProgram` as-is and
`find` uses the existing interpreter path.  No API change.

In Phase 1 (initial release) the eligibility criteria are conservative:

- Eligible: patterns whose programs contain only `Match`, `Char`, `AnyChar`,
  `Class`, `Shorthand`, `Anchor`, `Jump`, `Fork`, `ForkNext`, `Save`,
  `KeepStart`, `RetIfCalled`, `Call`, `AtomicStart`, `AtomicEnd`,
  `CheckGroup`, `LookStart`, `LookEnd`.
- Ineligible (Phase 1): `BackRef`, `AbsenceStart`, `FoldSeq` (deferred to
  Phase 2).

---

## 6. Lookaround Sub-Execution Strategy

Lookaround bodies (`LookStart`/`LookEnd`) are kept in the **interpreter** even
after the outer program is JIT-compiled.  This is because lookaround bodies
are small, executed infrequently relative to the outer loop, and have complex
recursion structure (depth tracking, isolated `State`).

The `jit_lookaround` helper:

1. Extracts the current `slots` / `keep_pos` / `call_stack` into a Rust-owned
   `State`.
2. Calls the existing `exec_lookaround(ctx, lk_pc, pos, &mut state, depth,
   memo)`.
3. On success, writes the updated `State` back through the pointer arguments
   and returns the post-lookaround `pos`.
4. On failure, returns -1 and the JIT failure path calls `bt_pop`.

This keeps the correctness proof of lookarounds entirely within the existing
interpreter code, at the cost of a function call per lookaround evaluation (no
change from the interpreter baseline).

---

## 7. `JitModule` and Lifetime

A `JitModule` wraps the Cranelift `JITModule` and holds:

```rust
struct JitModule {
    module:    cranelift_jit::JITModule,
    func_ptr:  unsafe extern "C" fn(/* … */) -> isize,
    // The JITModule owns the memory; func_ptr is valid for module's lifetime.
}
```

`JitModule` is stored inside `CompiledProgram` behind an `Option<Arc<JitModule>>`
(or `Option<Box<JitModule>>` for single-threaded use).  `Arc` allows `Regex`
to be `Clone` without recompiling.

`JITModule` is not `Send` by default; we wrap it in a `SendWrapper` guard
(checked at compile time via a `Sync` bound on `JitModule`), or alternatively
use `cranelift-object` to emit a `.o` file and mmap it — though this is more
complex.  The simplest approach for Phase 1 is to make `Regex` `!Send` when
the `jit` feature is enabled; this can be relaxed in Phase 2 using
`cranelift-jit`'s `unsafe impl Send` (the memory is immutable after
finalisation).

---

## 8. Integration with the Public API

No public API changes are required.  The selection between JIT and interpreter
is internal to `CompiledRegex::find`:

```rust
fn find(&self, text: &str, start_pos: usize) -> Option<Match> {
    // pre-filters (RequiredChar, StartStrategy) are unchanged
    // …
    #[cfg(feature = "jit")]
    if let Some(jit) = &self.jit_module {
        return find_jit(jit, text, start_pos, &self.program);
    }
    find_interp(/* … */)
}
```

`Regex::new` gains an extra step after `compile()`:

```rust
#[cfg(feature = "jit")]
let jit_module = jit::try_compile(&compiled_program);
```

If JIT compilation fails (unsupported instruction set, out of memory, or
ineligible program) the `Option` is `None` and the interpreter runs.

---

## 9. Implementation Plan

The work is divided into four phases.  Each phase delivers a working, tested,
merged increment.

### Phase 1 — Core JIT infrastructure (no lookarounds, no absence)

**Scope**: JIT compilation of programs that contain only: `Match`, `Char`,
`AnyChar`, `Class`, `Shorthand`, `Anchor`, `Jump`, `Fork`, `ForkNext`,
`Save`, `KeepStart`, `Call`, `RetIfCalled`, `AtomicStart`, `AtomicEnd`,
`CheckGroup`.

**Tasks**:

1. Add `cranelift-jit`, `cranelift-codegen`, `cranelift-frontend` as optional
   dependencies under the `jit` feature flag.
2. Create `src/jit.rs` with:
   - `JitModule` struct and `try_compile(prog: &CompiledProgram) -> Option<JitModule>`.
   - `is_eligible(prog: &CompiledProgram) -> bool`.
   - Cranelift IR builder: one `Block` per PC; translate eligible instructions.
   - `BtStack` JIT variant: `Vec<BtJit>` where `BtJit::Retry` stores a
     `*const u8` block address instead of `usize` pc.
3. Implement `extern "C"` helpers: `bt_pop`, `memo_check_fork`,
   `memo_record_fork`, `jit_backref` (stub — returns -1; Phase 1 patterns
   don't use it).
4. Integrate into `CompiledRegex::find` behind `#[cfg(feature = "jit")]`.
5. Add a test gate: run the full existing test suite twice — once without the
   feature flag, once with — and assert identical results.
6. Add a `bench_jit` benchmark group mirroring existing benchmarks to measure
   JIT vs. interpreter.

**Acceptance criteria**: all existing tests pass with `--features jit`; no
regression without the flag; JIT is measurably faster (≥ 1.5×) on the
`quantifier/greedy_match_500` and `literal/match_mid_1k` benchmarks.

---

### Phase 2 — Lookarounds and `FoldSeq`

**Scope**: Extend JIT eligibility to include `LookStart`/`LookEnd` and
`FoldSeq`/`FoldSeqBack`.

**Tasks**:

1. Implement `jit_lookaround` and `jit_look_cache_check` Rust helpers
   (described in §6).
2. Implement `jit_fold_seq` and `jit_fold_seq_back` helpers delegating to the
   existing `fold_advance` / `fold_retreat` functions.
3. Extend `is_eligible` to permit these instructions.
4. Extend the block-builder to emit `call_indirect` into the helpers for
   `LookStart` and `FoldSeq` blocks.
5. Re-run benchmarks and update `doc/BENCHMARKS.md`.

**Acceptance criteria**: `case_insensitive/match` benchmark improves; existing
lookaround integration tests pass under `--features jit`.

---

### Phase 3 — Absence operator

**Scope**: JIT-compile programs containing `AbsenceStart`/`AbsenceEnd`.

**Tasks**:

1. Implement `jit_absence` helper that:
   a. Calls `check_inner_in_range` (interpreter) to collect valid end
      positions.
   b. Pushes `BtJit::Retry` entries (with block addresses derived from the
      outer continuation's first block) onto the `BtStack`.
   c. Returns the address of the first (longest) candidate block.
2. Extend `is_eligible` to permit `AbsenceStart`/`AbsenceEnd`.
3. Add dedicated absence benchmarks.

**Acceptance criteria**: all absence-related integration tests pass under
`--features jit`.

---

### Phase 4 — Thread safety and release

**Scope**: Make `Regex` implement `Send + Sync` with the `jit` feature, and
finalise documentation.

**Tasks**:

1. Audit `JITModule` memory: after `module.finalize_definitions()` the
   generated code is immutable; add `unsafe impl Send for JitModule` and
   `unsafe impl Sync for JitModule` with a safety comment.
2. Store `JitModule` in `Arc<JitModule>` inside `CompiledProgram` so `Clone`
   does not recompile.
3. Add a `#[test] fn jit_is_send_sync()` compile-time assertion.
4. Update `doc/DESIGN.md` to remove the "No JIT" limitation note and add a
   cross-reference to this document.
5. Update `doc/BENCHMARKS.md` with final JIT vs. interpreter numbers across
   all benchmark groups.
6. Tag a `0.2.0` release.

---

## 10. Expected Performance Impact

The table below gives rough estimates based on PCRE2-JIT literature and the
structure of Aigumo's instruction set.  Actual numbers will be measured in
Phase 1 and updated in `doc/BENCHMARKS.md`.

| Benchmark | Interpreter | JIT (estimated) | Reason |
|-----------|-------------|-----------------|--------|
| `literal/match_mid_1k` | ~98 ns | ~30–50 ns | Char run compiled to tight cmp loop |
| `quantifier/greedy_match_500` | ~8 µs | ~2–4 µs | Fork/retry loop without dispatch |
| `pathological/n=20` | ~18 µs (memo) | ~6–10 µs | Same O(n²) algo, less per-step cost |
| `email/find_all` | ~3.3 µs | ~1–2 µs | Many Fork/Char; JIT fuses them |
| `case_insensitive/match` (Phase 2) | ~14 µs | ~5–8 µs | `FoldSeq` inlined via helper |

Patterns containing `BackRef` are unaffected (interpreter used).

---

## 11. File Layout After Implementation

```
src/
  jit.rs           ← new: JIT compiler (feature-gated)
  jit/
    builder.rs     ← new: Cranelift IR construction
    helpers.rs     ← new: extern "C" Rust helpers
    bt_stack.rs    ← new: JIT backtrack stack types
  lib.rs           ← add: JitModule field in CompiledProgram; dispatch in find()
  vm.rs            ← unchanged (interpreter remains as fallback)
doc/
  JIT.md           ← this file
  DESIGN.md        ← updated: remove "No JIT" note; add cross-reference
  BENCHMARKS.md    ← updated: Phase 1 & 4 results added
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
