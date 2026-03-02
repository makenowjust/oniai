# Oniai — TODO / Performance Goals

This file tracks planned improvements, with a focus on closing the performance
gap between oniai and other regex engines.  See `doc/BENCHMARKS.md` for the
baseline numbers (measured 2026-02-28, macOS, Apple Silicon).

---

## Where oniai already wins

| Pattern | oniai/jit | Nearest competitor | Advantage |
|---------|----------:|-------------------:|-----------|
| `a*b` no-match (memoization) | 27 ns | 816 ns (regex) | **30×** faster |
| `\Aabc` anchored | 18 ns | 34 ns (pcre2) | 2× faster |
| `(?>a+)b` atomic group | 27 ns | 37 ns (pcre2) | 1.4× faster |
| Pathological `(a?)^20 a^20` | 4.8 µs | 35 ms (pcre2) | **7 000×** faster |

These advantages must be preserved.  Add benchmark assertions / regression
floors if any optimization touches memoization or atomic-group handling.

---

## TODO-1 — Fix JIT lookbehind codegen regression ✅ DONE

**Priority: High** (small, safe, measurable)

**Problem**: oniai/jit (22.5 ms) was slower than oniai/interp (20.5 ms) on the
`lookbehind/word_after_period` benchmark.  JIT must never be slower than the
interpreter.

**Root cause found**: `exec_lookaround_for_jit` built an intermediate `State`
from `jctx` (1 allocation), then passed it to `exec_lookaround` which cloned it
again for the sub-execution (2nd allocation).  The interpreter path had only one
allocation per lookaround call.

**Fix**: Restructured `exec_lookaround_for_jit` in two ways:
1. **Cache-first**: check `look_results` memo cache *before* constructing any
   `State`, applying the delta directly to `jctx.slots_ptr` on a hit (zero
   allocation on cache hits).
2. **Single allocation**: on a cache miss, build the sub-execution `State`
   directly from `jctx` and call `exec` directly, eliminating the redundant
   intermediate clone.

**Result (2026-02-28)**:
- oniai/jit: 20.82 ms (was 22.65 ms, **-8.1%**)
- oniai/interp: 20.79 ms (unchanged)
- JIT/interp ratio: 1.002× (was 1.09×) — regression eliminated
- Memoization benchmarks: no regression (all within noise)

---

## TODO-2 — Implement literal pre-scan (memmem / Aho-Corasick) ✅ PARTIAL

**Priority: High** (largest absolute impact)

**Problem**: oniai runs the full NFA at every character position.  Engines like
`regex` first skip forward to where a match could start using SIMD `memmem`
(single literal) or Aho-Corasick (alternation of literals).  This causes:

| Benchmark | oniai/jit | pcre2 | regex | Gap (vs regex) |
|-----------|----------:|------:|------:|---------------:|
| alternation/4_alts_match | 18.9 µs | 208 ns | 30 ns | **630×** |
| case_insensitive/match | 14.3 µs | 79 ns | 83 ns | **172×** |
| literal/match_mid_1k | 143 ns | 58 ns | 25 ns | 5.7× |
| real_world/literal_count | 134 µs | 26 µs | 10 µs | 13× |

**Done (2026-02-28)**:

1. **`LiteralSet` strategy**: for top-level alternation of case-sensitive
   literals (each ≥ 2 chars), extract the literal set with `extract_literal_set`
   and run `str::find` for each, jumping to the leftmost candidate.
   `foo|bar|baz|qux` → `LiteralSet(["foo","bar","baz","qux"])`.

2. **`FirstChars` hot path**: replaced the `chars_eq_ci` closure (which calls
   `unicode_casefold` for every non-matching char) with multiple
   `str::find(char)` calls.  All case variants are already in the `chars` vec,
   so a plain equality match suffices.

**Results (2026-02-28)**:
| Benchmark | Before | After | Target | Status |
|-----------|-------:|------:|-------:|--------|
| alternation/4_alts_match | 18.9 µs | **694 ns** | ≤ 300 ns | Near (2.3×) |
| alternation/4_alts_no_match | 46.7 µs | **787 ns** | ≤ 600 ns | Near (1.3×) |
| case_insensitive/match | 14.3 µs | 14.3 µs | ≤ 400 ns | Unchanged |
| real_world/literal_count | 134 µs | ~140 µs | ≤ 20 µs | Within noise |

**Regression check**: all memoization benchmarks within noise.

---

## TODO-2b — Eliminate `fold_advance` / `fold_retreat` heap allocation ✅ DONE

**Priority: High** (case-insensitive patterns blocked on this)

**Problem**: `fold_advance` (vm.rs:1083) did `Vec::with_capacity(folded.len() + 2)`
on every call.  It is called once per `FoldSeq` instruction per NFA attempt —
meaning hundreds of thousands of times during a large-text scan.  The
allocation completely dominated `case_insensitive/match`:

| Benchmark | oniai/jit | pcre2 | regex |
|-----------|----------:|------:|------:|
| case_insensitive/match (before) | 14.3 µs | 79 ns | 83 ns |

**Fix (2026-02-28)**:
- `fold_advance`: track an index `fi` into `folded` and compare `ch.case_fold()`
  chars directly against `folded[fi]` — **zero allocation**.
- `fold_retreat`: collect each char's fold into a 4-element stack array
  (`['\0'; 4]`) — **no heap allocation** (max Unicode case-fold expansion is 3).

**Result (2026-02-28)**:
| Benchmark | Before | After | Gap vs pcre2 |
|-----------|-------:|------:|-------------:|
| case_insensitive/match/oniai/jit | 14.3 µs | **7.97 µs (-44%)** | 101× |
| case_insensitive/match/oniai/interp | 14.3 µs | **7.74 µs (-46%)** | 98× |

**Remaining gap**: oniai is still ~100× slower than pcre2/regex (79–83 ns).
The remaining cost is that `(?i)hello` compiles to `FoldSeq(['h','e','l','l','o'])` as the
first instruction, but `collect_first_chars` returns `None` for multi-char FoldSeq, so the
strategy falls to `Anywhere` — trying the NFA at **every** position.
See TODO-2c for the fix.

---

## TODO-2c — Add `CaselessPrefix` start strategy for case-insensitive literal patterns ✅ DONE

**Priority: High** (closes the ~100× gap for `(?i)literal` patterns)

**Problem**: `(?i)hello` compiles to a single `FoldSeq(['h','e','l','l','o'])` instruction
(consecutive literals merged in `compile.rs`).  `collect_first_chars` returned `None` for any
multi-char `FoldSeq` (can't enumerate first chars safely), so `StartStrategy::compute` fell
to `Anywhere` — the NFA was attempted at **every** character position.

For the benchmark haystack (300 x's + "HELLO" + 300 x's = 605 chars), this meant 605 NFA
calls × ~13 µs overhead = the observed ~8 µs.

**Fix (2026-02-28)**:
1. New `StartStrategy::CaselessPrefix { folded, non_ascii_first_bytes }` — stores the full
   folded sequence plus non-ASCII first bytes from the ByteTrie.
2. `extract_caseless_prefix(prog)` — returns `(folded.clone(), foldseq_pc)` if first meaningful
   instruction is `FoldSeq(folded)` with non-empty folded.
3. In `StartStrategy::compute`, check between `LiteralPrefix` and `LiteralSet`.
4. In scan loop (`find_with_scratch` + `find_interp`):
   - Pre-compute ASCII case variants of `folded[0]` (e.g. `['H','h']`)
   - SIMD `str::find(c)` per variant → leftmost hit
   - Scan raw bytes for `non_ascii_first_bytes` (ByteTrie-derived; avoids `case_fold()` per char)
   - At each candidate, `fold_advance(text, pos, folded)` pre-filter (zero-alloc) before NFA

**Result (2026-02-28)**:
| Benchmark | Before | After | Gap vs pcre2 |
|-----------|-------:|------:|-------------:|
| case_insensitive/match/oniai/jit | 7.97 µs | **294 ns (-96.3%)** | 3.8× |
| case_insensitive/match/oniai/interp | 7.74 µs | **312 ns (-95.9%)** | 4.0× |

The remaining 3.8× gap vs pcre2 (78 ns) is due to the overhead of one NFA call
(~200–250 ns) per match found.  Closing this would require a DFA or other
non-backtracking match engine — out of scope.

---

## TODO-4 — Unicode case-fold correctness fixes and compile-time UTF-8 byte-trie optimization ✅ DONE

**Priority: High** (correctness and performance)

**Problems**:
1. `/(?i)[a-z]/` incorrectly matched `ß` (whose full fold is `ss`, not in `[a-z]`), because
   `matches_slow` used `case_fold().next()` (first codepoint only) as the comparison key.
2. `/(?i)s/` failed to match `ſ` (U+017F), because `extract_caseless_prefix` skipped single-char
   `FoldSeq` instructions, falling through to `collect_first_chars` which only emitted the ASCII
   pair `{s, S}`.
3. `FoldSeq` and case-insensitive `Class` instructions called `case_fold()` and decoded UTF-8
   at every match position — O(n) `case_fold()` calls for a length-n text.

**Fixes (2026-03-01)**:
1. `matches_slow`: only substitute the fold result when the fold is a single codepoint (simple
   fold); otherwise compare the original character.  Effect: `/(?i)[a-z]/` no longer matches `ß`.
2. Route any non-empty `FoldSeq` (including length-1) through `CaselessPrefix` scanner, whose
   gap scan correctly finds non-ASCII codepoints like `ſ`.
3. At `Regex::new()` time, `build_match_tries` constructs a `ByteTrie` for each `FoldSeq`,
   `FoldSeqBack`, and simple case-insensitive `Class`/`ClassBack` instruction.  At match time,
   `exec()` walks the trie directly over raw bytes — no UTF-8 decoding, no `case_fold()` calls.
4. `CaselessPrefix` scanner uses the ByteTrie's non-ASCII first bytes for raw-byte scanning
   instead of calling `case_fold()` per character in the gap scan.

**New modules**:
- `src/bytetrie.rs`: `ByteTrie`/`TrieNode` data structure with `insert`, `advance`, `advance_back`, `reversed`, `first_bytes`.
- `src/casefold_trie.rs`: `fold_seq_to_trie`, `fold_seq_to_trie_back`, `charset_to_bytetrie`, `charset_to_bytetrie_back`.

**Result (2026-03-01)**:
| Benchmark | Before (post TODO-2c) | After | Change |
|-----------|----------------------:|------:|-------:|
| case_insensitive/match/oniai/jit | 294 ns | **283 ns** | −4% |
| case_insensitive/match/oniai/interp | 312 ns | **257 ns** | −17% |

The interp path is now only 3× slower than `regex` / pcre2 (vs 4× before).
The remaining gap is the per-match NFA startup cost (~200–250 ns).

---



**Priority: Low** (housekeeping, but important before any further optimization)

Codify the numbers below as Criterion baseline comparisons or `#[test]`
assertions so any future change that regresses them is caught automatically.

| Benchmark | Current | Floor (must not exceed) |
|-----------|--------:|------------------------:|
| `quantifier/greedy_no_match_500` (oniai/jit) | 27 ns | 60 ns |
| `pathological/oniai/jit/20` | 4.8 µs | 15 µs |
| `pathological_iter/oniai/jit/20` | 11.0 µs | 30 µs |
| `atomic_group/no_match_500` (oniai/jit) | 27 ns | 60 ns |

Save a named Criterion baseline after every optimization pass:
```sh
cargo bench -- --save-baseline pre-<change-name>
```

---

## Out of scope

- Matching `regex`'s DFA speed on pure-DFA patterns — requires replacing the
  backtracking engine architecture entirely.
- Matching pcre2's throughput on large-text character-class iteration
  (`\d+`, `[a-z]+`) — a 2–4× gap against a C-library SIMD loop is acceptable.
- Reducing Cranelift JIT compile time — already fast enough for practical use.
