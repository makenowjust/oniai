# TODO: Use `memchr` crate for `FirstChars` and `LiteralPrefix` scanning

## Status: Planned

## Problem

`StartStrategy::FirstChars` scans the haystack by calling `text[pos..].find(c)`
for each candidate character.  Rust's `str::find(char)` is reasonably fast for
single-byte ASCII chars but is not SIMD-vectorized on all platforms.

`StartStrategy::LiteralPrefix` uses `text[pos..].find(prefix_str)` which goes
through `str::find(&str)`.  Depending on prefix length and platform this may not
use the fastest available SIMD substring-search algorithm.

The [`memchr`](https://crates.io/crates/memchr) crate provides:
- `memchr::memchr(b, haystack)` — single-byte SIMD search (AVX2/SSE2/NEON)
- `memchr::memchr2(b1, b2, haystack)` — 2-byte SIMD search
- `memchr::memchr3(b1, b2, b3, haystack)` — 3-byte SIMD search
- `memchr::memmem::find(haystack, needle)` — multi-byte substring search
  (Two-Way + Raita + SIMD acceleration)

`memchr` is already an indirect dependency of the `regex` crate (used in
benchmarks), so there is no new transitive dependency risk.

## Proposed Solution

### `Cargo.toml`

```toml
[dependencies]
memchr = "2"
```

### `StartStrategy::FirstChars` scan loop

When `chars` has 1–3 ASCII-only characters, convert them to bytes and delegate
to `memchr::memchr` / `memchr2` / `memchr3`:

```rust
StartStrategy::FirstChars(chars) => {
    // Fast path: 1–3 ASCII bytes → SIMD memchr
    let bytes: Vec<u8> = chars.iter()
        .filter(|&&c| (c as u32) < 128)
        .map(|&c| c as u8)
        .collect();
    let has_non_ascii = chars.iter().any(|&c| (c as u32) >= 128);

    let haystack = text[start_pos..].as_bytes();
    // ... use memchr/memchr2/memchr3, then fall back to try_at only at
    // candidate offsets; non-ASCII chars still use str::find fallback.
}
```

### `StartStrategy::LiteralPrefix` scan loop

Replace `text[pos..].find(prefix.as_str())` with
`memchr::memmem::find(text[pos..].as_bytes(), prefix.as_bytes())`:

```rust
StartStrategy::LiteralPrefix(prefix) => {
    let needle = prefix.as_bytes();
    let mut haystack_start = start_pos;
    while let Some(off) = memchr::memmem::find(&text.as_bytes()[haystack_start..], needle) {
        let candidate = haystack_start + off;
        if let Some(r) = self.try_at(text, candidate, &mut memo, scratch) {
            return Some(r);
        }
        haystack_start = candidate + 1;
    }
    None
}
```

## Expected Benchmark Impact

- `literal/match_mid_1k`: small improvement (prefix scanning is already fast
  via `str::find`, but `memmem` may win on longer prefixes).
- `FirstChars` with 1–3 ASCII chars: **5–15%** improvement on platforms with
  AVX2 (the SIMD path processes 32 bytes per cycle).

The improvement is most visible on longer haystacks.

## Implementation Steps

1. [ ] Add `memchr = "2"` to `[dependencies]` in `Cargo.toml`.
2. [ ] Update `StartStrategy::FirstChars` scan arm in `find_with_scratch` to
       use `memchr`/`memchr2`/`memchr3` for 1–3 ASCII chars.
3. [ ] Update `StartStrategy::FirstChars` scan arm in `find_interp` similarly.
4. [ ] Update `StartStrategy::LiteralPrefix` scan arm in both paths to use
       `memchr::memmem::find`.
5. [ ] Run `cargo test` + `cargo clippy --tests`.
6. [ ] Run `cargo bench -- oniai` and save log to `log/`.
