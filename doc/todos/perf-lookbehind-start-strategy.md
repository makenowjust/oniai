# TODO: Skip lookbehind prefix in StartStrategy::compute

## Status: Done (commit `mvnvmkpr`)

## Problem

Patterns that begin with a lookbehind assertion (e.g. `(?<=\. )[A-Z]\w+`) fall
back to `StartStrategy::Anywhere` today, causing the engine to invoke `exec`/JIT
at **every** byte-aligned position in the haystack.

The root cause is that `StartStrategy::compute` skips only `Save` and `KeepStart`
instructions when probing the first "real" instruction.  When it encounters
`LookStart`, it does not know how to skip past the entire lookbehind body; the
`collect_first_chars` walker also returns `None` for `LookStart`.  Both paths
therefore fall through to `Anywhere`.

### Benchmark evidence

```
lookbehind/word_after_period   oniai/jit = 27.3 ms   pcre2 = 618 µs   (44× gap)
```

Pattern: `(?<=\. )[A-Z]\w+`, haystack: `benches/fixtures/stud.txt` (~580 KB).

With `Anywhere`, the engine tries ~580,000 positions.  With `AsciiClassStart`
(the strategy that would be chosen for `[A-Z]` if not obstructed by the leading
`LookStart`), the engine would scan to uppercase letters only — a small fraction
of positions in typical prose — and check the lookbehind only at those positions.

## Proposed Solution

### 1. Skip `LookStart` blocks when probing the first real instruction

In `StartStrategy::compute`, after the existing `Save`/`KeepStart` skip loop,
also skip `LookStart { positive, end_pc }` blocks:

```rust
// existing skip of Save/KeepStart
let mut pc = 0;
loop {
    match prog.get(pc) {
        Some(Inst::Save(_)) | Some(Inst::KeepStart) => pc += 1,
        // NEW: skip entire lookbehind body
        Some(Inst::LookStart { end_pc, .. }) => pc = end_pc + 1,
        _ => break,
    }
}
```

This handles any number of consecutive lookbehind (or lookahead) prefixes.

### 2. Apply the same skip in `collect_first_chars`

Add a `LookStart` arm that jumps past the body:

```rust
Inst::LookStart { end_pc, .. } => {
    collect_first_chars(prog, end_pc + 1, out, visited)?;
}
```

### 3. Correctness argument

Skipping a lookbehind prefix is safe for start-strategy purposes because:

- For **positive** lookbehind `(?<=X)Y`: a match at position P requires both X
  (ending at P) and Y (starting at P) to hold.  If Y's first instruction
  rejects P, the overall match fails regardless of X.  So we can use Y's first
  chars to skip positions safely.
- For **negative** lookbehind `(?<!X)Y`: same reasoning — Y must still match at
  P (the lookbehind only gates whether X holds before P).
- For **lookahead** (LookDir::Ahead): both positive (`(?=X)Y`) and negative
  (`(?!X)Y`) have Y starting at the same position, so skipping to Y's first
  instruction is equally valid.

The actual lookaround check still runs inside `exec`/JIT at each candidate
position — we only skip the start scan step.

## Expected Benchmark Impact

| Benchmark | Current | Expected |
|-----------|---------|----------|
| `lookbehind/word_after_period/jit` | 27.3 ms | ~1–3 ms (−90%) |
| `lookbehind/word_after_period/interp` | ~similar | similar gain |
| `lookahead/word_before_comma/jit` | 24.3 ms | ~1–5 ms (−80%) |

## Implementation Steps

1. [ ] In `StartStrategy::compute`, extend the leading-instruction skip loop to
       also jump over `LookStart { end_pc, .. }` blocks.
2. [ ] In `collect_first_chars`, add a `Inst::LookStart { end_pc, .. }` arm
       that recurses at `end_pc + 1`.
3. [ ] Add integration tests:
       - `(?<=\. )[A-Z]\w+` matches "Hello World. Foo" at correct positions.
       - `(?<!#)[a-z]+` skips `#abc` but matches bare `abc`.
4. [ ] Run `cargo test` and `cargo clippy --tests`.
5. [ ] Run `cargo bench -- lookbehind` and save log.

## Benchmark Results

Log: `log/bench-lookbehind-start-strategy-2026-03-02.txt`

| Benchmark | Before | After | Δ |
|-----------|--------|-------|---|
| `lookbehind/word_after_period/jit` | 27.3 ms | **579 µs** | **−97.9% (47×)** |
| `lookbehind/word_after_period/interp` | ~similar | **613 µs** | **−97.8% (43×)** |
| `lookahead/word_before_comma/jit` | 24.3 ms | 18.7 ms | −23% (benchmark re-eval noise) |

The lookbehind gap vs pcre2 closed from **44×** to **1.37×** (579 µs vs 422 µs).

### What changed

- `StartStrategy::compute`: the `pc` skip loop now also jumps past `LookStart`
  blocks (to `end_pc + 1`), allowing `AsciiClassStart` or `FirstChars` to be
  selected based on the continuation's first instruction.
- `collect_first_chars`: added `Inst::LookStart { end_pc }` arm that recurses
  at `end_pc + 1`, enabling `FirstChars` for patterns like `(?<=@)\w+`.
- 4 new integration tests added in `tests/integration_test.rs`.
