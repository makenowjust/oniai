# Optimization 2: SIMD span acceleration

## Problem

The IR span pass emits `IrTerminator::SpanChar` and `IrTerminator::SpanClass`
terminators. The IR JIT (`src/ir/jit.rs`) compiles them as tight Cranelift loops
that check one byte at a time. For dense patterns like `[[:digit:]]+` over a
1 MB text, this is 9.7× slower than `regex` (which uses SIMD DFA scanning).

The key insight: **span loops are the hottest code in character-class and
digit-heavy patterns**. Replacing the per-byte Cranelift loop with a call to a
SIMD-accelerated helper eliminates the most expensive inner loop.

## Benchmark targets

| Benchmark | oniai/jit | regex | gap |
|-----------|----------:|------:|-----|
| `real_world/posix_digits` | 121 µs | 12.6 µs | 9.7× |
| `charclass/alpha_iter` | 2.25 µs | — | — |
| `charclass/posix_digit_iter` | 2.0 µs | — | — |

## Approach

### Step 1 — SIMD helper functions in `src/jit/helpers.rs`

Add two new `extern "C"` helpers:

```rust
/// Returns the number of consecutive bytes at text[pos..] that equal `byte`.
/// Uses memchr to find the first non-matching byte.
pub extern "C" fn jit_span_char(
    text_ptr: *const u8,
    text_len: u64,
    pos: u64,
    byte: u32,
) -> u64 {
    let slice = unsafe { std::slice::from_raw_parts(text_ptr, text_len as usize) };
    let haystack = &slice[pos as usize..];
    // Find the first byte != `byte` (i.e., the end of the span).
    // We invert: find first byte that is NOT equal.
    // Strategy: scan manually; LLVM will vectorize this loop.
    let end = haystack.iter().position(|&b| b != byte as u8)
        .unwrap_or(haystack.len());
    end as u64
}

/// Returns the number of consecutive bytes at text[pos..] that are in the
/// ASCII class defined by `ascii_bits` (256-bit bitmap, 4× u64).
/// Non-ASCII bytes (>= 0x80) stop the span.
pub extern "C" fn jit_span_class_ascii(
    text_ptr: *const u8,
    text_len: u64,
    pos: u64,
    bits_ptr: *const u64,  // points to [u64; 4]
) -> u64 {
    let slice = unsafe { std::slice::from_raw_parts(text_ptr, text_len as usize) };
    let bits = unsafe { std::slice::from_raw_parts(bits_ptr, 4) };
    let haystack = &slice[pos as usize..];
    let end = haystack.iter().position(|&b| {
        b >= 0x80 || (bits[(b >> 6) as usize] >> (b & 63)) & 1 == 0
    }).unwrap_or(haystack.len());
    end as u64
}
```

For `SpanChar` where the char is ASCII, `jit_span_char` can be further improved
by using `memchr` to find the first non-matching byte:

```rust
// Invert: find first byte that is NOT `byte`.
// memchr finds the byte; we want the complement.
// Use a small wrapper: find min of all non-byte positions.
```

However, since memchr finds a specific byte (not the complement), use a manual
SIMD-style scan instead. LLVM reliably auto-vectorizes `position(|&b| b != target)`
for contiguous slices. Measure and tune.

For `SpanClass`, store the 256-bit bitmap (`[u64; 4]`) in the JIT module's read-only
data section and pass a pointer. This avoids re-encoding the bitmap on every call.

### Step 2 — Embed bitmap constants in IR JIT

In `emit_ir_function` (`src/ir/jit.rs`), before emitting blocks, for each
`SpanClass` terminator in the program:
1. Serialize the charset's full 256-bit bitmap as `[u64; 4]`.
2. Store it as a constant in a `cranelift_module::DataContext` (read-only section).
3. Record the `DataId` → `GlobalValue` in a per-region table.

At emit time for `SpanClass { id, exit }`, emit:
```
pos_after = call jit_span_class_ascii(text_ptr, text_len, pos, bitmap_gv)
pos = pos_after_base + pos_after   // absolute position
jump exit
```

### Step 3 — Replace SpanChar Cranelift loop with helper call

For `SpanChar { c, exit }` where `c.is_ascii()`:
```
span_len = call jit_span_char(text_ptr, text_len, pos, c as u32)
pos = pos + span_len
jump exit
```

For non-ASCII chars, keep the existing byte-by-byte Cranelift loop (rare case).

### Step 4 — Non-ASCII SpanClass fallback

If the charset can match non-ASCII codepoints, the helper only handles the ASCII
prefix of the span. After `jit_span_class_ascii` returns, check if `text[pos]`
is non-ASCII and continue with the existing Cranelift loop for multi-byte chars.
Most real-world use cases (`\d`, `\w`, `[A-Z]`, `[[:alpha:]]`) are ASCII-only and
will hit the fast path entirely.

### Step 5 — Interpreter span loops (optional, lower priority)

The interpreter in `src/vm.rs` has no equivalent of `SpanChar`/`SpanClass` — it
uses the standard NFA execution. This optimization is JIT-only. Mark as
future work if interpreter span loops are added later.

## Files to change

- `src/jit/helpers.rs` — add `jit_span_char`, `jit_span_class_ascii`
- `src/ir/jit.rs` — replace `SpanChar`/`SpanClass` emit with helper calls;
  embed bitmap constants in data section

## Success criterion

- `real_world/posix_digits/jit` improves significantly (target: < 50 µs)
- `charclass/posix_digit_iter/jit` and `charclass/alpha_iter/jit` improve
- No regressions on other JIT benchmarks
- All 138 tests pass, zero clippy warnings
