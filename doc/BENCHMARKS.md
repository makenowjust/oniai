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
| 10 | ~30 µs | ~30 µs | **4.4 µs** | **7×** |
| 15 | ~1.15 ms | ~1.15 ms | **10.0 µs** | **115×** |
| 20 | ~45.6 ms | ~45.6 ms | **17.8 µs** | **2,560×** |

`RequiredChar` does not help (every `a?` matches `'a'`, so `'a'` is present).
Memoization records each `(fork_pc, pos)` pair when both alternatives fail.
Future visits to the same fork at the same position short-circuit immediately,
reducing the complexity from **O(2^n)** to **O(n²)** for this pattern class.

---

### Other benchmarks (current timings)

| Benchmark | Time | Notes |
|-----------|------|-------|
| captures/two_groups | 621 ns | |
| captures/iter_all | 2.61 µs | |
| email/find_all | 3.29 µs | |
| charclass/alpha_iter | 31.2 µs | |
| charclass/posix_digit_iter | 24.8 µs | |
| case_insensitive/match | 14.1 µs | `FoldSeq` forces `Anywhere` start strategy (see note below) |
| find_iter_scale/100 | 2.73 µs | |
| find_iter_scale/500 | 13.4 µs | |
| find_iter_scale/1000 | 26.7 µs | |
| find_iter_scale/5000 | 133 µs | |

`find_iter_scale` confirms linear scaling continues to hold.

> **Note — `case_insensitive/match`**: Prior to the `FoldSeq` instruction (see
> §4 below), the pattern `(?i)hello` was compiled to five individual
> `Char(c, true)` instructions, giving a `FirstChars({'h','H'})` start strategy
> and a ~1 µs match time.  With `FoldSeq(['h','e','l','l','o'])`,
> `collect_first_chars` conservatively returns `None` for multi-char fold
> sequences (because a single Unicode source char can fold to a string
> *starting with* the target char, e.g. ẖ→h+\u{331}), forcing `Anywhere`
> and ~14 µs.  Improving `StartStrategy` for multi-char `FoldSeq` is a
> known optimisation opportunity.

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

### 3. Full memoization (Algorithms 5–7 of Fujinami & Hasuo 2024)

Implements the complete memoization framework from:
> Fujinami, H. & Hasuo, I. (2024).  "Efficient Matching with Memoization for
> Regexes with Look-around and Atomic Grouping."  arXiv:2401.12639.

A single `MemoState` is created once per `find()` call and shared across all
`exec()` invocations (including lookaround sub-executions).  It contains:

- **`fork_failures: HashMap<u64, usize>`** — maps `(pc, pos)` to the minimum
  `atomic_depth` at which failure was observed.  Future visits short-circuit
  when `stored_depth ≤ current_atomic_depth`.  Bounding Fork-state visits to
  **O(|prog| × |text|)** reduces `(a?)^n a^n` from O(2^n) to O(n²).

- **`look_results: HashMap<u64, LookCacheEntry>`** — maps `(lk_pc, pos)` to the
  cached outcome of a lookaround sub-execution.  Stores only the capture *delta*
  (index/value pairs that changed) so re-application is correct regardless of
  outer capture state.  Prevents exponential re-evaluation of the same lookahead
  body on different backtracking paths (Algorithm 6).

- **Depth-tagged failures** (Algorithm 7) — `Bt::AtomicBarrier` entries track
  `atomic_depth` so that failures recorded inside an atomic group are not
  incorrectly reused outside it.

Memoization is disabled when the compiled program contains `BackRef`,
`BackRefRelBack`, or `CheckGroup` instructions, whose outcomes depend on outer
capture state and therefore cannot be keyed on `(pc, pos)` alone.

---

### 4. `FoldSeq` / `FoldSeqBack` — multi-codepoint Unicode case folding

Case-insensitive patterns (e.g. `(?i)hello`) are compiled to `FoldSeq` /
`FoldSeqBack` instructions instead of individual `Char(c, true)` /
`CharBack(c, true)`.  This enables correct matching across multi-codepoint
Unicode case folds such as `ß` ↔ `ss` and `ﬁ` ↔ `fi`.

**`fold_advance`** (forward, used by `FoldSeq`): allocates one small `Vec<char>`
per `exec()` call, accumulates the Unicode case folds of successive text chars,
and returns as soon as the accumulated sequence equals the compiled fold
sequence.

**`fold_retreat`** (backward, used by `FoldSeqBack` in lookbehind): previously
allocated two new `Vec<char>` allocations per iteration.  Fixed to extend the
existing buffer then `rotate_right` in-place, reducing allocations to
amortised O(1).

---

## Known remaining bottlenecks

| Pattern class | Behaviour | Fix |
|---------------|-----------|-----|
| Exponential alternation (`a?^n a^n`) | **O(n²)** with memo | ✅ memoization implemented |
| `a*b` no-match on all-`'a'` text | O(n) with required_char | ✅ required_char implemented |
| Look-around in inner loops | ✅ sub-exec results cached (Alg. 6) | ✅ implemented |
| Back-references, subexpression calls | Cannot use DFA/NFA simulation; memo disabled | Inherent in feature set |
| Greedy match itself (`a+` on long text) | O(n) per start pos attempted | Inherent; mitigated by `LiteralPrefix`/`FirstChars` |
| Case-insensitive multi-char patterns (`(?i)hello`) | `FoldSeq` forces `Anywhere` start strategy | `FoldFirstChar` start strategy (future work) |
