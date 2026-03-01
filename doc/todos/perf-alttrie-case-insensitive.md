# TODO: AltTrie for case-insensitive alternation (`(?i)foo|bar|baz`)

## Status: Done (commit `oswwxqpp`)

## Problem

`Inst::AltTrie` is only emitted when `!flags.ignore_case` (see
`compile_alternation_inner`).  Patterns like `(?i)get|post|put|delete` cannot
use the trie and fall back to a Fork chain, even though a case-folded trie
would work correctly.

## Proposed Solution

At trie-build time, insert all simple-case-fold variants of each string into
the `ByteTrie`.  This requires:

1. For each alternative string `s`, enumerate all byte sequences that are
   case-fold-equivalent to `s` under the ASCII simple case fold (e.g. `get` →
   `get`, `Get`, `gEt`, …, `GET` — 8 variants for a 3-ASCII-char string).

2. For non-ASCII chars that have single-codepoint folds, add those variants
   too.

3. Multi-codepoint folds (ß → ss) make the strings non-prefix-free and would
   require special handling; skip for now.

The `is_prefix_free` check must still pass after adding all variants.

## Constraints / Complexity

- For a string of length `n` with `k` case-foldable ASCII chars, there are
  `2^k` variants.  Strings longer than ~20 chars or with many foldable chars
  explode combinatorially.  Impose a cap: only emit `AltTrie` if the total
  variant count across all alternatives is ≤ 1024.

- The reversed trie for `AltTrieBack` must also be built from case-folded
  reversed strings.

- `StartStrategy::LiteralSet` emitted for `AltTrie` at the start must also
  enumerate case-folded variants (or fall back to `CaselessPrefix`).

## Expected Benchmark Impact

Benefits only patterns that use `(?i)` with plain-string alternations.  No
current benchmark exercises this; a new benchmark would be needed to measure.

## Implementation Steps

1. [ ] Add `expand_alts_case_folded(alts: &[String]) -> Option<Vec<String>>`
       in `compile.rs` — returns `None` if variant count exceeds cap.
2. [ ] In `compile_alternation_inner`, call this when `ic=true` and all alts
       are plain strings; build the AltTrie from the expanded set.
3. [ ] Ensure `is_prefix_free` still holds on the expanded set.
4. [ ] Update `StartStrategy::compute` to handle case-insensitive `AltTrie`
       (use `CaselessPrefix` or `LiteralSet` with variants).
5. [ ] Add a benchmark for `(?i)get|post|put|delete` on a long haystack.
6. [ ] Run `cargo test` + `cargo clippy --tests`.
7. [ ] Run `cargo bench -- oniai` and save log to `log/`.
