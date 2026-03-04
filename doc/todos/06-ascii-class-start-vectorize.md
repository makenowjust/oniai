# TODO 06: Vectorize AsciiClassStart Start Scan

## Problem

The `AsciiClassStart` arm in `find_with_scratch` and `find_interp` increments `pos`
one byte at a time:

```rust
StartStrategy::AsciiClassStart { ascii_bits, can_match_non_ascii } => {
    loop {
        if pos >= bytes.len() { return None; }
        let b = bytes[pos];
        if b < 0x80 {
            if bitmap_test(b, ascii_bits) && try_at(...) { return ...; }
            pos += 1;
        } else {
            // non-ASCII: advance by char length
            ...
            pos += ch_len;
        }
    }
}
```

This is a scalar loop — LLVM cannot vectorize it because each iteration may call
`try_at()` (a large function with side effects).  The *scan phase* (skipping bytes that
can't start a match) is never separated from the *try phase*.

By contrast, `RangeStart` uses `bytes[pos..].iter().position(...)` which LLVM
auto-vectorizes (subtract + compare → SIMD).

## Approach

For `can_match_non_ascii == false` (the common case): restructure so the scan and try
phases are separate.

```rust
StartStrategy::AsciiClassStart { ascii_bits, can_match_non_ascii: false } => {
    let bits = *ascii_bits;
    loop {
        // Vectorizable scan: skip bytes that cannot start a match.
        let offset = bytes[pos..].iter().position(|&b| {
            b < 0x80 && (bits[(b >> 6) as usize] >> (b & 63)) & 1 != 0
        })?;
        let candidate = pos + offset;
        if let Some(result) = self.try_at(text, candidate, &mut memo, scratch) {
            return Some(result);
        }
        pos = candidate + 1;
    }
}
// Fallback arm for can_match_non_ascii == true: keep existing byte-by-byte logic.
```

Note: the `position()` predicate returns `true` on the *first matching* byte.  Because
we're looking for a byte that CAN start a match (not one that can't), the predicate is
`bitmap_matches(b)`.  Non-ASCII bytes (b >= 0x80) don't match, so the position() scan
naturally skips them — no separate `can_match_non_ascii` check needed inside the loop.

## Files to Change

- `src/vm.rs` — `find_with_scratch()` and `find_interp()`, `AsciiClassStart` arms

## Expected Improvement

Depends on the pattern and haystack density.  For `[A-Z][a-z]+` on a dense text where
many bytes are letters, the scan overhead is small.  For `[A-Z][a-z]+` on sparse text
(mostly non-alpha), the vectorized scan skips long runs of non-matching bytes faster.

Estimated: 5-15% improvement on `AsciiClassStart` patterns with sparse haystacks.

## Notes

- `RangeStart` already does this correctly — this change makes `AsciiClassStart`
  consistent with `RangeStart`.
- After TODO 05 (pure-span fast path), pure `\d+`/`\w+` patterns bypass `AsciiClassStart`
  entirely, so this only helps non-pure-span patterns starting with an ASCII class.

## Success Criteria

- All tests pass
- No regressions (the change is strictly equivalent for the scan logic)
- `cargo bench -- "alpha_iter|text_pattern"` shows mild improvement or no regression
