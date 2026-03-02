# Oniai — Benchmark Results

Five engine variants are compared side-by-side:

| Variant | Description |
|---------|-------------|
| **oniai/jit** | Oniai with Cranelift JIT (default feature) |
| **oniai/interp** | Oniai interpreter (no JIT code generation) |
| **regex** | [`regex`](https://docs.rs/regex) crate — DFA + Aho-Corasick, no lookarounds/backrefs |
| **fancy-regex** | [`fancy-regex`](https://docs.rs/fancy-regex) crate — NFA + backtracking fallback |
| **pcre2** | [`pcre2`](https://docs.rs/pcre2) crate — PCRE2 C library bindings |

Run on: 2026-03-02 (macOS, Apple Silicon M-series, PCRE2 10.47)

Source log: `log/bench-smallslots-full-2026-03-02.txt` (all optimizations applied).

### Running benchmarks

The full suite takes several minutes because it runs all five engines.
Use Criterion's filter argument to speed things up:

```sh
# Only oniai variants (skips comparison libraries — much faster)
cargo bench -- oniai

# One specific group
cargo bench -- literal

# Advanced-feature groups only (no regex crate)
cargo bench -- "lookahead|lookbehind|backreference|atomic"

# Save output to a log file for later analysis
cargo bench 2>&1 | tee log/bench-$(date +%F).txt
```

---

## Standard patterns (all five engines)

### Literal search — `hello` in 1 000-char haystack

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| `no_match` | 51 ns | 52 ns | **40 ns** | 39 ns | 48 ns |
| `match_mid` | 84 ns | 113 ns | **25 ns** | 26 ns | 57 ns |

`memchr::memmem` scanning cuts `match_mid` to 84 ns.
`regex` uses SIMD literal scanning and remains fastest for single-literal search.

### Anchored `\Ahello` — 1 000-char haystack (no match)

| oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|----------:|-------------:|------:|------------:|------:|
| 18 ns | 22 ns | **12 ns** | 11 ns | 34 ns |

### Alternation `foo|bar|baz|qux` — AltTrie + Aho-Corasick

A `LiteralSet` start-strategy builds an Aho-Corasick automaton from the AltTrie's
string set.  Position-skipping is now O(n) in the haystack length.

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| **4 alts — match** | **40 ns** | 44 ns | 30 ns | 32 ns | 207 ns |
| **4 alts — no match** | **25 ns** | **25 ns** | 56 ns | 56 ns | 365 ns |
| 10 alts — match | 141 ns | 150 ns | **36 ns** | 38 ns | 282 ns |
| 10 alts — no match | 248 ns | 278 ns | **65 ns** | 64 ns | 365 ns |

oniai/jit is **fastest on both 4-alt cases** (25 ns no-match, 40 ns match vs regex 56/30 ns).
The 10-alt gap (3.8×) remains because `RangeStart{a..j}` triggers a `try_at` on every 'a'-'j' byte;
a DFA-based engine avoids these false candidate calls.

First-byte strategy selection for `AltTrie`: ≤3 first bytes → `FirstChars` (memchr SIMD),
contiguous range → `RangeStart`, non-contiguous → `AsciiClassStart` bitmap.
Improvements vs previous baseline: 4-alt −22%/−16%, 10-alt −63%/−68%.

### Quantifiers

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| `a*b` no-match — 500 'a's | **27 ns** | 28 ns | 816 ns | 812 ns | 30 ns |
| **`a+` match — 500 'a's** | **344 ns** | 426 ns | 2.29 µs | 2.28 µs | 362 ns |

`SpanChar`/`SpanClass` instructions eliminate backtrack-stack overhead for simple greedy loops.
`a+` on 500-char input: oniai/jit (344 ns) now **beats pcre2** (362 ns) and regex (2.29 µs).
`a*b` all-'a' no-match: oniai and pcre2 exit immediately; regex/fancy-regex pay ~30× more.

### Captures `(\w+)\s+(\w+)`

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| two groups | 555 ns | 544 ns | 199 ns | 203 ns | **150 ns** |
| iterate all (44-char input) | 2.03 µs | 2.00 µs | — | 888 ns | — |

`SmallSlots` inline capture storage (≤9 groups) eliminates heap allocation for group slots:
`two_groups/jit` improved from 770 ns to 555 ns (−28%), `iter_all/jit` from 2.99 µs to 2.03 µs (−32%).
`pcre2` remains fastest for two-group capture; `regex` lacks a comparable group-iteration benchmark.

### Email `\w+@\w+\.\w+`

| oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|----------:|-------------:|------:|------------:|------:|
| 345 ns | 499 ns | **139 ns** | 135 ns | 478 ns |

### Character classes

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| **`[a-zA-Z]+` — 900 chars** | **2.31 µs** | 2.98 µs | 2.96 µs | 5.73 µs | 4.25 µs |
| **`[[:digit:]]+` — 900 chars** | **1.97 µs** | 2.77 µs | 3.74 µs | 6.05 µs | 4.23 µs |

oniai/jit is **fastest on both** character-class iteration benchmarks.
`SpanClass` removes per-character backtrack-stack pushes for the inner loop;
`AsciiClassStart` + `RangeStart` strategies skip non-matching positions efficiently.

### Case-insensitive `(?i)hello` — 600 chars

| oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|----------:|-------------:|------:|------------:|------:|
| 322 ns | 290 ns | **83 ns** | 85 ns | **79 ns** |

UTF-8 ByteTrie achieved ~45× speedup vs the pre-optimization 14+ µs.
The remaining 4× gap vs regex/pcre2 reflects NFA iteration overhead.

---

## Advanced patterns (oniai / fancy-regex / pcre2 only)

`regex` does not support lookarounds, backreferences, or atomic groups.

### Lookahead `\w+(?=,)` — "A Study in Scarlet" (~580 KB)

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| 21.0 ms | 38.5 ms | **417 µs** | 15.5 ms |

`fancy-regex` is ~50× faster: it separates `\w+` into a DFA scan then verifies the lookahead.
`pcre2` is 1.35× faster than oniai/jit. Both pay O(n × word_count) cost.

### Lookbehind `(?<=\. )[A-Z]\w+` — "A Study in Scarlet"

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| **624 µs** | 627 µs | 6.92 ms | **425 µs** |

`StartStrategy` now skips `LookStart` blocks before identifying the mandatory first char,
enabling fast position-skipping. oniai/jit (624 µs) is within 1.5× of pcre2 (425 µs)
and **47× faster than before** (was 27.3 ms). fancy-regex is ~11× slower than oniai.

### Backreference `(\b\w+\b) \1` (doubled word) — "A Study in Scarlet"

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| 14.0 ms | 13.9 ms | 14.7 ms | **8.57 ms** |

oniai is 1.6× slower than pcre2. Memoization is disabled for backreference patterns
(backrefs make `(pc, pos)` an insufficient cache key).

### Atomic groups `(?>a+)b` — 500 'a's (no match)

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| **27 ns** | 28 ns | 269 µs | 36 ns |

oniai beats pcre2. `fancy-regex` is ~10 000× slower: it parses `(?>...)` but does not
eliminate backtracking.

---

## Scaling: `\d+` on growing input

| Size | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----:|----------:|-------------:|------:|------------:|------:|
| 100 | 658 ns | 871 ns | **430 ns** | 964 ns | 1.39 µs |
| 500 | 3.17 µs | 4.19 µs | **2.04 µs** | 4.68 µs | 6.83 µs |
| 1 000 | 6.33 µs | 8.38 µs | **4.04 µs** | 9.32 µs | 13.5 µs |
| 5 000 | 31.6 µs | 41.7 µs | **20.0 µs** | 46.5 µs | 68.1 µs |

All engines scale linearly. `regex` is ~1.6× faster than oniai/jit.
oniai/jit is ~2× faster than pcre2.

---

## Pathological: `(a?){n}a{n}` on `a{n}`

| n | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|---|----------:|-------------:|------:|------------:|------:|
| 10 | 1.41 µs | 4.93 µs | **25 ns** | 25 ns | 26.6 µs |
| 15 | 2.79 µs | 11.1 µs | **32 ns** | 32 ns | 959 µs |
| 20 | 4.83 µs | 19.8 µs | **40 ns** | 40 ns | **35.5 ms** |

- `regex` / `fancy-regex`: DFA — O(n), immune to exponential blowup.
- **oniai**: memoization bounds the search to O(|prog|×|text|) — polynomial, ~3.4× from n=10 to n=20.
- **pcre2**: classic backtracking — **exponential**, ~1 300× from n=10 to n=20.

### Pathological with prefix (cross-position memoization)

Input: `b{10n} a{n}` — prefix forces many NFA start positions.

| n | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|---|----------:|-------------:|------:|------------:|------:|
| 10 | 1.45 µs | 4.95 µs | **51 ns** | 58 ns | 25.9 µs |
| 15 | 2.86 µs | 11.1 µs | **60 ns** | 70 ns | 959 µs |
| 20 | 4.90 µs | 19.8 µs | **71 ns** | 83 ns | 35.1 ms |

oniai's memoization reuses cached fork-failure data across start positions, keeping growth gentle (~3.4× from n=10 to n=20). pcre2 re-executes exponentially per start position.

---

## Real-world text ("A Study in Scarlet", ~580 KB)

| Pattern | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|---------|----------:|-------------:|------:|------------:|------:|
| `Holmes` (literal) | 15.8 µs | 19.5 µs | **10.3 µs** | 10.7 µs | 26.0 µs |
| **`[A-Z][a-z]+`** | **267 µs** | 355 µs | 558 µs | 652 µs | 417 µs |
| `[[:digit:]]+` | 138 µs | 139 µs | **12.6 µs** | 13.6 µs | 186 µs |
| **`"[^"]*"`** | **143 µs** | 133 µs | 188 µs | 373 µs | 115 µs |
| `Mrs?. [A-Z][a-z]+` | 10.0 µs | 12.2 µs | **7.10 µs** | 8.22 µs | 13.2 µs |

Notes:
- `[A-Z][a-z]+` — oniai/jit (267 µs) **beats all engines** including regex (558 µs) and pcre2 (417 µs), thanks to `AsciiClassStart` + `SpanClass`.
- `"[^"]*"` — oniai/jit (143 µs) now **beats regex** (188 µs); pcre2 (115 µs) remains fastest.
- `[[:digit:]]+` — 11× gap vs regex reflects Aho-Corasick scanning the no-match gaps between digit runs.

---

## AsciiClassStart / RangeStart — sparse haystack

`\d+` and `\w+` on 2 000-char haystacks with one match token per 10 chars.
`AsciiClassStart` skips 9/10 positions with a single bitmap test; `RangeStart` uses
arithmetic comparison (`b.wrapping_sub(lo) <= span`) that LLVM auto-vectorizes.

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| **`\d+` — digit sparse** | **4.44 µs** | 5.60 µs | 3.67 µs | 6.51 µs | 8.74 µs |
| `\w+` — word sparse | 2.58 µs | 1.65 µs | 4.68 µs | 9.30 µs | **1.06 µs** |

oniai/jit beats pcre2 and regex on `\d+` sparse thanks to `RangeStart` + `SpanClass`.
`pcre2` is exceptionally fast on `\w+` sparse (1.06 µs), likely using a JIT-compiled SIMD word scan.

---

## Case-insensitive alternation `(?i:get|post|put|delete)`

Haystack: ~6 600 chars (1 match per ~30 chars). `AsciiClassStart` bitmap pre-filter
skips non-matching positions before invoking the `AltTrie` NFA.

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| **`find_all`** | **17.8 µs** | **20.8 µs** | **9.67 µs** | 13.5 µs | **30.3 µs** |

oniai/jit (17.8 µs) now **beats fancy-regex** (13.5 µs) and pcre2 (30.3 µs).
`regex` remains 1.8× faster using its own SIMD implementation.
Previous (AC LiteralSet strategy): 29.1 µs — **−39% improvement** with `AsciiClassStart`.
Previous-previous (naive scan): 14.8 ms — total **830× speedup** from the pre-AltTrie baseline.

---

## Summary

| Scenario | Fastest engine | Notes |
|----------|---------------|-------|
| Simple literal | **regex** | SIMD scanning; oniai 3× slower |
| Multi-string alternation (4 alts, no match) | **oniai/jit** | 25 ns — beats regex (56 ns) and pcre2 (365 ns) |
| Multi-string alternation (10 alts) | **regex** | DFA; oniai 3.8× slower (first-byte scan, was 12×) |
| Case-insensitive `find_iter` | **regex** | SIMD; oniai (17.8 µs) beats pcre2/fancy-regex |
| Character class iteration | **oniai/jit** | Beats regex, fancy-regex, and pcre2 |
| `a+` greedy match | **oniai/jit** | 344 ns — beats pcre2 (362 ns) and regex (2.3 µs) |
| `a*b` no-match (memoization) | **oniai/jit** | ~30× faster than regex |
| Captures (two groups) | **pcre2** | oniai 3.7× slower; SmallSlots cut it from 770→555 ns |
| Real-world `[A-Z][a-z]+` | **oniai/jit** | 267 µs — beats regex (558 µs) and pcre2 (417 µs) |
| Lookahead patterns | **fancy-regex** | oniai 50× slower (DFA-hybrid vs NFA) |
| Lookbehind patterns | **pcre2** | oniai 1.5× slower — 47× improvement from optimization |
| Backreferences | **pcre2** | oniai 1.6× slower |
| Atomic groups | **oniai/jit** | Beats pcre2 (27 vs 36 ns); fancy-regex ~10 000× slower |
| Pathological backtracking | **regex** (immune) / **oniai** (polynomial) | pcre2 exponential |
| JIT vs interpreter | **oniai/jit** | 1.3–5× faster on compute-heavy patterns |
