# perf-jit-span-quantifier — JIT tight loop for simple greedy quantifiers

**Status:** Planned

## Problem

Greedy quantifiers over a single-character body (e.g. `a+`, `[a-z]+`, `\w+`)
are compiled as:

```
Fork(exit)          ; push backtrack entry
Char('a')           ; match body (or Class(idx))
Jump(Fork)          ; loop back
exit:
```

Each iteration of the NFA executor pushes a `Bt::Retry` entry onto the
backtracking stack, then pops and discards it once the pattern succeeds.
For a 500-char run of `a`s, this means 500 push/pop cycles plus 500
`try_at` invocations.

The JIT compiles this as a loop over these same instructions; the overhead
of stack management dominates on long spans.

### Benchmark evidence

| Benchmark | oniai/jit | pcre2 | gap |
|-----------|----------:|------:|----:|
| `quantifier/greedy_match_500` | 1.97 µs | 372 ns | **5.3×** |
| `find_iter_scale/oniai/jit/5000` | 58.5 µs | 68.0 µs | ~0.9× |
| `real_world/[[:digit:]]+/jit` | 139 µs | 184 µs | oniai wins |

Note: oniai already beats pcre2 on full-text digit iteration thanks to
`AsciiClassStart`.  The gap on the single-match `greedy_match_500` benchmark
is the pure inner-loop cost.

## Proposed fix: `SpanChar` and `SpanClass` instructions

Add two new VM instructions that implement a **possessive-style tight scan
loop** for the common case of a greedy quantifier with a simple body:

```rust
/// Advance pos as long as text[pos] == c. Then jump to exit_pc.
/// No backtrack entries are pushed (greedy, no rollback needed until
/// the continuation fails — handled by the surrounding Fork at compile time).
SpanChar { c: char, exit_pc: usize },

/// Advance pos as long as charsets[idx].matches(text[pos]).
/// Then jump to exit_pc.
SpanClass { idx: usize, ic: bool, exit_pc: usize },
```

### Compiler change

In `compile_quantifier_inner`, when the quantifier is `Greedy` with
`min == 0` or `min == 1`, and the body node is a simple `Char` or `Class`
(detected via a helper `is_simple_char_node`), emit the mandatory `min`
copies as before, then instead of `Fork + body + Jump`, emit a single
`SpanChar`/`SpanClass` instruction.

```
(original greedy loop for a+)
Char('a')      ; mandatory copy (min=1)
Fork(exit)     ; greedy loop header
Char('a')
Jump(Fork)
exit:

(new with SpanChar)
Char('a')      ; mandatory copy (min=1)
SpanChar { c: 'a', exit_pc: next }
```

The `SpanChar`/`SpanClass` instruction in the VM runs a tight Rust loop:

```rust
Inst::SpanChar { c, exit_pc } => {
    while pos < text.len() && text[pos..].starts_with(*c) {
        pos += c.len_utf8();
    }
    pc = *exit_pc;
}
```

No backtrack entries are pushed.  The outer NFA will have a `Fork` before
the mandatory body copy if needed (for `a*`), which handles the "zero match"
case.

### JIT change

Emit a tight native loop in the JIT for `SpanChar`/`SpanClass`:

```
; SpanChar('a', exit_pc):
loop_top:
  cmp ctx_pos, text_len
  jge exit_pc
  load byte text[ctx_pos]
  cmp byte, 'a'
  jne exit_pc
  inc ctx_pos
  jmp loop_top
exit_pc:
```

For `SpanClass`, the loop body calls `jit_match_class` inline (bitmap for
ASCII, helper for non-ASCII).

### Correctness note

`SpanChar`/`SpanClass` is only valid when the body **cannot** produce a
zero-length match (which is always true for `Char`/`Class`).  The
`can_match_empty` guard in the quantifier compiler already handles this.

The instruction is **greedy without backtracking** — equivalent to a
possessive `(a)++`.  This is correct because the body is a single character:
backtracking into the span would only reduce the match length without
enabling anything new.  (Standard greedy semantics are preserved because
the continuation checks after `SpanChar` returns; if the continuation fails,
the enclosing backtracking unwinds to a point before the span began.)

> Note: this optimization is valid only when the quantifier is in a context
> where the body is **not** the only thing that can match the next char.
> If the continuation requires the body to give back characters, the
> instruction would be incorrect.  Gate the optimization conservatively:
> only apply when the outer structure is a complete `a+` or `a*` with no
> continuation that backtracks into the span.

This is the same conservative approach used by PCRE2's JIT "auto-possessive"
optimization.

## Steps

- [ ] Add `SpanChar` and `SpanClass` to `Inst` enum in `vm.rs`
- [ ] Implement `SpanChar`/`SpanClass` in the interpreter `exec()`
- [ ] Add compiler logic in `compile_quantifier_inner` to detect simple bodies
- [ ] Implement JIT codegen for `SpanChar` in `jit/builder.rs`
- [ ] Implement JIT codegen for `SpanClass` in `jit/builder.rs`
- [ ] Run `cargo test` and `cargo bench -- quantifier greedy_match`
