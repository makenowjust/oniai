# Oniai — Benchmark Results

Five engine variants are compared side-by-side:

| Variant | Description |
|---------|-------------|
| **oniai/jit** | Oniai with Cranelift JIT (default feature) |
| **oniai/interp** | Oniai interpreter (no JIT code generation) |
| **regex** | [`regex`](https://docs.rs/regex) crate — DFA + Aho-Corasick, no lookarounds/backrefs |
| **fancy-regex** | [`fancy-regex`](https://docs.rs/fancy-regex) crate — NFA + backtracking fallback |
| **pcre2** | [`pcre2`](https://docs.rs/pcre2) crate — PCRE2 C library bindings |

Run on: 2026-02-28 (macOS, Apple Silicon, PCRE2 10.47)

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

### Literal search

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| `no_match` 1 000 'a's | 51 ns | 52 ns | 40 ns | 40 ns | 49 ns |
| `match_mid` 1 000 chars | 143 ns | 172 ns | **25 ns** | 26 ns | 58 ns |

`regex` uses Aho-Corasick / SIMD literal scanning; oniai scans character by character.

### Anchored match `\Aabc` — 1 000 chars

| oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|----------:|-------------:|------:|------------:|------:|
| 18 ns | 19 ns | **12 ns** | 12 ns | 34 ns |

### Alternation `foo|bar|baz|qux`

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| match at mid (400 chars) | 18.9 µs | 18.8 µs | **30 ns** | 32 ns | 208 ns |
| no match (500 chars) | 46.7 µs | 46.6 µs | **56 ns** | 56 ns | 366 ns |

`regex` compiles multi-literal alternations to Aho-Corasick (630× faster than oniai for this case).

### Quantifiers

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| `a*b` no-match 500 'a's | **27 ns** | 27 ns | 816 ns | 819 ns | 31 ns |
| `a+` match 500 'a's | 1.9 µs | 9.6 µs | 2.3 µs | 2.3 µs | **366 ns** |

`a*b` on all-'a' input: oniai and pcre2 fail immediately (memoization / early-exit); `regex`/`fancy-regex` walk every position (~30× slower).

### Captures `(\w+)\s+(\w+)`

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| first capture | 621 ns | 618 ns | 201 ns | 205 ns | **151 ns** |
| iterate all (44-char input) | 2.6 µs | 2.6 µs | **110 ns** | 898 ns | 730 ns |

### Email `\w+@\w+\.\w+`

| oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|----------:|-------------:|------:|------------:|------:|
| 1.2 µs | 3.3 µs | **140 ns** | 136 ns | 484 ns |

### Character classes

| Benchmark | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----------|----------:|-------------:|------:|------------:|------:|
| `[a-zA-Z]+` (900 chars) | 7.9 µs | 29.6 µs | **3.0 µs** | 5.7 µs | 4.2 µs |
| `[[:digit:]]+` (900 chars) | 10.5 µs | 22.8 µs | **3.8 µs** | 6.0 µs | 4.2 µs |

### Case-insensitive `(?i)hello` — 600 chars

| oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|----------:|-------------:|------:|------------:|------:|
| 283 ns | **257 ns** | 83 ns | 85 ns | **79 ns** |

The 3× gap vs pcre2/regex is due to one NFA call (~200–250 ns) per match found;
closing it would require a DFA engine.  Both paths improved significantly after
the UTF-8 byte-trie optimization (from ~14 µs, a ×55 speedup).

---

## Advanced patterns (oniai / fancy-regex / pcre2 only)

`regex` does not support lookarounds, backreferences, or atomic groups.

### Lookahead: `\w+(?=,)` — "A Study in Scarlet" (~580 KB)

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| 20.2 ms | 34.0 ms | **418 µs** | 15.5 ms |

`fancy-regex` is 50× faster than pcre2 and 48× faster than oniai/jit on a large text.
oniai's JIT is 1.7× faster than its interpreter for this case.

### Lookbehind: `(?<=\. )[A-Z]\w+` — "A Study in Scarlet"

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| 22.2 ms | 22.4 ms | 6.9 ms | **421 µs** |

`pcre2` is 50× faster than `fancy-regex` and ~50× faster than oniai.

### Backreference: `(\b\w+\b) \1` (doubled word) — "A Study in Scarlet"

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| 20.8 ms | 20.7 ms | 14.6 ms | **8.5 ms** |

`pcre2` is 2.4× faster than `fancy-regex` and 2.4× faster than oniai.

### Atomic groups: `(?>a+)b` — 500 'a's (no match)

| oniai/jit | oniai/interp | fancy-regex | pcre2 |
|----------:|-------------:|------------:|------:|
| **27 ns** | 27 ns | 272 µs | 37 ns |

`fancy-regex` is **10 000× slower** than oniai/pcre2: it parses `(?>...)` syntax but does not
optimize away backtracking — the engine still explores all 500 positions.  
oniai and pcre2 correctly treat the group as possessive: one O(1) failure.

---

## Scaling: `\d+` on growing input

| Size | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|-----:|----------:|-------------:|------:|------------:|------:|
| 100 | 1.85 µs | 2.92 µs | **424 ns** | 962 ns | 1.40 µs |
| 500 | 9.0 µs | 14.4 µs | **2.05 µs** | 4.68 µs | 6.84 µs |
| 1 000 | 17.8 µs | 28.6 µs | **4.05 µs** | 9.34 µs | 13.5 µs |
| 5 000 | 87.9 µs | 143.0 µs | **20.1 µs** | 46.3 µs | 68.4 µs |

All engines are linear in input size. `regex` is ~4× faster than oniai/jit.

---

## Pathological: `(a?)^n a^n` on `a^n`

Demonstrates backtracking complexity. `regex` and `fancy-regex` use DFA and are immune.

| n | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|---|----------:|-------------:|------:|------------:|------:|
| 10 | 1.42 µs | 4.50 µs | 26 ns | 26 ns | 26 µs |
| 15 | 2.80 µs | 10.2 µs | 33 ns | 33 ns | 958 µs |
| 20 | 4.80 µs | 18.0 µs | 40 ns | 40 ns | **35 ms** |

- `regex` / `fancy-regex`: DFA, O(n) — immune to exponential blowup.
- oniai: memoization bounds the search to O(|prog| × |text|) — polynomial, not exponential.
- **pcre2**: classic backtracking, **exponential** — 35 ms at n=20, grows as ~32× per 5-step increase.

### Pathological with prefix (cross-position memoization)

Input: `b^(10n) a^n` — prefix forces many NFA start positions.

| n | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|---|----------:|-------------:|------:|------------:|------:|
| 10 | 4.55 µs | 7.68 µs | 52 ns | 58 ns | 26.5 µs |
| 15 | 7.45 µs | 14.8 µs | 57 ns | 70 ns | 952 µs |
| 20 | 11.0 µs | 23.9 µs | 71 ns | 83 ns | 35.1 ms |

oniai's memoization reuses cached fork-failure data across start positions, keeping growth gentle (2.4× from n=10 to n=20). pcre2 re-executes exponentially per start position.

---

## Real-world text ("A Study in Scarlet", ~580 KB)

| Pattern | oniai/jit | oniai/interp | regex | fancy-regex | pcre2 |
|---------|----------:|-------------:|------:|------------:|------:|
| `Holmes` (literal) | 134 µs | 138 µs | **10.3 µs** | 10.8 µs | 26.2 µs |
| `[A-Z][a-z]+` | 3.0 ms | 3.5 ms | **561 µs** | 654 µs | 422 µs |
| `[[:digit:]]+` | 3.0 ms | 3.0 ms | **12.6 µs** | 13.6 µs | 188 µs |
| `"[^"]*"` | 5.3 ms | 7.2 ms | **189 µs** | 374 µs | 115 µs |
| `Mrs?. [A-Z][a-z]+` | 133 µs | 145 µs | **7.0 µs** | 8.2 µs | 13.1 µs |

---

## Summary

| Scenario | Fastest engine | Notes |
|----------|---------------|-------|
| Simple literal / alternation | **regex** | Aho-Corasick + SIMD, up to 600× faster |
| Case-insensitive search | **pcre2** or **regex** | Both ~80 ns |
| `a*b` no-match (memoization) | **oniai/jit** | ~30× faster than regex |
| Lookahead patterns | **fancy-regex** | NFA with lookahead optimization |
| Lookbehind / backreference | **pcre2** | C library, tight inner loop |
| Atomic groups | **oniai/jit** or **pcre2** | fancy-regex lacks atomic optimization (~10 000×) |
| Pathological backtracking | **regex** (immune) or **oniai** (polynomial) | pcre2 is exponential |
| JIT vs interpreter | **oniai/jit** | 1.5–5× faster on compute-heavy patterns |
