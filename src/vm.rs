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
        behind_lens: Vec<usize>,
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
/// `AtomicBarrier` entries are skipped (they mean the atomic body failed
/// entirely; the barrier itself is discarded and backtracking continues).
fn do_backtrack(bt: &mut Vec<Bt>, pc: &mut usize, pos: &mut usize, state: &mut State) -> bool {
    loop {
        match bt.pop() {
            None => return false,
            Some(Bt::AtomicBarrier) => continue,
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
fn exec(ctx: &Ctx<'_>, start_pc: usize, start_pos: usize, state: &mut State, depth: usize) -> Option<usize> {
    if depth > MAX_DEPTH {
        return None;
    }

    let mut pc = start_pc;
    let mut pos = start_pos;
    let mut bt: Vec<Bt> = Vec::new();

    'vm: loop {
        // Macro: trigger backtracking or return None if the stack is empty.
        macro_rules! fail {
            () => {{
                if !do_backtrack(&mut bt, &mut pc, &mut pos, state) {
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
                push_retry(&mut bt, *alt, pos, state);
                pc += 1;
            }

            // Lazy fork: try alt first; save pc+1 as a backtrack point.
            Inst::ForkNext(alt) => {
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
                bt.push(Bt::AtomicBarrier);
                pc += 1;
            }

            Inst::AtomicEnd => {
                // Commit: discard all backtrack entries up to and including
                // the nearest AtomicBarrier (innermost atomic group).
                loop {
                    match bt.pop() {
                        None | Some(Bt::AtomicBarrier) => break,
                        Some(Bt::Retry { .. }) => {}
                    }
                }
                pc += 1;
            }

            Inst::LookStart { positive, dir, end_pc, behind_lens } => {
                let positive = *positive;
                let end_pc   = *end_pc;
                let matched  = exec_lookaround(ctx, pc + 1, pos, state, *dir, behind_lens, depth);
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
                    !check_inner_in_range(ctx, pc + 1, inner_end, start, end, state, depth)
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
fn exec_lookaround(
    ctx: &Ctx<'_>,
    body_pc: usize,
    pos: usize,
    state: &mut State,
    dir: LookDir,
    behind_lens: &[usize],
    depth: usize,
) -> bool {
    match dir {
        LookDir::Ahead => {
            let mut sub = State { slots: state.slots.clone(), keep_pos: state.keep_pos, call_stack: Vec::new() };
            if exec(ctx, body_pc, pos, &mut sub, depth + 1).is_some() {
                state.slots    = sub.slots;
                state.keep_pos = sub.keep_pos;
                true
            } else {
                false
            }
        }
        LookDir::Behind => {
            for &len in behind_lens {
                if pos < len { continue; }
                let try_pos = pos - len;
                if !ctx.text.is_char_boundary(try_pos) { continue; }
                let mut sub = State { slots: state.slots.clone(), keep_pos: state.keep_pos, call_stack: Vec::new() };
                if exec(ctx, body_pc, try_pos, &mut sub, depth + 1)
                    .map(|end| end == pos)
                    .unwrap_or(false)
                {
                    state.slots    = sub.slots;
                    state.keep_pos = sub.keep_pos;
                    return true;
                }
            }
            false
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
) -> bool {
    // Check if the inner pattern (at [inner_start_pc..inner_end_pc]) matches
    // at any start position i in [range_start..range_end], ending at some j <= range_end.
    for i in range_start..=range_end {
        if !ctx.text.is_char_boundary(i) { continue; }
        let saved_slots    = state.slots.clone();
        let saved_keep     = state.keep_pos;
        let saved_call = state.call_stack.clone();
        let result = exec(ctx, inner_start_pc, i, state, depth + 1);
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
// CompiledRegex
// ---------------------------------------------------------------------------

pub struct CompiledRegex {
    prog: Vec<Inst>,
    charsets: Vec<CharSet>,
    pub named_groups: Vec<(String, usize)>, // (name, 1-based index)
    num_groups: usize,
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

        Ok(CompiledRegex {
            prog: prog_data.prog,
            charsets: prog_data.charsets,
            named_groups,
            num_groups: prog_data.num_groups,
        })
    }

    /// Find the leftmost match starting search from `start_pos`.
    /// Returns `(match_start, match_end, capture_slots)`.
    pub fn find(
        &self,
        text: &str,
        start_pos: usize,
    ) -> Option<(usize, usize, Vec<Option<usize>>)> {
        let mut pos = start_pos;
        loop {
            let ctx = Ctx {
                prog: &self.prog,
                charsets: &self.charsets,
                text,
                search_start: pos,
            };
            let mut state = State::new(self.num_groups);
            if let Some(end) = exec(&ctx, 0, pos, &mut state, 0) {
                let match_start = state.keep_pos.unwrap_or(pos);
                // Ensure slots vector is large enough
                state.slots.resize(self.num_groups * 2, None);
                return Some((match_start, end, state.slots));
            }
            if pos >= text.len() { break; }
            // Advance by one character
            pos += text[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        None
    }
}
