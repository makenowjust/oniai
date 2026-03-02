# TODO: JIT — inline bitmap check for `Class` instruction

## Status: Done (commit `ptvvwopv`)

## Problem

`inline_charclass_fwd` (and its backward counterpart) previously emitted a
cascade of unsigned range checks for ASCII bytes, growing with the number of
ASCII ranges in the charset.

## Solution Implemented

- Added `emit_ascii_bitmap_check()` using Cranelift `select` + shift + AND
  (branch-free 3-op sequence).
- Added `emit_ascii_check()` dispatcher: use bitmap for ≥3 ASCII ranges,
  range cascade for ≤2 ranges (break-even threshold determined empirically).
- Restored `emit_ascii_ranges_check` / `emit_range_check` / `emit_eq_check`
  as fallbacks for simple charsets (`[0-9]`, `[a-zA-Z]`).
- Updated both `inline_charclass_fwd` and `inline_charclass_back`.

## Benchmark Results

| Benchmark | Change |
|-----------|--------|
| `charclass/posix_digit_iter/jit` | -4% |
| `captures/iter_all/jit` | -1.7% |
| `[0-9]` / `[a-zA-Z]` patterns | no regression |

Log: `log/bench-jit-inline-bitmap-v2-2026-03-02.txt`


## Problem

`inline_charclass_fwd` (and its backward counterpart) currently emits a cascade
of unsigned range checks for ASCII bytes:

```
\w  →  (byte-48)≤9 | (byte-65)≤25 | byte==95 | (byte-97)≤25
        = 4 range checks + 3 BOR = ~16 JIT instructions
```

Every `Class` instruction in a tight loop (e.g. `\w+`, `\d+`) executes this
cascade on every character.  The branch predictor can mis-predict the OR chain,
and code size grows with the number of ASCII ranges.

The `CharSet` struct already stores a precomputed `ascii_bits: [u64; 2]`
128-bit bitmap after the recent bitmap commit.  Embedding these two `u64`
constants directly in the JIT code reduces the ASCII path to:

```
if byte < 128:
    word  = ascii_bits[byte >> 6]   ; 1 load (constant)
    bit   = word >> (byte & 63)     ; 1 shift
    ok    = bit & 1                 ; 1 AND
```

That is 3 JIT operations instead of up to 16+ for large charsets.

## Proposed Solution

### Replace `emit_ascii_ranges_check` with `emit_ascii_bitmap_check`

```rust
/// Emit: `(ascii_bits[byte >> 6] >> (byte & 63)) & 1 != 0`
/// `byte` must be an I32 value known to be < 128.
/// `bits` is the precomputed [u64; 2] from `cs.ascii_bits`.
fn emit_ascii_bitmap_check(
    builder: &mut FunctionBuilder<'_>,
    byte: Value,      // I32, value 0..127
    bits: [u64; 2],
) -> Value {
    // word_idx = byte >> 6  (0 or 1)
    let shift6 = builder.ins().iconst(types::I32, 6);
    let word_idx = builder.ins().ushr(byte, shift6);
    let word_idx64 = builder.ins().uextend(types::I64, word_idx);

    // Load the correct u64 word from an inline table.
    // Embed as two separate constants and select:
    let w0 = builder.ins().iconst(types::I64, bits[0] as i64);
    let w1 = builder.ins().iconst(types::I64, bits[1] as i64);
    let one = builder.ins().iconst(types::I64, 1);
    let word = builder.ins().select(
        builder.ins().icmp(IntCC::Equal, word_idx64,
            builder.ins().iconst(types::I64, 0)),
        w0, w1
    );

    // bit_idx = byte & 63
    let mask63 = builder.ins().iconst(types::I32, 63);
    let bit_idx = builder.ins().band(byte, mask63);
    let bit_idx64 = builder.ins().uextend(types::I64, bit_idx);

    // ok = (word >> bit_idx) & 1
    let shifted = builder.ins().ushr(word, bit_idx64);
    builder.ins().band(shifted, one)
}
```

Cranelift's `select` lowers to a `cmov` on x86-64, so this is branch-free.

### Update `inline_charclass_fwd` / `inline_charclass_back`

Replace the `emit_ascii_ranges_check(builder, ascii_ranges, byte_p)` call with
`emit_ascii_bitmap_check(builder, byte_p, cs.ascii_bits)`.

The `negate` handling stays the same (XOR with 1 for negated charsets).

### Remove `charset_ascii_ranges` and `emit_ascii_ranges_check`

These helpers are no longer needed once all callers are updated.

## Expected Benchmark Impact

For JIT paths using charsets with many ASCII ranges (e.g. `\w` = 4 ranges,
`[[:alpha:]]` = 2 ranges, `[[:print:]]` = many):

- Reduced JIT code size → better i-cache utilization.
- Fewer branch instructions in the tight inner loop → fewer mispredictions.
- Estimated improvement: **5–15%** for Class-dominated JIT benchmarks
  (`charclass/alpha_iter/jit`, `find_iter_scale/jit`, `captures/jit`).

The non-ASCII path is unchanged (still calls `jit_match_class` helper).

## Implementation Steps

1. [ ] Add `emit_ascii_bitmap_check` function to `jit/builder.rs`.
2. [ ] Update `inline_charclass_fwd` to call `emit_ascii_bitmap_check` instead
       of `emit_ascii_ranges_check`.
3. [ ] Update `inline_charclass_back` similarly.
4. [ ] Remove (or `#[allow(dead_code)]`) `charset_ascii_ranges` and
       `emit_ascii_ranges_check` if no longer used.
5. [ ] Run `cargo test` + `cargo clippy --tests`.
6. [ ] Run `cargo bench -- oniai` and save log to `log/`.
