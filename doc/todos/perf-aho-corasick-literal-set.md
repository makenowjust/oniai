# perf-aho-corasick-literal-set — Aho-Corasick for LiteralSet strategy

**Status:** Done

## Results (2026-03-02)

| Benchmark | Before | After | Improvement |
|-----------|-------:|------:|------------:|
| `alternation/4_alts_match/oniai/jit` | 665 ns | **54 ns** | 12× faster |
| `alternation/4_alts_no_match/oniai/jit` | 816 ns | **31 ns** | 26× faster — **now beats regex (55 ns)** |
| `alternation/10_alts_match/oniai/jit` | 1.26 µs | **379 ns** | 3.3× faster |
| `alternation/10_alts_no_match/oniai/jit` | 1.53 µs | **778 ns** | 2.0× faster |
| `case_insensitive_alt/find_all/oniai/jit` | 14.8 ms | **29 µs** | **510× faster** |

`case_insensitive_alt` gap vs regex: was 1 500× → now 3× (29 µs vs 9.5 µs).
`4_alts_no_match`: oniai now **faster than regex and fancy-regex**.

Log: `log/bench-aho-corasick-2026-03-02.txt`

## Problem

`StartStrategy::LiteralSet` is used whenever the pattern begins with a
top-level alternation of string literals (e.g. `foo|bar|baz|qux`) or an
`AltTrie` whose strings are long enough.  The current implementation calls
`str::find` for **each literal** independently and takes the minimum:

```rust
StartStrategy::LiteralSet(lits) => {
    let candidate = lits
        .iter()
        .filter_map(|lit| text[pos..].find(lit.as_str()).map(|o| pos + o))
        .min()?;
    ...
}
```

This is **O(k · n)** per search call, where `k` is the number of literals and
`n` is the remaining haystack length.  `find_iter` calls `find()` once per
match, so total work is **O(matches · k · n)**.

The most extreme case is `(?i:get|post|put|delete)`.  After Unicode
case-fold expansion, the `AltTrie` encodes every case variant of the four
words.  `all_strings()` returns many strings (every codepoint-combination
variant), and `LiteralSet` iterates all of them at every call.

### Benchmark evidence

| Benchmark | oniai/jit | regex | gap |
|-----------|----------:|------:|----:|
| `alternation/4_alts_match` | 665 ns | 30 ns | **22×** |
| `alternation/4_alts_no_match` | 816 ns | 58 ns | **14×** |
| `alternation/10_alts_match` | 1.26 µs | 36 ns | **35×** |
| `case_insensitive_alt/find_all` | 14.8 ms | 9.6 µs | **1 500×** |

`regex` uses Aho-Corasick internally (`aho-corasick` crate) for all
multi-string alternation — one pass over the haystack, O(n) total.

## Proposed fix

Replace the naive k-way `str::find` loop in `LiteralSet` with a streaming
**Aho-Corasick** automaton from the `aho-corasick` crate.

### Key design

1. **Add dependency**: `aho-corasick = "1"` to `Cargo.toml` (no default features
   needed; already used transitively by `regex`).

2. **Store a prebuilt `AhoCorasick` automaton** inside `StartStrategy::LiteralSet`
   alongside (or instead of) the `Vec<String>`:
   ```rust
   LiteralSet {
       ac: AhoCorasick,   // prebuilt at Regex::new() time
   }
   ```

3. **Streaming search in `find_with_scratch` / `find_interp`**:
   Instead of calling `ac.find()` per `find()` invocation, integrate the AC
   automaton as a **streaming iterator** inside the `FindIter` struct.  For
   each candidate position yielded by the AC iterator, call `try_at`.  This
   makes the total scan work O(n) for the entire `find_iter` walk.

   For the simpler case (`find()` returning a single match), use
   `ac.find(&text[pos..])` and adjust the offset.

4. **Limit**: only use AC when `lits.len() >= 2` and all literals are valid
   UTF-8 (always true here).

### Expected impact

- `alternation/4_alts_match`: 665 ns → ~30–100 ns (matching regex order of magnitude)
- `case_insensitive_alt/find_all`: 14.8 ms → O(n) scan, potentially < 100 µs
- `alternation/10_alts_match`: 1.26 µs → ~50–120 ns

### Complexity note

The streaming integration requires threading the AC iterator state through
`FindIter`.  The existing `find_with_scratch` API makes a single-match search
easy; the iteration optimization requires a refactor of `FindIter::next` to
hold the AC iterator state between calls.  A simpler first step is to use
`ac.find()` per call (fixes the O(k·n) → O(n) per call issue) and defer the
iterator-level streaming to a follow-up.

## Steps

- [ ] Add `aho-corasick = "1"` to `Cargo.toml`
- [ ] Change `LiteralSet` variant to hold `AhoCorasick` (built at strategy compute time)
- [ ] Replace naive scan with `ac.find()` in `find_with_scratch` and `find_interp`
- [ ] Add benchmark for `alternation/4_alts_match` baseline vs new
- [ ] (stretch) Integrate streaming AC iterator into `FindIter` for O(n) total scan
- [ ] Run `cargo test` and `cargo bench -- alternation case_insensitive_alt`
