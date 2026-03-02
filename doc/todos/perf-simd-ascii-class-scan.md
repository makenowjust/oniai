# TODO: SIMD-accelerated AsciiClassStart scanning

## Status: Done (Option B implemented; Option A pending)

## Problem

`StartStrategy::AsciiClassStart` currently scans the haystack one byte at a
time, performing a 128-bit bitmap lookup per byte to decide whether a position
is a match candidate.  For patterns like `[[:digit:]]+` or `\w+` on large
real-world haystacks, this scalar scan is the dominant cost.

The `regex` crate uses SIMD to process 16–32 bytes per cycle during the
scanning phase, giving it a structural advantage on long haystacks.

### Benchmark evidence

```
real_world/posix_digits     oniai/jit = 138 µs   regex = 12.7 µs   (11× gap)
class_start/word_sparse     oniai/jit =  2.6 ms   pcre2 =  1.1 ms    (2.4× gap)
```

Both patterns use `AsciiClassStart`; the gap tracks haystack size.

## Proposed Solutions

Two complementary approaches, ordered by implementation effort:

### Option A: 8-byte chunk scan (pure safe Rust, ~2–4×)

For ASCII-only char classes (`can_match_non_ascii == false`), process 8 bytes
at a time by loading a `u64` word and checking each byte with unrolled bitmap
lookups.  If none of the 8 bytes hits the bitmap, advance 8 positions without
calling `exec`.

```rust
// Fast path: whole 8-byte chunks with no non-ASCII bytes
while pos + 8 <= bytes.len() {
    let word = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
    if word & 0x8080_8080_8080_8080 != 0 {
        break; // contains non-ASCII — fall through to byte loop
    }
    // Unrolled bitmap check for all 8 bytes
    let any = (0..8).any(|i| {
        let b = (word >> (8 * i)) as u8;
        (ascii_bits[(b >> 6) as usize] >> (b & 63)) & 1 != 0
    });
    if any { break; }
    pos += 8;
}
// byte-by-byte for remainder / non-ASCII
```

LLVM should auto-vectorize the inner `any` loop.

### Option B: Range-check specialization ✅ DONE

Detect at `StartStrategy::compute` time whether the `ascii_bits` bitmap
represents a simple contiguous byte range `[lo, hi]` (e.g. digits `0x30–0x39`,
uppercase `0x41–0x5A`).  If so, store `(lo, hi)` and use a range subtract:

```rust
// b.wrapping_sub(lo) <= (hi - lo) is branch-free
```

This compiles to a 2-instruction range check that LLVM auto-vectorizes
aggressively — typically achieving 16–32 bytes/cycle with AVX2.

Common classes that benefit: `[0-9]`, `[A-Z]`, `[a-z]`, `[A-Za-z]` (two ranges).

### Option C: Precomputed `memchr3` prefetch (low effort, partial win)

For classes with 1–3 representative bytes (e.g. the three digit chars `'0'`,
`'5'`, `'9'`), use `memchr::memchr3` to find the next candidate quickly, then
fall back to the bitmap for verification.  Introduces false positives (need to
re-check bitmap) but reduces long stretches of no-match scanning.

This is already done for `StartStrategy::FirstChars`; we would apply the same
idea to small `AsciiClassStart` classes.

## Recommended Approach

Implement **Option B** first (range detection covers the most common benchmark
patterns: digits, alpha), then add **Option A** as a fallback for irregular
bitmaps.

## Actual Benchmark Results (Option B — `RangeStart`)

From `log/bench-rangestart-2026-03-02.txt`:

```
charclass/posix_digit_iter/oniai/jit   2.65 µs → 1.96 µs   −26%
charclass/posix_digit_iter/oniai/interp 3.07 µs → 2.23 µs  −27%
real_world/posix_digits/oniai/jit      138 µs  → 138 µs     0%  (JIT exec dominates)
real_world/posix_digits/oniai/interp   168 µs  → 138 µs    −18%
```

Note: `real_world/posix_digits/jit` shows no improvement because the JIT
execution time dominates over the scan overhead for this 260 KB haystack.
The interp path improved −18%, confirming the scan optimization is correct.

## Implementation Steps

1. [x] Add `RangeStart { lo: u8, hi: u8 }` variant to `StartStrategy` (for Option B).
2. [x] In `StartStrategy::compute`, detect single contiguous range in `ascii_bits`
       and emit `RangeStart` instead of `AsciiClassStart`.
3. [x] Add `RangeStart` scan arms in `find_with_scratch` and `find_interp`
       using `b.wrapping_sub(lo) <= (hi - lo)`.
4. [ ] Implement the 8-byte chunk scan (Option A) inside the existing
       `AsciiClassStart` arm as a fast prefix loop.
5. [x] Add unit tests for range detection (digits, alpha, alnum).
6. [x] Run `cargo test` and `cargo clippy --tests`.
7. [x] Run `cargo bench -- "class_start|posix_digits|real_world"` and save log.


## Problem

`StartStrategy::AsciiClassStart` currently scans the haystack one byte at a
time, performing a 128-bit bitmap lookup per byte to decide whether a position
is a match candidate.  For patterns like `[[:digit:]]+` or `\w+` on large
real-world haystacks, this scalar scan is the dominant cost.

The `regex` crate uses SIMD to process 16–32 bytes per cycle during the
scanning phase, giving it a structural advantage on long haystacks.

### Benchmark evidence

```
real_world/posix_digits     oniai/jit = 138 µs   regex = 12.7 µs   (11× gap)
class_start/word_sparse     oniai/jit =  2.6 ms   pcre2 =  1.1 ms    (2.4× gap)
```

Both patterns use `AsciiClassStart`; the gap tracks haystack size.

## Proposed Solutions

Two complementary approaches, ordered by implementation effort:

### Option A: 8-byte chunk scan (pure safe Rust, ~2–4×)

For ASCII-only char classes (`can_match_non_ascii == false`), process 8 bytes
at a time by loading a `u64` word and checking each byte with unrolled bitmap
lookups.  If none of the 8 bytes hits the bitmap, advance 8 positions without
calling `exec`.

```rust
// Fast path: whole 8-byte chunks with no non-ASCII bytes
while pos + 8 <= bytes.len() {
    let word = u64::from_le_bytes(bytes[pos..pos+8].try_into().unwrap());
    if word & 0x8080_8080_8080_8080 != 0 {
        break; // contains non-ASCII — fall through to byte loop
    }
    // Unrolled bitmap check for all 8 bytes
    let any = (0..8).any(|i| {
        let b = (word >> (8 * i)) as u8;
        (ascii_bits[(b >> 6) as usize] >> (b & 63)) & 1 != 0
    });
    if any { break; }
    pos += 8;
}
// byte-by-byte for remainder / non-ASCII
```

LLVM should auto-vectorize the inner `any` loop.

### Option B: Range-check specialization (targeted, medium effort)

Detect at `StartStrategy::compute` time whether the `ascii_bits` bitmap
represents a simple contiguous byte range `[lo, hi]` (e.g. digits `0x30–0x39`,
uppercase `0x41–0x5A`).  If so, store `(lo, hi)` and use a range subtract:

```rust
// b.wrapping_sub(lo) <= (hi - lo) is branch-free
```

This compiles to a 2-instruction range check that LLVM auto-vectorizes
aggressively — typically achieving 16–32 bytes/cycle with AVX2.

Common classes that benefit: `[0-9]`, `[A-Z]`, `[a-z]`, `[A-Za-z]` (two ranges).

### Option C: Precomputed `memchr3` prefetch (low effort, partial win)

For classes with 1–3 representative bytes (e.g. the three digit chars `'0'`,
`'5'`, `'9'`), use `memchr::memchr3` to find the next candidate quickly, then
fall back to the bitmap for verification.  Introduces false positives (need to
re-check bitmap) but reduces long stretches of no-match scanning.

This is already done for `StartStrategy::FirstChars`; we would apply the same
idea to small `AsciiClassStart` classes.

## Recommended Approach

Implement **Option B** first (range detection covers the most common benchmark
patterns: digits, alpha), then add **Option A** as a fallback for irregular
bitmaps.

## Expected Benchmark Impact

| Benchmark | Current | Expected |
|-----------|---------|----------|
| `real_world/posix_digits/jit` | 138 µs | 20–40 µs (−70–85%) |
| `class_start/word_sparse/jit` | 2.6 ms | 1.0–1.5 ms (−40–60%) |
| `charclass/posix_digit_iter/jit` | 2.6 µs | 1.5–2.0 µs (−20–40%) |

## Implementation Steps

1. [ ] Add `RangeStart { lo: u8, hi: u8 }` variant to `StartStrategy` (for Option B).
2. [ ] In `StartStrategy::compute`, detect single contiguous range in `ascii_bits`
       and emit `RangeStart` instead of `AsciiClassStart`.
3. [ ] Add `RangeStart` scan arms in `find_with_scratch` and `find_interp`
       using `b.wrapping_sub(lo) <= (hi - lo)`.
4. [ ] Implement the 8-byte chunk scan (Option A) inside the existing
       `AsciiClassStart` arm as a fast prefix loop.
5. [ ] Add unit tests for range detection (digits, alpha, alnum).
6. [ ] Run `cargo test` and `cargo clippy --tests`.
7. [ ] Run `cargo bench -- "class_start|posix_digits|real_world"` and save log.
