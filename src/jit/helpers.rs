//! `extern "C"` helper functions called from Cranelift-generated JIT code.
//!
//! Each function receives a `*mut JitExecCtx` as its first argument so it can
//! read immutable context data and mutate capture state / the backtrack stack.
//!
//! **Safety**: every function is `unsafe extern "C"`.  Callers (the JIT
//! compiled code) must guarantee that `ctx` is a valid, non-null pointer for
//! the duration of the call and that all pointer fields within the struct are
//! likewise valid.

use crate::ast::{AnchorKind, Flags, Shorthand};
use crate::charset;
use crate::vm::{BtJit, CharSet, JitExecCtx, MemoState, bt_pop, bt_push, exec_lookaround_for_jit, memo_key};
use unicode_casefold::UnicodeCaseFold;

// ---------------------------------------------------------------------------
// Internal utilities
// ---------------------------------------------------------------------------

#[inline]
unsafe fn text_from_ctx(ctx: &JitExecCtx) -> &str {
    unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            ctx.text_ptr,
            ctx.text_len as usize,
        ))
    }
}

#[inline]
fn char_at(text: &str, pos: usize) -> Option<(char, usize)> {
    if pos >= text.len() {
        return None;
    }
    let c = text[pos..].chars().next()?;
    Some((c, c.len_utf8()))
}

#[inline]
fn char_before(text: &str, pos: usize) -> Option<(char, usize)> {
    if pos == 0 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut start = pos - 1;
    while start > 0 && bytes[start] & 0xC0 == 0x80 {
        start -= 1;
    }
    let c = text[start..pos].chars().next()?;
    Some((c, pos - start))
}

#[inline]
fn chars_eq_ci(a: char, b: char) -> bool {
    if a == b {
        return true;
    }
    a.case_fold().eq(b.case_fold())
}

/// Push a `BtJit::Retry` entry.  Slots are **not** snapshotted here; instead
/// each `Save` instruction pushes a `SaveUndo` entry before modifying a slot,
/// so backtracking naturally reverses every slot write since the last fork.
unsafe fn push_retry(ctx: &mut JitExecCtx, block_id: u32, pos: u64) {
    ctx.bt_retry_count += 1;
    unsafe {
        bt_push(ctx, BtJit::retry(block_id, pos, ctx.keep_pos));
    }
}

// ---------------------------------------------------------------------------
// Character matching helpers
// ---------------------------------------------------------------------------

/// Match `ch` (optionally case-insensitive) at `text[pos..]`.
/// Returns the new `pos` on success, -1 on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_char(
    ctx: *const JitExecCtx,
    pos: u64,
    ch: u32,
    ignore_case: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let ch = char::from_u32(ch).unwrap_or('\0');
        let ic = ignore_case != 0;
        match char_at(text, pos as usize) {
            Some((c, len)) if ic && chars_eq_ci(ch, c) => (pos as usize + len) as i64,
            Some((c, len)) if !ic && ch == c => (pos as usize + len) as i64,
            _ => -1,
        }
    }
}

/// Match any character (`.`) at `text[pos..]`, respecting `dotall`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_any_char(ctx: *const JitExecCtx, pos: u64, dotall: u32) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let dotall = dotall != 0;
        match char_at(text, pos as usize) {
            Some((c, len)) if dotall || c != '\n' => (pos as usize + len) as i64,
            _ => -1,
        }
    }
}

/// Match `charsets[idx]` at `text[pos..]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_class(
    ctx: *const JitExecCtx,
    pos: u64,
    idx: u32,
    ignore_case: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let ic = ignore_case != 0;
        let charsets = std::slice::from_raw_parts(
            ctx.charsets_ptr as *const CharSet,
            ctx.charsets_len as usize,
        );
        match char_at(text, pos as usize) {
            Some((c, len)) if charsets[idx as usize].matches(c, false, ic) => {
                (pos as usize + len) as i64
            }
            _ => -1,
        }
    }
}

/// Match shorthand `\w` / `\d` / `\s` / `\h` at `text[pos..]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_shorthand(
    ctx: *const JitExecCtx,
    pos: u64,
    sh_code: u32,
    ascii_range: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let sh = shorthand_from_u32(sh_code);
        let ar = ascii_range != 0;
        match char_at(text, pos as usize) {
            Some((c, len)) if charset::matches_shorthand(sh, c, ar) => (pos as usize + len) as i64,
            _ => -1,
        }
    }
}

/// Match Unicode property at `text[pos..]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_prop(
    ctx: *const JitExecCtx,
    pos: u64,
    name_ptr: *const u8,
    name_len: u64,
    negate: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let name =
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len as usize));
        let neg = negate != 0;
        match char_at(text, pos as usize) {
            Some((c, len)) if charset::matches_unicode_prop(name, c, neg) => {
                (pos as usize + len) as i64
            }
            _ => -1,
        }
    }
}

// ---- backward variants (lookbehind) ----

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_char_back(
    ctx: *const JitExecCtx,
    pos: u64,
    ch: u32,
    ignore_case: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let ch = char::from_u32(ch).unwrap_or('\0');
        let ic = ignore_case != 0;
        match char_before(text, pos as usize) {
            Some((c, len)) if ic && chars_eq_ci(ch, c) => (pos as usize - len) as i64,
            Some((c, len)) if !ic && ch == c => (pos as usize - len) as i64,
            _ => -1,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_any_char_back(
    ctx: *const JitExecCtx,
    pos: u64,
    dotall: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let dotall = dotall != 0;
        match char_before(text, pos as usize) {
            Some((c, len)) if dotall || c != '\n' => (pos as usize - len) as i64,
            _ => -1,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_class_back(
    ctx: *const JitExecCtx,
    pos: u64,
    idx: u32,
    ignore_case: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let ic = ignore_case != 0;
        let charsets = std::slice::from_raw_parts(
            ctx.charsets_ptr as *const CharSet,
            ctx.charsets_len as usize,
        );
        match char_before(text, pos as usize) {
            Some((c, len)) if charsets[idx as usize].matches(c, false, ic) => {
                (pos as usize - len) as i64
            }
            _ => -1,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_shorthand_back(
    ctx: *const JitExecCtx,
    pos: u64,
    sh_code: u32,
    ascii_range: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let sh = shorthand_from_u32(sh_code);
        let ar = ascii_range != 0;
        match char_before(text, pos as usize) {
            Some((c, len)) if charset::matches_shorthand(sh, c, ar) => (pos as usize - len) as i64,
            _ => -1,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_match_prop_back(
    ctx: *const JitExecCtx,
    pos: u64,
    name_ptr: *const u8,
    name_len: u64,
    negate: u32,
) -> i64 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let name =
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len as usize));
        let neg = negate != 0;
        match char_before(text, pos as usize) {
            Some((c, len)) if charset::matches_unicode_prop(name, c, neg) => {
                (pos as usize - len) as i64
            }
            _ => -1,
        }
    }
}

// ---------------------------------------------------------------------------
// Anchor
// ---------------------------------------------------------------------------

/// Returns 1 if the anchor matches at `pos`, 0 otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_check_anchor(
    ctx: *const JitExecCtx,
    pos: u64,
    kind_code: u32,
    _flags_raw: u32,
) -> u32 {
    unsafe {
        let ctx = &*ctx;
        let text = text_from_ctx(ctx);
        let pos = pos as usize;
        let kind = anchor_from_u32(kind_code);
        let ok = match kind {
            AnchorKind::Start => pos == 0 || text.as_bytes().get(pos - 1) == Some(&b'\n'),
            AnchorKind::End => pos == text.len() || text.as_bytes().get(pos) == Some(&b'\n'),
            AnchorKind::StringStart => pos == 0,
            AnchorKind::StringEnd => pos == text.len(),
            AnchorKind::StringEndOrNl => {
                pos == text.len()
                    || (pos + 1 == text.len() && text.as_bytes().get(pos) == Some(&b'\n'))
            }
            AnchorKind::WordBoundary => is_word_boundary(text, pos),
            AnchorKind::NonWordBoundary => !is_word_boundary(text, pos),
            AnchorKind::SearchStart => pos == ctx.search_start as usize,
        };
        ok as u32
    }
}

fn is_word_boundary(text: &str, pos: usize) -> bool {
    let before = pos > 0 && text[..pos].chars().last().is_some_and(is_word_char);
    let after = pos < text.len() && text[pos..].chars().next().is_some_and(is_word_char);
    before != after
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---------------------------------------------------------------------------
// Capture state helpers
// ---------------------------------------------------------------------------

/// Write `pos` into `ctx.slots[slot]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_save(ctx: *mut JitExecCtx, slot: u32, pos: u64) {
    unsafe {
        let ctx = &mut *ctx;
        let slots = std::slice::from_raw_parts_mut(ctx.slots_ptr, ctx.slots_len as usize);
        let idx = slot as usize;
        if idx < slots.len() {
            slots[idx] = pos;
        }
    }
}

/// Push a `SaveUndo` entry for `slot` with its current value before the
/// caller writes a new value.  Called from inlined `Save` blocks in JIT code.
/// Skips the push when there are no active `Retry` entries (no backtracking
/// possible, so undo data would never be used).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_push_save_undo(ctx: *mut JitExecCtx, slot: u32, old_value: u64) {
    unsafe {
        let ctx = &mut *ctx;
        if ctx.bt_retry_count == 0 {
            return;
        }
        bt_push(ctx, BtJit::save_undo(slot, old_value));
    }
}

/// Set `ctx.keep_pos = pos` (`\K`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_keep_start(ctx: *mut JitExecCtx, pos: u64) {
    unsafe {
        (*ctx).keep_pos = pos;
    }
}

/// Returns 1 if the group (open slot = `slot`, close = `slot+1`) has matched,
/// 0 otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_check_group(ctx: *const JitExecCtx, slot: u32) -> u32 {
    unsafe {
        let ctx = &*ctx;
        let slots = std::slice::from_raw_parts(ctx.slots_ptr, ctx.slots_len as usize);
        let i = slot as usize;
        let has = slots.get(i).is_some_and(|&v| v != u64::MAX)
            && slots.get(i + 1).is_some_and(|&v| v != u64::MAX);
        has as u32
    }
}

// ---------------------------------------------------------------------------
// Fork / backtracking helpers
// ---------------------------------------------------------------------------

/// Greedy fork: try `pc+1` (fall-through), save `alt_block` as retry.
///
/// Returns 0 if a memoised failure applies (caller should fail immediately),
/// 1 if the caller should fall through to `pc+1`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_fork(
    ctx: *mut JitExecCtx,
    fork_pc: u32,
    alt_block: u32,
    pos: u64,
) -> u32 {
    unsafe {
        let ctx = &mut *ctx;
        let memo = &mut *(ctx.memo_ptr as *mut MemoState);
        let use_memo = ctx.use_memo != 0;

        if use_memo {
            // Fast-path: skip the hash lookup when no failures have been
            // recorded yet (common case for non-pathological patterns).
            if !memo.fork_failures.is_empty() {
                let key = memo_key(fork_pc as usize, pos as usize);
                if let Some(&d) = memo.fork_failures.get(&key)
                    && d <= ctx.atomic_depth as usize
                {
                    return 0;
                }
            }
            bt_push(ctx, BtJit::memo_mark(fork_pc, pos));
        }

        push_retry(ctx, alt_block, pos);
        1
    }
}

/// Lazy fork: try `alt_block` (fall-through), save `main_block` as retry.
///
/// Returns 0 if a memoised failure applies, 1 if caller should jump to
/// `alt_block`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_fork_next(
    ctx: *mut JitExecCtx,
    fork_pc: u32,
    main_block: u32,
    pos: u64,
) -> u32 {
    unsafe {
        let ctx = &mut *ctx;
        let memo = &mut *(ctx.memo_ptr as *mut MemoState);
        let use_memo = ctx.use_memo != 0;

        if use_memo {
            if !memo.fork_failures.is_empty() {
                let key = memo_key(fork_pc as usize, pos as usize);
                if let Some(&d) = memo.fork_failures.get(&key)
                    && d <= ctx.atomic_depth as usize
                {
                    return 0;
                }
            }
            bt_push(ctx, BtJit::memo_mark(fork_pc, pos));
        }

        push_retry(ctx, main_block, pos);
        1
    }
}

/// Pop the next usable retry point.
///
/// Mirrors `do_backtrack` in vm.rs: skips `AtomicBarrier` (decrementing
/// `ctx.atomic_depth`) and records `MemoMark` failures.
///
/// Returns 1 and writes `*out_block_id` / `*out_pos` on success; returns 0
/// if the stack is empty (no match).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_bt_pop(
    ctx: *mut JitExecCtx,
    out_block_id: *mut u32,
    out_pos: *mut u64,
) -> u32 {
    unsafe {
        let ctx = &mut *ctx;
        let memo = &mut *(ctx.memo_ptr as *mut MemoState);
        let use_memo = ctx.use_memo != 0;

        loop {
            let Some(e) = bt_pop(ctx) else { return 0 };
            match e.tag {
                BtJit::TAG_ATOMIC_BARRIER => {
                    ctx.atomic_depth -= 1;
                }
                BtJit::TAG_MEMO_MARK => {
                    if use_memo {
                        let key = memo_key(e.a as usize, e.b as usize);
                        memo.fork_failures
                            .entry(key)
                            .and_modify(|d| *d = (*d).min(ctx.atomic_depth as usize))
                            .or_insert(ctx.atomic_depth as usize);
                        ctx.memo_has_failures = 1;
                    }
                }
                BtJit::TAG_SAVE_UNDO => {
                    let slots =
                        std::slice::from_raw_parts_mut(ctx.slots_ptr, ctx.slots_len as usize);
                    let idx = e.a as usize;
                    if idx < slots.len() {
                        slots[idx] = e.b;
                    }
                }
                _ => {
                    // TAG_RETRY
                    ctx.bt_retry_count -= 1;
                    ctx.keep_pos = e.c;
                    *out_block_id = e.a;
                    *out_pos = e.b;
                    return 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic group helpers
// ---------------------------------------------------------------------------

/// `AtomicStart`: increment `atomic_depth` and push an `AtomicBarrier`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_atomic_start(ctx: *mut JitExecCtx) {
    unsafe {
        let ctx = &mut *ctx;
        ctx.atomic_depth += 1;
        bt_push(ctx, BtJit::atomic_barrier());
    }
}

/// `AtomicEnd`: drain bt stack to the nearest `AtomicBarrier` (commit).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_atomic_end(ctx: *mut JitExecCtx) {
    unsafe {
        let ctx = &mut *ctx;
        while let Some(e) = bt_pop(ctx) {
            match e.tag {
                BtJit::TAG_ATOMIC_BARRIER => {
                    ctx.atomic_depth -= 1;
                    break;
                }
                BtJit::TAG_RETRY => {
                    ctx.bt_retry_count -= 1;
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lookaround helper
// ---------------------------------------------------------------------------

/// Execute a lookaround body.  Implements the full Algorithm-6 memoisation
/// and positive/negative sense handling.
///
/// Returns 1 if the outer execution should **proceed** (lookaround "passed"),
/// 0 if it should **fail** (lookaround "blocked").
#[unsafe(no_mangle)]
pub unsafe extern "C" fn jit_lookaround(
    ctx: *mut JitExecCtx,
    lk_pc: u32,
    body_pc: u32,
    pos: u64,
    positive: u32,
) -> u32 {
    unsafe {
        let positive = positive != 0;
        let matched = exec_lookaround_for_jit(
            ctx,
            lk_pc as usize,
            body_pc as usize,
            pos as usize,
            positive,
        );
        (matched == positive) as u32
    }
}

// ---------------------------------------------------------------------------
// Codec helpers — convert u32 discriminants to enum variants
// ---------------------------------------------------------------------------

fn shorthand_from_u32(v: u32) -> Shorthand {
    match v {
        0 => Shorthand::Word,
        1 => Shorthand::NonWord,
        2 => Shorthand::Digit,
        3 => Shorthand::NonDigit,
        4 => Shorthand::Space,
        5 => Shorthand::NonSpace,
        6 => Shorthand::HexDigit,
        7 => Shorthand::NonHexDigit,
        _ => Shorthand::Word,
    }
}

fn anchor_from_u32(v: u32) -> AnchorKind {
    match v {
        0 => AnchorKind::Start,
        1 => AnchorKind::End,
        2 => AnchorKind::StringStart,
        3 => AnchorKind::StringEnd,
        4 => AnchorKind::StringEndOrNl,
        5 => AnchorKind::WordBoundary,
        6 => AnchorKind::NonWordBoundary,
        7 => AnchorKind::SearchStart,
        _ => AnchorKind::Start,
    }
}

// Suppress unused-import warning; Flags is used only in vm.rs helpers context.
#[allow(dead_code)]
fn _use_flags(_: Flags) {}

/// Symbol table: maps helper name → raw function pointer.
/// Used by `JITBuilder::symbol()` to resolve external calls in JIT code.
pub(super) fn register_symbols(jit_builder: &mut cranelift_jit::JITBuilder) {
    macro_rules! sym {
        ($fn:ident) => {
            jit_builder.symbol(stringify!($fn), $fn as *const u8);
        };
    }
    sym!(jit_match_char);
    sym!(jit_match_any_char);
    sym!(jit_match_class);
    sym!(jit_match_shorthand);
    sym!(jit_match_prop);
    sym!(jit_match_char_back);
    sym!(jit_match_any_char_back);
    sym!(jit_match_class_back);
    sym!(jit_match_shorthand_back);
    sym!(jit_match_prop_back);
    sym!(jit_check_anchor);
    sym!(jit_save);
    sym!(jit_push_save_undo);
    sym!(jit_keep_start);
    sym!(jit_check_group);
    sym!(jit_fork);
    sym!(jit_fork_next);
    sym!(jit_bt_pop);
    sym!(jit_atomic_start);
    sym!(jit_atomic_end);
    sym!(jit_lookaround);
}
