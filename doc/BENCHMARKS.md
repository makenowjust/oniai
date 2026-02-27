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

## Real-world benchmarks (`benches/fixtures/stud.txt`)

Haystack: *A Study in Scarlet* by Arthur Conan Doyle (~260 KB plain text).
Measures full `find_iter().count()` over the entire text.

| Benchmark | Pattern | Interpreter | JIT Phase 4 | JIT Phase 5 | JIT Phase 6 | Speedup |
|-----------|---------|-------------|-------------|-------------|-------------|---------|
| `real_world/literal_count` | `Holmes` | 132.6 µs | 132.3 µs | 131.3 µs | 138.5 µs | 0.96× |
| `real_world/capitalized_words` | `[A-Z][a-z]+` | 4.33 ms | **2.75 ms** | **2.67 ms** | **2.25 ms** | **1.92×** ✅ |
| `real_world/posix_digits` | `[[:digit:]]+` | 3.21 ms | **2.25 ms** | **2.23 ms** | **1.91 ms** | **1.68×** ✅ |
| `real_world/quoted_strings` | `"[^"]*"` | 7.38 ms | **6.78 ms** | **5.85 ms** | **5.66 ms** | **1.30×** ✅ |
| `real_world/title_name` | `Mrs?\. [A-Z][a-z]+` | 148.2 µs | 143.3 µs | 141.0 µs | 140.7 µs | 1.05× |

**Notes:**
- `literal_count` uses the `LiteralPrefix` start strategy (SIMD `str::find`), so both paths are dominated by the same scan; JIT overhead is negligible.
- `capitalized_words` and `posix_digits` show the biggest gains because the JIT Phase 3/4 charclass inline (`[A-Z]`, `[a-z]`, `[[:digit:]]`) replaces helper calls with inlined range comparisons across a 260 KB scan.  Phase 5 gives a further small gain from cheaper Fork bookkeeping.
- `quoted_strings` uses a negated charclass (`[^"]`) which falls back to the `jit_match_class` helper.  Phase 5 gives a larger relative gain here (~14%) because `"[^"]*"` has many Fork iterations with no intra-loop saves.
- `title_name` is dominated by the literal-prefix scan for `Mr`; the charclass portion is a small fraction of total work.

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

---

## JIT compilation (`--features jit`)

Added in Phase 1; inlined in Phase 2.  Compiles eligible `Vec<Inst>` programs to native machine
code via Cranelift, creating one basic block per instruction and routing
backtracking through a `br_table` dispatch block.

### Phase 1 vs Phase 2 vs Phase 3 vs Phase 4 vs Phase 5 vs Interpreter — median times

Phase 1: all instructions as `extern "C"` helper calls.  
Phase 2: `Char`, `AnyChar`, `Shorthand` (ASCII fast-path), `Save`, `Anchor` inlined as Cranelift IR.  
Phase 3: additionally inline `CharClass`/`ClassBack` for simple ASCII charsets (all items ASCII `Char` or `Range`, no negation).  
Phase 4: extend `CharClass`/`ClassBack` inline to POSIX always-ASCII classes and ASCII-safe `Shorthand` items; inline `ShorthandBack`.  
Phase 5: capture-delta undo log — push `SaveUndo` before each slot write instead of snapshotting the full `slots` Vec on every `Fork`.  
Phase 6: inline fork push and bt-pop fast path — write `MemoMark` + `Retry` directly in Cranelift IR; peek-and-pop top `Retry` inline; hoist `Vec<BtJit>` allocation across `exec_jit` calls; 24-byte `repr(C)` `BtJit` struct (stable layout, zero padding waste).

| Benchmark | Interpreter | JIT Phase 1 | JIT Phase 2 | JIT Phase 3 | JIT Phase 4 | JIT Phase 5 | JIT Phase 6 |
|-----------|-------------|-------------|-------------|-------------|-------------|-------------|-------------|
| literal/no\_match\_1k | 49.8 ns | 49.7 ns (1.00×) | 49.5 ns (1.01×) | 49.8 ns (1.00×) | 50.3 ns (0.99×) | 50.0 ns (1.00×) | 50.0 ns (1.00×) |
| literal/match\_mid\_1k | 145 ns | 180 ns (0.81×) | 139 ns (**1.04×**) | 138 ns (**1.05×**) | 140 ns (**1.04×**) | 138 ns (**1.05×**) | 138 ns (**1.05×**) |
| anchored/no\_match\_1k | 16.1 ns | 47.2 ns (0.34×) | 14.0 ns (**1.15×**) | 13.6 ns (**1.18×** ✅) | 13.7 ns (**1.17×** ✅) | 13.4 ns (**1.20×** ✅) | 13.0 ns (**1.24×** ✅) |
| alternation/4\_alts\_match | 18.9 µs | 18.9 µs (1.00×) | 18.9 µs (1.00×) | 18.8 µs (1.00×) | 18.9 µs (1.00×) | 18.8 µs (1.00×) | 18.7 µs (1.01×) |
| alternation/4\_alts\_no\_match | 46.8 µs | 46.7 µs (1.00×) | 46.7 µs (1.00×) | 46.7 µs (1.00×) | 46.7 µs (1.00×) | 46.6 µs (1.00×) | 46.7 µs (1.00×) |
| quantifier/greedy\_no\_match\_500 | 25.8 ns | 25.8 ns (1.00×) | 27.3 ns (0.95×) | 25.8 ns (1.00×) | 25.7 ns (1.00×) | 25.8 ns (1.00×) | 25.8 ns (1.00×) |
| quantifier/greedy\_match\_500 | 9.17 µs | 6.11 µs (**1.50×**) | 5.61 µs (**1.63×**) | 5.43 µs (**1.69×** ✅) | 5.45 µs (**1.68×** ✅) | **3.00 µs (3.06×** ✅) | **1.83 µs (5.01×** ✅) |
| captures/two\_groups | 612 ns | 603 ns (1.01×) | 627 ns (0.98×) | 620 ns (0.99×) | 616 ns (0.99×) | 617 ns (0.99×) | 597 ns (**1.03×**) |
| captures/iter\_all | 2.57 µs | 2.54 µs (1.01×) | 2.62 µs (0.98×) | 2.58 µs (1.00×) | 2.59 µs (0.99×) | 2.57 µs (1.00×) | 2.54 µs (1.01×) |
| email/find\_all | 3.25 µs | 4.19 µs (0.78×) | 3.53 µs (0.92×) | 3.46 µs (0.94×) | 3.50 µs (0.93×) | 3.28 µs (0.99×) | **2.71 µs (1.20×** ✅) |
| charclass/alpha\_iter | 31.2 µs | 38.4 µs (0.80×) | 39.7 µs (0.78×) | **24.9 µs (1.25×** ✅) | **25.0 µs (1.25×** ✅) | **21.0 µs (1.49×** ✅) | **17.4 µs (1.79×** ✅) |
| charclass/posix\_digit\_iter | 24.4 µs | 37.3 µs (0.65×) | 37.9 µs (0.64×) | 37.7 µs (0.65×) | **21.4 µs (1.14×** ✅) | **19.1 µs (1.28×** ✅) | **13.6 µs (1.79×** ✅) |
| case\_insensitive/match | 14.0 µs | 14.2 µs (0.98×) | 14.0 µs (1.00×) | 13.9 µs (1.01×) | 14.1 µs (0.99×) | 14.0 µs (1.00×) | 14.0 µs (1.00×) |
| find\_iter\_scale/100 | 2.72 µs | 5.22 µs (0.52×) | 3.37 µs (0.81×) | 3.31 µs (0.82×) | 3.29 µs (0.83×) | 3.14 µs (0.87×) | **2.48 µs (1.10×** ✅) |
| find\_iter\_scale/500 | 13.3 µs | 25.5 µs (0.52×) | 16.5 µs (0.81×) | 16.2 µs (0.82×) | 16.2 µs (0.82×) | 15.3 µs (0.87×) | **12.1 µs (1.10×** ✅) |
| find\_iter\_scale/1000 | 26.7 µs | 51.3 µs (0.52×) | 33.1 µs (0.81×) | 32.6 µs (0.82×) | 32.3 µs (0.83×) | 30.6 µs (0.87×) | **24.2 µs (1.10×** ✅) |
| find\_iter\_scale/5000 | 133 µs | 255 µs (0.52×) | 165 µs (0.81×) | 161 µs (0.83×) | 162 µs (0.82×) | 153 µs (0.87×) | **121 µs (1.10×** ✅) |
| pathological/10 | 4.39 µs | 6.38 µs (0.69×) | 5.30 µs (0.83×) | 5.30 µs (0.83×) | 5.29 µs (0.83×) | 5.09 µs (0.86×) | 4.75 µs (0.92×) |
| pathological/15 | 9.94 µs | 14.2 µs (0.70×) | 11.9 µs (0.84×) | 11.9 µs (0.84×) | 11.95 µs (0.83×) | 11.54 µs (0.86×) | 10.96 µs (0.91×) |
| pathological/20 | 17.7 µs | 25.0 µs (0.71×) | 21.1 µs (0.84×) | 21.0 µs (0.84×) | 21.2 µs (0.84×) | 20.5 µs (0.86×) | 19.5 µs (0.91×) |

### Analysis

#### Phase 1 (helper calls only)

Phase 1 JIT is slower than the interpreter in most cases.  The sole
speedup is `quantifier/greedy_match_500` (+50%), where the tight
`Fork → Char → Jump` loop benefits from direct block-to-block jumps that
eliminate the interpreter's `match`-dispatch overhead.

The regressions have two root causes:

1. **Per-instruction helper calls.**  Every instruction (`Char`, `Class`,
   `Shorthand`, …) emits an `extern "C"` call to a Rust helper function.
   Cranelift eliminates the interpreter's `match` dispatch (~2 cycles), but
   the C ABI function call costs ~10–20 cycles.  The `find_iter_scale`
   benchmarks show a consistent **1.92× slowdown** because every position check
   calls `jit_match_shorthand`.

2. **`exec_jit` setup per match attempt.**  Each call allocates a new
   `Vec<BtJit>` and fills in a 16-field `JitExecCtx` struct.  For short haystacks
   the fixed overhead dominates.  `anchored/no_match_1k` (2.9× slowdown) is
   the clearest example.

#### Phase 2 (inlined IR)

Phase 2 inlines `Char`, `AnyChar`, `Shorthand` (ASCII fast-path), `Save`,
and `Anchor` instructions as Cranelift IR.  Results:

- **`anchored/no_match_1k`: 2.9× slower → 1.15× faster** — the Anchor
  inline eliminates all instruction dispatch for the anchor check, recovering
  the per-call setup cost almost entirely.
- **`quantifier/greedy_match_500`: 1.50× → 1.63× faster** — inline `Char`
  tightens the inner loop further.
- **`find_iter_scale`: 1.92× → 1.24× slower** — shorthand ASCII fast-path
  reduces calls to `jit_match_shorthand` for ASCII text, cutting the regression
  significantly.  Non-ASCII paths still call the helper for correctness.
- **`literal/match_mid_1k`: 0.81× → 1.04× faster** — inline `Char` beats
  the interpreter for this pattern.
- **`pathological`: 0.70× → 0.84×** — consistent improvement across Fork-heavy
  patterns; the `Save` inline reduces per-iteration overhead.

**Remaining regressions** — `charclass/alpha_iter` and `charclass/posix_digit_iter`
still regress (~1.5×) because `CharClass` matching still calls a helper.
`find_iter_scale` remains 1.24× slower due to residual JIT overhead and the
non-ASCII fallback call on each character boundary.

#### Phase 3 (inline simple ASCII CharClass)

Phase 3 adds `is_simple_ascii_charset` to detect charsets containing only
ASCII `Char`/`Range` items with no negation or intersections, and inlines
both `Class` (forward) and `ClassBack` (backward) for such charsets.

- **`charclass/alpha_iter`: 1.27× slower → 1.25× faster** — the pattern
  `[a-zA-Z]+` has two ASCII ranges; inlining them as two `(byte - lo) ≤ span`
  unsigned comparisons eliminates the `jit_match_class` call, making the JIT
  25% faster than the interpreter.
- **`quantifier/greedy_match_500`: 1.63× → 1.69× faster** — minor further
  improvement from reduced code-cache pressure.
- **`anchored/no_match_1k`: 1.15× → 1.18× faster** — slight improvement.
- **`charclass/posix_digit_iter`**: unchanged at 0.65× — this pattern uses
  a POSIX character class (`\p{Digit}`), which requires a helper call and is
  not inlinable.

#### Phase 4 (POSIX and Shorthand charclass items; ShorthandBack inline)

Phase 4 extends the charclass inline to handle POSIX classes that are always
ASCII (Digit, Space, Blank, XDigit, Ascii, Cntrl, Punct) and ASCII-safe
Shorthand items (Digit, Space, HexDigit; Word when `ascii_range=true`).  Only
non-negated POSIX items are eligible: a negated item like `[[:^digit:]]` can
match non-ASCII bytes so the "fail on non-ASCII" fast path would be incorrect.
`ShorthandBack` is now also inlined (previously called a helper unconditionally).

- **`charclass/posix_digit_iter`: 0.65× → 1.14× faster** — `[[:digit:]]` is
  now inlined as a single `Range('0','9')` check; the 73% regression is fully
  reversed into a 14% speedup. ✅
- All other benchmarks are stable (within noise); the charclass and shorthand
  gains from Phase 3 are preserved.

**Remaining regressions** — `find_iter_scale` and `pathological` still run at
~0.82–0.84× of interpreter speed.  Profiling points to `BtJit::Retry` storing
`slots: Vec<u64>` (a heap allocation per Fork attempt) as the primary cause.
For `\d+` on an all-`'a'` haystack the JIT creates one `Vec` allocation for
each start position tried, whereas the interpreter uses an inline stack entry.

#### Phase 5 (capture-delta undo log)

Phase 5 eliminates the `slots: Vec<u64>` field from `BtJit::Retry` entirely.
Instead of snapshotting the full capture-slots array on every `Fork`, each
`Save` instruction now emits a `jit_push_save_undo(ctx, slot, old_value)` call
**before** writing the new value, pushing a tiny `BtJit::SaveUndo { slot,
old_value }` entry onto the backtrack stack.  Backtracking replays these undo
entries in reverse order until it reaches the `Retry` entry — exactly the
approach used by Onigmo and Oniguruma.

`BtJit::Retry` is now `{ block_id: u32, pos: u64, keep_pos: u64 }` — 20 bytes
on the stack, zero heap allocation.  For patterns with no capture groups (or
where the hot fork loop contains no `Save` instructions) there is no write to
the undo log at all inside the loop.

- **`quantifier/greedy_match_500`: 1.68× → 3.06× faster** ✅ — the tight
  `Fork → Char → Jump` loop has no `Save` instructions; the per-fork heap
  allocation is gone, and `Retry` now fits in a single cache line.
- **`charclass/alpha_iter`: 1.25× → 1.49× faster** ✅ — same reason: `[a-zA-Z]+`
  has no intra-loop saves.
- **`charclass/posix_digit_iter`: 1.14× → 1.28× faster** ✅ — `[[:digit:]]+`
  has no intra-loop saves.
- **`find_iter_scale`: 0.82× → 0.87×** — still below interpreter speed
  (residual overhead from `exec_jit` setup and the `jit_push_save_undo` call
  on the initial `Save(0)` / `Save(1)` for group 0), but the gap narrowed.
- **`pathological`: 0.84× → 0.86×** — slight improvement from smaller `Retry`.
- `captures` and `case_insensitive` benchmarks are within noise.

#### Phase 6 (inline fork push and bt-pop fast path)

Phase 6 is a set of mutually-reinforcing changes that together eliminate almost
all `extern "C"` overhead from the hot fork/backtrack loop:

1. **24-byte `repr(C)` `BtJit` struct** — replaces the `#[repr(C, u32)]` enum
   (32 bytes due to C-ABI discriminant + 4-byte padding gap) with a `repr(C)`
   struct `{ tag: u32, a: u32, b: u64, c: u64 }` (4+4+8+8 = 24 bytes, same
   size as Rust's niche-optimised non-repr layout but with stable field offsets).
   The 33% reduction in stack-entry size cuts cache pressure: at peak the
   `find_iter_scale/1000` bt stack is ~48 KB vs the regressed ~64 KB.

2. **Inline bt-pop fast path** — at the start of `bt_resume_block`, the JIT
   peeks at the top entry's tag without a function call.  If `tag == 0` (Retry),
   it decrements `bt_len`, reads `a/b/c` directly, updates `ctx.keep_pos` and
   `ctx.bt_retry_count`, then jumps to the dispatch table.  The `extern "C"`
   `jit_bt_pop` call is only issued when the top entry is not a Retry (i.e.,
   there is a `SaveUndo`, `AtomicBarrier`, or `MemoMark` to process first).

3. **Inline fork push** — `Fork` and `ForkNext` now emit Cranelift IR that
   writes the `MemoMark` and `Retry` entries directly to the raw bt buffer,
   bypassing `jit_fork` / `jit_fork_next` entirely when two conditions hold:
   (a) `bt_len + N ≤ bt_cap` (capacity check — no realloc needed), and
   (b) `memo_has_failures == 0` (no recorded fork failures — the memo fast-path
   short-circuit check is not needed).
   When either condition fails the slow path calls the `extern "C"` helper.

4. **Hoisted bt allocation** — `exec_jit` now transfers ownership of the
   `Vec<BtJit>` allocation to `JitExecCtx` via raw-pointer fields
   (`bt_data_ptr`, `bt_len`, `bt_cap`) and reclaims it after each call.  The
   allocation is pre-sized once and reused across all `exec_jit` calls within a
   `find()` invocation (typically hundreds of calls for `find_iter`).

5. **`bt_retry_count` guard** — `jit_push_save_undo` returns immediately when
   `ctx.bt_retry_count == 0` (no active retry points), skipping the push
   entirely for every `Save` instruction that fires before any `Fork`.

- **`quantifier/greedy_match_500`: 3.06× → 5.01× faster** ✅ — the tight
  `Fork → Char → Jump` loop with no captures now runs entirely in inlined
  Cranelift IR; every fork avoids two extern C calls.
- **`charclass/alpha_iter`: 1.49× → 1.79× faster** ✅ — same: inline charclass
  IR + inline fork together make the tight `[a-zA-Z]+` loop nearly overhead-free.
- **`charclass/posix_digit_iter`: 1.28× → 1.79× faster** ✅ — same gains.
- **`find_iter_scale`: 0.87× → 1.10× faster** ✅ — the JIT is now faster than
  the interpreter for `\d+` scanning.  The `bt_retry_count` guard eliminates the
  `jit_push_save_undo` call for every start-of-match `Save(0)` before the first
  `Fork`, and the inline fork eliminates `jit_fork` / `jit_bt_pop` from the
  tight inner loop.
- **`email/find_all`: 0.99× → 1.20× faster** ✅ — similar pattern to find_iter.
- **`pathological`: 0.86× → 0.91×** — still below interpreter speed.  For the
  `(a?)^n a^n` pattern, `memo_has_failures` becomes 1 after the first few failed
  fork states, causing every subsequent fork to fall back to the slow `jit_fork`
  path that checks the failure cache.

### Known bottlenecks and future work

| Root cause | Current | Future plan |
|------------|---------|-------------|
| `pathological` overhead | 0.91× | Inline memoisation check in JIT IR; currently falls back to extern C once any failure is recorded |
| Non-ASCII shorthand fallback | helper call per non-ASCII char | Inline two/three-byte UTF-8 decode for common cases |
| Unicode / negated `CharClass` | helper call ~15 cy | Inline POSIX Alpha/Word for `ascii_range=true` patterns |

See `doc/JIT.md` for the full design.
