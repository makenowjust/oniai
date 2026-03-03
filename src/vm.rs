use crate::ast::{AnchorKind, Flags};
use crate::bytetrie::ByteTrie;
use crate::casefold::case_fold;
use crate::casefold_trie::{
    charset_to_bytetrie, charset_to_bytetrie_back, fold_seq_to_trie, fold_seq_to_trie_back,
};
use crate::compile::CompileOptions;
use crate::error::Error;
use crate::ir;
use crate::parser::parse;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Character set types (used by instructions)
// ---------------------------------------------------------------------------

/// A character class compiled to a sorted, merged inversion list.
///
/// All expansion (shorthands, POSIX classes, Unicode properties, nested
/// classes, intersections, and case-fold equivalents) is performed at compile
/// time by `compile_charset`.  At match time only a binary search is needed.
#[derive(Debug, Clone)]
pub struct CharSet {
    pub negate: bool,
    /// Sorted, merged `(lo, hi)` inclusive codepoint ranges.
    /// For `ignore_case` patterns the ranges already include all single-codepoint
    /// case-fold equivalents; multi-codepoint folds (e.g. ß→"ss") are handled
    /// separately by the ByteTrie.
    pub ranges: Vec<(char, char)>,
    /// 128-bit ASCII fast-path bitmap (one bit per codepoint < 128).
    /// Avoids binary search for the common case of ASCII input.
    pub(crate) ascii_bits: [u64; 2],
}

impl CharSet {
    /// Construct a `CharSet` from a negation flag and a sorted, merged range list.
    pub fn new(negate: bool, ranges: Vec<(char, char)>) -> Self {
        let ascii_bits = Self::build_ascii_bits(&ranges);
        CharSet {
            negate,
            ranges,
            ascii_bits,
        }
    }

    fn build_ascii_bits(ranges: &[(char, char)]) -> [u64; 2] {
        let mut bits = [0u64; 2];
        for &(lo, hi) in ranges {
            let lo_u = lo as u32;
            if lo_u >= 128 {
                break; // ranges are sorted; no ASCII codepoints follow
            }
            let hi_u = (hi as u32).min(127);
            for cp in lo_u..=hi_u {
                bits[(cp >> 6) as usize] |= 1u64 << (cp & 63);
            }
        }
        bits
    }

    /// Returns `true` when `ch` is contained in this character set.
    ///
    /// ASCII codepoints use a precomputed 128-bit bitmap (O(1)).
    /// Non-ASCII codepoints fall back to binary search on `ranges`.
    ///
    /// No case folding is performed here; the caller is responsible for
    /// ensuring that the ranges were built with case-fold expansion when
    /// needed.  Multi-codepoint folds are handled at the instruction level via
    /// the ByteTrie.
    pub fn matches(&self, ch: char) -> bool {
        let found = if (ch as u32) < 128 {
            let cp = ch as u32;
            (self.ascii_bits[(cp >> 6) as usize] >> (cp & 63)) & 1 != 0
        } else {
            self.ranges
                .binary_search_by(|&(lo, hi)| {
                    if ch < lo {
                        std::cmp::Ordering::Greater
                    } else if ch > hi {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .is_ok()
        };
        if self.negate { !found } else { found }
    }
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Inst {
    /// Successful match
    Match,

    /// Match a single character (exact match only; case-insensitive literals use FoldSeq)
    Char(char),

    /// `.` — match any character; bool = dotall (matches \n)
    AnyChar(bool),

    /// Character class (index into charsets vec, ignore_case)
    Class(usize, bool),

    /// Match a single character ending at `pos`; decrement pos by char len.
    CharBack(char),
    /// `.` backward — match any char ending at `pos`; decrement pos.
    AnyCharBack(bool),
    /// Character class backward.
    ClassBack(usize, bool),

    /// Case-fold-sequence match: consume text chars until their accumulated full
    /// case fold equals `folded`.  Handles multi-codepoint folds (e.g. `ß` ↔ `ss`).
    FoldSeq(Vec<char>),
    /// Backward version of `FoldSeq`.
    FoldSeqBack(Vec<char>),

    /// Non-capturing string-alternation trie (forward).  Matches the longest
    /// string in `alt_tries[idx]` at the current position.  Replaces a Fork
    /// chain over plain-string alternatives for O(len) instead of O(len×k)
    /// matching.  Only emitted when `!ignore_case` and all alternatives are
    /// pure literal strings.
    AltTrie(usize),
    /// Backward version of `AltTrie` (uses a reversed trie).
    AltTrieBack(usize),

    /// Anchor (`^`, `$`, `\b`, `\A`, etc.)
    Anchor(AnchorKind, Flags),

    /// Unconditional jump to absolute PC
    Jump(usize),

    /// Greedy fork: try pc+1; on failure try `usize`.
    /// The optional `char` is a compile-time guard: if `text[pos]` does not
    /// equal the guard character the primary path (pc+1) is guaranteed to fail
    /// immediately, so the VM skips directly to the alternative without any
    /// stack push.
    Fork(usize, Option<char>),

    /// Lazy fork: try `usize` first; on failure try pc+1.
    /// Guard semantics mirror `Fork`: if `text[pos]` does not equal the guard
    /// the primary path (`usize`) fails immediately, so skip to pc+1.
    ForkNext(usize, Option<char>),

    /// Advance `pos` by one character without a bounds or match check.
    /// Only emitted immediately after a `Fork`/`ForkNext` whose guard has
    /// already verified that `text[pos]` equals this character.
    #[allow(dead_code)]
    CharFast(char),

    /// Possessive greedy span for a single character.
    ///
    /// Emitted by `spanify_greedy_loops` when the body of a `{1,}` greedy
    /// quantifier is `Char(c)` and `c` is provably disjoint from the
    /// continuation's first-character set.  Advances `pos` as long as
    /// `text[pos] == c`, then jumps to `exit_pc`.  No backtrack entries are
    /// pushed (possessive semantics — safe because disjointness guarantees
    /// backtracking into the span could never help the continuation succeed).
    SpanChar { c: char, exit_pc: usize },

    /// Possessive greedy span for a character class (non-case-folding).
    ///
    /// Same as `SpanChar` but matches `charsets[idx]` at each position.
    SpanClass { idx: usize, exit_pc: usize },

    /// Record `(pos, bt_depth)` in null-check slot `n`.
    /// Must appear **before** the Fork/ForkNext of the loop so that the saved
    /// `bt_depth` is below the Fork's retry entry on the backtrack stack.
    NullCheckStart(usize),

    /// Commit the current loop iteration and exit the loop if position has not
    /// advanced since the matching `NullCheckStart`.
    /// On null: truncate the backtrack stack to the saved depth (discarding the
    /// Fork retry and any body save/undo entries without executing them, so
    /// captures from this iteration are **kept**), then jump to `exit_pc`.
    /// This matches Onigmo's semantics: the empty-match iteration is committed.
    NullCheckEnd { slot: usize, exit_pc: usize },

    /// Save current position to slot
    Save(usize),

    /// Reset match-start to current position (`\K`)
    KeepStart,

    /// Backreference to 1-based group number; ignore_case; optional recursion level
    BackRef(u32, bool, Option<i32>),

    /// Relative-backward backreference — resolved at compile time to `BackRef`;
    /// kept for documentation purposes but never emitted.
    #[allow(dead_code)]
    BackRefRelBack(u32, bool),

    /// Push (pc+1) onto call stack and jump to absolute target
    Call(usize),

    /// Pop call stack and jump to the saved address
    #[allow(dead_code)]
    Ret,

    /// If call stack is non-empty, pop and jump to return addr; otherwise fall through
    RetIfCalled,

    /// Atomic group start; end_pc = index of AtomicEnd instruction
    AtomicStart(usize),

    /// Marks end of atomic body (acts like Match when executing inner)
    AtomicEnd,

    /// Lookaround start
    LookStart { positive: bool, end_pc: usize },

    /// Marks end of lookaround body
    LookEnd,

    /// Conditional group: check if group `slot` has matched; jump to yes/no
    CheckGroup {
        slot: usize,
        yes_pc: usize,
        no_pc: usize,
    },

    /// Absence operator start; inner program at [pc+1..inner_end_pc]
    AbsenceStart(usize),

    /// Marks end of absence inner program
    AbsenceEnd,

    /// Initialize repeat-counter slot to 0.
    /// Emitted at the start of a counter-based exact-repetition loop (`x{n}` with n ≥ threshold).
    RepeatInit { slot: usize },

    /// Increment repeat-counter slot; if the new value is less than `count` jump to `body_pc`
    /// (start of the loop body); otherwise fall through.
    /// Emitted at the end of a counter-based exact-repetition loop body.
    RepeatNext {
        slot: usize,
        count: u32,
        body_pc: usize,
    },
}

// ---------------------------------------------------------------------------
// Capture slot storage
// ---------------------------------------------------------------------------

/// Number of capture slots that fit inline (= 8 capture groups + group 0).
const SLOTS_INLINE: usize = 18;

/// Sentinel value encoding `None` in the slot array.
const NO_SLOT: usize = usize::MAX;

/// Compact capture slot storage.  For patterns with ≤ `SLOTS_INLINE` slots,
/// the data lives in an inline fixed-size array — no heap allocation.
/// Larger patterns fall back to a heap-backed `Vec`.
///
/// Slots are encoded as plain `usize` with `NO_SLOT` (`usize::MAX`) meaning
/// `None`, saving half the memory compared to `Option<usize>` (which is 16
/// bytes on 64-bit).  Clone of the inline variant is a simple 144-byte memcpy
/// with no heap allocation, dramatically reducing fork overhead.
#[derive(Clone)]
pub(crate) enum SmallSlots {
    Inline {
        len: u16,
        data: [usize; SLOTS_INLINE],
    },
    Heap(Vec<usize>),
}

impl SmallSlots {
    fn new(len: usize) -> Self {
        if len <= SLOTS_INLINE {
            SmallSlots::Inline {
                len: len as u16,
                data: [NO_SLOT; SLOTS_INLINE],
            }
        } else {
            SmallSlots::Heap(vec![NO_SLOT; len])
        }
    }

    #[inline]
    fn len(&self) -> usize {
        match self {
            SmallSlots::Inline { len, .. } => *len as usize,
            SmallSlots::Heap(v) => v.len(),
        }
    }

    /// Return the slot value at `idx`, or `None` if out-of-range or unset.
    #[inline]
    fn get(&self, idx: usize) -> Option<usize> {
        let raw = match self {
            SmallSlots::Inline { len, data } => {
                if idx >= *len as usize {
                    return None;
                }
                data[idx]
            }
            SmallSlots::Heap(v) => {
                if idx >= v.len() {
                    return None;
                }
                v[idx]
            }
        };
        if raw == NO_SLOT { None } else { Some(raw) }
    }

    /// Set slot `idx` to `Some(pos)`, growing the storage if necessary.
    #[inline]
    fn set(&mut self, idx: usize, pos: usize) {
        match self {
            SmallSlots::Inline { len, data } => {
                if idx < SLOTS_INLINE {
                    if idx >= *len as usize {
                        *len = (idx + 1) as u16;
                    }
                    data[idx] = pos;
                    return;
                }
                // Spill to heap.
                let mut v = vec![NO_SLOT; idx + 1];
                v[..*len as usize].copy_from_slice(&data[..*len as usize]);
                v[idx] = pos;
                *self = SmallSlots::Heap(v);
            }
            SmallSlots::Heap(v) => {
                if idx >= v.len() {
                    v.resize(idx + 1, NO_SLOT);
                }
                v[idx] = pos;
            }
        }
    }

    /// Set slot `idx` to `None`, growing the storage if necessary.
    #[inline]
    fn clear(&mut self, idx: usize) {
        match self {
            SmallSlots::Inline { len, data } => {
                if idx < SLOTS_INLINE {
                    if idx >= *len as usize {
                        *len = (idx + 1) as u16;
                    }
                    data[idx] = NO_SLOT;
                } else {
                    let mut v = vec![NO_SLOT; idx + 1];
                    v[..*len as usize].copy_from_slice(&data[..*len as usize]);
                    *self = SmallSlots::Heap(v);
                }
            }
            SmallSlots::Heap(v) => {
                if idx >= v.len() {
                    v.resize(idx + 1, NO_SLOT);
                }
                v[idx] = NO_SLOT;
            }
        }
    }

    /// Set slot `idx` to `val` (either `Some` or `None`).
    #[inline]
    fn set_option(&mut self, idx: usize, val: Option<usize>) {
        match val {
            Some(pos) => self.set(idx, pos),
            None => self.clear(idx),
        }
    }

    /// Resize to exactly `new_len` slots (truncating or extending with `None`).
    fn resize(&mut self, new_len: usize) {
        match self {
            SmallSlots::Inline { len, data } => {
                if new_len <= SLOTS_INLINE {
                    let old = *len as usize;
                    *len = new_len as u16;
                    if new_len > old {
                        data[old..new_len].fill(NO_SLOT);
                    }
                } else {
                    let mut v = vec![NO_SLOT; new_len];
                    v[..*len as usize].copy_from_slice(&data[..*len as usize]);
                    *self = SmallSlots::Heap(v);
                }
            }
            SmallSlots::Heap(v) => v.resize(new_len, NO_SLOT),
        }
    }

    /// Convert to `Vec<Option<usize>>` for the public API.
    fn to_vec_options(&self) -> Vec<Option<usize>> {
        (0..self.len()).map(|i| self.get(i)).collect()
    }
}

// ---------------------------------------------------------------------------
// VM state
// ---------------------------------------------------------------------------

pub(crate) struct State {
    /// Flat capture slots: slots[2*(n-1)] = start, slots[2*(n-1)+1] = end (1-based groups)
    pub(crate) slots: SmallSlots,
    /// Where `\K` reset the match start
    pub(crate) keep_pos: Option<usize>,
    /// Return address stack for subexpression calls
    pub(crate) call_stack: Vec<usize>,
    /// Per-loop-guard saved `(pos, bt_depth)` for null-loop checks.
    /// Indexed by the slot number in `NullCheckStart`/`NullCheckEnd`.
    /// Initialised to `(usize::MAX, 0)` (no saved position).
    pub(crate) null_check_slots: Vec<(usize, usize)>,
}

impl State {
    fn new(num_groups: usize, num_null_checks: usize) -> Self {
        State {
            slots: SmallSlots::new(num_groups * 2),
            keep_pos: None,
            call_stack: Vec::new(),
            null_check_slots: vec![(usize::MAX, 0); num_null_checks],
        }
    }
}

// ---------------------------------------------------------------------------
// Execution context (immutable per-search data)
// ---------------------------------------------------------------------------

pub(crate) struct Ctx<'t> {
    pub(crate) prog: &'t [Inst],
    pub(crate) charsets: &'t [CharSet],
    /// Parallel to `prog`: precomputed ByteTrie for FoldSeq/FoldSeqBack/Class(ic=true)
    /// instructions.  `None` for all other instruction types.
    pub(crate) match_tries: &'t [Option<ByteTrie>],
    /// Per-alternation ByteTrie for `AltTrie`/`AltTrieBack` instructions.
    pub(crate) alt_tries: &'t [ByteTrie],
    pub(crate) text: &'t str,
    pub(crate) search_start: usize,
    pub(crate) use_memo: bool,
    /// Number of null-check guard slots in the program; used to size `State::null_check_slots`.
    pub(crate) num_null_checks: usize,
    /// Number of repeat-counter slots in the program; used to size the `counters` vec in `exec()`.
    pub(crate) num_repeat_counters: usize,
}

impl<'t> Ctx<'t> {
    fn char_at(&self, pos: usize) -> Option<(char, usize)> {
        if pos >= self.text.len() {
            return None;
        }
        let ch = self.text[pos..].chars().next()?;
        Some((ch, ch.len_utf8()))
    }

    fn char_before(&self, pos: usize) -> Option<(char, usize)> {
        if pos == 0 {
            return None;
        }
        let bytes = self.text.as_bytes();
        let mut start = pos - 1;
        while start > 0 && bytes[start] & 0xC0 == 0x80 {
            start -= 1;
        }
        let c = self.text[start..pos].chars().next()?;
        Some((c, pos - start))
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
        slots: SmallSlots,
        keep_pos: Option<usize>,
        call_stack: Vec<usize>,
    },
    /// Atomic group fence.  Removed when the atomic body commits (`AtomicEnd`);
    /// silently skipped when encountered while backtracking (the body has
    /// exhausted all its internal alternatives and failed as a unit).
    AtomicBarrier,
    /// Memoization marker pushed alongside every `Fork`/`ForkNext` alternative.
    /// When popped during backtracking it means **both** paths of the fork have
    /// been exhausted; we record `(fork_pc, counters_snapshot, fork_pos)` as a
    /// known-failure entry so that any future visit to the same fork state at the
    /// same position and counter context can short-circuit immediately
    /// (Algorithm 5 of Fujinami & Hasuo 2024).
    MemoMark {
        fork_pc: usize,
        fork_pos: usize,
        /// Snapshot of the repeat-counter vector at the time the Fork executed.
        /// Empty when the program has no counter-based repetitions.
        counters_snapshot: Vec<u32>,
    },
}

/// Pack a `(pc, pos)` pair into a single `u64` key for the memo table.
#[inline]
pub(crate) fn memo_key(pc: usize, pos: usize) -> u64 {
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
pub(crate) enum LookCacheEntry {
    /// The lookaround body matched.
    /// `slot_delta`: `(slot_index, new_value)` for every slot whose value
    ///   changed during the successful body execution.
    /// `keep_pos_delta`: new `keep_pos` if the body contained `\K` and
    ///   changed the value; `None` means "leave outer keep_pos unchanged".
    BodyMatched {
        slot_delta: Vec<(usize, Option<usize>)>,
        keep_pos_delta: Option<Option<usize>>,
    },
    /// The lookaround body did not match.  No outer-state changes occurred.
    BodyNotMatched,
}

/// Shared memoization state, created once per `find()` call and threaded
/// through the entire `exec` / `exec_lookaround` / `check_inner_in_range`
/// call tree.
pub(crate) struct MemoState {
    /// Fork failure depth bitmasks — fast path for patterns **without** counter-based
    /// repetitions (`num_repeat_counters == 0`).
    /// Maps `memo_key(fork_pc, fork_pos)` → depth bitmask (unchanged from the original design).
    pub(crate) fork_failures: HashMap<u64, u8>,
    /// Fork failure depth bitmasks — slow path for patterns **with** counter-based
    /// repetitions (`num_repeat_counters > 0`).
    /// The counter snapshot is included in the key so that two visits to the same
    /// Fork at the same position but under different counter contexts are not
    /// conflated (see design notes in plan.md).
    pub(crate) fork_failures_counted: HashMap<(u64, Vec<u32>), u8>,
    /// Lookaround result cache (Algorithm 6).
    /// Maps `memo_key(lk_pc, lk_pos)` → whether the lookaround body matched
    /// and, for positive lookaheads, the capture-slot changes it produced.
    pub(crate) look_results: HashMap<u64, LookCacheEntry>,
}

impl MemoState {
    fn new() -> Self {
        MemoState {
            fork_failures: HashMap::new(),
            fork_failures_counted: HashMap::new(),
            look_results: HashMap::new(),
        }
    }
}

/// Compute which capture slots changed between `pre` and `post`.
/// Returns `(slot_index, new_value)` pairs for every slot that differs.
fn compute_slot_delta(pre: &SmallSlots, post: &SmallSlots) -> Vec<(usize, Option<usize>)> {
    let len = pre.len().max(post.len());
    (0..len)
        .filter_map(|i| {
            let old = pre.get(i);
            let new = post.get(i);
            if old != new { Some((i, new)) } else { None }
        })
        .collect()
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
/// - `MemoMark` entries record `(fork_pc, counters_snapshot, fork_pos)` as a
///   failure tagged with the CURRENT `atomic_depth` (Algorithm 7: depth-tagged
///   failures).  Only entries recorded at depth ≤ current depth may be reused
///   later, so we keep the minimum depth seen for each key.
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
            Some(Bt::MemoMark {
                fork_pc,
                fork_pos,
                counters_snapshot,
            }) => {
                if use_memo {
                    let bit = 1u8 << (*atomic_depth).min(7);
                    if counters_snapshot.is_empty() {
                        // Fast path: plain u64 key.
                        let key = memo_key(fork_pc, fork_pos);
                        memo.fork_failures
                            .entry(key)
                            .and_modify(|b| *b |= bit)
                            .or_insert(bit);
                    } else {
                        // Slow path: counter-aware key.
                        let key = (memo_key(fork_pc, fork_pos), counters_snapshot);
                        memo.fork_failures_counted
                            .entry(key)
                            .and_modify(|b| *b |= bit)
                            .or_insert(bit);
                    }
                }
                continue;
            }
            Some(Bt::Retry {
                pc: rpc,
                pos: rpos,
                slots,
                keep_pos,
                call_stack,
            }) => {
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
pub(crate) fn exec(
    ctx: &Ctx<'_>,
    start_pc: usize,
    start_pos: usize,
    state: &mut State,
    depth: usize,
    memo: &mut MemoState,
) -> Option<usize> {
    if depth > MAX_DEPTH {
        return None;
    }

    let mut pc = start_pc;
    let mut pos = start_pos;
    let mut bt: Vec<Bt> = Vec::new();
    // Repeat-counter slots for counter-based exact repetition loops.
    // Each exec() call starts with all counters zeroed; exec_lookaround
    // sub-calls also start fresh (counters are NOT part of State).
    let mut counters = vec![0u32; ctx.num_repeat_counters];
    // Local atomic-group nesting counter (Algorithm 7).
    // Incremented by AtomicStart, decremented by AtomicEnd (success path) and
    // when AtomicBarrier is popped during backtracking (failure path).
    let mut atomic_depth: usize = 0;

    'vm: loop {
        // Macro: trigger backtracking or return None if the stack is empty.
        macro_rules! fail {
            () => {{
                if !do_backtrack(
                    &mut bt,
                    &mut pc,
                    &mut pos,
                    state,
                    memo,
                    &mut atomic_depth,
                    ctx.use_memo,
                ) {
                    return None;
                }
                continue 'vm;
            }};
        }

        match &ctx.prog[pc] {
            Inst::Match => return Some(pos),

            // Terminators for sub-executions (lookaround / absence inner program)
            Inst::LookEnd | Inst::AbsenceEnd => return Some(pos),

            Inst::Char(ch) => match ctx.char_at(pos) {
                Some((c, len)) if *ch == c => {
                    pos += len;
                    pc += 1;
                }
                _ => fail!(),
            },

            Inst::AnyChar(dotall) => match ctx.char_at(pos) {
                Some((c, len)) if *dotall || c != '\n' => {
                    pos += len;
                    pc += 1;
                }
                _ => fail!(),
            },

            Inst::Class(idx, ignore_case) => {
                let cs = &ctx.charsets[*idx];
                // For case-insensitive, check multi-char fold ByteTrie first.
                // The ByteTrie contains fold sequences for chars in the charset
                // that have multi-codepoint full case folds (e.g. ß → "ss").
                if *ignore_case && let Some(trie) = ctx.match_tries.get(pc).and_then(|o| o.as_ref())
                {
                    if cs.negate {
                        // Negated: if a multi-char fold of the inner charset matches
                        // here, the inner charset would match → negation → fail.
                        if trie.advance(ctx.text.as_bytes(), pos).is_some() {
                            fail!();
                        }
                    } else if let Some(new_pos) = trie.advance(ctx.text.as_bytes(), pos) {
                        pos = new_pos;
                        pc += 1;
                        continue 'vm;
                    }
                }
                // Single-char check; cs.ranges already contains all single-char
                // case-fold equivalents for ignore_case=true patterns.
                match ctx.char_at(pos) {
                    Some((c, len)) if cs.matches(c) => {
                        pos += len;
                        pc += 1;
                    }
                    _ => fail!(),
                }
            }

            Inst::CharBack(ch) => match ctx.char_before(pos) {
                Some((c, len)) if *ch == c => {
                    pos -= len;
                    pc += 1;
                }
                _ => fail!(),
            },

            Inst::AnyCharBack(dotall) => match ctx.char_before(pos) {
                Some((c, len)) if *dotall || c != '\n' => {
                    pos -= len;
                    pc += 1;
                }
                _ => fail!(),
            },

            Inst::ClassBack(idx, ignore_case) => {
                let cs = &ctx.charsets[*idx];
                if *ignore_case && let Some(trie) = ctx.match_tries.get(pc).and_then(|o| o.as_ref())
                {
                    if cs.negate {
                        if trie.advance_back(ctx.text.as_bytes(), pos).is_some() {
                            fail!();
                        }
                    } else if let Some(new_pos) = trie.advance_back(ctx.text.as_bytes(), pos) {
                        pos = new_pos;
                        pc += 1;
                        continue 'vm;
                    }
                }
                match ctx.char_before(pos) {
                    Some((c, len)) if cs.matches(c) => {
                        pos -= len;
                        pc += 1;
                    }
                    _ => fail!(),
                }
            }

            Inst::FoldSeq(folded) => {
                let new_pos = if let Some(trie) = ctx.match_tries.get(pc).and_then(|o| o.as_ref()) {
                    trie.advance(ctx.text.as_bytes(), pos)
                } else {
                    fold_advance(ctx.text, pos, folded)
                };
                match new_pos {
                    Some(new_pos) => {
                        pos = new_pos;
                        pc += 1;
                    }
                    None => fail!(),
                }
            }

            Inst::FoldSeqBack(folded) => {
                let new_pos = if let Some(trie) = ctx.match_tries.get(pc).and_then(|o| o.as_ref()) {
                    trie.advance_back(ctx.text.as_bytes(), pos)
                } else {
                    fold_retreat(ctx.text, pos, folded)
                };
                match new_pos {
                    Some(new_pos) => {
                        pos = new_pos;
                        pc += 1;
                    }
                    None => fail!(),
                }
            }

            Inst::AltTrie(idx) => match ctx.alt_tries[*idx].advance(ctx.text.as_bytes(), pos) {
                Some(new_pos) => {
                    pos = new_pos;
                    pc += 1;
                }
                None => fail!(),
            },

            Inst::AltTrieBack(idx) => {
                match ctx.alt_tries[*idx].advance_back(ctx.text.as_bytes(), pos) {
                    Some(new_pos) => {
                        pos = new_pos;
                        pc += 1;
                    }
                    None => fail!(),
                }
            }

            Inst::Anchor(kind, flags) => {
                if matches_anchor(ctx, pos, *kind, *flags) {
                    pc += 1;
                } else {
                    fail!();
                }
            }

            Inst::Jump(target) => {
                pc = *target;
            }

            // Greedy fork: try pc+1 first; save alt as a backtrack point.
            Inst::Fork(alt, guard) => {
                // Guard fast-path: if a required first char is known and the
                // current character doesn't match it, the primary path (pc+1)
                // will fail on its very first instruction.  Skip directly to
                // `alt` without touching the backtrack stack.
                if matches!(guard, Some(gc) if !matches!(ctx.char_at(pos), Some((c, _)) if c == *gc))
                {
                    pc = *alt;
                    continue 'vm;
                }
                if ctx.use_memo {
                    let depth_mask = (1u16 << (atomic_depth + 1)).wrapping_sub(1) as u8;
                    if ctx.num_repeat_counters == 0 {
                        // Fast path (no counter loops): plain u64 key, no Vec allocation.
                        let key = memo_key(pc, pos);
                        if let Some(&bitmask) = memo.fork_failures.get(&key)
                            && bitmask & depth_mask != 0
                        {
                            fail!();
                        }
                        bt.push(Bt::MemoMark {
                            fork_pc: pc,
                            fork_pos: pos,
                            counters_snapshot: Vec::new(),
                        });
                    } else {
                        // Slow path (counter loops): include counter snapshot in key.
                        let snapshot = counters.clone();
                        let key = (memo_key(pc, pos), snapshot.clone());
                        if let Some(&bitmask) = memo.fork_failures_counted.get(&key)
                            && bitmask & depth_mask != 0
                        {
                            fail!();
                        }
                        bt.push(Bt::MemoMark {
                            fork_pc: pc,
                            fork_pos: pos,
                            counters_snapshot: snapshot,
                        });
                    }
                }
                push_retry(&mut bt, *alt, pos, state);
                pc += 1;
            }

            // Lazy fork: try alt first; save pc+1 as a backtrack point.
            Inst::ForkNext(alt, guard) => {
                // Guard fast-path: if the primary path (`alt`) starts with a
                // char that doesn't match, skip directly to pc+1.
                if matches!(guard, Some(gc) if !matches!(ctx.char_at(pos), Some((c, _)) if c == *gc))
                {
                    pc += 1;
                    continue 'vm;
                }
                if ctx.use_memo {
                    let depth_mask = (1u16 << (atomic_depth + 1)).wrapping_sub(1) as u8;
                    if ctx.num_repeat_counters == 0 {
                        let key = memo_key(pc, pos);
                        if let Some(&bitmask) = memo.fork_failures.get(&key)
                            && bitmask & depth_mask != 0
                        {
                            fail!();
                        }
                        bt.push(Bt::MemoMark {
                            fork_pc: pc,
                            fork_pos: pos,
                            counters_snapshot: Vec::new(),
                        });
                    } else {
                        let snapshot = counters.clone();
                        let key = (memo_key(pc, pos), snapshot.clone());
                        if let Some(&bitmask) = memo.fork_failures_counted.get(&key)
                            && bitmask & depth_mask != 0
                        {
                            fail!();
                        }
                        bt.push(Bt::MemoMark {
                            fork_pc: pc,
                            fork_pos: pos,
                            counters_snapshot: snapshot,
                        });
                    }
                }
                push_retry(&mut bt, pc + 1, pos, state);
                pc = *alt;
            }

            // CharFast: the preceding Fork guard already verified text[pos] == c,
            // so skip the bounds check and comparison — just advance.
            Inst::CharFast(c) => {
                pos += c.len_utf8();
                pc += 1;
            }

            Inst::SpanChar { c, exit_pc } => {
                while let Some((ch, len)) = ctx.char_at(pos) {
                    if ch != *c {
                        break;
                    }
                    pos += len;
                }
                pc = *exit_pc;
            }

            Inst::SpanClass { idx, exit_pc } => {
                let cs = &ctx.charsets[*idx];
                while let Some((ch, len)) = ctx.char_at(pos) {
                    if !cs.matches(ch) {
                        break;
                    }
                    pos += len;
                }
                pc = *exit_pc;
            }

            Inst::NullCheckStart(slot) => {
                // Save (pos, bt_depth) BEFORE the loop's Fork/ForkNext pushes its retry.
                state.null_check_slots[*slot] = (pos, bt.len());
                pc += 1;
            }

            Inst::NullCheckEnd { slot, exit_pc } => {
                let (saved_pos, saved_bt_len) = state.null_check_slots[*slot];
                if pos == saved_pos {
                    // Commit: discard the Fork's retry + body save/undo entries by
                    // truncating the bt stack.  Captures from this (empty) iteration
                    // are kept in state.slots — this matches Onigmo's behaviour.
                    bt.truncate(saved_bt_len);
                    pc = *exit_pc;
                } else {
                    pc += 1;
                }
            }

            Inst::Save(slot) => {
                let slot = *slot;
                state.slots.set(slot, pos);
                pc += 1;
            }

            Inst::KeepStart => {
                state.keep_pos = Some(pos);
                pc += 1;
            }

            Inst::BackRef(group, ignore_case, _level) => {
                let slot_open = ((*group - 1) * 2) as usize;
                let slot_close = slot_open + 1;
                let start = match state.slots.get(slot_open) {
                    Some(s) => s,
                    None => fail!(),
                };
                let end = match state.slots.get(slot_close) {
                    Some(e) => e,
                    None => fail!(),
                };
                let captured = &ctx.text[start..end];
                if *ignore_case {
                    match caseless_advance(ctx.text, pos, captured) {
                        Some(new_pos) => pos = new_pos,
                        None => fail!(),
                    }
                } else {
                    let len = captured.len();
                    if ctx.text.get(pos..pos + len) != Some(captured) {
                        fail!();
                    }
                    pos += len;
                }
                pc += 1;
            }

            Inst::BackRefRelBack(_n, _ic) => {
                fail!();
            } // not yet implemented

            Inst::Call(target) => {
                if state.call_stack.len() >= MAX_CALL_DEPTH {
                    fail!();
                }
                state.call_stack.push(pc + 1);
                pc = *target;
            }

            Inst::Ret => match state.call_stack.pop() {
                Some(r) => pc = r,
                None => fail!(),
            },

            Inst::RetIfCalled => match state.call_stack.pop() {
                Some(r) => pc = r,
                None => {
                    pc += 1;
                }
            },

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
                        Some(Bt::AtomicBarrier) => {
                            atomic_depth -= 1;
                            break;
                        }
                        Some(Bt::Retry { .. }) | Some(Bt::MemoMark { .. }) => {}
                    }
                }
                pc += 1;
            }

            Inst::LookStart { positive, end_pc } => {
                let positive = *positive;
                let end_pc = *end_pc;
                let lk_key = memo_key(pc, pos);

                let matched = if ctx.use_memo {
                    if let Some(entry) = memo.look_results.get(&lk_key).cloned() {
                        match entry {
                            LookCacheEntry::BodyMatched {
                                slot_delta,
                                keep_pos_delta,
                            } => {
                                if positive {
                                    for (idx, val) in slot_delta {
                                        state.slots.set_option(idx, val);
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
                        let pre_slots = state.slots.clone();
                        let pre_keep = state.keep_pos;
                        let m = exec_lookaround(ctx, pc + 1, pos, state, depth, memo);
                        let entry = if m {
                            let slot_delta = compute_slot_delta(&pre_slots, &state.slots);
                            let keep_pos_delta = if state.keep_pos != pre_keep {
                                Some(state.keep_pos)
                            } else {
                                None
                            };
                            LookCacheEntry::BodyMatched {
                                slot_delta,
                                keep_pos_delta,
                            }
                        } else {
                            LookCacheEntry::BodyNotMatched
                        };
                        memo.look_results.insert(lk_key, entry);
                        m
                    }
                } else {
                    exec_lookaround(ctx, pc + 1, pos, state, depth, memo)
                };

                if matched == positive {
                    pc = end_pc + 1;
                } else {
                    fail!();
                }
            }

            Inst::CheckGroup {
                slot,
                yes_pc,
                no_pc,
            } => {
                let has = state.slots.get(*slot).is_some() && state.slots.get(*slot + 1).is_some();
                pc = if has { *yes_pc } else { *no_pc };
            }

            Inst::AbsenceStart(inner_end_pc) => {
                let inner_end = *inner_end_pc;
                let continuation_pc = inner_end + 1;
                let start = pos;
                let text_len = ctx.text.len();

                // Collect valid end positions (longest first).
                let mut valid_ends: Vec<usize> = (start..=text_len)
                    .filter(|&e| ctx.text.is_char_boundary(e))
                    .collect();
                valid_ends.reverse();
                valid_ends.retain(|&end| {
                    !check_inner_in_range(ctx, pc + 1, inner_end, start, end, state, depth, memo)
                });

                if valid_ends.is_empty() {
                    fail!();
                }

                // Push shorter alternatives as backtrack points (shortest last = popped first
                // only after the longer ones fail), then jump to the longest.
                for &end in valid_ends[1..].iter().rev() {
                    push_retry(&mut bt, continuation_pc, end, state);
                }
                pos = valid_ends[0];
                pc = continuation_pc;
            }

            Inst::RepeatInit { slot } => {
                counters[*slot] = 0;
                pc += 1;
            }

            Inst::RepeatNext {
                slot,
                count,
                body_pc,
            } => {
                counters[*slot] += 1;
                if counters[*slot] < *count {
                    pc = *body_pc;
                } else {
                    pc += 1;
                }
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
pub(crate) fn exec_lookaround(
    ctx: &Ctx<'_>,
    body_pc: usize,
    pos: usize,
    state: &mut State,
    depth: usize,
    memo: &mut MemoState,
) -> bool {
    let mut sub = State {
        slots: state.slots.clone(),
        keep_pos: state.keep_pos,
        call_stack: Vec::new(),
        null_check_slots: vec![(usize::MAX, 0); ctx.num_null_checks],
    };
    if exec(ctx, body_pc, pos, &mut sub, depth + 1, memo).is_some() {
        state.slots = sub.slots;
        state.keep_pos = sub.keep_pos;
        true
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn check_inner_in_range(
    ctx: &Ctx<'_>,
    inner_start_pc: usize,
    _inner_end_pc: usize,
    range_start: usize,
    range_end: usize,
    state: &mut State,
    depth: usize,
    memo: &mut MemoState,
) -> bool {
    // Check if the inner pattern (at [inner_start_pc..inner_end_pc]) matches
    // at any start position i in [range_start..range_end], ending at some j <= range_end.
    for i in range_start..=range_end {
        if !ctx.text.is_char_boundary(i) {
            continue;
        }
        let saved_slots = state.slots.clone();
        let saved_keep = state.keep_pos;
        let saved_call = state.call_stack.clone();
        let result = exec(ctx, inner_start_pc, i, state, depth + 1, memo);
        state.slots = saved_slots;
        state.keep_pos = saved_keep;
        state.call_stack = saved_call;
        if let Some(j) = result
            && j <= range_end
        {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Anchor matching
// ---------------------------------------------------------------------------

fn matches_anchor(ctx: &Ctx<'_>, pos: usize, kind: AnchorKind, _flags: Flags) -> bool {
    let text = ctx.text;
    match kind {
        AnchorKind::Start => {
            // ^ in Ruby is always multiline (matches after \n)
            pos == 0 || text.as_bytes().get(pos - 1) == Some(&b'\n')
        }
        AnchorKind::End => {
            // $ in Ruby matches before \n or at end (always multiline)
            pos == text.len() || text.as_bytes().get(pos) == Some(&b'\n')
        }
        AnchorKind::StringStart => pos == 0,
        AnchorKind::StringEnd => pos == text.len(),
        AnchorKind::StringEndOrNl => {
            pos == text.len() || (pos + 1 == text.len() && text.as_bytes().get(pos) == Some(&b'\n'))
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

/// Advance through `text[start..]`, consuming chars until their accumulated
/// Unicode case fold equals `folded`.  Returns the new position on success.
pub(crate) fn fold_advance(text: &str, start: usize, folded: &[char]) -> Option<usize> {
    if folded.is_empty() {
        return Some(start);
    }
    let mut fi = 0usize; // index into folded
    let mut pos = start;
    for ch in text[start..].chars() {
        pos += ch.len_utf8();
        for fc in case_fold(ch).chars() {
            if fi >= folded.len() || *fc != folded[fi] {
                return None;
            }
            fi += 1;
        }
        if fi == folded.len() {
            return Some(pos);
        }
    }
    if fi == folded.len() { Some(pos) } else { None }
}

/// Retreat backwards through `text[..pos]`, consuming chars (right-to-left)
/// until their accumulated Unicode case fold equals `folded` (which was built
/// left-to-right, so we prepend each new char's fold).  Returns the new
/// (lower) position on success.
pub(crate) fn fold_retreat(text: &str, mut pos: usize, folded: &[char]) -> Option<usize> {
    if folded.is_empty() {
        return Some(pos);
    }
    let mut fi = folded.len(); // we need to match folded[0..fi] from the right
    loop {
        if fi == 0 {
            return Some(pos);
        }
        if pos == 0 {
            return None;
        }
        // Find the char ending at `pos`
        let mut char_start = pos - 1;
        while char_start > 0 && !text.is_char_boundary(char_start) {
            char_start -= 1;
        }
        let ch = text[char_start..pos].chars().next()?;
        // Collect this char's case fold into a small stack buffer (max 3 for Unicode)
        let mut cbuf = ['\0'; 4];
        let mut clen = 0usize;
        for fc in case_fold(ch).chars() {
            cbuf[clen] = *fc;
            clen += 1;
        }
        if clen > fi || cbuf[..clen] != folded[fi - clen..fi] {
            return None;
        }
        fi -= clen;
        pos = char_start;
    }
}

/// Try to match `pattern` case-insensitively at `text[start..]`.
/// Returns the new position (end of the matched text slice) on success, or `None`.
/// Handles multi-character Unicode case folds (e.g. `ß` ↔ `ss`, `ﬁ` ↔ `fi`).
fn caseless_advance(text: &str, start: usize, pattern: &str) -> Option<usize> {
    let folded: Vec<char> = pattern
        .chars()
        .flat_map(|c| {
            let f = case_fold(c);
            // Collect into a small owned vec; Multi is &'static so cheap to iterate
            f.chars().to_vec()
        })
        .collect();
    fold_advance(text, start, &folded)
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
    /// Top-level alternation of case-sensitive literals (each ≥ 2 chars).
    /// Uses an Aho-Corasick automaton to find the leftmost candidate in O(n).
    LiteralSet(aho_corasick::AhoCorasick),
    /// Pattern starts with a case-insensitive literal (FoldSeq).
    /// Stores the full folded sequence for pre-filtering; the scanner uses
    /// SIMD str::find for ASCII first-byte variants and raw-byte scans for
    /// `non_ascii_first_bytes` (derived from the ByteTrie root transitions) to
    /// find non-ASCII candidates without calling `case_fold()` at scan time.
    CaselessPrefix {
        folded: Vec<char>,
        /// Non-ASCII bytes that can start an accepted sequence (derived from
        /// the precomputed ByteTrie for this fold sequence).
        non_ascii_first_bytes: Vec<u8>,
    },
    /// Pattern's first character must be one of these.
    /// Use `str::find(char)` per candidate and take the leftmost hit.
    FirstChars(Vec<char>),
    /// Pattern's first real instruction is a case-sensitive `Class` whose
    /// accepted bytes form a single contiguous ASCII range `[lo, hi]`
    /// (e.g. `[0-9]` → `lo=0x30, hi=0x39`, `[A-Z]` → `lo=0x41, hi=0x5A`).
    ///
    /// Scan uses `b.wrapping_sub(lo) <= hi.wrapping_sub(lo)`, which is a
    /// simple arithmetic predicate that LLVM auto-vectorizes to a SIMD range
    /// check (typically PCMPGTB / PCMPEQB on x86-64).
    ///
    /// Only emitted when the charset is ASCII-only (`can_match_non_ascii == false`).
    RangeStart { lo: u8, hi: u8 },
    /// Pattern's first real instruction is a case-sensitive `Class`.
    /// Use the precomputed ASCII bitmap to skip positions that cannot start
    /// a match without invoking the full exec machinery.
    AsciiClassStart {
        /// 128-bit bitmap of ASCII codepoints accepted by the charset
        /// (negation already applied): bit `b` set means byte `b` is a
        /// valid match-start candidate.
        ascii_bits: [u64; 2],
        /// `true` if the charset (after negation) can match at least one
        /// non-ASCII codepoint.  When `false`, non-ASCII bytes are skipped.
        can_match_non_ascii: bool,
    },
    /// There is a mandatory ASCII byte somewhere at a fixed offset range in every
    /// match.  Use `memchr` to find each occurrence, then try NFA starts within
    /// the lookbehind window.
    RequiredByte {
        byte: u8,
        /// Maximum distance from the NFA start position to where `byte` appears.
        /// I.e., `byte` appears at most this many bytes after `text[pos]`.
        max_offset: usize,
    },
    /// No restriction; try every byte-aligned position.
    Anywhere,
}

/// If `ascii_bits` has exactly one contiguous run of set bits in [0, 127],
/// return `(lo, hi)` where `lo..=hi` is that range.  Returns `None` for
/// multi-range bitmaps (e.g. `[a-zA-Z]`).
fn detect_ascii_range(bits: [u64; 2]) -> Option<(u8, u8)> {
    // Find lowest set bit
    let lo: u8 = if bits[0] != 0 {
        bits[0].trailing_zeros() as u8
    } else if bits[1] != 0 {
        64 + bits[1].trailing_zeros() as u8
    } else {
        return None; // empty bitmap
    };
    // Find highest set bit
    let hi: u8 = if bits[1] != 0 {
        127 - bits[1].leading_zeros() as u8
    } else {
        63 - bits[0].leading_zeros() as u8
    };
    // Build expected bitmap for the range [lo, hi] and verify it matches.
    // This check ensures there are no gaps in the range.
    let mut expected = [0u64; 2];
    for b in lo..=hi {
        expected[(b >> 6) as usize] |= 1u64 << (b & 63);
    }
    if bits == expected {
        Some((lo, hi))
    } else {
        None
    }
}

impl StartStrategy {
    fn compute(
        prog: &[Inst],
        charsets: &[CharSet],
        match_tries: &[Option<ByteTrie>],
        alt_tries: &[ByteTrie],
    ) -> Self {
        // Check for anchored start (skip over zero-width Save/KeepStart prefix)
        let first_real = prog
            .iter()
            .find(|i| !matches!(i, Inst::Save(_) | Inst::KeepStart));
        if matches!(first_real, Some(Inst::Anchor(AnchorKind::StringStart, _))) {
            return StartStrategy::Anchored;
        }

        // Try to extract a case-sensitive literal prefix
        let prefix = extract_literal_prefix(prog);
        if prefix.len() >= 2 {
            return StartStrategy::LiteralPrefix(prefix);
        }

        // Try to extract a case-insensitive literal prefix (FoldSeq)
        if let Some((folded, foldseq_pc)) = extract_caseless_prefix(prog) {
            // Extract non-ASCII first bytes from the precomputed ByteTrie so the
            // scanner can scan for them with raw byte comparisons instead of calling
            // `case_fold()` on every non-ASCII character.
            let non_ascii_first_bytes: Vec<u8> = match_tries
                .get(foldseq_pc)
                .and_then(|o| o.as_ref())
                .map(|trie| trie.first_bytes().filter(|&b| b >= 0x80).collect())
                .unwrap_or_default();
            return StartStrategy::CaselessPrefix {
                folded,
                non_ascii_first_bytes,
            };
        }

        // AltTrie at the start: use the trie's strings as a LiteralSet.
        // Skip over zero-width instructions (and lookaround prefixes) to find
        // the first real instruction that determines valid match-start positions.
        // Skipping LookStart blocks is safe: if the continuation's first char
        // can't match at position P, neither can the whole pattern (regardless
        // of what the lookaround checks).
        let mut pc = 0;
        loop {
            match prog.get(pc) {
                Some(Inst::Save(_) | Inst::KeepStart) => pc += 1,
                Some(Inst::LookStart { end_pc, .. }) => pc = end_pc + 1,
                _ => break,
            }
        }
        if let Some(Inst::AltTrie(idx)) = prog.get(pc) {
            // Choose the start strategy based on the ByteTrie's first bytes.
            //
            // AC "Teddy" SIMD is fast for few (≤ ~8) short patterns; for larger
            // AltTrie sets (common in case-insensitive alternation, which expands
            // to all fold-variants) the AC automaton exceeds Teddy's limits and
            // falls back to a slow NFA/DFA.  We use faster alternatives:
            //
            //  ≤ 3 distinct ASCII first bytes → FirstChars (memchr SIMD, fastest)
            //  > 3 ASCII first bytes, contiguous range → RangeStart (LLVM auto-vec)
            //  > 3 ASCII first bytes, non-contiguous → AsciiClassStart (bitmap scan)
            //
            // For small AltTries (≤ 8 all-strings, each ≤ 8 bytes) where AC Teddy
            // would normally win, we prefer FirstChars/RangeStart because they are
            // equally or more competitive and remain fast even as pattern count grows.
            let trie = &alt_tries[*idx];
            let mut ascii_bits = [0u64; 2];
            let mut can_match_non_ascii = false;
            for b in trie.first_bytes() {
                if b < 128 {
                    ascii_bits[(b >> 6) as usize] |= 1u64 << (b & 63);
                } else {
                    can_match_non_ascii = true;
                }
            }
            let ascii_count = ascii_bits[0].count_ones() + ascii_bits[1].count_ones();
            if ascii_count > 0 {
                if !can_match_non_ascii && ascii_count <= 3 {
                    // Collect actual char values for memchr/memchr2/memchr3.
                    let mut first_chars: Vec<char> = Vec::new();
                    for word in 0..2u8 {
                        let mut bits = ascii_bits[word as usize];
                        while bits != 0 {
                            let bit = bits.trailing_zeros() as u8;
                            first_chars.push((word * 64 + bit) as char);
                            bits &= bits - 1;
                        }
                    }
                    first_chars.sort_unstable();
                    return StartStrategy::FirstChars(first_chars);
                }
                if !can_match_non_ascii && let Some((lo, hi)) = detect_ascii_range(ascii_bits) {
                    return StartStrategy::RangeStart { lo, hi };
                }
                return StartStrategy::AsciiClassStart {
                    ascii_bits,
                    can_match_non_ascii,
                };
            }
        }

        // Try to extract a set of literals from a top-level alternation
        if let Some(lits) = extract_literal_set(prog) {
            let ac =
                aho_corasick::AhoCorasick::new(&lits).expect("literal set strings are valid UTF-8");
            return StartStrategy::LiteralSet(ac);
        }

        // Collect the set of possible first characters
        let mut chars: Vec<char> = Vec::new();
        let mut visited = std::collections::HashSet::new();
        if collect_first_chars(prog, 0, &mut chars, &mut visited).is_some() && !chars.is_empty() {
            chars.sort_unstable();
            chars.dedup();
            return StartStrategy::FirstChars(chars);
        }

        // If the first real instruction is a case-sensitive Class, use its
        // precomputed ASCII bitmap to skip non-matching ASCII positions.
        let class_idx = match prog.get(pc) {
            Some(Inst::Class(idx, false)) => Some(*idx),
            _ => None,
        };
        if let Some(idx) = class_idx {
            let cs = &charsets[idx];
            // Build an accept bitmap with negation already applied.
            let accept_bits = if cs.negate {
                [!cs.ascii_bits[0], !cs.ascii_bits[1]]
            } else {
                cs.ascii_bits
            };
            // The charset can match non-ASCII iff it (after negation) contains
            // any codepoint >= 128.
            let can_match_non_ascii = if cs.negate {
                // Conservative: negated charsets almost always accept non-ASCII.
                true
            } else {
                cs.ranges.last().is_some_and(|&(_, hi)| (hi as u32) >= 128)
            };
            // Prefer RangeStart for ASCII-only contiguous ranges: the range
            // check `b.wrapping_sub(lo) <= span` is a simple arithmetic
            // predicate that LLVM auto-vectorizes to SIMD.
            if !can_match_non_ascii && let Some((lo, hi)) = detect_ascii_range(accept_bits) {
                return StartStrategy::RangeStart { lo, hi };
            }
            return StartStrategy::AsciiClassStart {
                ascii_bits: accept_bits,
                can_match_non_ascii,
            };
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
            Inst::Char(c) | Inst::CharFast(c) => {
                prefix.push(*c);
                pc += 1;
            }
            _ => break,
        }
    }
    prefix
}

/// If the program starts with a `FoldSeq` (after zero-width prefix
/// instructions), return its folded char sequence.  Returns `None` otherwise.
/// Returns `(folded_chars, foldseq_pc)` when the program starts with a FoldSeq
/// (possibly preceded by zero-width prefix instructions).
fn extract_caseless_prefix(prog: &[Inst]) -> Option<(Vec<char>, usize)> {
    let mut pc = 0;
    while pc < prog.len() {
        match &prog[pc] {
            Inst::Save(_) | Inst::KeepStart => pc += 1,
            Inst::Anchor(AnchorKind::StringStart, _) => pc += 1,
            Inst::FoldSeq(folded) if !folded.is_empty() => return Some((folded.clone(), pc)),
            _ => return None,
        }
    }
    None
}

/// Try to extract a literal from the program starting at `pc`.
/// Skips zero-width instructions at the front and collects consecutive
/// case-sensitive `Char` / `CharFast` instructions.
/// Returns `Some(literal)` with at least 2 chars, or `None`.
fn extract_one_literal(prog: &[Inst], start_pc: usize) -> Option<String> {
    let mut s = String::new();
    let mut pc = start_pc;
    while let Some(inst) = prog.get(pc) {
        match inst {
            Inst::Save(_) | Inst::KeepStart => pc += 1,
            Inst::Char(c) | Inst::CharFast(c) => {
                s.push(*c);
                pc += 1;
            }
            _ => break,
        }
    }
    if s.len() >= 2 { Some(s) } else { None }
}

/// Walk the top-level Fork chain and extract one literal per alternative.
/// Returns `Some(Vec<String>)` only if every branch yields a literal of
/// length ≥ 2 and there are at least 2 branches (≥ 3 would beat
/// `LiteralPrefix` anyway).
fn extract_literal_set(prog: &[Inst]) -> Option<Vec<String>> {
    // Skip zero-width prefix instructions to find the first Fork.
    let mut pc = 0;
    while pc < prog.len() {
        match &prog[pc] {
            Inst::Save(_) | Inst::KeepStart => pc += 1,
            Inst::Fork(_, _) => break,
            _ => return None, // not a top-level alternation
        }
    }
    if pc >= prog.len() {
        return None;
    }

    let mut literals: Vec<String> = Vec::new();

    // Walk the chain of Fork instructions that make up the alternation.
    // Each Fork(alt, _) splits into pc+1 (current branch) and alt (next alt).
    loop {
        match prog.get(pc) {
            Some(Inst::Fork(alt_pc, _)) => {
                let branch_pc = pc + 1;
                let alt_pc = *alt_pc;
                // Extract the literal from this branch.
                let lit = extract_one_literal(prog, branch_pc)?;
                literals.push(lit);
                pc = alt_pc;
            }
            // Last alternative: no Fork, just a literal body.
            _ => {
                let lit = extract_one_literal(prog, pc)?;
                literals.push(lit);
                break;
            }
        }
    }

    if literals.len() >= 2 {
        Some(literals)
    } else {
        None
    }
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
    if !visited.insert(pc) {
        return Some(());
    }
    match prog.get(pc)? {
        Inst::Char(c) | Inst::CharFast(c) => out.push(*c),
        Inst::FoldSeq(folded) if folded.len() == 1 => {
            // Cannot enumerate all Unicode codepoints whose single-char case fold
            // equals folded[0] (e.g. ſ, Kelvin sign K, …).  Return None so the
            // caller falls back to Anywhere scanning.
            return None;
        }
        Inst::FoldSeq(folded) if !folded.is_empty() => {
            // Multi-char fold: text could start with a single char that folds
            // to this sequence (e.g. FoldSeq(['s','s']) can match 'ß'), so we
            // can't enumerate starting chars.
            return None;
        }
        Inst::FoldSeqBack(_) => return None,
        // AltTrie: first chars are the first chars of each alternative string.
        // Since all alternatives are plain ASCII-or-UTF-8 strings, their first
        // codepoints can be enumerated from the trie root transitions.
        // We can't directly decode the root bytes to chars here (no alt_tries
        // reference), so fall back to Anywhere to be safe.
        Inst::AltTrie(_) | Inst::AltTrieBack(_) => return None,
        // Classes / wildcards — first char is unbounded
        Inst::AnyChar(_) | Inst::Class(..) => return None,
        // Greedy fork: both branches may start a match
        Inst::Fork(alt, _) => {
            let alt = *alt;
            collect_first_chars(prog, pc + 1, out, visited)?;
            collect_first_chars(prog, alt, out, visited)?;
        }
        // Lazy fork: same — both branches may start a match
        Inst::ForkNext(alt, _) => {
            let alt = *alt;
            collect_first_chars(prog, alt, out, visited)?;
            collect_first_chars(prog, pc + 1, out, visited)?;
        }
        Inst::Jump(t) => collect_first_chars(prog, *t, out, visited)?,
        // Zero-width instructions: skip and analyse the next instruction
        Inst::Save(_) | Inst::KeepStart => collect_first_chars(prog, pc + 1, out, visited)?,
        // Lookaround prefix: the continuation starts at end_pc+1; the
        // lookaround body does not contribute to first-char selection.
        Inst::LookStart { end_pc, .. } => {
            collect_first_chars(prog, end_pc + 1, out, visited)?;
        }
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
            Inst::Fork(t, _) | Inst::ForkNext(t, _) | Inst::Jump(t) => Some(*t),
            _ => None,
        };
        if let Some(t) = target
            && t > pc
            && t <= match_pc
        {
            return true;
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
            Inst::Char(c) | Inst::CharFast(c) => return Some(*c),
            // Zero-width instructions: keep walking backwards
            Inst::Save(_) | Inst::KeepStart | Inst::RetIfCalled => {}
            // Any other instruction (branch, charset, etc.) — stop
            _ => break,
        }
    }
    None
}

/// Map each `Fork`/`ForkNext` instruction to a compact index 0..num_forks.
/// Returns `true` when the program could exhibit exponential backtracking.
///
/// A pattern is potentially pathological when some backward `Jump` (loop)
/// contains two or more `Fork`/`ForkNext` instructions in its body.  That
/// means the loop can revisit the same fork position, potentially creating
/// an exponential number of backtrack states.
///
/// When this returns `false`, memoization provides no speed benefit and can
/// be disabled entirely — eliminating the HashMap/dense-array overhead for
/// the common case (non-pathological patterns like `a+`, `a?b`, etc.).
/// Returns a `Vec<Option<u32>>` of the same length as `prog`.
#[cfg(feature = "jit")]
pub(crate) fn compute_fork_pc_indices(prog: &[Inst]) -> Vec<Option<u32>> {
    let mut indices = vec![None; prog.len()];
    let mut idx: u32 = 0;
    for (pc, inst) in prog.iter().enumerate() {
        if matches!(inst, Inst::Fork(_, _) | Inst::ForkNext(_, _)) {
            indices[pc] = Some(idx);
            idx += 1;
        }
    }
    indices
}

/// Build a `Vec<Option<ByteTrie>>` parallel to `prog`.
///
/// Each entry is `Some(trie)` for instructions that can benefit from byte-level
/// matching:
/// - `FoldSeq(folded)` → trie of all UTF-8 sequences that fold to `folded`
/// - `FoldSeqBack(folded)` → reversed trie (for backward matching)
/// - `Class(idx, true)` with a non-negated simple charset → positive advancer trie
/// - `Class(idx, true)` with a negated simple charset → inner-content trie used
///   as a **rejection guard** (the interpreter checks separately)
/// - `ClassBack(idx, true)` → reversed trie of the above
///
/// All other instruction types get `None`.
fn build_match_tries(prog: &[Inst], charsets: &[CharSet]) -> Vec<Option<ByteTrie>> {
    prog.iter()
        .map(|inst| match inst {
            Inst::FoldSeq(folded) => Some(fold_seq_to_trie(folded)),
            Inst::FoldSeqBack(folded) => Some(fold_seq_to_trie_back(folded)),
            Inst::Class(idx, true) => {
                let cs = &charsets[*idx];
                // All charsets are now inversion lists; ByteTrie covers
                // multi-codepoint folds (positive advancer or negation guard).
                Some(charset_to_bytetrie(cs, true))
            }
            Inst::ClassBack(idx, true) => {
                let cs = &charsets[*idx];
                Some(charset_to_bytetrie_back(cs, true))
            }
            _ => None,
        })
        .collect()
}

/// Per-charset ByteTrie for case-insensitive `Class` instructions.  Indexed by
/// charset index (parallel to `charsets`).  Used by the JIT helper which
/// dispatches by charset index rather than PC.
///
/// For non-negated charsets: trie is a positive advancer.
/// For negated charsets: trie is the inner-content trie used as a
/// rejection guard in `jit_match_class`.
#[cfg(feature = "jit")]
fn build_class_tries(charsets: &[CharSet]) -> Vec<Option<ByteTrie>> {
    charsets
        .iter()
        .map(|cs| Some(charset_to_bytetrie(cs, true)))
        .collect()
}

pub struct CompiledRegex {
    prog: Vec<Inst>,
    charsets: Vec<CharSet>,
    pub named_groups: Vec<(String, usize)>, // (name, 1-based index)
    num_groups: usize,
    /// Number of null-check guard slots emitted by the compiler.
    num_null_checks: usize,
    /// Number of repeat-counter slots emitted by the compiler.
    num_repeat_counters: usize,
    start_strategy: StartStrategy,
    /// A character that must appear in the text for any match to be possible.
    required_char: Option<char>,
    /// Whether memoization is safe to use for this pattern.
    /// Set to `false` when the pattern contains backreferences, because the
    /// outcome of a Fork at `(pc, pos)` depends on the current capture-slot
    /// values and therefore cannot be memoized by position alone.
    use_memo: bool,
    /// Parallel to `prog`: precomputed `ByteTrie` for `FoldSeq`, `FoldSeqBack`,
    /// `Class(_, true)`, and `ClassBack(_, true)` instructions.  `None` for all
    /// other instruction types.  Used by `exec()` to match raw bytes without
    /// any `case_fold()` calls or UTF-8 decoding at match time.
    match_tries: Vec<Option<ByteTrie>>,
    /// Per-alternation ByteTrie for `AltTrie`/`AltTrieBack` instructions.
    alt_tries: Vec<ByteTrie>,
    /// Per-charset ByteTrie for case-insensitive `Class` instructions, indexed
    /// by charset index (parallel to `charsets`).  Used by the JIT helper
    /// `jit_match_class` which cannot index by PC.
    #[cfg(feature = "jit")]
    class_tries: Vec<Option<ByteTrie>>,
    /// JIT-compiled executor (present when the `jit` feature is enabled and
    /// the program is eligible for JIT compilation).
    #[cfg(feature = "jit")]
    jit: Option<crate::jit::JitModule>,
    /// Maps each PC to its compact fork index (0..num_forks), or `None` for
    /// non-fork instructions.  Used by the JIT dense-memo optimization.
    #[cfg(feature = "jit")]
    fork_pc_indices: Vec<Option<u32>>,
    /// Total number of Fork/ForkNext instructions.  Used to size the dense memo array.
    #[cfg(feature = "jit")]
    fork_pc_count: u32,
    /// Kept alive when the IR JIT was selected: the compiled JIT code embeds raw
    /// pointers into the `IrProgram`'s heap data (e.g. `MatchFoldSeq` char slices),
    /// so `ir_prog` must outlive the `JitModule`.
    #[cfg(feature = "jit")]
    _ir_prog: Option<ir::IrProgram>,
}

impl CompiledRegex {
    pub fn new(pattern: &str, _opts: CompileOptions) -> Result<Self, Error> {
        let (ast, named) = parse(pattern)?;
        let mut ir_prog = ir::build::build(&ast, named, _opts)?;
        if cfg!(debug_assertions) {
            ir::verify::verify(&ir_prog).map_err(Error::Compile)?;
        }
        ir::pass::run_passes(&mut ir_prog);
        let prog_data = ir::lower::lower(&ir_prog);

        let named_groups: Vec<(String, usize)> = prog_data
            .named_groups
            .into_iter()
            .map(|(n, i)| (n, i as usize))
            .collect();

        // Build per-PC byte tries for FoldSeq/FoldSeqBack and case-insensitive
        // Class/ClassBack.  `None` at all other PCs.
        let match_tries = build_match_tries(&prog_data.prog, &prog_data.charsets);

        let mut start_strategy = StartStrategy::compute(
            &prog_data.prog,
            &prog_data.charsets,
            &match_tries,
            &prog_data.alt_tries,
        );
        // If the Vec<Inst> analysis didn't find a restriction but the IR can,
        // upgrade from Anywhere to AsciiClassStart / RangeStart.
        if matches!(start_strategy, StartStrategy::Anywhere)
            && let Some(bits) = ir::prefilter::first_byte_set(&ir_prog)
        {
            let non_zero = bits[0] != 0 || bits[1] != 0;
            if non_zero {
                if let Some((lo, hi)) = detect_ascii_range(bits) {
                    start_strategy = StartStrategy::RangeStart { lo, hi };
                } else {
                    start_strategy = StartStrategy::AsciiClassStart {
                        ascii_bits: bits,
                        can_match_non_ascii: false,
                    };
                }
            }
        }
        // If still Anywhere, try required-byte prefilter.
        if matches!(start_strategy, StartStrategy::Anywhere)
            && let Some((byte, max_offset)) = ir::prefilter::required_byte(&ir_prog)
            && max_offset <= 256
        {
            start_strategy = StartStrategy::RequiredByte { byte, max_offset };
        }
        let required_char = compute_required_char(&prog_data.prog);
        // use_memo from IR (already correctly computed during building)
        let use_memo = ir_prog.use_memo;

        #[cfg(feature = "jit")]
        let fork_pc_indices = compute_fork_pc_indices(&prog_data.prog);
        #[cfg(feature = "jit")]
        let fork_pc_count = fork_pc_indices.iter().filter(|x| x.is_some()).count() as u32;

        #[cfg(feature = "jit")]
        let class_tries = build_class_tries(&prog_data.charsets);

        // Try the IR JIT first.  If it succeeds we must keep `ir_prog` alive because
        // the compiled code contains raw pointers into its heap data (e.g. MatchFoldSeq
        // char slices).  If the IR JIT fails, fall back to the Vec<Inst>-based JIT.
        #[cfg(feature = "jit")]
        let (jit, _ir_prog) = {
            let ir_jit = crate::jit::try_compile_from_ir(&ir_prog);
            if ir_jit.is_some() {
                (ir_jit, Some(ir_prog))
            } else {
                let old_jit = crate::jit::try_compile(
                    &prog_data.prog,
                    &prog_data.charsets,
                    &prog_data.alt_tries,
                    use_memo,
                    &fork_pc_indices,
                );
                (old_jit, None)
            }
        };

        Ok(CompiledRegex {
            prog: prog_data.prog,
            charsets: prog_data.charsets,
            alt_tries: prog_data.alt_tries,
            named_groups,
            num_groups: prog_data.num_groups,
            num_null_checks: prog_data.num_null_checks,
            num_repeat_counters: prog_data.num_repeat_counters,
            start_strategy,
            required_char,
            use_memo,
            match_tries,
            #[cfg(feature = "jit")]
            class_tries,
            #[cfg(feature = "jit")]
            jit,
            #[cfg(feature = "jit")]
            fork_pc_indices,
            #[cfg(feature = "jit")]
            fork_pc_count,
            #[cfg(feature = "jit")]
            _ir_prog,
        })
    }

    /// Try to match at exactly `pos`.  Returns `(match_start, end, slots)` or `None`.
    fn try_at(
        &self,
        text: &str,
        pos: usize,
        memo: &mut MemoState,
        scratch: &mut ExecScratch,
    ) -> Option<(usize, usize, Vec<Option<usize>>)> {
        #[cfg(feature = "jit")]
        if let Some(ref jit) = self.jit {
            return crate::jit::exec_jit(
                jit,
                &self.prog,
                &self.charsets,
                &self.class_tries,
                &self.alt_tries,
                text,
                pos,
                self.num_groups,
                self.num_null_checks,
                self.use_memo,
                memo,
                scratch,
                &self.fork_pc_indices,
                self.fork_pc_count,
            );
        }
        let _ = scratch; // unused in interpreter path

        let ctx = Ctx {
            prog: &self.prog,
            charsets: &self.charsets,
            match_tries: &self.match_tries,
            alt_tries: &self.alt_tries,
            text,
            search_start: pos,
            use_memo: self.use_memo,
            num_null_checks: self.num_null_checks,
            num_repeat_counters: self.num_repeat_counters,
        };
        let mut state = State::new(self.num_groups, self.num_null_checks);
        let end = exec(&ctx, 0, pos, &mut state, 0, memo)?;
        let match_start = state.keep_pos.unwrap_or(pos);
        state.slots.resize(self.num_groups * 2);
        Some((match_start, end, state.slots.to_vec_options()))
    }

    /// Find the leftmost match starting search from `start_pos`.
    /// Returns `(match_start, match_end, capture_slots)`.
    ///
    /// `scratch` is a reusable buffer passed in by the caller.  Callers that
    /// perform many successive searches on the same text (e.g., `find_iter`)
    /// should keep the same `ExecScratch` alive across calls so that the
    /// dense fork-memo array is allocated only once.
    pub fn find_with_scratch(
        &self,
        text: &str,
        start_pos: usize,
        scratch: &mut ExecScratch,
    ) -> Option<(usize, usize, Vec<Option<usize>>)> {
        // Fast pre-filter: if the pattern requires a specific character, check
        // that it appears before running the search loop.  For `Anchored` we
        // skip the scan (one exec() call is cheaper than a memchr over the whole
        // text).  All other strategies benefit from the early-out.
        if !matches!(self.start_strategy, StartStrategy::Anchored)
            && let Some(rc) = self.required_char
            && !text[start_pos..].contains(rc)
        {
            return None;
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
            StartStrategy::Anchored => self.try_at(text, start_pos, &mut memo, scratch),

            // Use str::find(prefix_str) to jump directly to each candidate.
            StartStrategy::LiteralPrefix(prefix) => {
                let needle = prefix.as_bytes();
                let mut pos = start_pos;
                loop {
                    let offset = memchr::memmem::find(&text.as_bytes()[pos..], needle)?;
                    let candidate = pos + offset;
                    if let Some(result) = self.try_at(text, candidate, &mut memo, scratch) {
                        return Some(result);
                    }
                    // Advance one char past the failed candidate
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }

            // Use Aho-Corasick to find the leftmost literal candidate in O(n).
            StartStrategy::LiteralSet(ac) => {
                let mut pos = start_pos;
                loop {
                    let m = ac.find(&text.as_bytes()[pos..])?;
                    let candidate = pos + m.start();
                    if let Some(result) = self.try_at(text, candidate, &mut memo, scratch) {
                        return Some(result);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }

            // Case-insensitive literal prefix: use SIMD str::find for ASCII
            // case variants of folded[0]; scan raw bytes for non-ASCII first
            // bytes derived from the precomputed ByteTrie (avoids calling
            // `case_fold()` on every non-ASCII char in the input).
            StartStrategy::CaselessPrefix {
                folded,
                non_ascii_first_bytes,
            } => {
                let fc0 = folded[0];
                // ASCII case variants of fc0 (computed once, amortised over the search).
                let ascii_vars: Vec<char> = fc0
                    .to_lowercase()
                    .chain(fc0.to_uppercase())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let mut pos = start_pos;
                loop {
                    // SIMD scan for ASCII first-char variants.
                    let simd_pos = ascii_vars
                        .iter()
                        .filter_map(|&c| text[pos..].find(c).map(|off| pos + off))
                        .min();
                    // Scan for non-ASCII first bytes from the ByteTrie without
                    // calling case_fold().  Each byte is a possible start of an
                    // encoding that folds to the required prefix.
                    let non_ascii_pos: Option<usize> = non_ascii_first_bytes
                        .iter()
                        .filter_map(|&b| {
                            let gap_end = simd_pos.unwrap_or(text.len());
                            text.as_bytes()[pos..gap_end]
                                .iter()
                                .position(|&x| x == b)
                                .map(|off| pos + off)
                        })
                        .min();
                    let candidate = match (non_ascii_pos, simd_pos) {
                        (Some(a), Some(b)) => a.min(b),
                        (Some(a), None) | (None, Some(a)) => a,
                        (None, None) => return None,
                    };
                    // Pre-filter: verify the full fold sequence before the NFA.
                    if fold_advance(text, candidate, folded).is_some()
                        && let Some(result) = self.try_at(text, candidate, &mut memo, scratch)
                    {
                        return Some(result);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }

            // Use str::find(char) for each candidate first-char — SIMD-
            // accelerated for ASCII chars.  All case variants are already
            // enumerated in `chars` by `collect_first_chars`, so a plain
            // equality match suffices (no case-fold overhead per character).
            StartStrategy::FirstChars(chars) => {
                // SIMD fast path for 1–3 pure-ASCII chars; fall back to str::find otherwise.
                let text_bytes = text.as_bytes();
                let mut pos = start_pos;
                loop {
                    let candidate = match chars.as_slice() {
                        [c1] if (*c1 as u32) < 128 => {
                            memchr::memchr(*c1 as u8, &text_bytes[pos..]).map(|o| pos + o)
                        }
                        [c1, c2] if (*c1 as u32) < 128 && (*c2 as u32) < 128 => {
                            memchr::memchr2(*c1 as u8, *c2 as u8, &text_bytes[pos..])
                                .map(|o| pos + o)
                        }
                        [c1, c2, c3]
                            if (*c1 as u32) < 128 && (*c2 as u32) < 128 && (*c3 as u32) < 128 =>
                        {
                            memchr::memchr3(*c1 as u8, *c2 as u8, *c3 as u8, &text_bytes[pos..])
                                .map(|o| pos + o)
                        }
                        _ => chars
                            .iter()
                            .filter_map(|&c| text[pos..].find(c).map(|o| pos + o))
                            .min(),
                    };
                    let candidate = candidate?;
                    if let Some(result) = self.try_at(text, candidate, &mut memo, scratch) {
                        return Some(result);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }

            // Contiguous ASCII range [lo, hi]: scan using wrapping subtraction,
            // which LLVM auto-vectorizes to a SIMD range check.
            StartStrategy::RangeStart { lo, hi } => {
                let bytes = text.as_bytes();
                let lo = *lo;
                let span = hi.wrapping_sub(lo);
                let mut pos = start_pos;
                loop {
                    let candidate = bytes[pos..]
                        .iter()
                        .position(|&b| b.wrapping_sub(lo) <= span)
                        .map(|o| pos + o)?;
                    if let Some(result) = self.try_at(text, candidate, &mut memo, scratch) {
                        return Some(result);
                    }
                    pos = candidate + 1;
                }
            }

            // Use the charset's precomputed ASCII bitmap to skip positions
            // that cannot start a match, without invoking the full exec.
            StartStrategy::AsciiClassStart {
                ascii_bits,
                can_match_non_ascii,
            } => {
                let bytes = text.as_bytes();
                let mut pos = start_pos;
                loop {
                    if pos >= bytes.len() {
                        return None;
                    }
                    let b = bytes[pos];
                    if b < 0x80 {
                        // ASCII byte: one bitmap lookup.
                        if (ascii_bits[(b >> 6) as usize] >> (b & 63)) & 1 != 0
                            && let Some(result) = self.try_at(text, pos, &mut memo, scratch)
                        {
                            return Some(result);
                        }
                        pos += 1;
                    } else {
                        // Non-ASCII: advance by full char length.
                        let ch_len = text[pos..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        if *can_match_non_ascii
                            && let Some(result) = self.try_at(text, pos, &mut memo, scratch)
                        {
                            return Some(result);
                        }
                        pos += ch_len;
                    }
                }
            }

            // Use memchr to find required byte, then try NFA from lookbehind window.
            StartStrategy::RequiredByte { byte, max_offset } => {
                let bytes = text.as_bytes();
                let byte = *byte;
                let max_offset = *max_offset;
                let mut scan_pos = start_pos;
                loop {
                    // Jump to next occurrence of the required byte.
                    let found = memchr::memchr(byte, &bytes[scan_pos..])?;
                    let byte_pos = scan_pos + found;
                    // Try NFA from positions in the lookbehind window.
                    let try_from = byte_pos.saturating_sub(max_offset).max(start_pos);
                    for start in try_from..=byte_pos {
                        if let Some(r) = self.try_at(text, start, &mut memo, scratch) {
                            return Some(r);
                        }
                    }
                    scan_pos = byte_pos + 1;
                }
            }

            // Original: try every byte-aligned position.
            StartStrategy::Anywhere => {
                let mut pos = start_pos;
                loop {
                    if let Some(result) = self.try_at(text, pos, &mut memo, scratch) {
                        return Some(result);
                    }
                    if pos >= text.len() {
                        return None;
                    }
                    pos += text[pos..]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                }
            }
        }
    }

    /// Find the leftmost match starting search from `start_pos`.
    /// Creates a temporary `ExecScratch`; callers doing many successive searches
    /// (e.g., `find_iter`) should use `find_with_scratch` directly and pass a
    /// persistent scratch to avoid repeated allocations of the dense fork-memo array.
    pub fn find(&self, text: &str, start_pos: usize) -> Option<(usize, usize, Vec<Option<usize>>)> {
        #[cfg(feature = "jit")]
        let mut scratch = ExecScratch::new();
        #[cfg(not(feature = "jit"))]
        let mut scratch = ExecScratch;
        self.find_with_scratch(text, start_pos, &mut scratch)
    }

    /// Force the interpreter path, bypassing JIT even when it is compiled in.
    /// Available in test, fuzzing, and JIT-enabled builds (for benchmarking).
    #[cfg(feature = "jit")]
    pub fn find_interp(
        &self,
        text: &str,
        start_pos: usize,
    ) -> Option<(usize, usize, Vec<Option<usize>>)> {
        if !matches!(self.start_strategy, StartStrategy::Anchored)
            && let Some(rc) = self.required_char
            && !text[start_pos..].contains(rc)
        {
            return None;
        }
        let mut memo = MemoState::new();
        match &self.start_strategy {
            StartStrategy::Anchored => self.exec_interp(text, start_pos, &mut memo),
            StartStrategy::LiteralPrefix(prefix) => {
                let needle = prefix.as_bytes().to_vec();
                let mut pos = start_pos;
                loop {
                    let offset = memchr::memmem::find(&text.as_bytes()[pos..], &needle)?;
                    let candidate = pos + offset;
                    if let Some(r) = self.exec_interp(text, candidate, &mut memo) {
                        return Some(r);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }
            StartStrategy::LiteralSet(ac) => {
                let mut pos = start_pos;
                loop {
                    let m = ac.find(&text.as_bytes()[pos..])?;
                    let candidate = pos + m.start();
                    if let Some(r) = self.exec_interp(text, candidate, &mut memo) {
                        return Some(r);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }
            StartStrategy::CaselessPrefix {
                folded,
                non_ascii_first_bytes,
            } => {
                let folded = folded.clone();
                let non_ascii_first_bytes = non_ascii_first_bytes.clone();
                let fc0 = folded[0];
                let ascii_vars: Vec<char> = fc0
                    .to_lowercase()
                    .chain(fc0.to_uppercase())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let mut pos = start_pos;
                loop {
                    let simd_pos = ascii_vars
                        .iter()
                        .filter_map(|&c| text[pos..].find(c).map(|off| pos + off))
                        .min();
                    let non_ascii_pos: Option<usize> = non_ascii_first_bytes
                        .iter()
                        .filter_map(|&b| {
                            let gap_end = simd_pos.unwrap_or(text.len());
                            text.as_bytes()[pos..gap_end]
                                .iter()
                                .position(|&x| x == b)
                                .map(|off| pos + off)
                        })
                        .min();
                    let candidate = match (non_ascii_pos, simd_pos) {
                        (Some(a), Some(b)) => a.min(b),
                        (Some(a), None) | (None, Some(a)) => a,
                        (None, None) => return None,
                    };
                    if fold_advance(text, candidate, &folded).is_some()
                        && let Some(r) = self.exec_interp(text, candidate, &mut memo)
                    {
                        return Some(r);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }
            StartStrategy::FirstChars(chars) => {
                let text_bytes = text.as_bytes();
                let mut pos = start_pos;
                loop {
                    let candidate = match chars.as_slice() {
                        [c1] if (*c1 as u32) < 128 => {
                            memchr::memchr(*c1 as u8, &text_bytes[pos..]).map(|o| pos + o)
                        }
                        [c1, c2] if (*c1 as u32) < 128 && (*c2 as u32) < 128 => {
                            memchr::memchr2(*c1 as u8, *c2 as u8, &text_bytes[pos..])
                                .map(|o| pos + o)
                        }
                        [c1, c2, c3]
                            if (*c1 as u32) < 128 && (*c2 as u32) < 128 && (*c3 as u32) < 128 =>
                        {
                            memchr::memchr3(*c1 as u8, *c2 as u8, *c3 as u8, &text_bytes[pos..])
                                .map(|o| pos + o)
                        }
                        _ => chars
                            .iter()
                            .filter_map(|&c| text[pos..].find(c).map(|o| pos + o))
                            .min(),
                    };
                    let candidate = candidate?;
                    if let Some(r) = self.exec_interp(text, candidate, &mut memo) {
                        return Some(r);
                    }
                    pos = candidate
                        + text[candidate..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                    if pos > text.len() {
                        return None;
                    }
                }
            }
            StartStrategy::RangeStart { lo, hi } => {
                let bytes = text.as_bytes();
                let lo = *lo;
                let span = hi.wrapping_sub(lo);
                let mut pos = start_pos;
                loop {
                    let candidate = bytes[pos..]
                        .iter()
                        .position(|&b| b.wrapping_sub(lo) <= span)
                        .map(|o| pos + o)?;
                    if let Some(r) = self.exec_interp(text, candidate, &mut memo) {
                        return Some(r);
                    }
                    pos = candidate + 1;
                }
            }
            StartStrategy::AsciiClassStart {
                ascii_bits,
                can_match_non_ascii,
            } => {
                let ascii_bits = *ascii_bits;
                let can_match_non_ascii = *can_match_non_ascii;
                let bytes = text.as_bytes();
                let mut pos = start_pos;
                loop {
                    if pos >= bytes.len() {
                        return None;
                    }
                    let b = bytes[pos];
                    if b < 0x80 {
                        if (ascii_bits[(b >> 6) as usize] >> (b & 63)) & 1 != 0
                            && let Some(r) = self.exec_interp(text, pos, &mut memo)
                        {
                            return Some(r);
                        }
                        pos += 1;
                    } else {
                        let ch_len = text[pos..]
                            .chars()
                            .next()
                            .map(|c| c.len_utf8())
                            .unwrap_or(1);
                        if can_match_non_ascii
                            && let Some(r) = self.exec_interp(text, pos, &mut memo)
                        {
                            return Some(r);
                        }
                        pos += ch_len;
                    }
                }
            }
            StartStrategy::RequiredByte { byte, max_offset } => {
                let bytes = text.as_bytes();
                let byte = *byte;
                let max_offset = *max_offset;
                let mut scan_pos = start_pos;
                loop {
                    let found = memchr::memchr(byte, &bytes[scan_pos..])?;
                    let byte_pos = scan_pos + found;
                    let try_from = byte_pos.saturating_sub(max_offset).max(start_pos);
                    for start in try_from..=byte_pos {
                        if let Some(r) = self.exec_interp(text, start, &mut memo) {
                            return Some(r);
                        }
                    }
                    scan_pos = byte_pos + 1;
                }
            }
            StartStrategy::Anywhere => {
                let mut pos = start_pos;
                loop {
                    if let Some(r) = self.exec_interp(text, pos, &mut memo) {
                        return Some(r);
                    }
                    if pos >= text.len() {
                        return None;
                    }
                    pos += text[pos..]
                        .chars()
                        .next()
                        .map(|c| c.len_utf8())
                        .unwrap_or(1);
                }
            }
        }
    }

    /// Run the interpreter at exactly `pos`, bypassing JIT.
    #[cfg(feature = "jit")]
    fn exec_interp(
        &self,
        text: &str,
        pos: usize,
        memo: &mut MemoState,
    ) -> Option<(usize, usize, Vec<Option<usize>>)> {
        let ctx = Ctx {
            prog: &self.prog,
            charsets: &self.charsets,
            match_tries: &self.match_tries,
            alt_tries: &self.alt_tries,
            text,
            search_start: pos,
            use_memo: self.use_memo,
            num_null_checks: self.num_null_checks,
            num_repeat_counters: self.num_repeat_counters,
        };
        let mut state = State::new(self.num_groups, self.num_null_checks);
        let end = exec(&ctx, 0, pos, &mut state, 0, memo)?;
        let match_start = state.keep_pos.unwrap_or(pos);
        state.slots.resize(self.num_groups * 2);
        Some((match_start, end, state.slots.to_vec_options()))
    }
}

// ---------------------------------------------------------------------------
// JIT support types and exec-lookaround wrapper (cfg(feature = "jit") only)
/// Scratch buffers reused across all `exec_jit` calls within a single `find()`.
/// Allocated lazily on first use and reused on subsequent calls, eliminating
/// per-attempt heap allocations for patterns that scan many positions.
#[cfg(feature = "jit")]
pub(crate) struct ExecScratch {
    pub bt: Vec<BtJit>,
    pub slots: Vec<u64>,
    /// Null-check guard slots: one `u64` per loop guard; `u64::MAX` = unset.
    pub null_check: Vec<u64>,
    /// Dense fork-failure memo array, stored as raw parts to avoid the
    /// `mem::take` + `mem::forget` + reconstruct overhead on every exec_jit call.
    ///
    /// Invariant: the three fields are consistent and describe a valid allocation
    /// (or all-zero when no allocation has been made yet).
    pub fork_memo_ptr: *mut u8,
    pub fork_memo_len: usize,
    pub fork_memo_cap: usize,
}

#[cfg(feature = "jit")]
impl ExecScratch {
    pub fn new() -> Self {
        ExecScratch {
            bt: Vec::new(),
            slots: Vec::new(),
            null_check: Vec::new(),
            fork_memo_ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(),
            fork_memo_len: 0,
            fork_memo_cap: 0,
        }
    }
}

#[cfg(feature = "jit")]
impl Drop for ExecScratch {
    fn drop(&mut self) {
        if self.fork_memo_cap > 0 {
            // SAFETY: if cap > 0, the raw parts describe a valid Vec allocation.
            let _ = unsafe {
                Vec::from_raw_parts(self.fork_memo_ptr, self.fork_memo_len, self.fork_memo_cap)
            };
        }
    }
}

/// Zero-sized stub used when the `jit` feature is disabled.
#[cfg(not(feature = "jit"))]
pub(crate) struct ExecScratch;

// SAFETY: ExecScratch owns its allocation exclusively (not shared); the raw
// pointer `fork_memo_ptr` is valid as long as the ExecScratch is alive.
#[cfg(feature = "jit")]
unsafe impl Send for ExecScratch {}
#[cfg(feature = "jit")]
unsafe impl Sync for ExecScratch {}

// ---------------------------------------------------------------------------

/// JIT backtrack stack entry — flat `repr(C)` struct so both Rust helpers and
/// inline Cranelift IR can access fields at stable, known byte offsets.
///
/// Layout (24 bytes):
/// ```text
/// offset 0  : tag  (u32) — 0=Retry, 1=SaveUndo, 2=AtomicBarrier, 3=MemoMark
/// offset 4  : a    (u32) — block_id / slot / fork_block_id
/// offset 8  : b    (u64) — pos / old_value / fork_pos
/// offset 16 : c    (u64) — keep_pos (Retry); unused otherwise
/// ```
#[cfg(feature = "jit")]
#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct BtJit {
    pub tag: u32,
    pub a: u32,
    pub b: u64,
    pub c: u64,
}

#[cfg(feature = "jit")]
impl BtJit {
    pub const TAG_RETRY: u32 = 0;
    pub const TAG_SAVE_UNDO: u32 = 1;
    pub const TAG_ATOMIC_BARRIER: u32 = 2;
    pub const TAG_MEMO_MARK: u32 = 3;

    #[inline(always)]
    pub fn retry(block_id: u32, pos: u64, keep_pos: u64) -> Self {
        Self {
            tag: 0,
            a: block_id,
            b: pos,
            c: keep_pos,
        }
    }
    #[inline(always)]
    pub fn save_undo(slot: u32, old_value: u64) -> Self {
        Self {
            tag: 1,
            a: slot,
            b: old_value,
            c: 0,
        }
    }
    #[inline(always)]
    pub fn atomic_barrier() -> Self {
        Self {
            tag: 2,
            a: 0,
            b: 0,
            c: 0,
        }
    }
    #[inline(always)]
    pub fn memo_mark(fork_block_id: u32, fork_pos: u64) -> Self {
        Self {
            tag: 3,
            a: fork_block_id,
            b: fork_pos,
            c: 0,
        }
    }
}

// Verify layout used by inline JIT IR
#[cfg(feature = "jit")]
const _: () = assert!(std::mem::size_of::<BtJit>() == 24);

/// Context block passed by pointer to the JIT-compiled exec function and to
/// all `extern "C"` helpers.  **Must be `#[repr(C)]`** because the
/// Cranelift-generated code accesses fields at known byte offsets.
#[cfg(feature = "jit")]
#[repr(C)]
pub(crate) struct JitExecCtx {
    // ---- immutable for this exec call --------------------------------
    pub text_ptr: *const u8,
    pub text_len: u64,
    pub search_start: u64,
    pub use_memo: u64,           // 0 or 1, padded to align
    pub charsets_ptr: *const (), // *const [CharSet]
    pub charsets_len: u64,
    pub prog_ptr: *const (), // *const [Inst]  — for lookaround sub-exec
    pub prog_len: u64,
    pub num_groups: u64,
    // ---- mutable capture state ----------------------------------------
    pub slots_ptr: *mut u64, // len = num_groups * 2; u64::MAX = None
    pub slots_len: u64,
    pub keep_pos: u64, // u64::MAX = None
    // ---- backtrack stack & memo (heap-allocated Rust types) -----------
    /// Raw data pointer for the JIT bt stack Vec<BtJit>.
    /// Updated by helpers that may reallocate the Vec.
    pub bt_data_ptr: *mut BtJit,
    /// Current length (number of elements) of the bt stack.
    pub bt_len: u64,
    /// Current capacity (number of elements) of the bt stack.
    pub bt_cap: u64,
    pub memo_ptr: *mut (), // *mut MemoState
    /// 1 when `fork_failures` is non-empty; inline JIT code fast-paths on this.
    pub memo_has_failures: u64,
    // ---- atomic group depth ------------------------------------------
    pub atomic_depth: u64,
    /// Number of `BtJit::Retry` entries currently on the bt stack.
    /// `SaveUndo` pushes are skipped when this is zero (no retry to backtrack to).
    pub bt_retry_count: u64,
    /// Pointer into the dense fork-failure memo array (`Vec<u8>` owned by ExecScratch).
    /// Lent to ctx via mem::forget; rebuilt in exec_jit after the call.
    pub fork_memo_data_ptr: *mut u8,
    /// Current length of the dense memo array (0 until first failure is recorded).
    pub fork_memo_len: u64,
    /// Current capacity of the dense memo array (for safe reconstruction after growth).
    pub fork_memo_cap: u64,
    /// Pointer to the null-check slot array (one `u64` per slot; `u64::MAX` = unset).
    /// Lent from `ExecScratch::null_check`.
    pub null_check_ptr: *mut u64,
    /// Length of the null-check slot array (== `num_null_checks` for the pattern).
    pub null_check_len: u64,
    /// Pointer to the per-charset ByteTrie slice (`*const Option<ByteTrie>`).
    /// Indexed by charset index.  Used by `jit_match_class` to handle
    /// multi-codepoint case folds (e.g. ß → "ss") correctly.
    pub class_tries_ptr: *const (),
    /// Length of the class_tries slice (== number of charsets).
    pub class_tries_len: u64,
    /// Pointer to the alternation ByteTrie slice (`*const ByteTrie`).
    /// Indexed by trie index from `AltTrie`/`AltTrieBack` instructions.
    pub alt_tries_ptr: *const (),
    /// Length of the alt_tries slice.
    pub alt_tries_len: u64,
}

/// Push one entry onto the raw JIT backtrack stack.  May reallocate.
///
/// # Safety
/// `ctx.bt_data_ptr/bt_len/bt_cap` must be consistent and valid.
#[cfg(feature = "jit")]
pub(crate) unsafe fn bt_push(ctx: &mut JitExecCtx, entry: BtJit) {
    let len = ctx.bt_len as usize;
    if len < ctx.bt_cap as usize {
        unsafe { ctx.bt_data_ptr.add(len).write(entry) };
        ctx.bt_len += 1;
    } else {
        // Slow path: grow by reconstructing Vec, pushing, then extracting raw parts.
        let mut v = unsafe { Vec::from_raw_parts(ctx.bt_data_ptr, len, ctx.bt_cap as usize) };
        v.push(entry);
        ctx.bt_data_ptr = v.as_mut_ptr();
        ctx.bt_cap = v.capacity() as u64;
        ctx.bt_len = v.len() as u64;
        std::mem::forget(v);
    }
}

/// Pop one entry from the raw JIT backtrack stack.  Returns `None` if empty.
///
/// # Safety
/// Same as `bt_push`.
#[cfg(feature = "jit")]
pub(crate) unsafe fn bt_pop(ctx: &mut JitExecCtx) -> Option<BtJit> {
    if ctx.bt_len == 0 {
        return None;
    }
    ctx.bt_len -= 1;
    // SAFETY: entry at ctx.bt_len was previously written via bt_push.
    // BtJit contains no heap-owning fields, so no destructor is needed.
    Some(unsafe { ctx.bt_data_ptr.add(ctx.bt_len as usize).read() })
}

/// Run a lookaround body using the **interpreter** from a JIT execution
/// context.  Called by the `jit_lookaround` helper.
///
/// # Safety
/// All pointers inside `ctx` must be valid for the lifetime of this call.
#[cfg(feature = "jit")]
pub(crate) unsafe fn exec_lookaround_for_jit(
    ctx: *mut JitExecCtx,
    lk_pc: usize, // PC of the LookStart instruction (memo key)
    body_pc: usize,
    pos: usize,
    positive: bool,
) -> bool {
    let jctx = unsafe { &mut *ctx };
    let use_memo = jctx.use_memo != 0;
    let memo = unsafe { &mut *(jctx.memo_ptr as *mut MemoState) };
    let lk_key = memo_key(lk_pc, pos);

    // Fast path: check the Algorithm-6 lookaround cache *before* constructing
    // a State (which heap-allocates a Vec for capture slots).  On a cache hit
    // this avoids an allocation entirely, making repeated lookaround evaluation
    // at already-visited (lk_pc, pos) pairs much cheaper.
    if use_memo && let Some(entry) = memo.look_results.get(&lk_key).cloned() {
        return match entry {
            LookCacheEntry::BodyMatched {
                slot_delta,
                keep_pos_delta,
            } => {
                // Apply the captured slot delta directly to jctx.slots_ptr
                // without constructing an intermediate State.
                if positive {
                    let slots_out = unsafe {
                        std::slice::from_raw_parts_mut(jctx.slots_ptr, jctx.slots_len as usize)
                    };
                    if jctx.bt_retry_count > 0 {
                        for &(idx, val) in &slot_delta {
                            if idx < slots_out.len() {
                                let new_u64 = val.map(|v| v as u64).unwrap_or(u64::MAX);
                                if new_u64 != slots_out[idx] {
                                    unsafe {
                                        bt_push(jctx, BtJit::save_undo(idx as u32, slots_out[idx]));
                                    }
                                }
                            }
                        }
                    }
                    for (idx, val) in slot_delta {
                        if idx < slots_out.len() {
                            slots_out[idx] = val.map(|v| v as u64).unwrap_or(u64::MAX);
                        }
                    }
                    if let Some(kp) = keep_pos_delta {
                        jctx.keep_pos = kp.map(|v| v as u64).unwrap_or(u64::MAX);
                    }
                }
                true
            }
            LookCacheEntry::BodyNotMatched => false,
        };
    }

    // Slow path: cache miss (or memoization disabled).
    // Reconstruct the interpreter context and run the body.
    let prog =
        unsafe { std::slice::from_raw_parts(jctx.prog_ptr as *const Inst, jctx.prog_len as usize) };
    let charsets = unsafe {
        std::slice::from_raw_parts(
            jctx.charsets_ptr as *const CharSet,
            jctx.charsets_len as usize,
        )
    };
    let alt_tries_for_interp: &[ByteTrie] = if jctx.alt_tries_len > 0 {
        unsafe {
            std::slice::from_raw_parts(
                jctx.alt_tries_ptr as *const ByteTrie,
                jctx.alt_tries_len as usize,
            )
        }
    } else {
        &[]
    };
    let text = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(
            jctx.text_ptr,
            jctx.text_len as usize,
        ))
    };
    let interp_ctx = Ctx {
        prog,
        charsets,
        // JIT lookaround sub-exec: no match_tries available; fall back to
        // fold_advance / CharSet::matches for any FoldSeq/Class instructions.
        match_tries: &[],
        alt_tries: alt_tries_for_interp,
        text,
        search_start: jctx.search_start as usize,
        use_memo,
        num_null_checks: jctx.null_check_len as usize,
        // JIT-eligible programs never contain RepeatInit/RepeatNext, so 0 is safe.
        num_repeat_counters: 0,
    };

    // Build the sub-execution state directly from jctx rather than going
    // through an intermediate `state` that would be cloned again by
    // `exec_lookaround` — saving one Vec allocation per call.
    let slots_slice =
        unsafe { std::slice::from_raw_parts(jctx.slots_ptr, jctx.slots_len as usize) };
    let mut sub = State {
        slots: {
            let len = jctx.slots_len as usize;
            let mut ss = SmallSlots::new(len);
            for (i, &v) in slots_slice.iter().enumerate() {
                if v != u64::MAX {
                    ss.set(i, v as usize);
                }
            }
            ss
        },
        keep_pos: if jctx.keep_pos == u64::MAX {
            None
        } else {
            Some(jctx.keep_pos as usize)
        },
        call_stack: Vec::new(),
        null_check_slots: vec![(usize::MAX, 0); jctx.null_check_len as usize],
    };

    let matched = if use_memo {
        // Cache miss — run the body and store the result for future hits.
        let pre_slots = sub.slots.clone();
        let pre_keep = sub.keep_pos;
        let m = exec(&interp_ctx, body_pc, pos, &mut sub, 1, memo).is_some();
        let entry = if m {
            let slot_delta = compute_slot_delta(&pre_slots, &sub.slots);
            let keep_pos_delta = if sub.keep_pos != pre_keep {
                Some(sub.keep_pos)
            } else {
                None
            };
            LookCacheEntry::BodyMatched {
                slot_delta,
                keep_pos_delta,
            }
        } else {
            LookCacheEntry::BodyNotMatched
        };
        memo.look_results.insert(lk_key, entry);
        m
    } else {
        exec(&interp_ctx, body_pc, pos, &mut sub, 1, memo).is_some()
    };

    // On success, propagate state changes back into the JIT context.
    // Push SaveUndo entries for any slot the lookaround body modified so that
    // backtracking past this point correctly reverts the slot changes.
    if matched {
        let slots_out =
            unsafe { std::slice::from_raw_parts_mut(jctx.slots_ptr, jctx.slots_len as usize) };
        if jctx.bt_retry_count > 0 {
            for i in 0..sub.slots.len() {
                let s = sub.slots.get(i);
                if i < slots_out.len() {
                    let new_val = s.map(|v| v as u64).unwrap_or(u64::MAX);
                    if new_val != slots_out[i] {
                        unsafe {
                            bt_push(jctx, BtJit::save_undo(i as u32, slots_out[i]));
                        }
                    }
                }
            }
        }
        for i in 0..sub.slots.len() {
            let s = sub.slots.get(i);
            if i < slots_out.len() {
                slots_out[i] = s.map(|v| v as u64).unwrap_or(u64::MAX);
            }
        }
        jctx.keep_pos = sub.keep_pos.map(|v| v as u64).unwrap_or(u64::MAX);
    }

    matched
}
