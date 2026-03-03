# Optimization 3: Mandatory-byte/literal prefilter

## Problem

For patterns with a mandatory literal substring or mandatory single byte — a
string that must appear in any match — we can scan the entire haystack for that
literal using `memchr`/`memmem` (SIMD) and only attempt NFA execution at
those positions. This can reduce the number of NFA starts from O(n) to
O(matches + occurrences_of_literal).

The existing `StartStrategy::LiteralPrefix` already handles the case where the
literal appears at the **beginning** of the pattern. The missing optimization
is: **required literals that appear elsewhere** in the pattern (e.g., `@` in
`\w+@\w+\.\w+`, or `\.` after optional prefix in `Mrs?\.\s+`).

## Benchmark targets

| Benchmark | oniai/jit | regex | gap |
|-----------|----------:|------:|-----|
| `literal/match_mid_1k` | 84 ns | 25 ns | 3.4× |
| `email/find_all` | 337 ns | — | — |
| `real_world/title_name` | 10 µs | 7 µs | 1.4× |

(The literal/match_mid gap is already partially addressed by `LiteralPrefix`,
but a more general "required byte" filter would help further for patterns that
start with a variable-width section.)

## Approach

### Step 1 — Required-literal extraction from IR

Add a function (in `src/ir/mod.rs` or a new `src/ir/prefilter.rs`):

```rust
/// Finds the longest mandatory literal (sequence of fixed chars) that must
/// appear in any match, scanning all paths from the entry block of the main
/// region.  Returns `None` if no such literal can be found (e.g., `.*`).
///
/// Algorithm: walk the IR in "must-reach" order (follow only `Branch`
/// terminators; stop at `Fork`, `SpanChar`, `SpanClass`). Collect
/// consecutive `MatchChar` statements into a candidate literal.
/// If `Fork` is encountered, intersect the mandatory prefix of both arms.
pub fn required_literal(prog: &IrProgram) -> Option<String>
```

For a single mandatory byte, also provide:

```rust
/// Like `required_literal` but returns the first mandatory byte only.
/// Faster to compute and sufficient for a `memchr`-based prefilter.
pub fn required_byte(prog: &IrProgram) -> Option<u8>
```

### Step 2 — New `StartStrategy` variants

```rust
/// There is a mandatory byte somewhere in every match.
/// Use `memchr` to find candidate positions, then walk back up to
/// `max_lookbehind` bytes to find a valid NFA start.
RequiredByte {
    byte: u8,
    /// Maximum distance from the required byte to the start of a match.
    /// E.g., for `\w+@…`, `@` is required and the match starts up to
    /// `max_lookbehind` bytes before `@`.
    max_lookbehind: usize,
},

/// There is a mandatory literal substring in every match.
/// Use `memmem` to find candidate positions.
RequiredLiteral {
    literal: String,
    max_lookbehind: usize,
},
```

`max_lookbehind` is the maximum number of bytes that can precede the required
literal in any match. It is computed during the IR walk: it equals the maximum
total byte length of all IR statements that appear before the required literal
on any path from the entry block.

### Step 3 — Search loop implementation

For `RequiredByte { byte, max_lookbehind }`:

```rust
StartStrategy::RequiredByte { byte, max_lookbehind } => {
    let bytes = text.as_bytes();
    let mut scan_pos = start_pos;
    loop {
        // Jump to next occurrence of the required byte.
        let found = memchr::memchr(*byte, &bytes[scan_pos..])?;
        let byte_pos = scan_pos + found;
        // Try NFA from all positions within lookbehind window.
        let try_from = byte_pos.saturating_sub(*max_lookbehind);
        let try_from = try_from.max(start_pos);  // never before start_pos
        for start in try_from..=byte_pos {
            if let Some(r) = self.try_at(text, start, &mut memo, scratch) {
                return Some(r);
            }
        }
        scan_pos = byte_pos + 1;
    }
}
```

For `RequiredLiteral`, replace `memchr` with `memmem::find`.

**Optimization**: if `max_lookbehind == 0`, the required literal/byte is at the
very start of the match — this degenerates to the existing `LiteralPrefix` /
`FirstChars` strategies, which are already handled. Only emit `RequiredByte`/
`RequiredLiteral` when `max_lookbehind > 0`.

### Step 4 — Priority in `StartStrategy::compute`

`StartStrategy::compute` (and the IR-derived override in `CompiledRegex::new`)
should prefer strategies in this order:

1. `Anchored`
2. `LiteralPrefix` / `LiteralSet` / `CaselessPrefix`  (existing, highest quality)
3. `RequiredLiteral` with `max_lookbehind > 0`  (new)
4. `RequiredByte` with `max_lookbehind > 0`  (new)
5. `FirstChars` / `RangeStart` / `AsciiClassStart` / `ByteSetStart` (existing)
6. `Anywhere`  (last resort)

### Step 5 — Interp search loop

Add the same `RequiredByte` / `RequiredLiteral` match arms to the interpreter
search loop in `find_interp` (`vm.rs` ~line 2383).

## Files to change

- `src/ir/mod.rs` (or new `src/ir/prefilter.rs`) — `required_literal()`, `required_byte()`
- `src/vm.rs` — new `RequiredByte` / `RequiredLiteral` variants + search loop arms

## Interaction with Optimization #1

Both optimizations augment `StartStrategy`. They can coexist:
- `ByteSetStart` (from opt #1) covers the "first byte must be in set" case.
- `RequiredByte` (from opt #3) covers the "required byte somewhere inside match" case.
- If both apply, prefer `RequiredByte` when `|haystack| / |matches|` is large
  (i.e., sparse matches), and `ByteSetStart` when the match density is high.
  In practice, always prefer the more specific strategy first.

## Success criterion

- `email/find_all/jit` improves (target: close to 200 ns)
- `real_world/title_name/jit` holds or improves
- `literal/match_mid_1k/jit` holds (already good via `LiteralPrefix`)
- All 138 tests pass, zero clippy warnings
