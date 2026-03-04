# TODO 05: Pure-Span Fast Path (Bypassing NFA)

## Problem

For patterns like `\d+`, `\w+`, `a+` that compile to a single `SpanChar`/`SpanClass`
instruction with only the implicit group-0 capture, `find_with_scratch` still calls
`try_at()` for every candidate position.  `try_at()` sets up a full NFA `State`,
allocates a `bt` (backtrack) vector, invokes the JIT or interpreter, and finally extracts
the result.  For short matches (e.g., `\d+` on sparse text where each digit run is 1-5
chars) this overhead dominates over the actual work.

**Benchmark evidence** (`class_start/digit_sparse`):
- oniai/jit: 4.3 µs — regex: 3.67 µs (gap: 1.17×)
- oniai/interp: 6.2 µs
- Real-world `[[:digit:]]+` (580 KB): oniai/jit 120 µs — regex 12.6 µs (gap: **10×**)

## Approach

### Detection at compile time

Detect "pure span" patterns in the IR before lowering:

```rust
enum SpanOnlyKind {
    Char(char),                 // `c+` where c is ASCII
    AsciiClass { bits: [u64; 2] },  // `[class]+` where class is non-negated ASCII-only
}

fn detect_span_only(ir: &IrProgram) -> Option<SpanOnlyKind>
```

Conditions:
1. `ir.regions.len() == 1` (no sub-regions: no lookarounds, no atomic groups)
2. `ir.num_captures == 1` (only the implicit group-0)
3. Entry block stmts are only `SaveCapture` (no anchors, no KeepStart)
4. Entry block term is `SpanChar { c: ascii, exit: B }` or `SpanClass { id: ascii-only, exit: B }`
5. Exit block `B` stmts are only `SaveCapture`, term is `Match`

### Fast path execution

Add `span_only: Option<SpanOnlyKind>` to `CompiledRegex`.

In `find_with_scratch` (and `find_interp`), before the start-strategy dispatch, check:
```rust
if let Some(ref sk) = self.span_only {
    return self.find_span_only(text, start_pos, sk);
}
```

`find_span_only` integrates the start-position scan and the span scan in one tight loop,
returning `(start, end, vec![Some(start), Some(end)])` directly.

The implementation uses the existing start-strategy variants:
- `RangeStart`: bytes[pos..].position(|b| b.wrapping_sub(lo) <= span) → find start
- `AsciiClassStart`: bytes[pos..].position(|b| bitmap matches) → find start
- Then: bytes[start..].position(|b| not-in-class) → find end
- Return if end > start (always true since we found a matching byte)

## Files to Change

- `src/vm.rs` — add `SpanOnlyKind` enum, `detect_span_only()`, `find_span_only()`,
  `find_span_only_interp()` methods; add `span_only` field to `CompiledRegex`
- `src/ir/mod.rs` — no changes needed (uses existing IR types)

## Expected Improvement

- `class_start/digit_sparse/jit`: ~3-5× faster (4.3 µs → ~1-1.5 µs)
- `class_start/digit_sparse/interp`: ~3-5× faster (6.2 µs → ~1.5-2 µs)
- Real-world `[[:digit:]]+` (580 KB): closes 10× gap to regex significantly

## Success Criteria

- All tests pass
- `cargo bench -- "digit_sparse|word_sparse|greedy_match_500"` shows large improvement
- No regression on patterns that are NOT pure-span
