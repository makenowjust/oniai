# Optimization 1: First-byte prefilter from IR analysis

## Problem

`StartStrategy::compute` currently works only on `Vec<Inst>`. It derives scanning
strategies (`FirstChars`, `RangeStart`, `AsciiClassStart`, etc.) that let the
unanchored search loop skip positions that cannot start a match.

There are two gaps:

1. **Coverage gap**: The IR's first block (after DCE, guard, and span passes) has
   a cleaner structure than `Vec<Inst>`, making first-byte analysis easier and
   more complete. Patterns whose first real IR statement is, e.g., a `Fork` with
   `IrGuard::Char` candidates may currently fall through to `StartStrategy::Anywhere`.

2. **Speed gap**: The existing `RangeStart` / `AsciiClassStart` scans loop byte-by-byte
   in Rust. For a range like `[0-9]` (10 possible bytes), `memchr` + SIMD would be
   faster. For an arbitrary byte-set (e.g., `[A-Za-z]`), a 256-bit lookup table
   combined with a word-at-a-time scan is faster than the current bitmap check.

## Benchmark targets

| Benchmark | oniai/jit | regex | gap |
|-----------|----------:|------:|-----|
| `real_world/title_name` | 10 µs | 7 µs | 1.4× |
| `case_insensitive_alt/find_all` | 17.9 µs | 9.7 µs | 1.9× |
| `class_start/digit_sparse` | 4.5 µs | 3.7 µs | 1.2× |

## Approach

### Step 1 — Derive `FirstByteSet` from `IrProgram`

Add a function in `src/ir/mod.rs` (or a new `src/ir/prefilter.rs`):

```rust
pub fn first_byte_set(prog: &IrProgram) -> Option<[u64; 4]>
```

Walk the entry block of `IrRegion::Main`. For each IR statement/terminator,
determine the set of bytes that can appear at `text[pos]` for the NFA to have
any chance of matching:

- `IrStmt::MatchChar(c)` → single byte (if ASCII) or UTF-8 leading bytes
- `IrStmt::MatchClass { id, .. }` → the charset's `ascii_bits`
- `IrTerminator::Fork { candidates }` → union of each candidate's first byte if
  guard is `IrGuard::Char(c)` (easy) or if the candidate's first block starts
  with a `MatchChar`/`MatchClass` stmt
- `IrTerminator::SpanChar { c, exit }` → include `c` and whatever `exit` starts with
- `IrTerminator::Branch` → follow the target block
- `IrStmt::MatchAnyChar { .. }` / non-deterministic cases → bail out (`None`)

The result is a 256-bit bitmap (`[u64; 4]`) covering all 256 byte values.

### Step 2 — New `StartStrategy::ByteSetStart` variant

```rust
ByteSetStart {
    /// 256-bit bitmap: bit `b` set means byte `b` is a valid start candidate.
    bits: [u64; 4],
    /// Set if every byte in the bitmap is ASCII (< 0x80).
    ascii_only: bool,
}
```

Add this variant to the `StartStrategy` enum in `src/vm.rs`.

The search loop for `ByteSetStart`:

```rust
StartStrategy::ByteSetStart { bits, ascii_only } => {
    let bytes = text.as_bytes();
    let mut pos = start_pos;
    loop {
        // Find next candidate using word-at-a-time scan.
        let candidate = bytes[pos..].iter()
            .position(|&b| (bits[(b >> 6) as usize] >> (b & 63)) & 1 != 0)
            .map(|o| pos + o)?;
        if let Some(result) = self.try_at(text, candidate, &mut memo, scratch) {
            return Some(result);
        }
        pos = candidate + 1;
    }
}
```

(LLVM auto-vectorizes the bitmap check loop on ARM/x86 when the bitmap is
constant. If benchmarks show this is insufficient, replace with an explicit
`memchr`-style SWAR loop.)

### Step 3 — Wire into `StartStrategy::compute` and IR path

In `CompiledRegex::new`:
1. After building `IrProgram`, call `first_byte_set(&ir_prog)`.
2. If it returns `Some(bits)` and the existing `StartStrategy` is `Anywhere`,
   override with `ByteSetStart`.
3. If `bits` represents a single contiguous ASCII range, keep the existing
   `RangeStart` (it already compiles well).
4. If `bits` represents a single ASCII byte, downgrade to `FirstChars([c])` (memchr).

### Step 4 — Extend interp search loop

The interpreter's `find_interp` (around line 2383 in `vm.rs`) also uses
`StartStrategy`. The same `ByteSetStart` match arm needs to be added there.

## Files to change

- `src/ir/mod.rs` (or new `src/ir/prefilter.rs`) — `first_byte_set()`
- `src/vm.rs` — new `ByteSetStart` variant + search loop arms

## Success criterion

- `class_start/digit_sparse` and `case_insensitive_alt/find_all` improve or stay equal
- `real_world/title_name` closes further toward 7 µs
- All 138 tests pass, zero clippy warnings
