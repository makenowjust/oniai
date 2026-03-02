# TODO: Optimize alternation scanning for many alternatives

## Status: Done

## Problem

With 10 case-insensitive alternatives, the Aho-Corasick approach was
12× slower than the `regex` crate:

```
alternation/10_alts_no_match   oniai/jit = 784 ns   regex = 65 ns   (12× gap)
alternation/10_alts_match      oniai/jit = 380 ns   regex = 36 ns   (11× gap)
```

Pattern: `(?i)(apple|banana|cherry|date|elderberry|fig|grape|honeydew|kiwi|lime)`.

### Root cause

The `aho-corasick` crate's "Teddy" SIMD algorithm only activates for ≤ ~8 short
patterns.  For 10+ patterns (or many case-fold variants), it falls back to a
slow NFA/DFA, making the AC scan byte-by-byte.

For case-insensitive AltTrie patterns, the trie already encodes all fold
variants — we only need to scan for possible first bytes, not full strings.

## Solution Implemented

In `StartStrategy::compute`, for `AltTrie` instructions, extract first bytes
from the `ByteTrie` root node and choose the fastest matching strategy:

| First-byte count | Strategy | Mechanism |
|-----------------|----------|-----------|
| ≤ 3 distinct ASCII | `FirstChars` | `memchr`/`memchr2`/`memchr3` SIMD |
| > 3, contiguous range | `RangeStart` | LLVM-vectorized range check |
| > 3, non-contiguous | `AsciiClassStart` | 128-bit bitmap scan |
| any non-ASCII | `Anywhere` | no pre-filter |

For `foo|bar|baz|qux`: first bytes {b, f, q} → 3 bytes → `FirstChars` → memchr3 SIMD
For `alpha|bravo|...|juliet`: first bytes [a..j] contiguous → `RangeStart{97,106}`
For `(?i:get|post|put|delete)`: first bytes {g,G,p,P,d,D} non-contiguous → `AsciiClassStart`

## Benchmark Results (commit xlzmxkyl, log: bench-alttrie-firstbyte2-2026-03-02.txt)

| Benchmark | Before (AC) | After (first-byte) | vs regex |
|-----------|-------------|-------------------|---------|
| `4_alts_no_match/jit` | 30 ns | **25 ns (−16%)** | regex 56 ns — oniai wins! |
| `4_alts_match/jit` | 51 ns | **40 ns (−22%)** | regex 30 ns (1.3×) |
| `10_alts_no_match/jit` | 784 ns | **248 ns (−68%)** | regex 65 ns (3.8×) |
| `10_alts_match/jit` | 380 ns | **141 ns (−63%)** | regex 36 ns (3.9×) |
| `case_insensitive_alt/find_all/jit` | 29 µs | **17.8 µs (−39%)** | regex 9.6 µs (1.8×) |

The remaining 10-alt gap (3.8×) is because `RangeStart{97,106}` matches every
'a'-'j' byte in the haystack (200 'a's + "alpha" + 200 'a's), causing many
`try_at` calls.  The `regex` crate's DFA avoids these false candidates.

## Files Changed

- `src/vm.rs`: replaced `AltTrie → LiteralSet(AC)` block with first-byte strategy selection
- `src/bytetrie.rs`: added `#[allow(dead_code)]` to `all_strings`/`collect_strings`
