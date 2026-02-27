# Benchmark Results

Benchmarks are run with [Criterion](https://github.com/bheisler/criterion.rs) on the native host.
All timings are wall-clock medians from 100 samples.

---

## Setup

```
Platform : Apple M-series (macOS)
Toolchain: stable Rust (release mode)
Runner   : cargo bench (Criterion 0.5)
```

---

## Results by optimization phase

Four snapshots were recorded:

| Phase | Description |
|-------|-------------|
| **Baseline** | Plain backtracking NFA — no search heuristics |
| **+StartStrategy** | Anchored / literal-prefix / first-char-set skip logic |
| **+RequiredChar** | Mandatory-char pre-filter (memchr before the outer loop) |
| **+Memoization** | Fork-state memoization — Algorithm 5 of Fujinami & Hasuo 2024 |

### literal/no_match_1k
Pattern `abcde`, 1000 × `'a'`, no match expected.

| Phase | Time | Speedup |
|-------|------|---------|
| Baseline | 6,685 ns | 1× |
| +StartStrategy | 885 ns | **7.6×** |
| +RequiredChar | 50 ns | **134×** |
| +Memoization | 52 ns | **129×** |

`StartStrategy` uses `str::find("abcde")` to scan; `RequiredChar` detects `'e'` is
mandatory and calls `memchr` once — no `exec()` invocations needed.
Memoization has no visible effect here (exec is never called).

---

### literal/match_mid_1k
Pattern `abcde`, match at position 500 in a 1001-char string.

| Phase | Time | Speedup |
|-------|------|---------|
| Baseline | 3,031 ns | 1× |
| +StartStrategy | 98 ns | **31×** |
| +RequiredChar | ~98 ns | same |
| +Memoization | ~148 ns | ~same |

Literal-prefix `str::find` jumps directly to the candidate.
`RequiredChar` does not help here because `'e'` is present.
Memoization adds ~1 extra push per Fork — minor overhead on match paths.

---

### anchored/no_match_1k
Pattern `\Aabc`, 1000 × `'a'`, no match.

| Phase | Time | Speedup |
|-------|------|---------|
| Baseline | 5,918 ns | 1× |
| +StartStrategy | 11 ns | **538×** |
| +RequiredChar | 13 ns | **455×** |
| +Memoization | 17 ns | **348×** |

`Anchored` strategy tries `exec()` once at position 0 only.
`RequiredChar` is intentionally skipped for `Anchored` patterns.
Memoization adds trivial overhead (no Fork states visited here).

---

### alternation/4\_alts\_no\_match
Pattern `foo|bar|baz|qux`, 500 × `'x'`, no match.

| Phase | Time | Speedup |
|-------|------|---------|
| Baseline | 34,280 ns | 1× |
| +StartStrategy | 1,592 ns | **21×** |
| +RequiredChar | ~1,592 ns | same |
| +Memoization | ~1,440 ns | **24×** |

`FirstChars` strategy collects `{f, b, q}` and skips positions without those
chars.  No mandatory suffix char (alternation has multiple endings).

---

### alternation/4\_alts\_match
Pattern `foo|bar|baz|qux`, match at position 200.

| Phase | Time | Speedup |
|-------|------|---------|
| Baseline | 13,784 ns | 1× |
| +StartStrategy | 714 ns | **19×** |
| +RequiredChar | ~714 ns | same |
| +Memoization | ~689 ns | **20×** |

---

### quantifier/greedy\_no\_match\_500
Pattern `a*b`, 500 × `'a'`, no match.

| Phase | Time | Speedup |
|-------|------|---------|
| Baseline | 2,572,898 ns | 1× |
| +StartStrategy | 2,669,192 ns | ~1× |
| +RequiredChar | 26 ns | **~99,000×** |
| +Memoization | 26 ns | **~99,000×** |

`RequiredChar` already handles this completely (no `exec()` called).
Memoization has no additional effect here.

---

### quantifier/greedy\_match\_500
Pattern `a+`, 500 × `'a'`, match expected.

| Phase | Time |
|-------|------|
| All phases | ~8 µs |

Greedy quantifier match is O(n); no optimisation changes this.

---

### pathological/n  (`a?^n a^n` on `a^n`)
Classic exponential-backtracking pattern — the **headline result** for memoization.

| n | Baseline | +RequiredChar | **+Memoization** | Speedup |
|---|----------|---------------|------------------|---------|
| 10 | ~30 µs | ~30 µs | **4.2 µs** | **7×** |
| 15 | ~1.15 ms | ~1.15 ms | **9.6 µs** | **120×** |
| 20 | ~45.6 ms | ~45.6 ms | **17.4 µs** | **2,600×** |

`RequiredChar` does not help (every `a?` matches `'a'`, so `'a'` is present).
Memoization records each `(fork_pc, pos)` pair when both alternatives fail.
Future visits to the same fork at the same position short-circuit immediately,
reducing the complexity from **O(2^n)** to **O(n²)** for this pattern class.

---

### Other benchmarks (+Memoization timings)

The extra `Bt::MemoMark` push per Fork adds ~20–30% overhead on patterns with
many Fork instructions and few backtrack paths (no catastrophic backtracking).

| Benchmark | +RequiredChar | +Memoization | Change |
|-----------|---------------|--------------|--------|
| captures/two_groups | 480 ns | 625 ns | +30% |
| captures/iter_all | 2.1 µs | 2.5 µs | +19% |
| email/find_all | 2.8 µs | ~2.8 µs | ~same |
| charclass/alpha_iter | 21.8 µs | ~21 µs | ~same |
| case_insensitive/match | 846 ns | ~847 ns | ~same |
| find_iter_scale/100 | 2.3 µs | 2.3 µs | ~same |
| find_iter_scale/1000 | 22.9 µs | 22.9 µs | ~same |

`find_iter_scale` confirms linear scaling continues to hold.

---

## Summary of optimisations

### 1. `StartStrategy` (compile-time analysis)

Analyses the compiled program at build time and chooses one of four strategies:

| Strategy | Trigger | Behaviour |
|----------|---------|-----------|
| `Anchored` | Program starts with `\A`/`^` | Try `exec()` only at `start_pos` |
| `LiteralPrefix(s)` | Sequence of case-sensitive `Char` at start | `str::find(s)` to jump to candidates |
| `FirstChars(set)` | Reachable first chars are a finite set | `str::find(closure)` to skip non-starters |
| `Anywhere` | Fallback | Try every byte position |

### 2. `RequiredChar` (compile-time analysis)

Walks backwards from the `Match` instruction looking for the last mandatory
case-sensitive `Char` on every execution path.  A `Char` is considered mandatory
only when no branch instruction bypasses it (`bypasses()` check).

At search time, applied before the outer position loop for all non-`Anchored` strategies:

```rust
if !text[start_pos..].contains(required_char) {
    return None; // impossible match — skip all exec() calls
}
```

Uses Rust's built-in `str::contains(char)` which compiles to a SIMD `memchr`
on supported platforms.

### 3. Fork-state memoization (Algorithm 5 of Fujinami & Hasuo 2024)

Implements the core memoization technique from:
> Fujinami, H. & Hasuo, I. (2024).  "Efficient Matching with Memoization for
> Regexes with Look-around and Atomic Grouping."  arXiv:2401.12639.

Each call to `exec()` maintains a local `HashSet<u64>` memo table.  When both
alternatives of a `Fork`/`ForkNext` instruction fail, the `(pc, pos)` pair is
recorded.  Future visits to the same fork state at the same text position
short-circuit immediately, bounding the total Fork-state work to
**O(|prog| × |text|)**.

The implementation uses a `Bt::MemoMark` stack entry (pushed below the
`Bt::Retry`) to trigger memo recording only after the second alternative is
also exhausted — ensuring correctness without any extra recursion.

---

## Known remaining bottlenecks

| Pattern class | Behaviour | Fix |
|---------------|-----------|-----|
| Exponential alternation (`a?^n a^n`) | **O(n²)** with memo | ✅ memoization implemented |
| `a*b` no-match on all-`'a'` text | O(n) with required_char | ✅ required_char implemented |
| Back-references, subexpression calls | Cannot use DFA/NFA simulation | Inherent in feature set |
| Look-around in inner loops | Sub-exec memo not shared (Alg. 6) | Future work |
| Greedy match itself (`a+` on long text) | O(n) per start pos attempted | Inherent; mitigated by `LiteralPrefix`/`FirstChars` |
