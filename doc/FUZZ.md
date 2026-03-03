# Fuzzing Oniai

Oniai uses [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer)
to find bugs in the parser, compiler, VM, and JIT.

---

## Prerequisites

```sh
rustup toolchain install nightly   # cargo-fuzz requires nightly
cargo install cargo-fuzz
```

---

## Fuzz targets

| Target | File | What it tests |
|--------|------|---------------|
| `fuzz_pattern` | `fuzz/fuzz_targets/fuzz_pattern.rs` | Feed arbitrary bytes as a regex pattern. Finds panics and crashes in the parser and compiler. |
| `fuzz_match` | `fuzz/fuzz_targets/fuzz_match.rs` | Split input into pattern + subject, run `re.find()`. Finds panics in the full VM pipeline (JIT enabled). |
| `fuzz_match_diff` | `fuzz/fuzz_targets/fuzz_match_diff.rs` | Run the same input through both the JIT executor and the pure interpreter, panic if their results disagree. Finds semantic divergence between the two engines. |

### Input format for `fuzz_match` and `fuzz_match_diff`

The first byte `s` encodes a split ratio.  The remaining bytes are divided
into pattern and subject:

```
pat_len = (rest.len() * s) / 256
pattern = rest[..pat_len]   (UTF-8 lossy)
subject = rest[pat_len..]   (UTF-8 lossy)
```

---

## Running a target

```sh
# Run fuzz_match_diff for 60 seconds
cargo +nightly fuzz run fuzz_match_diff -- -max_total_time=60

# Run fuzz_pattern with 4 parallel jobs
cargo +nightly fuzz run fuzz_pattern -- -jobs=4

# Run until a crash is found (no time limit)
cargo +nightly fuzz run fuzz_match
```

### Build only (no run)

```sh
cargo +nightly fuzz build
```

---

## Seed corpus

Pre-seeded corpus entries (extracted from `tests/integration_test.rs`) live in:

```
fuzz/corpus/fuzz_pattern/
fuzz/corpus/fuzz_match/
fuzz/corpus/fuzz_match_diff/
```

libFuzzer merges newly discovered interesting inputs into the corpus
automatically during a run.

---

## Reproducing a crash

Crashes are saved under `fuzz/artifacts/<target>/`.

```sh
# Reproduce and minimise
cargo +nightly fuzz run fuzz_match_diff fuzz/artifacts/fuzz_match_diff/crash-<hash>

# Minimise a crash to the smallest reproducer
cargo +nightly fuzz tmin fuzz_match_diff fuzz/artifacts/fuzz_match_diff/crash-<hash>
```

---

## Bugs found so far

### `(?(0))` — integer underflow in the compiler (fixed)

**Target:** `fuzz_pattern`  
**Minimised input:** `(?(0))`  
**Root cause:** `parse_condition()` accepted group number 0 (groups are
1-indexed).  `compile_conditional()` then computed `(n - 1) * 2` with `n = 0`,
causing an unsigned-integer underflow panic.  
**Fix:** `parse_condition()` in `src/parser.rs` now returns a parse error when
the group number is 0.  
**Regression test:** `group_conditional_num_zero_is_error`

---

### `((?~#)*)` — OOM on absent operator in unbounded loop (fixed)

**Target:** `fuzz_match_diff`  
**Minimised input:** pattern `((?~#)*)`, subject `$`  
**Root cause:** `can_match_empty()` in `src/compile.rs` incorrectly handled
`Node::Absence(inner)` by returning `can_match_empty(inner)` instead of
`!can_match_empty(inner)`.  The absent operator `(?~X)` can match the empty
string whenever `X` cannot match the empty string (the empty string never
contains a match of a non-empty-matching `X`).  Because `can_match_empty`
returned `false` for `(?~#)`, the compiler emitted a bare `Fork/Jump` loop
without `NullCheckStart`/`NullCheckEnd` guards.  The VM then kept re-entering
the loop body at the same position, pushing an unbounded number of backtrack
entries until OOM.  
**Fix:** `can_match_empty` for `Node::Absence(node)` now returns
`!can_match_empty(node)`, matching the true semantics of the absent operator.  
**Regression test:** `null_loop_check_absence_body`

---

### `()+` — infinite loop on empty-matching loop bodies (fixed)

**Target:** `fuzz_match_diff`  
**Minimised input:** pattern `()+`, subject `ar)bar`  
**Root cause:** Unbounded loops (`*`, `+`, `{n,}`) whose body can match the
empty string caused an infinite loop: the engine kept retrying without
advancing position.  
**Fix:** `NullCheckStart`/`NullCheckEnd` instructions bracket the optional body
of every unbounded loop whose body is nullable (determined statically by
`can_match_empty()` in `src/compile.rs`).  On a null (zero-length) iteration
the engine commits the current captures and exits the loop — matching Onigmo's
behaviour.  Non-nullable bodies (e.g. `[a-z]+`) are unaffected and incur no
overhead.  
**Regression test:** `null_loop_check_empty_body`

---

### `\u{FFFD}??\x02\u{FFFD}` — JIT/interpreter divergence via wrong CharFast at ForkNext alt (fixed)

**Target:** `fuzz_match_diff`  
**Minimised input:** pattern `\u{FFFD}??\x02\u{FFFD}`, subject `6\u{FFFD}\u{FFFD}\x02\x02\x02`  
**Root cause:** The JIT emitted a `CharFast` pre-filter at the *alternate* (fall-through) branch of a `ForkNext` instruction without verifying that the current subject byte matched the pre-filter before executing the `CharFast` instruction. When the non-ASCII body of a lazy repetition was bypassed, the fall-through landed directly on `CharFast` for the *next* atom, which then mismatched against a byte it was never supposed to consume.  
**Fix:** Restrict `CharFast` promotion so it is not emitted at positions reachable as the `ForkNext` alternate without first re-validating the leading byte.  
**Regression tests:** `test_fuzz_regression_2`, `test_fuzz_regression_3`

---

### `[\x7fe-\x7fa--]` — subtract-with-overflow panic in JIT `emit_range_check` (fixed)

**Target:** `fuzz_match_diff`  
**Minimised input:** pattern `[\x7fe-\x7fa--]`  
**Root cause:** The parser accepted character class ranges where `lo > hi` (e.g. `[a--]` where `'a'=0x61 > '-'=0x2D`). `emit_range_check` in the JIT then computed `(hi - lo)` as a `u8` subtraction, panicking on underflow.  
**Fix:** `parse_class_item` in `src/parser.rs` now returns a parse error when `lo > hi`. Defense-in-depth: `charset_ascii_ranges` in the JIT skips clamped ranges where `hi < lo`.  
**Regression test:** `test_fuzz_regression_4`

---

### `*\u{FFFD}*` — JIT SpanChar non-ASCII pre-filter wrong leading byte (fixed)

**Target:** `fuzz_match_diff`  
**Minimised input:** pattern `*\u{FFFD}*`, subject contains `\xff` bytes (lossy-decoded to `\u{FFFD}`)  
**Root cause:** The JIT `SpanChar` handler for non-ASCII chars computed the expected UTF-8 leading byte as `(*c as u32).to_le_bytes()[0]` — the first byte of the little-endian Unicode codepoint, **not** the UTF-8 leading byte. For U+FFFD (codepoint 0xFFFD), this gives `0xFD` instead of the correct UTF-8 leading byte `0xEF`. The pre-filter comparison always failed, causing `SpanChar` to exit immediately without consuming any U+FFFD characters.  
**Fix:** Use `c.encode_utf8(&mut buf)[0]` to obtain the correct UTF-8 leading byte.  
**Regression tests:** `test_fuzz_regression_5`, `test_fuzz_regression_6`
