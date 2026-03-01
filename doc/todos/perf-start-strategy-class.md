# TODO: StartStrategy — AsciiClassStart for Class-first patterns

## Status: Planned

## Problem

Patterns whose first real instruction is a `Class` (e.g. `\d+`, `\w+@\w+\.\w+`,
`[a-z]+`) currently fall back to `StartStrategy::Anywhere`, which tries every
byte-aligned position in the haystack.  For typical ASCII text most positions
are not valid match starts (e.g. `\d+` on `"abc123def"` only has 3 valid starts
out of 9 positions), so the engine wastes time invoking `exec`/JIT at positions
that fail immediately on the first `Class` check.

`CharSet` already stores a 128-bit ASCII bitmap (`ascii_bits: [u64; 2]`) after
the recent `perf: add ASCII bitmap fast-path to CharSet::matches` commit.  We
can reuse this bitmap in the `StartStrategy` scan loop to skip positions cheaply
without calling `exec` at all.

## Proposed Solution

### New `StartStrategy` variant

```rust
/// The pattern's first real instruction is `Class(idx, false)`.
/// Use the charset's ASCII bitmap to skip positions that cannot start a match.
AsciiClassStart {
    /// ASCII bitmap (one bit per codepoint 0–127).  Bit `cp` is set iff the
    /// charset accepts codepoint `cp`.  Used to skip ASCII bytes that can
    /// never start a match.
    ascii_bits: [u64; 2],
    /// `true` if the charset can match non-ASCII codepoints (e.g. `\w` in
    /// Unicode mode).  When `true`, non-ASCII leading bytes are always tried.
    /// When `false`, non-ASCII bytes are skipped too.
    can_match_non_ascii: bool,
},
```

### Changes in `StartStrategy::compute`

After the `FirstChars` probe and before the `Anywhere` fallback, check whether
the first real instruction is `Class(idx, false)`:

```rust
let pc0 = /* index of first non-Save/KeepStart instruction */;
if let Some(Inst::Class(idx, false)) = prog.get(pc0) {
    let cs = &charsets[*idx];
    let can_non_ascii = cs.ranges.iter().any(|&(_, hi)| hi as u32 >= 128)
        != cs.negate; // simplified; see impl notes
    return StartStrategy::AsciiClassStart {
        ascii_bits: cs.ascii_bits,
        can_match_non_ascii: can_non_ascii,
    };
}
```

### Changes in the scan loops (`find_with_scratch` and `find_interp`)

Replace the `Anywhere` arm with two arms:

**`AsciiClassStart` scan (JIT path `find_with_scratch`):**
```rust
StartStrategy::AsciiClassStart { ascii_bits, can_match_non_ascii } => {
    let bytes = text.as_bytes();
    let mut pos = start_pos;
    while pos < bytes.len() {
        let b = bytes[pos];
        let skip = if b < 128 {
            let word = ascii_bits[(b >> 6) as usize];
            (word >> (b & 63)) & 1 == 0   // bit clear → not a match start
        } else {
            !can_match_non_ascii           // skip non-ASCII if charset is ASCII-only
        };
        if !skip {
            if let Some(r) = self.try_at(text, pos, &mut memo, scratch) {
                return Some(r);
            }
        }
        pos += if b < 128 { 1 } else { /* utf8 char len */ };
    }
    None
}
```

(Interpreter path mirrors this with `exec_interp` instead of `try_at`.)

### `charsets` access

`StartStrategy::compute` needs access to the compiled `charsets` slice.  The
function signature must be extended:

```rust
fn compute(
    prog: &[Inst],
    charsets: &[CharSet],
    match_tries: &[Option<ByteTrie>],
    alt_tries: &[ByteTrie],
) -> Self
```

Call sites in `CompiledRegex::new` / `compile` already have the charsets
available.

## Expected Benchmark Impact

| Benchmark | Current | Expected |
|-----------|---------|----------|
| `find_iter_scale (\d+)` interp/jit | 2.7 µs / 1.9 µs | −30–50% |
| `email/find_all (\w+@…)` interp | 3.4 µs | −20–40% |
| `captures/iter_all (\w+\s+\w+)` | 2.5 µs | −10–30% |

Gains depend on haystack composition: sparser the match candidates, bigger the
win.  Worst case (all positions valid starts) is no regression (same as
`Anywhere` minus a small bitmap-check overhead).

## Implementation Steps

1. [ ] Add `AsciiClassStart` variant to `StartStrategy` enum.
2. [ ] Extend `StartStrategy::compute` signature to accept `charsets: &[CharSet]`.
3. [ ] Update both call sites of `compute` to pass `charsets`.
4. [ ] Add detection logic in `compute` after the `FirstChars` probe.
5. [ ] Add `AsciiClassStart` scan arm in `find_with_scratch` (JIT path).
6. [ ] Add `AsciiClassStart` scan arm in `find_interp` (interpreter path).
7. [ ] Add benchmark to measure `\d+` / `\w+` find-iter performance.
8. [ ] Run `cargo test` + `cargo clippy --tests` to verify correctness.
9. [ ] Run `cargo bench -- oniai` and save log to `log/`.
