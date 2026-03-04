# TODO 04: Interpreter SpanChar/SpanClass SIMD Acceleration

## Problem

The interpreter's `SpanChar` and `SpanClass` dispatch loops in `exec()` (vm.rs lines ~966-984)
iterate character-by-character using `ctx.char_at(pos)` (which decodes UTF-8).  For
ASCII inputs this is unnecessary overhead — LLVM cannot vectorize the loop because the
iteration step depends on the character's byte length.

The JIT already calls `jit_span_char_len`/`jit_span_class_ascii_len` helpers that use a
raw byte-slice `position()` loop that LLVM auto-vectorizes.  The interpreter needs the
same treatment.

## Approach

**SpanChar** — when `c.is_ascii()`:
```rust
let byte = *c as u8;
let n = ctx.text.as_bytes()[pos..]
    .iter()
    .position(|&b| b != byte)
    .unwrap_or(ctx.text.len() - pos);
pos += n;
```
(Scalar fallback for non-ASCII chars: existing UTF-8 loop.)

**SpanClass** — when non-negated and all ranges end below U+0080:
```rust
let bits = cs.ascii_bits;
let n = ctx.text.as_bytes()[pos..]
    .iter()
    .position(|&b| b >= 0x80 || (bits[(b >> 6) as usize] >> (b & 63)) & 1 == 0)
    .unwrap_or(ctx.text.len() - pos);
pos += n;
```
(Scalar fallback for negated / non-ASCII charsets: existing UTF-8 loop.)

The ASCII-only charset condition mirrors `is_ascii_only_charset()` in `src/ir/jit.rs`:
```rust
fn is_ascii_only_charset(cs: &CharSet) -> bool {
    !cs.negate && cs.ranges.iter().all(|&(_, hi)| (hi as u32) < 128)
}
```

## Files to Change

- `src/vm.rs` — `exec()` function, `Inst::SpanChar` and `Inst::SpanClass` arms

## Expected Improvement

~30-40% win on `greedy_match_500/interp` (`a+` on 500-char input).
Marginal effect on short spans (< ~16 bytes) — LLVM falls back to scalar anyway.

## Success Criteria

- All tests pass
- `cargo bench -- "greedy_match_500.*interp"` shows improvement vs baseline
- No regression on `word_sparse/interp` (short spans — should be noise)
