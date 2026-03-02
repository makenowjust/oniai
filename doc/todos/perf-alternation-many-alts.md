# TODO: Optimize alternation scanning for many alternatives

## Status: Pending

## Problem

With 10 case-insensitive alternatives, the current Aho-Corasick approach is
12× slower than the `regex` crate:

```
alternation/10_alts_no_match   oniai/jit = 783 ns   regex = 65 ns   (12× gap)
alternation/10_alts_match      oniai/jit = 382 ns   regex = 36 ns   (11× gap)
```

Pattern: `(?i)(apple|banana|cherry|date|elderberry|fig|grape|honeydew|kiwi|lime)`.

### Root cause

For 10 case-insensitive alternatives, case folding expands each pattern.
For example `"apple"` folds to multiple candidates.  The `AhoCorasick::new`
automaton must handle all folded variants, giving it many more states/transitions
than for the case-sensitive case.

The `regex` crate compiles this to a minimized DFA with SIMD-accelerated state
transitions, scanning 16–32 bytes per cycle.  The AC automaton transitions are
byte-by-byte.

### 4-alt comparison

For 4 alternatives the AC automaton is fast:
```
alternation/4_alts_no_match   oniai/jit = 31 ns   regex = 56 ns   (oniai wins!)
```

The 10-alt case regresses because the case-folded AC automaton becomes larger.

## Proposed Solutions

### Option A: Unicode case-folded normalization before AC construction

Instead of adding all Unicode case-fold variants of each alternative, convert
each string to a canonical case-folded form (lowercase) and use a case-folded
matching approach in the AC search.

Reduces the number of AC patterns from `K × fold_variants` to just `K`.
Requires a custom case-insensitive AC `find` implementation.

The `aho-corasick` crate supports `AhoCorasickBuilder::ascii_case_insensitive`
for ASCII-only patterns.  Using this would keep AC pattern count at K instead
of expanding all Unicode folds.

### Option B: Hybrid scan — AC pre-filter + full exec verification

Run Aho-Corasick in streaming mode (`find_iter`) to identify candidate start
positions where any pattern matches.  At each candidate, run `exec` for the
full pattern (including capture groups, anchors, etc.).

For no-match haystacks, AC quickly determines no pattern can start at any
position.  For sparse-match haystacks, we avoid exec overhead between matches.

This is what the current implementation already does for `LiteralSet` — the
issue is AC state count for case-insensitive patterns.

### Option C: Replace LiteralSet with ByteSet pre-filter

For case-insensitive alternation where all alternatives start with ASCII
letters, precompute the set of possible first bytes (case-folded) and use
`AsciiClassStart`-style scanning to find candidate positions.  Then verify with
a full exec from each candidate.

For `(?i)(apple|banana|...)` the first bytes are `{a, b, c, d, e, f, g, h, k, l}`
(10 bytes, all ASCII).  A bitmap scan for these 10 bytes is faster than a
10-alternative AC automaton.

Implementation: in `StartStrategy::compute`, when `LiteralSet` is chosen but
`can_use_ac_ascii_insensitive` is detected, use `AsciiClassStart` with the
first-byte bitmap instead.

### Option D: Lazy DFA construction (long term)

Build a minimal DFA from the NFA for the alternation pattern at `Regex::new`
time (or lazily on first use).  Use SIMD DFA state transitions.  This is the
approach the `regex` crate uses and would close the gap fully.

Very high implementation complexity; consider as a long-term project.

## Recommended Approach

Implement **Option C** (first-byte bitmap via `AsciiClassStart`) as a quick
win for case-insensitive many-alt patterns.  Follow with **Option A** (use
`aho-corasick`'s built-in ASCII case-insensitive mode) to reduce AC automaton
state count.

## Expected Benchmark Impact

| Benchmark | Current | Option C | Option A+C |
|-----------|---------|----------|------------|
| `10_alts_no_match/jit` | 783 ns | ~200 ns (−75%) | ~100 ns (−87%) |
| `10_alts_match/jit` | 382 ns | ~150 ns (−61%) | ~80 ns (−79%) |
| `4_alts_no_match/jit` | 31 ns | no regression | no regression |

## Implementation Steps

### Option C (first-byte bitmap for case-insensitive alternation)
1. [ ] In `StartStrategy::compute`, detect `LiteralSet` + case-insensitive flag
       and extract the set of possible first bytes.
2. [ ] If all first bytes are ASCII (common case), emit `AsciiClassStart` with
       the first-byte bitmap instead of `LiteralSet`.
3. [ ] Store the AC automaton as a secondary check inside the scan arm (verify
       candidate position with AC before calling exec).
4. [ ] Run `cargo test` and `cargo clippy --tests`.
5. [ ] Run `cargo bench -- alternation` and save log.

### Option A (ASCII case-insensitive AC)
1. [ ] Detect whether all literal strings in `LiteralSet` are ASCII-only.
2. [ ] Use `AhoCorasickBuilder::new().ascii_case_insensitive(true).build(&originals)`
       instead of building with all folded variants.
3. [ ] Adjust the `ac.find` call: use original case-folded bytes for the pattern,
       compare case-insensitively.
