/// Virtual machine for Onigmo-compatible regular expression matching.
///
/// Uses backtracking search with an explicit save/restore state.

use std::collections::HashMap;
use crate::ast::{AnchorKind, Flags, LookDir, Shorthand};
use crate::charset;
use crate::compile::{compile, CompileOptions};
use crate::error::Error;
use crate::parser::parse;

// ---------------------------------------------------------------------------
// Character set types (used by instructions)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CharSet {
    pub negate: bool,
    pub items: Vec<CharSetItem>,
    /// AND-intersected classes
    pub intersections: Vec<CharSet>,
}

#[derive(Debug, Clone)]
pub enum CharSetItem {
    Char(char),
    Range(char, char),
    Shorthand(Shorthand, bool),         // ascii_range
    Posix(crate::ast::PosixClass, bool), // negate
    Unicode(String, bool),               // name, negate
    Nested(CharSet),
}

impl CharSet {
    pub fn matches(&self, ch: char, ascii_range: bool, ignore_case: bool) -> bool {
        let test_ch = if ignore_case {
            ch.to_lowercase().next().unwrap_or(ch)
        } else {
            ch
        };
        let base = self.items.iter().any(|item| item.matches(ch, test_ch, ascii_range, ignore_case));
        let result = if self.intersections.is_empty() {
            base
        } else {
            self.intersections.iter().fold(base, |acc, cs| acc && cs.matches(ch, ascii_range, ignore_case))
        };
        if self.negate { !result } else { result }
    }
}

impl CharSetItem {
    fn matches(&self, ch: char, ch_lo: char, ascii_range: bool, ignore_case: bool) -> bool {
        match self {
            CharSetItem::Char(c) => {
                if ignore_case {
                    chars_eq_ci(*c, ch)
                } else {
                    *c == ch
                }
            }
            CharSetItem::Range(lo, hi) => {
                if ignore_case {
                    let lo_lo = lo.to_lowercase().next().unwrap_or(*lo);
                    let hi_lo = hi.to_lowercase().next().unwrap_or(*hi);
                    ch_lo >= lo_lo && ch_lo <= hi_lo
                } else {
                    ch >= *lo && ch <= *hi
                }
            }
            CharSetItem::Shorthand(sh, ar) => charset::matches_shorthand(*sh, ch, *ar),
            CharSetItem::Posix(cls, neg) => {
                let r = charset::matches_posix(*cls, ch, ascii_range);
                if *neg { !r } else { r }
            }
            CharSetItem::Unicode(name, neg) => charset::matches_unicode_prop(name, ch, *neg),
            CharSetItem::Nested(inner) => inner.matches(ch, ascii_range, ignore_case),
        }
    }
}

fn chars_eq_ci(a: char, b: char) -> bool {
    if a == b { return true; }
    a.to_lowercase().eq(b.to_lowercase()) || a.to_uppercase().eq(b.to_uppercase())
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Inst {
    /// Successful match
    Match,

    /// Match a single character (optionally case-insensitive)
    Char(char, bool),

    /// `.` — match any character; bool = dotall (matches \n)
    AnyChar(bool),

    /// Character class (index into charsets vec, ignore_case)
    Class(usize, bool),

    /// Shorthand `\w`, `\d`, etc.
    Shorthand(Shorthand, bool),

    /// Unicode property `\p{...}`
    Prop(String, bool),

    /// Anchor (`^`, `$`, `\b`, `\A`, etc.)
    Anchor(AnchorKind, Flags),

    /// Unconditional jump to absolute PC
    Jump(usize),

    /// Greedy fork: try pc+1; on failure try `usize`
    Fork(usize),

    /// Lazy fork: try `usize` first; on failure try pc+1
    ForkNext(usize),

    /// Save current position to slot
    Save(usize),

    /// Reset match-start to current position (`\K`)
    KeepStart,

    /// Backreference to 1-based group number; ignore_case; optional recursion level
    BackRef(u32, bool, Option<i32>),

    /// Relative-backward backreference (1-based relative index)
    BackRefRelBack(u32, bool),

    /// Push (pc+1) onto call stack and jump to absolute target
    Call(usize),

    /// Pop call stack and jump to the saved address
    Ret,

    /// If call stack is non-empty, pop and jump to return addr; otherwise fall through
    RetIfCalled,

    /// Atomic group start; end_pc = index of AtomicEnd instruction
    AtomicStart(usize),

    /// Marks end of atomic body (acts like Match when executing inner)
    AtomicEnd,

    /// Lookaround start
    LookStart {
        positive: bool,
        dir: LookDir,
        end_pc: usize,
        /// Pre-computed byte offsets to try for lookbehind.
        /// `None` means the body has variable/unbounded width — the VM will
        /// scan all positions from the start of the text up to `pos` at runtime.
        behind_lens: Option<Vec<usize>>,
    },

    /// Marks end of lookaround body
    LookEnd,

    /// Conditional group: check if group `slot` has matched; jump to yes/no
    CheckGroup { slot: usize, yes_pc: usize, no_pc: usize },

    /// Absence operator start; inner program at [pc+1..inner_end_pc]
    AbsenceStart(usize),

    /// Marks end of absence inner program
    AbsenceEnd,
}

// ---------------------------------------------------------------------------
// VM state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct State {
    /// Flat capture slots: slots[2*(n-1)] = start, slots[2*(n-1)+1] = end (1-based groups)
    slots: Vec<Option<usize>>,
    /// Where `\K` reset the match start
    keep_pos: Option<usize>,
    /// Return address stack for subexpression calls
    call_stack: Vec<usize>,
}

impl State {
    fn new(num_groups: usize) -> Self {
        State {
            slots: vec![None; num_groups * 2],
            keep_pos: None,
            call_stack: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Execution context (immutable per-search data)
// ---------------------------------------------------------------------------

struct Ctx<'t> {
    prog: &'t [Inst],
    charsets: &'t [CharSet],
    text: &'t str,
    search_start: usize, // position where the search started (for \G)
    /// Whether memoization is active for this regex.  Set to `false` when the
    /// pattern contains backreferences, because `(pc, pos)` alone does not
    /// determine the match outcome — the current capture-slot state does too.
    use_memo: bool,
}

impl<'t> Ctx<'t> {
    fn char_at(&self, pos: usize) -> Option<(char, usize)> {
        if pos >= self.text.len() { return None; }
        let ch = self.text[pos..].chars().next()?;
        Some((ch, ch.len_utf8()))
    }
}

// ---------------------------------------------------------------------------
// Backtrack stack
// ---------------------------------------------------------------------------

/// An entry on the explicit backtrack stack.
enum Bt {
    /// Retry point: restore state and resume execution at `pc`/`pos`.
    Retry {
        pc: usize,
        pos: usize,
        slots: Vec<Option<usize>>,
        keep_pos: Option<usize>,
        call_stack: Vec<usize>,
    },
    /// Atomic group fence.  Removed when the atomic body commits (`AtomicEnd`);
    /// silently skipped when encountered while backtracking (the body has
    /// exhausted all its internal alternatives and failed as a unit).
    AtomicBarrier,
    /// Memoization marker pushed alongside every `Fork`/`ForkNext` alternative.
    /// When popped during backtracking it means **both** paths of the fork have
    /// been exhausted; we record `(fork_pc, fork_pos)` as a known-failure entry
    /// so that any future visit to the same fork state at the same position can
    /// short-circuit immediately (Algorithm 5 of Fujinami & Hasuo 2024).
    MemoMark { fork_pc: usize, fork_pos: usize },
}

/// Pack a `(pc, pos)` pair into a single `u64` key for the memo table.
#[inline]
fn memo_key(pc: usize, pos: usize) -> u64 {
    ((pc as u64) << 32) | (pos as u64)
}

// ---------------------------------------------------------------------------
// Memoization state (Algorithm 6 of Fujinami & Hasuo 2024)
// ---------------------------------------------------------------------------

/// Cached outcome of one lookaround sub-execution.
///
/// For patterns without backreferences or conditional groups (`use_memo =
/// true`), the outcome of a lookaround body starting at `(body_pc, pos)` is
/// determined solely by the text and the program — it does not depend on the
/// current outer capture state.  We therefore cache it once and reuse it on
/// every subsequent visit to the same `(lk_pc, pos)`.
///
/// For *positive* lookaheads the successful execution may update the outer
/// capture slots.  We store only the *delta* — the indices and new values of
/// slots that actually changed — so that the re-application is correct even
/// when the outer slot state differs at the time of the cache hit.
#[derive(Clone)]
enum LookCacheEntry {
    /// The lookaround body matched.
    /// `slot_delta`: `(slot_index, new_value)` for every slot whose value
    ///   changed during the successful body execution.
    /// `keep_pos_delta`: new `keep_pos` if the body contained `\K` and
    ///   changed the value; `None` means "leave outer keep_pos unchanged".
    BodyMatched {
        slot_delta:     Vec<(usize, Option<usize>)>,
        keep_pos_delta: Option<Option<usize>>,
    },
    /// The lookaround body did not match.  No outer-state changes occurred.
    BodyNotMatched,
}

/// Shared memoization state, created once per `find()` call and threaded
/// through the entire `exec` / `exec_lookaround` / `check_inner_in_range`
/// call tree.
struct MemoState {
    /// Fork failure depths (Algorithms 5 & 7).
    /// Maps `memo_key(fork_pc, fork_pos)` → minimum atomic depth at which
    /// both alternatives of that fork were exhausted.  An entry with depth `d`
    /// may be reused whenever the current `atomic_depth >= d`.
    fork_failures: HashMap<u64, usize>,
    /// Lookaround result cache (Algorithm 6).
    /// Maps `memo_key(lk_pc, lk_pos)` → whether the lookaround body matched
    /// and, for positive lookaheads, the capture-slot changes it produced.
    look_results: HashMap<u64, LookCacheEntry>,
}

impl MemoState {
    fn new() -> Self {
        MemoState { fork_failures: HashMap::new(), look_results: HashMap::new() }
    }
}

/// Compute which capture slots changed between `pre` and `post`.
/// Returns `(slot_index, new_value)` pairs for every slot that differs.
fn compute_slot_delta(pre: &[Option<usize>], post: &[Option<usize>]) -> Vec<(usize, Option<usize>)> {
    let len = pre.len().max(post.len());
    (0..len).filter_map(|i| {
        let old = pre.get(i).copied().flatten();
        let new = post.get(i).copied().flatten();
        if old != new { Some((i, post.get(i).copied().flatten())) } else { None }
    }).collect()
}

fn push_retry(bt: &mut Vec<Bt>, pc: usize, pos: usize, state: &State) {
    bt.push(Bt::Retry {
        pc,
        pos,
        slots: state.slots.clone(),
        keep_pos: state.keep_pos,
        call_stack: state.call_stack.clone(),
    });
}

/// Pop the next usable retry point, restoring `pc`/`pos`/`state`.
/// - `AtomicBarrier` entries decrement `atomic_depth` and are skipped
///   (atomic body failed entirely).
/// - `MemoMark` entries record `(fork_pc, fork_pos)` as a failure tagged
///   with the CURRENT `atomic_depth` (Algorithm 7: depth-tagged failures).
///   Only entries recorded at depth ≤ current depth may be reused later,
///   so we keep the minimum depth seen for each `(pc, pos)` key.
fn do_backtrack(
    bt: &mut Vec<Bt>,
    pc: &mut usize,
    pos: &mut usize,
    state: &mut State,
    memo: &mut MemoState,
    atomic_depth: &mut usize,
    use_memo: bool,
) -> bool {
    loop {
        match bt.pop() {
            None => return false,
            Some(Bt::AtomicBarrier) => {
                *atomic_depth -= 1;
                continue;
            }
            Some(Bt::MemoMark { fork_pc, fork_pos }) => {
                if use_memo {
                    let key = memo_key(fork_pc, fork_pos);
                    // Keep the minimum depth: a lower depth is more general
                    // and can be reused in more contexts.
                    memo.fork_failures.entry(key)
                        .and_modify(|d| *d = (*d).min(*atomic_depth))
                        .or_insert(*atomic_depth);
                }
                continue;
            }
            Some(Bt::Retry { pc: rpc, pos: rpos, slots, keep_pos, call_stack }) => {
                *pc = rpc;
                *pos = rpos;
                state.slots = slots;
                state.keep_pos = keep_pos;
                state.call_stack = call_stack;
                return true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Iterative backtracking executor
// ---------------------------------------------------------------------------

/// Maximum nesting depth for lookaround / absence sub-executions.
const MAX_DEPTH: usize = 100;
const MAX_CALL_DEPTH: usize = 200;

/// Execute the program from `pc` at position `pos`.
/// Returns `Some(end_pos)` on success, leaving `state` with final captures.
/// `depth` limits nesting of lookaround / absence sub-executions; it does
/// **not** grow for ordinary backtracking (Fork/ForkNext use an explicit stack).
/// `memo` is the shared memoization table (Algorithm 5/6/7 of Fujinami & Hasuo
/// 2024); it is created once per `find()` call and shared across all
/// sub-executions so that failures discovered inside lookarounds are
/// propagated back to the outer execution.
fn exec(ctx: &Ctx<'_>, start_pc: usize, start_pos: usize, state: &mut State, depth: usize, memo: &mut MemoState) -> Option<usize> {
    if depth > MAX_DEPTH {
        return None;
    }

    let mut pc = start_pc;
    let mut pos = start_pos;
    let mut bt: Vec<Bt> = Vec::new();
    // Local atomic-group nesting counter (Algorithm 7).
    // Incremented by AtomicStart, decremented by AtomicEnd (success path) and
    // when AtomicBarrier is popped during backtracking (failure path).
    let mut atomic_depth: usize = 0;

    'vm: loop {
        // Macro: trigger backtracking or return None if the stack is empty.
        macro_rules! fail {
            () => {{
                if !do_backtrack(&mut bt, &mut pc, &mut pos, state, memo, &mut atomic_depth, ctx.use_memo) {
                    return None;
                }
                continue 'vm;
            }};
        }

        match &ctx.prog[pc] {
            Inst::Match => return Some(pos),

            // Terminators for sub-executions (lookaround / absence inner program)
            Inst::LookEnd | Inst::AbsenceEnd => return Some(pos),

            Inst::Char(ch, ignore_case) => {
                match ctx.char_at(pos) {
                    Some((c, len)) if *ignore_case && chars_eq_ci(*ch, c) => { pos += len; pc += 1; }
                    Some((c, len)) if !ignore_case && *ch == c            => { pos += len; pc += 1; }
                    _ => fail!(),
                }
            }

            Inst::AnyChar(dotall) => {
                match ctx.char_at(pos) {
                    Some((c, len)) if *dotall || c != '\n' => { pos += len; pc += 1; }
                    _ => fail!(),
                }
            }

            Inst::Class(idx, ignore_case) => {
                match ctx.char_at(pos) {
                    Some((c, len)) if ctx.charsets[*idx].matches(c, false, *ignore_case) => { pos += len; pc += 1; }
                    _ => fail!(),
                }
            }

            Inst::Shorthand(sh, ascii_range) => {
                match ctx.char_at(pos) {
                    Some((c, len)) if charset::matches_shorthand(*sh, c, *ascii_range) => { pos += len; pc += 1; }
                    _ => fail!(),
                }
            }

            Inst::Prop(name, negate) => {
                match ctx.char_at(pos) {
                    Some((c, len)) if charset::matches_unicode_prop(name, c, *negate) => { pos += len; pc += 1; }
                    _ => fail!(),
                }
            }

            Inst::Anchor(kind, flags) => {
                if matches_anchor(ctx, pos, *kind, *flags) { pc += 1; } else { fail!(); }
            }

            Inst::Jump(target) => { pc = *target; }

            // Greedy fork: try pc+1 first; save alt as a backtrack point.
            Inst::Fork(alt) => {
                if ctx.use_memo {
                    let key = memo_key(pc, pos);
                    // Reuse a recorded failure if it was recorded at a depth ≤
                    // current atomic_depth (Algorithm 7: a failure under fewer
                    // atomic constraints is valid under more constraints too).
                    if let Some(&d) = memo.fork_failures.get(&key) {
                        if d <= atomic_depth { fail!(); }
                    }
                    // MemoMark fires after both alternatives are exhausted.
                    bt.push(Bt::MemoMark { fork_pc: pc, fork_pos: pos });
                }
                push_retry(&mut bt, *alt, pos, state);
                pc += 1;
            }

            // Lazy fork: try alt first; save pc+1 as a backtrack point.
            Inst::ForkNext(alt) => {
                if ctx.use_memo {
                    let key = memo_key(pc, pos);
                    if let Some(&d) = memo.fork_failures.get(&key) {
                        if d <= atomic_depth { fail!(); }
                    }
                    bt.push(Bt::MemoMark { fork_pc: pc, fork_pos: pos });
                }
                push_retry(&mut bt, pc + 1, pos, state);
                pc = *alt;
            }

            Inst::Save(slot) => {
                let slot = *slot;
                if slot >= state.slots.len() { state.slots.resize(slot + 1, None); }
                state.slots[slot] = Some(pos);
                pc += 1;
            }

            Inst::KeepStart => { state.keep_pos = Some(pos); pc += 1; }

            Inst::BackRef(group, ignore_case, _level) => {
                let slot_open  = ((*group - 1) * 2) as usize;
                let slot_close = slot_open + 1;
                let start = match state.slots.get(slot_open).and_then(|x| *x)  { Some(s) => s, None => fail!() };
                let end   = match state.slots.get(slot_close).and_then(|x| *x) { Some(e) => e, None => fail!() };
                let captured = &ctx.text[start..end];
                if *ignore_case {
                    if pos + captured.len() > ctx.text.len() { fail!(); }
                    if !strings_eq_ci(captured, &ctx.text[pos..pos + captured.len()]) { fail!(); }
                    pos += captured.len();
                } else {
                    let len = captured.len();
                    if ctx.text.get(pos..pos + len) != Some(captured) { fail!(); }
                    pos += len;
                }
                pc += 1;
            }

            Inst::BackRefRelBack(_n, _ic) => { fail!(); } // not yet implemented

            Inst::Call(target) => {
                if state.call_stack.len() >= MAX_CALL_DEPTH { fail!(); }
                state.call_stack.push(pc + 1);
                pc = *target;
            }

            Inst::Ret => {
                match state.call_stack.pop() { Some(r) => pc = r, None => fail!() }
            }

            Inst::RetIfCalled => {
                match state.call_stack.pop() { Some(r) => pc = r, None => { pc += 1; } }
            }

            // Atomic group: push a fence so that on AtomicEnd we can drain
            // all the body's backtrack points (preventing outer retry of the body).
            Inst::AtomicStart(_end_pc) => {
                atomic_depth += 1;
                bt.push(Bt::AtomicBarrier);
                pc += 1;
            }

            Inst::AtomicEnd => {
                // Commit: discard all backtrack entries up to and including
                // the nearest AtomicBarrier (innermost atomic group).
                // MemoMark entries inside the atomic body are also discarded —
                // the body succeeded so there are no failures to record.
                // Decrement atomic_depth when the barrier is consumed.
                loop {
                    match bt.pop() {
                        None => break,
                        Some(Bt::AtomicBarrier) => { atomic_depth -= 1; break; }
                        Some(Bt::Retry { .. }) | Some(Bt::MemoMark { .. }) => {}
                    }
                }
                pc += 1;
            }

            Inst::LookStart { positive, dir, end_pc, behind_lens } => {
                let positive = *positive;
                let end_pc   = *end_pc;
                let lk_key   = memo_key(pc, pos);

                // Algorithm 6: check the lookaround result cache before
                // running the (potentially expensive) sub-execution.
                let matched = if ctx.use_memo {
                    if let Some(entry) = memo.look_results.get(&lk_key).cloned() {
                        // Cache hit: apply any slot changes and return cached result.
                        match entry {
                            LookCacheEntry::BodyMatched { slot_delta, keep_pos_delta } => {
                                if positive {
                                    // Re-apply the slot delta to the current outer state.
                                    for (idx, val) in slot_delta {
                                        if idx >= state.slots.len() {
                                            state.slots.resize(idx + 1, None);
                                        }
                                        state.slots[idx] = val;
                                    }
                                    if let Some(kp) = keep_pos_delta {
                                        state.keep_pos = kp;
                                    }
                                }
                                true
                            }
                            LookCacheEntry::BodyNotMatched => false,
                        }
                    } else {
                        // Cache miss: run the sub-execution and cache the result.
                        let pre_slots = state.slots.clone();
                        let pre_keep  = state.keep_pos;
                        let m = exec_lookaround(ctx, pc + 1, pos, state, *dir, behind_lens.as_deref(), depth, memo);
                        let entry = if m {
                            let slot_delta     = compute_slot_delta(&pre_slots, &state.slots);
                            let keep_pos_delta = if state.keep_pos != pre_keep {
                                Some(state.keep_pos)
                            } else {
                                None
                            };
                            LookCacheEntry::BodyMatched { slot_delta, keep_pos_delta }
                        } else {
                            LookCacheEntry::BodyNotMatched
                        };
                        memo.look_results.insert(lk_key, entry);
                        m
                    }
                } else {
                    exec_lookaround(ctx, pc + 1, pos, state, *dir, behind_lens.as_deref(), depth, memo)
                };

                if matched == positive {
                    pc = end_pc + 1;
                } else {
                    fail!();
                }
            }

            Inst::CheckGroup { slot, yes_pc, no_pc } => {
                let has = state.slots.get(*slot    ).and_then(|x| *x).is_some()
                       && state.slots.get(*slot + 1).and_then(|x| *x).is_some();
                pc = if has { *yes_pc } else { *no_pc };
            }

            Inst::AbsenceStart(inner_end_pc) => {
                let inner_end        = *inner_end_pc;
                let continuation_pc  = inner_end + 1;
                let start            = pos;
                let text_len         = ctx.text.len();

                // Collect valid end positions (longest first).
                let mut valid_ends: Vec<usize> = (start..=text_len)
                    .filter(|&e| ctx.text.is_char_boundary(e))
                    .collect();
                valid_ends.reverse();
                valid_ends.retain(|&end| {
                    !check_inner_in_range(ctx, pc + 1, inner_end, start, end, state, depth, memo)
                });

                if valid_ends.is_empty() { fail!(); }

                // Push shorter alternatives as backtrack points (shortest last = popped first
                // only after the longer ones fail), then jump to the longest.
                for &end in valid_ends[1..].iter().rev() {
                    push_retry(&mut bt, continuation_pc, end, state);
                }
                pos = valid_ends[0];
                pc  = continuation_pc;
            }
        }
    }
}

/// Run the lookaround body (instructions at `body_pc`…`LookEnd`) in an
/// isolated sub-execution.  For positive lookarounds the outer captures are
/// updated on success; for negative lookarounds (or failure) they are left
/// unchanged.
///
/// The `memo` table is shared with the outer execution (Algorithm 6) so that
/// failures discovered inside a lookaround body are visible to the outer VM
/// and vice-versa, giving the correct complexity guarantee for patterns that
/// combine Fork and lookaround.
fn exec_lookaround(
    ctx: &Ctx<'_>,
    body_pc: usize,
    pos: usize,
    state: &mut State,
    dir: LookDir,
    // `Some(lens)`: pre-computed byte offsets; `None`: scan all positions.
    behind_lens: Option<&[usize]>,
    depth: usize,
    memo: &mut MemoState,
) -> bool {
    match dir {
        LookDir::Ahead => {
            let mut sub = State { slots: state.slots.clone(), keep_pos: state.keep_pos, call_stack: Vec::new() };
            if exec(ctx, body_pc, pos, &mut sub, depth + 1, memo).is_some() {
                state.slots    = sub.slots;
                state.keep_pos = sub.keep_pos;
                true
            } else {
                false
            }
        }
        LookDir::Behind => {
            // Helper: try matching the body starting at `try_pos`, succeeding only
            // if the sub-execution ends exactly at `pos`.
            let mut try_behind = |try_pos: usize, state: &mut State| -> bool {
                if !ctx.text.is_char_boundary(try_pos) { return false; }
                let mut sub = State { slots: state.slots.clone(), keep_pos: state.keep_pos, call_stack: Vec::new() };
                if exec(ctx, body_pc, try_pos, &mut sub, depth + 1, memo)
                    .map(|end| end == pos)
                    .unwrap_or(false)
                {
                    state.slots    = sub.slots;
                    state.keep_pos = sub.keep_pos;
                    true
                } else {
                    false
                }
            };
            match behind_lens {
                Some(lens) => {
                    // Fixed or bounded width: only try the pre-computed offsets.
                    for &len in lens {
                        if pos < len { continue; }
                        if try_behind(pos - len, state) { return true; }
                    }
                    false
                }
                None => {
                    // Variable/unbounded width: scan every possible start position
                    // from the current position back to the beginning of the text.
                    // Iterate shortest-to-longest (pos, pos-1, …, 0).
                    let mut try_pos = pos;
                    loop {
                        if try_behind(try_pos, state) { return true; }
                        if try_pos == 0 { break; }
                        try_pos -= 1;
                    }
                    false
                }
            }
        }
    }
}

fn check_inner_in_range(
    ctx: &Ctx<'_>,
    inner_start_pc: usize,
    inner_end_pc: usize,
    range_start: usize,
    range_end: usize,
    state: &mut State,
    depth: usize,
    memo: &mut MemoState,
) -> bool {
    // Check if the inner pattern (at [inner_start_pc..inner_end_pc]) matches
    // at any start position i in [range_start..range_end], ending at some j <= range_end.
    for i in range_start..=range_end {
        if !ctx.text.is_char_boundary(i) { continue; }
        let saved_slots    = state.slots.clone();
        let saved_keep     = state.keep_pos;
        let saved_call = state.call_stack.clone();
        let result = exec(ctx, inner_start_pc, i, state, depth + 1, memo);
        state.slots      = saved_slots;
        state.keep_pos   = saved_keep;
        state.call_stack = saved_call;
        if let Some(j) = result {
            if j <= range_end {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Anchor matching
// ---------------------------------------------------------------------------

fn matches_anchor(ctx: &Ctx<'_>, pos: usize, kind: AnchorKind, flags: Flags) -> bool {
    let text = ctx.text;
    match kind {
        AnchorKind::Start => {
            if flags.multiline {
                pos == 0 || text.as_bytes().get(pos - 1) == Some(&b'\n')
            } else {
                pos == 0 || text.as_bytes().get(pos - 1) == Some(&b'\n')
                // ^ in Ruby is always multiline (matches after \n)
            }
        }
        AnchorKind::End => {
            if flags.multiline || true {
                // $ in Ruby matches before \n or at end
                pos == text.len() || text.as_bytes().get(pos) == Some(&b'\n')
            } else {
                pos == text.len()
            }
        }
        AnchorKind::StringStart => pos == 0,
        AnchorKind::StringEnd => pos == text.len(),
        AnchorKind::StringEndOrNl => {
            pos == text.len()
                || (pos + 1 == text.len() && text.as_bytes().get(pos) == Some(&b'\n'))
        }
        AnchorKind::WordBoundary => is_word_boundary(text, pos),
        AnchorKind::NonWordBoundary => !is_word_boundary(text, pos),
        AnchorKind::SearchStart => pos == ctx.search_start,
    }
}

fn is_word_boundary(text: &str, pos: usize) -> bool {
    let before = if pos == 0 {
        false
    } else {
        let prev_ch = text[..pos].chars().last().unwrap_or('\0');
        is_word_char(prev_ch)
    };
    let after = if pos >= text.len() {
        false
    } else {
        let next_ch = text[pos..].chars().next().unwrap_or('\0');
        is_word_char(next_ch)
    };
    before != after
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn strings_eq_ci(a: &str, b: &str) -> bool {
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(ac), Some(bc)) => {
                if !chars_eq_ci(ac, bc) { return false; }
            }
            _ => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// Start-position search strategy
// ---------------------------------------------------------------------------

/// How `CompiledRegex::find` advances through the text looking for candidates.
#[derive(Debug)]
enum StartStrategy {
    /// Pattern is anchored at the start (`\A`): only try `start_pos` once.
    Anchored,
    /// Pattern begins with a multi-char case-sensitive literal prefix.
    /// Use `str::find` to jump directly to each occurrence.
    LiteralPrefix(String),
    /// Pattern's first character must be one of these (case-sensitive).
    /// Use `str::find` with a closure to skip non-matching positions.
    FirstChars(Vec<char>),
    /// No restriction; try every byte-aligned position.
    Anywhere,
}

impl StartStrategy {
    fn compute(prog: &[Inst]) -> Self {
        // Check for anchored start (skip over zero-width Save/KeepStart prefix)
        let first_real = prog.iter().find(|i| !matches!(i, Inst::Save(_) | Inst::KeepStart));
        if matches!(first_real, Some(Inst::Anchor(AnchorKind::StringStart, _))) {
            return StartStrategy::Anchored;
        }

        // Try to extract a case-sensitive literal prefix
        let prefix = extract_literal_prefix(prog);
        if prefix.len() >= 2 {
            return StartStrategy::LiteralPrefix(prefix);
        }

        // Collect the set of possible first characters
        let mut chars: Vec<char> = Vec::new();
        let mut visited = std::collections::HashSet::new();
        if collect_first_chars(prog, 0, &mut chars, &mut visited).is_some() && !chars.is_empty() {
            chars.sort_unstable();
            chars.dedup();
            return StartStrategy::FirstChars(chars);
        }

        StartStrategy::Anywhere
    }
}

/// Collect the leading case-sensitive literal characters (skipping over
/// zero-width instructions at the front of the program).
fn extract_literal_prefix(prog: &[Inst]) -> String {
    let mut prefix = String::new();
    let mut pc = 0;
    while pc < prog.len() {
        match &prog[pc] {
            Inst::Save(_) | Inst::KeepStart => pc += 1,
            Inst::Anchor(AnchorKind::StringStart, _) => pc += 1,
            Inst::Char(c, false) => { prefix.push(*c); pc += 1; }
            _ => break,
        }
    }
    prefix
}

/// Walk the program from `pc` and collect all characters that could appear
/// at the very first consumed position.  Returns `None` if any branch can
/// match an unbounded character (e.g. `AnyChar`, character class, etc.).
fn collect_first_chars(
    prog: &[Inst],
    pc: usize,
    out: &mut Vec<char>,
    visited: &mut std::collections::HashSet<usize>,
) -> Option<()> {
    if !visited.insert(pc) { return Some(()); }
    match prog.get(pc)? {
        Inst::Char(c, false) => out.push(*c),
        Inst::Char(c, true) => {
            out.extend(c.to_lowercase());
            out.extend(c.to_uppercase());
        }
        // Classes / wildcards — first char is unbounded
        Inst::AnyChar(_) | Inst::Class(..) | Inst::Shorthand(..) | Inst::Prop(..) => return None,
        // Greedy fork: both branches may start a match
        Inst::Fork(alt) => {
            let alt = *alt;
            collect_first_chars(prog, pc + 1, out, visited)?;
            collect_first_chars(prog, alt, out, visited)?;
        }
        // Lazy fork: same — both branches may start a match
        Inst::ForkNext(alt) => {
            let alt = *alt;
            collect_first_chars(prog, alt, out, visited)?;
            collect_first_chars(prog, pc + 1, out, visited)?;
        }
        Inst::Jump(t) => collect_first_chars(prog, *t, out, visited)?,
        // Zero-width instructions: skip and analyse the next instruction
        Inst::Save(_) | Inst::KeepStart => collect_first_chars(prog, pc + 1, out, visited)?,
        Inst::Anchor(AnchorKind::StringStart, _) | Inst::Anchor(AnchorKind::Start, _) => {
            collect_first_chars(prog, pc + 1, out, visited)?;
        }
        // Unrestricted (empty match, other anchors, …)
        _ => return None,
    }
    Some(())
}

// ---------------------------------------------------------------------------
// CompiledRegex
// ---------------------------------------------------------------------------

/// Return `true` if any instruction in `prog` performs a jump whose target
/// lies strictly between `pc` (exclusive) and `match_pc` (inclusive).
/// Such a jump would represent a path to Match that bypasses `pc`, meaning
/// the instruction at `pc` is NOT mandatory.
fn bypasses(prog: &[Inst], pc: usize, match_pc: usize) -> bool {
    for inst in prog {
        let target = match inst {
            Inst::Fork(t) | Inst::ForkNext(t) | Inst::Jump(t) => Some(*t),
            _ => None,
        };
        if let Some(t) = target {
            if t > pc && t <= match_pc {
                return true;
            }
        }
    }
    false
}

/// Walk backwards from `Match`, looking for a mandatory case-sensitive `Char`.
/// An instruction is "mandatory" (on every path to Match) only if no branch
/// can reach Match by jumping over it.  The returned char, if present, must
/// appear in the text for any match to succeed.
fn compute_required_char(prog: &[Inst]) -> Option<char> {
    let match_pc = prog.iter().rposition(|i| matches!(i, Inst::Match))?;
    let mut pc = match_pc;
    while pc > 0 {
        pc -= 1;
        // If anything can jump past pc directly to a pc' ∈ (pc, match_pc],
        // then pc is not on all paths — stop the search.
        if bypasses(prog, pc, match_pc) {
            break;
        }
        match &prog[pc] {
            Inst::Char(c, false) => return Some(*c),
            // Zero-width instructions: keep walking backwards
            Inst::Save(_) | Inst::KeepStart | Inst::RetIfCalled => {}
            // Any other instruction (branch, charset, etc.) — stop
            _ => break,
        }
    }
    None
}

pub struct CompiledRegex {
    prog: Vec<Inst>,
    charsets: Vec<CharSet>,
    pub named_groups: Vec<(String, usize)>, // (name, 1-based index)
    num_groups: usize,
    start_strategy: StartStrategy,
    /// A character that must appear in the text for any match to be possible.
    required_char: Option<char>,
    /// Whether memoization is safe to use for this pattern.
    /// Set to `false` when the pattern contains backreferences, because the
    /// outcome of a Fork at `(pc, pos)` depends on the current capture-slot
    /// values and therefore cannot be memoized by position alone.
    use_memo: bool,
}

impl CompiledRegex {
    pub fn new(pattern: &str, _opts: CompileOptions) -> Result<Self, Error> {
        let (ast, named) = parse(pattern)?;
        let prog_data = compile(&ast, named, _opts)?;

        let named_groups: Vec<(String, usize)> = prog_data
            .named_groups
            .into_iter()
            .map(|(n, i)| (n, i as usize))
            .collect();

        let start_strategy = StartStrategy::compute(&prog_data.prog);
        let required_char  = compute_required_char(&prog_data.prog);
        // Disable memoization for patterns where the fork/lookaround outcome
        // can depend on the current capture-slot state (not just on pc + pos):
        //   • BackRef / BackRefRelBack  — outcome depends on captured text
        //   • CheckGroup               — branches on whether a group matched
        let use_memo = !prog_data.prog.iter().any(|i| matches!(
            i,
            Inst::BackRef(..) | Inst::BackRefRelBack(..) | Inst::CheckGroup { .. }
        ));

        Ok(CompiledRegex {
            prog: prog_data.prog,
            charsets: prog_data.charsets,
            named_groups,
            num_groups: prog_data.num_groups,
            start_strategy,
            required_char,
            use_memo,
        })
    }

    /// Try to match at exactly `pos`.  Returns `(match_start, end, slots)` or `None`.
    fn try_at(&self, text: &str, pos: usize, memo: &mut MemoState) -> Option<(usize, usize, Vec<Option<usize>>)> {
        let ctx = Ctx { prog: &self.prog, charsets: &self.charsets, text, search_start: pos, use_memo: self.use_memo };
        let mut state = State::new(self.num_groups);
        let end = exec(&ctx, 0, pos, &mut state, 0, memo)?;
        let match_start = state.keep_pos.unwrap_or(pos);
        state.slots.resize(self.num_groups * 2, None);
        Some((match_start, end, state.slots))
    }

    /// Find the leftmost match starting search from `start_pos`.
    /// Returns `(match_start, match_end, capture_slots)`.
    pub fn find(
        &self,
        text: &str,
        start_pos: usize,
    ) -> Option<(usize, usize, Vec<Option<usize>>)> {
        // Fast pre-filter: if the pattern requires a specific character, check
        // that it appears before running the search loop.  For `Anchored` we
        // skip the scan (one exec() call is cheaper than a memchr over the whole
        // text).  All other strategies benefit from the early-out.
        if !matches!(self.start_strategy, StartStrategy::Anchored) {
            if let Some(rc) = self.required_char {
                if !text[start_pos..].contains(rc) {
                    return None;
                }
            }
        }

        // The memoization table is created once per find() call and shared
        // across all exec() invocations (different starting positions) and all
        // sub-executions (lookaround bodies).  This implements Algorithm 6 of
        // Fujinami & Hasuo 2024: failures found inside lookarounds propagate to
        // the outer execution, and failures found at one starting position can
        // be reused at later starting positions for the same (pc, pos) pair.
        let mut memo = MemoState::new();

        match &self.start_strategy {
            // Only one position to try.
            StartStrategy::Anchored => self.try_at(text, start_pos, &mut memo),

            // Use str::find(prefix_str) to jump directly to each candidate.
            StartStrategy::LiteralPrefix(prefix) => {
                let mut pos = start_pos;
                loop {
                    let offset = text[pos..].find(prefix.as_str())?;
                    let candidate = pos + offset;
                    if let Some(result) = self.try_at(text, candidate, &mut memo) {
                        return Some(result);
                    }
                    // Advance one char past the failed candidate
                    pos = candidate + text[candidate..].chars().next()
                        .map(|c| c.len_utf8()).unwrap_or(1);
                    if pos > text.len() { return None; }
                }
            }

            // Use str::find(closure) to skip positions where the first char
            // cannot possibly start a match.
            StartStrategy::FirstChars(chars) => {
                let chars = chars.as_slice();
                let mut pos = start_pos;
                loop {
                    let offset = text[pos..].find(|c| chars.contains(&c))?;
                    let candidate = pos + offset;
                    if let Some(result) = self.try_at(text, candidate, &mut memo) {
                        return Some(result);
                    }
                    pos = candidate + text[candidate..].chars().next()
                        .map(|c| c.len_utf8()).unwrap_or(1);
                    if pos > text.len() { return None; }
                }
            }

            // Original: try every byte-aligned position.
            StartStrategy::Anywhere => {
                let mut pos = start_pos;
                loop {
                    if let Some(result) = self.try_at(text, pos, &mut memo) {
                        return Some(result);
                    }
                    if pos >= text.len() { return None; }
                    pos += text[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                }
            }
        }
    }
}
