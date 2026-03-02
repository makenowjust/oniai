# TODO: Reduce capture-group overhead in JIT

## Status: Done (Option A implemented)

## Problem

Patterns with capture groups are significantly slower than competing engines:

```
captures/two_groups   oniai/jit = 770 ns   pcre2 = 193 ns   regex = 258 ns   (4×)
captures/iter_all     oniai/jit = 2990 ns  pcre2 = 932 ns   regex = 142 ns   (21× vs regex)
```

Pattern: `(\w+)\s+(\w+)` — two capture groups.

### Root cause

Every `Save(slot)` instruction in the interpreter writes to `state.slots: Vec<Option<usize>>`.
In the JIT, `Save` calls the `h_save` helper which performs a bounds-checked
heap write.  On backtrack, `state.slots` is restored from the saved copy inside
each `Bt::Retry` frame (clone of the slots Vec).

This means every fork point:
1. Clones the full `slots` Vec (heap allocation or at least a `memcpy`).
2. On backtrack, restores from the clone (another `memcpy`).

For `(\w+)\s+(\w+)` on a short string like `"hello world"`:
- The pattern executes ~20–50 Save instructions and ~5–10 backtrack restores.
- Each restore copies `slots` (at minimum 6 `Option<usize>` = 48 bytes).

### Why `regex` is 21× faster on `iter_all`

The `regex` crate uses a lazy-NFA / hybrid DFA: it runs a DFA for the `find`
step (locating match boundaries), then only runs the NFA for capture extraction
when captures are explicitly requested.  The DFA step has no per-position
allocation.

## Proposed Solutions

### Option A: Fixed-size capture array on the stack ✅ DONE

Replace `Vec<Option<usize>>` with a small fixed-capacity array for patterns
with ≤ N capture groups (e.g. N = 8).  Use an `enum` that holds either a
`[usize; N]` or a heap-backed `Vec` for larger patterns.

Eliminates heap allocation for the common case; clone becomes a memcpy.

Implementation: `SmallSlots` enum with `Inline { len: u16, data: [usize; 18] }`
(18 = group 0 + 8 capture groups + some headroom) and `Heap(Vec<usize>)`.
Encoded as plain `usize` with `usize::MAX` = None (halves the per-slot size
vs `Option<usize>`).

### Option B: Copy-on-write slot array (medium effort)

Replace `Vec<Option<usize>>` with a reference-counted `Arc<Vec<...>>` plus a
"dirty" flag.  On fork, clone the `Arc` (cheap reference bump) rather than the
Vec.  Only copy-on-write when a `Save` actually modifies the Vec.

For patterns where most branches don't save (e.g. `\w+` inner loop), this
amortizes allocation cost significantly.  Downside: Arc overhead on `Save`.

### Option C: JIT-inline captures into registers (high effort)

For JIT-compiled patterns with ≤ 4 capture groups, allocate Cranelift variables
for the capture slots and pass them as additional function parameters /
stack-allocated locals.  Eliminate the `h_save` call entirely; emit
`stack_store` / `stack_load` directly.

This is the most impactful change but requires significant JIT builder changes.
Cranelift's SSA makes it non-trivial to handle backtrack restore (captures must
be rolled back on failure).

### Option D: Two-pass match/capture (medium effort)

Implement a "find-then-capture" two-pass strategy:
1. Run `find` (no capture tracking) to locate match boundaries quickly.
2. Re-run exec only over the matched slice, collecting captures.

For `find_iter` / `captures_iter`, this means the scan phase has zero capture
overhead.  Only the per-match re-run has capture cost.  Effective when matches
are rare relative to haystack size (typical case).

This is architecturally the same trick the `regex` crate uses and would close
the 21× `iter_all` gap.

## Actual Benchmark Results (Option A — `SmallSlots`)

From `log/bench-smallslots-2026-03-02.txt`:

```
captures/two_groups/oniai/jit    770 ns → 544 ns   −30%
captures/two_groups/oniai/interp 765 ns → 527 ns   −31%
captures/iter_all/oniai/jit     2990 ns → 2012 ns  −33%
captures/iter_all/oniai/interp  2962 ns → 1968 ns  −34%
```

Gap vs competitors (after SmallSlots):
- `two_groups/jit` 544 ns vs regex 199 ns (2.7×, was 4×)
- `iter_all/jit` 2012 ns vs regex 109 ns (18×, was 21×)

## Recommended Next Step

Option D (two-pass match/capture) would further reduce the `iter_all` gap
from 18× to ~3× by skipping capture tracking during the scan phase.

## Implementation Steps

### Option A
1. [x] Define `SmallSlots` type: inline `[usize; 18]` + overflow `Vec`.
2. [x] Replace `state.slots: Vec<Option<usize>>` with `SmallSlots`.
3. [x] Update all `slots` accesses (`Save`, `BackRef`, capture extraction).
4. [x] Update `Bt::Retry` saved-state representation.
5. [x] Run `cargo test` and `cargo clippy --tests`.
6. [x] Run `cargo bench -- captures` and save log.


## Problem

Patterns with capture groups are significantly slower than competing engines:

```
captures/two_groups   oniai/jit = 770 ns   pcre2 = 193 ns   regex = 258 ns   (4×)
captures/iter_all     oniai/jit = 2990 ns  pcre2 = 932 ns   regex = 142 ns   (21× vs regex)
```

Pattern: `(\w+)\s+(\w+)` — two capture groups.

### Root cause

Every `Save(slot)` instruction in the interpreter writes to `state.slots: Vec<Option<usize>>`.
In the JIT, `Save` calls the `h_save` helper which performs a bounds-checked
heap write.  On backtrack, `state.slots` is restored from the saved copy inside
each `Bt::Retry` frame (clone of the slots Vec).

This means every fork point:
1. Clones the full `slots` Vec (heap allocation or at least a `memcpy`).
2. On backtrack, restores from the clone (another `memcpy`).

For `(\w+)\s+(\w+)` on a short string like `"hello world"`:
- The pattern executes ~20–50 Save instructions and ~5–10 backtrack restores.
- Each restore copies `slots` (at minimum 6 `Option<usize>` = 48 bytes).

### Why `regex` is 21× faster on `iter_all`

The `regex` crate uses a lazy-NFA / hybrid DFA: it runs a DFA for the `find`
step (locating match boundaries), then only runs the NFA for capture extraction
when captures are explicitly requested.  The DFA step has no per-position
allocation.

## Proposed Solutions

### Option A: Fixed-size capture array on the stack (medium effort)

Replace `Vec<Option<usize>>` with a small fixed-capacity array for patterns
with ≤ N capture groups (e.g. N = 8).  Use an `enum` that holds either a
`[Option<usize>; 8]` or a heap-backed `Vec` for larger patterns.

Eliminates heap allocation for the common case; clone becomes a memcpy of 64 bytes.

### Option B: Copy-on-write slot array (medium effort)

Replace `Vec<Option<usize>>` with a reference-counted `Arc<Vec<...>>` plus a
"dirty" flag.  On fork, clone the `Arc` (cheap reference bump) rather than the
Vec.  Only copy-on-write when a `Save` actually modifies the Vec.

For patterns where most branches don't save (e.g. `\w+` inner loop), this
amortizes allocation cost significantly.  Downside: Arc overhead on `Save`.

### Option C: JIT-inline captures into registers (high effort)

For JIT-compiled patterns with ≤ 4 capture groups, allocate Cranelift variables
for the capture slots and pass them as additional function parameters /
stack-allocated locals.  Eliminate the `h_save` call entirely; emit
`stack_store` / `stack_load` directly.

This is the most impactful change but requires significant JIT builder changes.
Cranelift's SSA makes it non-trivial to handle backtrack restore (captures must
be rolled back on failure).

### Option D: Two-pass match/capture (medium effort)

Implement a "find-then-capture" two-pass strategy:
1. Run `find` (no capture tracking) to locate match boundaries quickly.
2. Re-run exec only over the matched slice, collecting captures.

For `find_iter` / `captures_iter`, this means the scan phase has zero capture
overhead.  Only the per-match re-run has capture cost.  Effective when matches
are rare relative to haystack size (typical case).

This is architecturally the same trick the `regex` crate uses and would close
the 21× `iter_all` gap.

## Recommended Approach

Implement **Option A** (fixed-size stack array) as the immediate win, then
evaluate **Option D** (two-pass) if the iter_all gap remains large.

## Expected Benchmark Impact

| Benchmark | Current | Option A | Option D |
|-----------|---------|----------|----------|
| `captures/two_groups/jit` | 770 ns | ~400 ns (−48%) | ~300 ns (−61%) |
| `captures/iter_all/jit` | 2990 ns | ~1800 ns (−40%) | ~500 ns (−83%) |

## Implementation Steps

### Option A
1. [ ] Define `SmallSlots` type: inline `[Option<usize>; 8]` + overflow `Vec`.
2. [ ] Replace `state.slots: Vec<Option<usize>>` with `SmallSlots`.
3. [ ] Update all `slots` accesses (`Save`, `BackRef`, capture extraction).
4. [ ] Update `Bt::Retry` saved-state representation.
5. [ ] Run `cargo test` and `cargo clippy --tests`.
6. [ ] Run `cargo bench -- captures` and save log.
