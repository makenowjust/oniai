//! Compiler: transforms a parsed AST into a VM instruction sequence.

use crate::ast::*;
use crate::bytetrie::ByteTrie;
use crate::casefold::{CaseFold, case_fold};
use crate::charset;
use crate::data::casefold_data::SIMPLE_CASE_FOLDS;
use crate::error::Error;
use crate::vm::{CharSet, Inst};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Nullable analysis
// ---------------------------------------------------------------------------

/// Returns `true` if `node` can match the empty string.
/// Used to decide whether unbounded loops need a null-check guard.
fn can_match_empty(node: &Node) -> bool {
    match node {
        Node::Empty => true,
        Node::Literal(_) | Node::AnyChar | Node::CharClass(_) | Node::Shorthand(_) => false,
        Node::UnicodeProp { .. } => false,
        Node::Anchor(_) | Node::Keep => true,
        Node::LookAround { .. } => true, // zero-width
        Node::Concat(nodes) => nodes.iter().all(can_match_empty),
        Node::Alternation(nodes) => nodes.iter().any(can_match_empty),
        Node::Quantifier { node, range, .. } => range.min == 0 || can_match_empty(node),
        Node::Capture { node, .. }
        | Node::NamedCapture { node, .. }
        | Node::Group { node, .. }
        | Node::Atomic(node)
        | Node::InlineFlags { node, .. } => can_match_empty(node),
        // (?~X) matches any string that does NOT contain X.
        // The empty string never contains X unless X itself can match empty,
        // so the absent operator can produce an empty match iff X cannot.
        Node::Absence(node) => !can_match_empty(node),
        // Conservative: backrefs and calls may match empty.
        Node::BackRef { .. } | Node::SubexpCall(_) => true,
        Node::Conditional { yes, no, .. } => can_match_empty(yes) || can_match_empty(no),
    }
}

/// If `node` is a pure sequence of literal characters (no captures, no
/// quantifiers, no case-folding), return the corresponding `String`.
/// Used to detect alternations eligible for the `AltTrie` optimization.
fn try_extract_plain_string(node: &Node) -> Option<String> {
    match node {
        Node::Literal(c) => Some(c.to_string()),
        Node::Concat(nodes) => {
            let mut s = String::new();
            for n in nodes {
                if let Node::Literal(c) = n {
                    s.push(*c);
                } else {
                    return None;
                }
            }
            Some(s)
        }
        _ => None,
    }
}

/// Maximum total number of case-fold variant strings before we give up
/// and fall back to the Fork chain for case-insensitive alternation.
const MAX_FOLD_VARIANTS: usize = 1024;

/// Return all chars whose simple case fold equals `target`.
///
/// Includes `target` itself (it folds to itself) and every source char in
/// `SIMPLE_CASE_FOLDS` that maps to `target`.
fn chars_folding_to(target: char) -> Vec<char> {
    let mut out = vec![target];
    for &(src, folded) in SIMPLE_CASE_FOLDS {
        if folded == target && src != target {
            out.push(src);
        }
    }
    out
}

/// For each alternative string in `alts`, enumerate all byte strings that
/// are case-fold-equivalent under Unicode single-codepoint folding.
///
/// Returns `None` if:
/// - any char in any alternative has a multi-codepoint fold (e.g. ß → ss),
/// - or the total variant count exceeds [`MAX_FOLD_VARIANTS`].
///
/// The returned list contains the original strings plus all variants; the
/// caller is responsible for checking `is_prefix_free` on the result.
fn expand_alts_case_folded(alts: &[String]) -> Option<Vec<String>> {
    let mut all_variants: Vec<String> = Vec::new();
    for s in alts {
        // Build a list of char alternatives for each position.
        let mut per_char: Vec<Vec<char>> = Vec::new();
        for c in s.chars() {
            let fold_target = match case_fold(c) {
                CaseFold::Single(f) => f,
                CaseFold::Multi(_) => return None, // e.g. ß → ss
            };
            per_char.push(chars_folding_to(fold_target));
        }
        // Count combinations; bail early if over the cap.
        let n_variants: usize = per_char.iter().map(|v| v.len()).product();
        if all_variants.len() + n_variants > MAX_FOLD_VARIANTS {
            return None;
        }
        // Enumerate all combinations via rolling expansion.
        let mut variants = vec![String::new()];
        for char_alts in &per_char {
            let mut next = Vec::with_capacity(variants.len() * char_alts.len());
            for base in &variants {
                for &c in char_alts {
                    let mut v = base.clone();
                    v.push(c);
                    next.push(v);
                }
            }
            variants = next;
        }
        all_variants.extend(variants);
    }
    Some(all_variants)
}

// ---------------------------------------------------------------------------
// Options passed to the compiler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    /// ONIG_OPTION_IGNORE_CASE
    pub ignore_case: bool,
    /// ONIG_OPTION_MULTILINE (Ruby (?m): dot matches newline)
    pub multiline: bool,
}

// ---------------------------------------------------------------------------
// Compiler context
// ---------------------------------------------------------------------------

struct Compiler {
    prog: Vec<Inst>,
    charsets: Vec<CharSet>,
    /// Tries for `AltTrie`/`AltTrieBack` instructions: each entry is the
    /// pre-built ByteTrie for a pure-string alternation detected at compile time.
    alt_tries: Vec<ByteTrie>,
    /// Maps 1-based group index → start PC (for subexpression calls)
    subexp_starts: HashMap<u32, usize>,
    /// Groups that need their start PCs backfilled after compilation.
    /// Tuple: (call instruction PC, target GroupRef, current_group at call site).
    pending_calls: Vec<(usize, GroupRef, u32)>,
    named_groups: Vec<(String, u32)>,
    #[allow(dead_code)]
    base_flags: Flags,
    /// Total number of capture groups (1-based max)
    num_groups: u32,
    /// 1-based index of the innermost capture group being compiled (0 = top-level).
    /// Used to resolve relative backreferences (`\k<-n>`) and subexpression calls
    /// (`\g<-n>`, `\g<+n>`) at compile time.
    current_group: u32,
    /// Number of null-check guard slots allocated so far (one per unbounded loop).
    num_null_checks: u32,
    /// Number of repeat counter slots allocated so far (one per counter-based exact loop).
    num_repeat_counters: u32,
}

impl Compiler {
    fn new(base_flags: Flags, named_groups: Vec<(String, u32)>) -> Self {
        Compiler {
            prog: Vec::new(),
            charsets: Vec::new(),
            alt_tries: Vec::new(),
            subexp_starts: HashMap::new(),
            pending_calls: Vec::new(),
            named_groups,
            base_flags,
            num_groups: 0,
            current_group: 0,
            num_null_checks: 0,
            num_repeat_counters: 0,
        }
    }

    fn emit(&mut self, inst: Inst) -> usize {
        let pc = self.prog.len();
        self.prog.push(inst);
        pc
    }

    fn pc(&self) -> usize {
        self.prog.len()
    }

    /// Allocate a fresh null-check guard slot and return its index.
    fn alloc_null_check(&mut self) -> usize {
        let slot = self.num_null_checks as usize;
        self.num_null_checks += 1;
        slot
    }

    /// Allocate a fresh repeat-counter slot and return its index.
    fn alloc_repeat_counter(&mut self) -> usize {
        let slot = self.num_repeat_counters as usize;
        self.num_repeat_counters += 1;
        slot
    }
    fn patch_jump(&mut self, pc: usize, target: usize) {
        match &mut self.prog[pc] {
            Inst::Jump(t) => *t = target,
            Inst::Fork(t, _) => *t = target,
            Inst::ForkNext(t, _) => *t = target,
            Inst::AtomicStart(t) => *t = target,
            Inst::LookStart { end_pc, .. } => *end_pc = target,
            Inst::AbsenceStart(t) => *t = target,
            Inst::CheckGroup { yes_pc, .. } => *yes_pc = target,
            other => panic!("patch_jump called on non-jump inst {:?}", other),
        }
    }

    fn patch_no_jump(&mut self, pc: usize, target: usize) {
        match &mut self.prog[pc] {
            Inst::CheckGroup { no_pc, .. } => *no_pc = target,
            other => panic!("patch_no_jump called on {:?}", other),
        }
    }

    fn add_charset(&mut self, cs: CharSet) -> usize {
        let idx = self.charsets.len();
        self.charsets.push(cs);
        idx
    }

    // -----------------------------------------------------------------------
    // Compile a node with current flags
    // -----------------------------------------------------------------------

    fn compile_node(&mut self, node: &Node, flags: Flags) -> Result<(), Error> {
        self.compile_node_inner(node, flags, false)
    }

    fn compile_node_inner(
        &mut self,
        node: &Node,
        flags: Flags,
        backward: bool,
    ) -> Result<(), Error> {
        let ic = flags.ignore_case;
        match node {
            Node::Empty => {}

            Node::Literal(c) => {
                if ic {
                    let folded: Vec<char> = case_fold(*c).chars().to_vec();
                    if backward {
                        self.emit(Inst::FoldSeqBack(folded));
                    } else {
                        self.emit(Inst::FoldSeq(folded));
                    }
                } else if backward {
                    self.emit(Inst::CharBack(*c));
                } else {
                    self.emit(Inst::Char(*c));
                }
            }

            Node::AnyChar => {
                if backward {
                    self.emit(Inst::AnyCharBack(flags.multiline));
                } else {
                    self.emit(Inst::AnyChar(flags.multiline));
                }
            }

            Node::Shorthand(sh) => {
                let cs = shorthand_charset(*sh, flags.ascii_range, ic);
                let idx = self.add_charset(cs);
                if backward {
                    self.emit(Inst::ClassBack(idx, ic));
                } else {
                    self.emit(Inst::Class(idx, ic));
                }
            }

            Node::UnicodeProp { name, negate } => {
                let cs = unicode_prop_charset(name, *negate, ic)?;
                let idx = self.add_charset(cs);
                if backward {
                    self.emit(Inst::ClassBack(idx, ic));
                } else {
                    self.emit(Inst::Class(idx, ic));
                }
            }

            Node::Anchor(kind) => {
                self.emit(Inst::Anchor(*kind, flags));
            }

            Node::CharClass(cc) => {
                let cs = compile_charset(cc, ic, flags.ascii_range)?;
                let idx = self.add_charset(cs);
                if backward {
                    self.emit(Inst::ClassBack(idx, ic));
                } else {
                    self.emit(Inst::Class(idx, ic));
                }
            }

            Node::Concat(nodes) => {
                if ic {
                    // Merge consecutive case-insensitive literals into a single
                    // FoldSeq/FoldSeqBack instruction to handle multi-codepoint
                    // folds (e.g. ß ↔ ss).
                    let mut fold_accum: Vec<char> = Vec::new();
                    let iter: Box<dyn Iterator<Item = &Node>> = if backward {
                        Box::new(nodes.iter().rev())
                    } else {
                        Box::new(nodes.iter())
                    };
                    for child in iter {
                        if let Node::Literal(c) = child {
                            fold_accum.extend(case_fold(*c).chars().iter().copied());
                            continue;
                        }
                        if !fold_accum.is_empty() {
                            let folded = std::mem::take(&mut fold_accum);
                            if backward {
                                self.emit(Inst::FoldSeqBack(folded));
                            } else {
                                self.emit(Inst::FoldSeq(folded));
                            }
                        }
                        self.compile_node_inner(child, flags, backward)?;
                    }
                    if !fold_accum.is_empty() {
                        if backward {
                            self.emit(Inst::FoldSeqBack(fold_accum));
                        } else {
                            self.emit(Inst::FoldSeq(fold_accum));
                        }
                    }
                } else if backward {
                    for n in nodes.iter().rev() {
                        self.compile_node_inner(n, flags, backward)?;
                    }
                } else {
                    for n in nodes {
                        self.compile_node_inner(n, flags, backward)?;
                    }
                }
            }

            Node::Alternation(alts) => {
                self.compile_alternation_inner(alts, flags, backward)?;
            }

            Node::Quantifier { node, range, kind } => {
                self.compile_quantifier_inner(node, range, kind, flags, backward)?;
            }

            Node::Capture {
                index,
                node,
                flags: inner_flags,
            } => {
                let f = flags.apply_on(&FlagMod {
                    on: *inner_flags,
                    off: Flags::default(),
                });
                let idx = *index;
                if idx > self.num_groups {
                    self.num_groups = idx;
                }
                let prev_group = self.current_group;
                self.current_group = idx;
                let start_pc = self.pc();
                self.subexp_starts.insert(idx, start_pc);
                let slot_open = ((idx - 1) * 2) as usize;
                let slot_close = slot_open + 1;
                if backward {
                    self.emit(Inst::Save(slot_close));
                    self.compile_node_inner(node, f, backward)?;
                    self.emit(Inst::Save(slot_open));
                } else {
                    self.emit(Inst::Save(slot_open));
                    self.compile_node_inner(node, f, backward)?;
                    self.emit(Inst::Save(slot_close));
                }
                self.emit(Inst::RetIfCalled);
                self.current_group = prev_group;
            }

            Node::NamedCapture {
                name: _,
                index,
                node,
                flags: inner_flags,
            } => {
                let f = flags.apply_on(&FlagMod {
                    on: *inner_flags,
                    off: Flags::default(),
                });
                let idx = *index;
                if idx > self.num_groups {
                    self.num_groups = idx;
                }
                let prev_group = self.current_group;
                self.current_group = idx;
                let start_pc = self.pc();
                self.subexp_starts.insert(idx, start_pc);
                let slot_open = ((idx - 1) * 2) as usize;
                let slot_close = slot_open + 1;
                if backward {
                    self.emit(Inst::Save(slot_close));
                    self.compile_node_inner(node, f, backward)?;
                    self.emit(Inst::Save(slot_open));
                } else {
                    self.emit(Inst::Save(slot_open));
                    self.compile_node_inner(node, f, backward)?;
                    self.emit(Inst::Save(slot_close));
                }
                self.emit(Inst::RetIfCalled);
                self.current_group = prev_group;
            }

            Node::Group {
                node,
                flags: inner_flags,
            } => {
                let f = flags.apply_on(&FlagMod {
                    on: *inner_flags,
                    off: Flags::default(),
                });
                self.compile_node_inner(node, f, backward)?;
            }

            Node::Atomic(node) => {
                let atomic_start = self.emit(Inst::AtomicStart(0));
                self.compile_node_inner(node, flags, backward)?;
                let end_pc = self.pc();
                self.emit(Inst::AtomicEnd);
                self.patch_jump(atomic_start, end_pc);
            }

            Node::LookAround { dir, pol, node } => {
                let positive = *pol == LookPol::Positive;
                let inner_backward = *dir == LookDir::Behind;
                let look_start = self.emit(Inst::LookStart {
                    positive,
                    end_pc: 0,
                });
                self.compile_node_inner(node, flags, inner_backward)?;
                let end_pc = self.pc();
                self.emit(Inst::LookEnd);
                self.patch_jump(look_start, end_pc);
            }

            Node::Keep => {
                self.emit(Inst::KeepStart);
            }

            Node::BackRef { target, level } => {
                let ignore_case = flags.ignore_case;
                let inst = match target {
                    GroupRef::Index(n) => Inst::BackRef(*n, ignore_case, *level),
                    GroupRef::Name(name) => {
                        let idx = self.resolve_name(name)?;
                        Inst::BackRef(idx, ignore_case, *level)
                    }
                    GroupRef::RelativeBack(n) => {
                        let abs = self.current_group.checked_sub(*n).filter(|&x| x >= 1);
                        match abs {
                            Some(idx) => Inst::BackRef(idx, ignore_case, *level),
                            None => {
                                return Err(Error::Compile(format!(
                                    "relative backreference \\k<-{}> out of range (current group {})",
                                    n, self.current_group
                                )));
                            }
                        }
                    }
                    GroupRef::RelativeFwd(_) => {
                        return Err(Error::Compile(
                            "relative-forward backreference not supported".into(),
                        ));
                    }
                    GroupRef::Whole => {
                        return Err(Error::Compile(
                            "\\k<0> backreference to whole pattern not supported".into(),
                        ));
                    }
                };
                self.emit(inst);
            }

            Node::SubexpCall(target) => {
                let call_pc = self.emit(Inst::Call(0));
                self.pending_calls
                    .push((call_pc, target.clone(), self.current_group));
            }

            Node::InlineFlags {
                flags: flag_mod,
                node,
            } => {
                let new_flags = flags.apply_on(flag_mod);
                self.compile_node_inner(node, new_flags, backward)?;
            }

            Node::Absence(inner) => {
                self.compile_absence(inner, flags)?;
            }

            Node::Conditional { cond, yes, no } => {
                self.compile_conditional(cond, yes, no, flags)?;
            }
        }
        Ok(())
    }

    fn resolve_name(&self, name: &str) -> Result<u32, Error> {
        // Find last group with this name (Onigmo semantics)
        self.named_groups
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, idx)| *idx)
            .ok_or_else(|| Error::Compile(format!("undefined group name {:?}", name)))
    }

    // -----------------------------------------------------------------------
    // Alternation
    // -----------------------------------------------------------------------

    #[allow(dead_code)]
    fn compile_alternation(&mut self, alts: &[Node], flags: Flags) -> Result<(), Error> {
        self.compile_alternation_inner(alts, flags, false)
    }

    fn compile_alternation_inner(
        &mut self,
        alts: &[Node],
        flags: Flags,
        backward: bool,
    ) -> Result<(), Error> {
        if alts.is_empty() {
            return Ok(());
        }
        if alts.len() == 1 {
            return self.compile_node_inner(&alts[0], flags, backward);
        }

        // Optimization: if all alternatives are plain literal strings and the
        // string set (after case-folding when ic=true) is prefix-free, emit a
        // single AltTrie/AltTrieBack instruction instead of a Fork chain.
        // Prefix-free is required so that the deterministic (longest-match)
        // trie produces the same result as the leftmost-wins Fork chain.
        {
            let strings: Option<Vec<String>> = alts.iter().map(try_extract_plain_string).collect();
            if let Some(strings) = strings {
                // For case-insensitive patterns expand to all fold variants first.
                let trie_strings = if flags.ignore_case {
                    expand_alts_case_folded(&strings)
                } else {
                    Some(strings)
                };
                if let Some(trie_strings) = trie_strings {
                    let mut trie = ByteTrie::new();
                    for s in &trie_strings {
                        trie.insert(s.as_bytes());
                    }
                    if trie.is_prefix_free() {
                        let idx = self.alt_tries.len();
                        if backward {
                            self.alt_tries.push(trie.reversed());
                            self.emit(Inst::AltTrieBack(idx));
                        } else {
                            self.alt_tries.push(trie);
                            self.emit(Inst::AltTrie(idx));
                        }
                        return Ok(());
                    }
                }
            }
        }

        // For N alternatives: emit Fork chain
        let mut fork_pcs = Vec::new();
        let mut jump_pcs = Vec::new();

        for (i, alt) in alts.iter().enumerate() {
            if i < alts.len() - 1 {
                // Fork: try next alternative if this one fails
                let guard = if backward {
                    None
                } else {
                    first_literal_of_node(alt, flags.ignore_case)
                };
                let fork_pc = self.emit(Inst::Fork(0, guard)); // patch to next alt
                fork_pcs.push(fork_pc);
            }
            self.compile_node_inner(alt, flags, backward)?;
            if i < alts.len() - 1 {
                let jump_pc = self.emit(Inst::Jump(0)); // patch to after all alts
                jump_pcs.push(jump_pc);
                // Patch the fork to point to just after the Jump (= start of next alt)
                let next_alt_pc = self.pc();
                self.patch_jump(*fork_pcs.last().unwrap(), next_alt_pc);
            }
        }
        // Patch all jumps to after everything
        let end_pc = self.pc();
        for jpc in jump_pcs {
            self.patch_jump(jpc, end_pc);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Quantifiers
    // -----------------------------------------------------------------------

    #[allow(dead_code)]
    fn compile_quantifier(
        &mut self,
        node: &Node,
        range: &QuantRange,
        kind: &QuantKind,
        flags: Flags,
    ) -> Result<(), Error> {
        self.compile_quantifier_inner(node, range, kind, flags, false)
    }

    fn compile_quantifier_inner(
        &mut self,
        node: &Node,
        range: &QuantRange,
        kind: &QuantKind,
        flags: Flags,
        backward: bool,
    ) -> Result<(), Error> {
        let min = range.min;
        let max = range.max;

        // Threshold above which we use a counter loop instead of duplicating the body.
        // Below the threshold the overhead of RepeatInit/RepeatNext is not worth it.
        const REPEAT_COUNTER_THRESHOLD: u32 = 4;

        // Emit `min` mandatory copies (or a counter loop for large min).
        if min >= REPEAT_COUNTER_THRESHOLD {
            let slot = self.alloc_repeat_counter();
            self.emit(Inst::RepeatInit { slot });
            let body_pc = self.pc();
            self.compile_node_inner(node, flags, backward)?;
            self.emit(Inst::RepeatNext {
                slot,
                count: min,
                body_pc,
            });
        } else {
            for _ in 0..min {
                self.compile_node_inner(node, flags, backward)?;
            }
        }

        match (max, kind) {
            // {n} — exactly n
            (Some(m), _) if m == min => {} // done above

            // Greedy / reluctant {n,}
            (None, QuantKind::Greedy) => {
                // Only emit null-check guards when the body can produce a zero-length match.
                // Bodies like `[a-z]` always advance; adding guards would be pure overhead.
                if can_match_empty(node) {
                    // Layout: NullCheckStart, Fork(exit), body, NullCheckEnd{exit}, Jump(NullCheckStart)
                    let slot = self.alloc_null_check();
                    let null_check_start_pc = self.pc();
                    self.emit(Inst::NullCheckStart(slot));
                    let fork_pc = self.pc();
                    let guard = if backward {
                        None
                    } else {
                        first_literal_of_node(node, flags.ignore_case)
                    };
                    self.emit(Inst::Fork(0, guard)); // patched to exit_pc below
                    self.compile_node_inner(node, flags, backward)?;
                    // exit_pc is the instruction after NullCheckEnd + Jump
                    let exit_pc = self.pc() + 2;
                    self.emit(Inst::NullCheckEnd { slot, exit_pc });
                    self.emit(Inst::Jump(null_check_start_pc));
                    // self.pc() == exit_pc now
                    self.patch_jump(fork_pc, exit_pc);
                } else {
                    // Simple loop: Fork(exit), body, Jump(Fork)
                    let guard = if backward {
                        None
                    } else {
                        first_literal_of_node(node, flags.ignore_case)
                    };
                    let fork_pc = self.emit(Inst::Fork(0, guard));
                    self.compile_node_inner(node, flags, backward)?;
                    self.emit(Inst::Jump(fork_pc));
                    self.patch_jump(fork_pc, self.pc());
                }
            }
            (None, QuantKind::Reluctant) => {
                if can_match_empty(node) {
                    // Layout: NullCheckStart, ForkNext(exit), body, NullCheckEnd{exit}, Jump(NullCheckStart)
                    // ForkNext(exit): try exit first (lazy), retry = body (pc+1)
                    let slot = self.alloc_null_check();
                    let null_check_start_pc = self.pc();
                    self.emit(Inst::NullCheckStart(slot));
                    let fork_pc = self.pc();
                    self.emit(Inst::ForkNext(0, None)); // patched to exit_pc below
                    self.compile_node_inner(node, flags, backward)?;
                    let exit_pc = self.pc() + 2;
                    self.emit(Inst::NullCheckEnd { slot, exit_pc });
                    self.emit(Inst::Jump(null_check_start_pc));
                    self.patch_jump(fork_pc, exit_pc);
                } else {
                    // Simple lazy loop: ForkNext(exit), body, Jump(ForkNext)
                    let fork_pc = self.emit(Inst::ForkNext(0, None));
                    self.compile_node_inner(node, flags, backward)?;
                    self.emit(Inst::Jump(fork_pc));
                    self.patch_jump(fork_pc, self.pc());
                }
            }
            (None, QuantKind::Possessive) => {
                if can_match_empty(node) {
                    // Layout: AtomicStart, NullCheckStart, Fork(loop_end), body,
                    //         NullCheckEnd{loop_end}, Jump(NullCheckStart), AtomicEnd
                    let slot = self.alloc_null_check();
                    let atomic_start = self.emit(Inst::AtomicStart(0)); // patched below
                    let null_check_start_pc = self.pc();
                    self.emit(Inst::NullCheckStart(slot));
                    let fork_pc = self.pc();
                    let guard = if backward {
                        None
                    } else {
                        first_literal_of_node(node, flags.ignore_case)
                    };
                    self.emit(Inst::Fork(0, guard)); // patched to loop_end below
                    self.compile_node_inner(node, flags, backward)?;
                    // loop_end is the pc of AtomicEnd (after NullCheckEnd + Jump)
                    let loop_end = self.pc() + 2;
                    self.emit(Inst::NullCheckEnd {
                        slot,
                        exit_pc: loop_end,
                    });
                    self.emit(Inst::Jump(null_check_start_pc));
                    // self.pc() == loop_end
                    self.emit(Inst::AtomicEnd);
                    self.patch_jump(fork_pc, loop_end);
                    self.patch_jump(atomic_start, loop_end);
                } else {
                    // Simple atomic loop: AtomicStart, Fork(loop_end), body, Jump(Fork), AtomicEnd
                    let atomic_start = self.emit(Inst::AtomicStart(0));
                    let guard = if backward {
                        None
                    } else {
                        first_literal_of_node(node, flags.ignore_case)
                    };
                    let fork_pc = self.emit(Inst::Fork(0, guard));
                    self.compile_node_inner(node, flags, backward)?;
                    self.emit(Inst::Jump(fork_pc));
                    let loop_end = self.pc();
                    self.emit(Inst::AtomicEnd);
                    self.patch_jump(fork_pc, loop_end);
                    self.patch_jump(atomic_start, loop_end);
                }
            }

            // Greedy {n,m}
            (Some(m), QuantKind::Greedy) => {
                let extra = m - min;
                let fork_pcs: Vec<usize> =
                    (0..extra).map(|_| self.emit(Inst::Fork(0, None))).collect();
                // We need to interleave: Fork, body, Fork, body, ...
                // But we've emitted all forks first which is wrong.
                // Redo: emit each fork then body
                // Let me rewrite this properly.
                // Actually I jumped the gun. The prog already has the mandatory iterations.
                // For {n,m}: after n mandatory, emit (m-n) optional ones:
                //   Fork(exit), body, Fork(exit), body, ..., Fork(exit), body
                // Each Fork jumps to the position after all optional iterations.
                // But we need to emit them in sequence.
                // Since I already started emitting fork_pcs (which is wrong), let me fix:
                // Remove the emitted Fork instructions and redo.
                // Actually we haven't emitted them yet (the code above is in the `map` closure
                // but only executed when we call collect). Wait, we DID call collect. Oops.
                // This code is wrong. Let me restructure.
                // The issue: I collect all fork PCs first, then need to interleave bodies.
                // Instead, remove those fork pcs and redo:
                let _ = fork_pcs; // ignore incorrectly emitted forks
                // Note: the forks we just emitted are wrong (they're all grouped together
                // with no bodies between them). We need to truncate and redo.
                // This is a design mistake. Let me truncate and redo properly.
                // ... we can't easily truncate. Let me just not use this approach.
                // The fix: don't pre-allocate fork_pcs. Instead compile iteratively.
                // Since this branch is already wrong, let's replace it entirely.
                // I'll fall through to a different implementation.
                //
                // REAL FIX: This needs to be rewritten. See below.
                // For now, emit a hacky version: since we already emitted `extra` Fork(0)
                // instructions, we need to fill them in. Let's insert bodies between them...
                // This is too complex to fix inline. I'll write a separate helper.
                return self
                    .compile_counted_optional(node, min, m, kind, flags, backward, &fork_pcs);
            }

            (Some(m), QuantKind::Reluctant) => {
                for _ in min..m {
                    let fork_pc = self.emit(Inst::ForkNext(0, None));
                    self.compile_node_inner(node, flags, backward)?;
                    let after = self.pc();
                    self.patch_jump(fork_pc, after);
                }
            }

            (Some(m), QuantKind::Possessive) => {
                let extra = m - min;
                // Atomic wrapper around optional iterations
                let atomic_start = self.emit(Inst::AtomicStart(0));
                for _ in 0..extra {
                    let guard = if backward {
                        None
                    } else {
                        first_literal_of_node(node, flags.ignore_case)
                    };
                    let fork_pc = self.emit(Inst::Fork(0, guard));
                    self.compile_node_inner(node, flags, backward)?;
                    let after = self.pc();
                    self.patch_jump(fork_pc, after); // hmm, all fork to same exit
                }
                let atom_end = self.pc();
                self.emit(Inst::AtomicEnd);
                self.patch_jump(atomic_start, atom_end);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_counted_optional(
        &mut self,
        node: &Node,
        min: u32,
        max: u32,
        _kind: &QuantKind,
        flags: Flags,
        backward: bool,
        pre_emitted_forks: &[usize],
    ) -> Result<(), Error> {
        // Remove pre-emitted (wrong) Fork instructions by truncating
        let truncate_to = if pre_emitted_forks.is_empty() {
            self.prog.len()
        } else {
            pre_emitted_forks[0]
        };
        self.prog.truncate(truncate_to);

        let extra = max - min;
        // All optional Fork instructions need to point to the same "exit" location.
        // We'll collect their pcs and patch them all at the end.
        let mut fork_pcs = Vec::new();
        for _ in 0..extra {
            let guard = if backward {
                None
            } else {
                first_literal_of_node(node, flags.ignore_case)
            };
            let fp = self.emit(Inst::Fork(0, guard));
            fork_pcs.push(fp);
            self.compile_node_inner(node, flags, backward)?;
        }
        let exit_pc = self.pc();
        for fp in fork_pcs {
            self.patch_jump(fp, exit_pc);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Absence operator (?~X)
    // -----------------------------------------------------------------------

    fn compile_absence(&mut self, inner: &Node, flags: Flags) -> Result<(), Error> {
        // Compile inner pattern as a sub-program stored inline.
        // Structure:
        //   AbsenceStart(inner_end_pc)  ← absence instruction; starts the greedy loop
        //   [inner pattern]             ← sub-program for "what to avoid"
        //   AbsenceEnd                  ← terminates inner (like Match for inner exec)
        // The VM interprets AbsenceStart by running the greedy loop.
        let absence_pc = self.emit(Inst::AbsenceStart(0)); // patch inner_end
        self.compile_node(inner, flags)?;
        let inner_end = self.pc();
        self.emit(Inst::AbsenceEnd);
        self.patch_jump(absence_pc, inner_end);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Conditional group
    // -----------------------------------------------------------------------

    fn compile_conditional(
        &mut self,
        cond: &Condition,
        yes: &Node,
        no: &Node,
        flags: Flags,
    ) -> Result<(), Error> {
        // CheckGroup(slot_pair, yes_pc, no_pc)
        // slot_pair: 0-based index into groups (group_num - 1) * 2
        let slot_pair = match cond {
            Condition::GroupNum(n) => ((*n - 1) * 2) as usize,
            Condition::GroupName(name) => {
                let idx = self.resolve_name(name).unwrap_or(1);
                ((idx - 1) * 2) as usize
            }
        };
        let check_pc = self.emit(Inst::CheckGroup {
            slot: slot_pair,
            yes_pc: 0,
            no_pc: 0,
        });
        // yes branch starts here
        let yes_pc = self.pc();
        self.compile_node(yes, flags)?;
        let jump_pc = self.emit(Inst::Jump(0));
        // no branch
        let no_pc = self.pc();
        self.compile_node(no, flags)?;
        let end_pc = self.pc();
        // patch
        self.patch_jump(check_pc, yes_pc);
        self.patch_no_jump(check_pc, no_pc);
        self.patch_jump(jump_pc, end_pc);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Backfill subexpression calls
    // -----------------------------------------------------------------------

    fn backfill_calls(&mut self) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending_calls);
        for (call_pc, target, call_group) in pending {
            let target_pc = match &target {
                GroupRef::Index(n) => self.subexp_starts.get(n).copied().ok_or_else(|| {
                    Error::Compile(format!("undefined group {} for subexpr call", n))
                })?,
                GroupRef::Name(name) => {
                    let idx = self
                        .named_groups
                        .iter()
                        .rev()
                        .find(|(n, _)| n == name)
                        .map(|(_, i)| *i)
                        .ok_or_else(|| {
                            Error::Compile(format!("undefined group name {:?}", name))
                        })?;
                    self.subexp_starts.get(&idx).copied().ok_or_else(|| {
                        Error::Compile(format!("group {:?} has no start PC", name))
                    })?
                }
                GroupRef::Whole => 0, // whole pattern starts at 0
                GroupRef::RelativeBack(n) => {
                    let abs = call_group.checked_sub(*n).filter(|&x| x >= 1);
                    let idx = abs.ok_or_else(|| {
                        Error::Compile(format!(
                            "relative subexpr call \\g<-{}> out of range (current group {})",
                            n, call_group
                        ))
                    })?;
                    self.subexp_starts.get(&idx).copied().ok_or_else(|| {
                        Error::Compile(format!("group {} (from \\g<-{}>) has no start PC", idx, n))
                    })?
                }
                GroupRef::RelativeFwd(n) => {
                    let idx = call_group + n;
                    self.subexp_starts.get(&idx).copied().ok_or_else(|| {
                        Error::Compile(format!(
                            "group {} (from \\g<+{}>) is undefined or not yet compiled",
                            idx, n
                        ))
                    })?
                }
            };
            match &mut self.prog[call_pc] {
                Inst::Call(t) => *t = target_pc,
                _ => panic!("expected Call at {}", call_pc),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CharSet construction from AST CharClass
// ---------------------------------------------------------------------------

/// Sort and merge overlapping or adjacent `(char, char)` ranges.
pub fn merge_ranges(mut v: Vec<(char, char)>) -> Vec<(char, char)> {
    if v.is_empty() {
        return v;
    }
    v.sort_unstable_by_key(|&(lo, _)| lo as u32);
    let mut merged: Vec<(char, char)> = Vec::with_capacity(v.len());
    for (lo, hi) in v {
        if let Some(last) = merged.last_mut() {
            // Merge if overlapping or adjacent.
            let next = char::from_u32(last.1 as u32 + 1);
            if lo <= last.1 || next == Some(lo) {
                if hi > last.1 {
                    last.1 = hi;
                }
                continue;
            }
        }
        merged.push((lo, hi));
    }
    merged
}

/// Return `true` when `ch` is covered by a sorted, merged range list.
fn char_in_ranges(ch: char, ranges: &[(char, char)]) -> bool {
    ranges
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
}

/// Expand `ranges` to include all single-codepoint simple-case-fold equivalents.
///
/// Uses `SIMPLE_CASE_FOLDS` to find every codepoint whose fold target is already
/// "touched" by `ranges`, then merges everything together.  Multi-codepoint full
/// case folds (e.g. ß → "ss") are NOT added here; they are handled at match time
/// via the ByteTrie.
pub fn expand_case_folds(ranges: Vec<(char, char)>) -> Vec<(char, char)> {
    // Collect the canonical (fold-target) values that are "touched" by ranges:
    // either the src char is in ranges, or the dst (canonical) char is in ranges.
    let mut touched: Vec<char> = Vec::new();
    for &(src, dst) in SIMPLE_CASE_FOLDS {
        if char_in_ranges(src, &ranges) || char_in_ranges(dst, &ranges) {
            let pos = touched.partition_point(|&d| d < dst);
            if touched.get(pos) != Some(&dst) {
                touched.insert(pos, dst);
            }
        }
    }
    // Add all members of every touched fold group.
    let mut extra = ranges;
    for &(src, dst) in SIMPLE_CASE_FOLDS {
        if touched.binary_search(&dst).is_ok() {
            extra.push((src, src));
            extra.push((dst, dst));
        }
    }
    merge_ranges(extra)
}

/// Complement of `ranges` within `['\0', '\u{10FFFF}']`, skipping surrogates.
fn complement_ranges(ranges: &[(char, char)]) -> Vec<(char, char)> {
    const SUR_LO: u32 = 0xD800;
    const SUR_HI: u32 = 0xDFFF;

    fn push_valid(out: &mut Vec<(char, char)>, lo_u: u32, hi_u: u32) {
        if hi_u < lo_u {
            return;
        }
        if hi_u < SUR_LO || lo_u > SUR_HI {
            if let (Some(lo), Some(hi)) = (char::from_u32(lo_u), char::from_u32(hi_u)) {
                out.push((lo, hi));
            }
        } else {
            if lo_u < SUR_LO
                && let (Some(lo), Some(hi)) = (char::from_u32(lo_u), char::from_u32(SUR_LO - 1))
            {
                out.push((lo, hi));
            }
            if hi_u > SUR_HI
                && let (Some(lo), Some(hi)) = (char::from_u32(SUR_HI + 1), char::from_u32(hi_u))
            {
                out.push((lo, hi));
            }
        }
    }

    let mut result = Vec::new();
    let mut pos: u32 = 0;
    for &(lo, hi) in ranges {
        let lo_u = lo as u32;
        if pos < lo_u {
            push_valid(&mut result, pos, lo_u - 1);
        }
        pos = (hi as u32).saturating_add(1);
        if pos > 0x10FFFF {
            return result;
        }
    }
    push_valid(&mut result, pos, 0x10FFFF);
    result
}

/// O(n+m) merge-based intersection of two sorted, merged range lists.
fn intersect_ranges(a: &[(char, char)], b: &[(char, char)]) -> Vec<(char, char)> {
    let mut result = Vec::new();
    let mut ai = 0;
    let mut bi = 0;
    while ai < a.len() && bi < b.len() {
        let (alo, ahi) = a[ai];
        let (blo, bhi) = b[bi];
        let lo = alo.max(blo);
        let hi = ahi.min(bhi);
        if lo <= hi {
            result.push((lo, hi));
        }
        if ahi <= bhi {
            ai += 1;
        }
        if bhi <= ahi {
            bi += 1;
        }
    }
    result
}

/// Build a `CharSet` for a shorthand (`\w`, `\d`, etc.) at compile time.
fn shorthand_charset(sh: Shorthand, ascii_range: bool, ignore_case: bool) -> CharSet {
    let raw = charset::shorthand_direct_ranges(sh, ascii_range);
    let ranges = merge_ranges(raw);
    let ranges = if ignore_case {
        expand_case_folds(ranges)
    } else {
        ranges
    };
    // Shorthands are never negated — \W etc. compile as NonWord which already
    // produces the positive (non-negated) complemented ranges.
    CharSet::new(false, ranges)
}

/// Build a `CharSet` for a Unicode property (`\p{...}`) at compile time.
fn unicode_prop_charset(name: &str, negate: bool, ignore_case: bool) -> Result<CharSet, Error> {
    let raw = charset::unicode_prop_direct_ranges(name)
        .ok_or_else(|| Error::Compile(format!("unknown Unicode property: {name:?}")))?;
    let ranges = merge_ranges(raw);
    let ranges = if ignore_case {
        expand_case_folds(ranges)
    } else {
        ranges
    };
    Ok(CharSet::new(negate, ranges))
}

pub fn compile_charset(
    cc: &CharClass,
    ignore_case: bool,
    ascii_range: bool,
) -> Result<CharSet, Error> {
    // Step 1: expand all items into raw (lo, hi) codepoint pairs.
    let mut raw: Vec<(char, char)> = Vec::new();
    for item in &cc.items {
        expand_class_item(item, ascii_range, ignore_case, &mut raw)?;
    }

    // Step 2: sort and merge.
    let mut ranges = merge_ranges(raw);

    // Step 3: case-fold expansion (single-codepoint equivalents only).
    if ignore_case {
        ranges = expand_case_folds(ranges);
    }

    // Step 4: pre-compute intersections (each intersection is a CharClass with
    // its own negate and items; apply negate before intersecting).
    for inner_cc in &cc.intersections {
        let inner_cs = compile_charset(inner_cc, ignore_case, ascii_range)?;
        let inner_eff = if inner_cs.negate {
            complement_ranges(&inner_cs.ranges)
        } else {
            inner_cs.ranges
        };
        ranges = intersect_ranges(&ranges, &inner_eff);
    }

    Ok(CharSet::new(cc.negate, ranges))
}

fn expand_class_item(
    item: &ClassItem,
    ascii_range: bool,
    ignore_case: bool,
    out: &mut Vec<(char, char)>,
) -> Result<(), Error> {
    match item {
        ClassItem::Char(c) => out.push((*c, *c)),
        ClassItem::Range(lo, hi) => out.push((*lo, *hi)),
        ClassItem::Shorthand(sh) => {
            let raw = charset::shorthand_direct_ranges(*sh, ascii_range);
            out.extend(raw);
        }
        ClassItem::Posix(cls, neg) => {
            let raw = charset::posix_direct_ranges(*cls, ascii_range);
            if *neg {
                out.extend(complement_ranges(&merge_ranges(raw)));
            } else {
                out.extend(raw);
            }
        }
        ClassItem::Unicode(name, neg) => {
            let raw = charset::unicode_prop_direct_ranges(name)
                .ok_or_else(|| Error::Compile(format!("unknown Unicode property: {name:?}")))?;
            if *neg {
                let negated = complement_ranges(&merge_ranges(raw));
                out.extend(negated);
            } else {
                out.extend(raw);
            }
        }
        ClassItem::Nested(inner_cc) => {
            let inner_cs = compile_charset(inner_cc, ignore_case, ascii_range)?;
            let eff = if inner_cs.negate {
                complement_ranges(&inner_cs.ranges)
            } else {
                inner_cs.ranges
            };
            out.extend(eff);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public compile function
// ---------------------------------------------------------------------------

/// Walk the instruction sequence starting at `start_pc`, skipping zero-width
/// instructions (`Save`, `KeepStart`, `NullCheckStart`), and return the char
/// from the first `Char(c, false)` (case-sensitive) instruction found, if any.
///
/// Return the first case-sensitive literal character that `node` is guaranteed
/// to consume when compiled in a forward (non-backward) context.
///
/// Returns `None` when:
/// - `ic` is true (case-insensitive: any char could match the literal), or
/// - the node might match without consuming a specific literal first (empty,
///   anchor, alternation, class, etc.).
///
/// This is used to set a syntactic guard on `Fork` instructions so the VM can
/// skip the backtrack-stack push when `text[pos]` does not equal the guard.
fn first_literal_of_node(node: &Node, ic: bool) -> Option<char> {
    if ic {
        return None;
    }
    match node {
        Node::Literal(c) => Some(*c),
        Node::Concat(nodes) => first_literal_of_node(nodes.first()?, ic),
        Node::Capture {
            node, flags: gf, ..
        }
        | Node::NamedCapture {
            node, flags: gf, ..
        } => first_literal_of_node(node, ic || gf.ignore_case),
        Node::Group { node, flags: gf } => first_literal_of_node(node, ic || gf.ignore_case),
        Node::Atomic(node) => first_literal_of_node(node, ic),
        Node::InlineFlags { flags: fmod, node } => {
            let new_ic = (ic || fmod.on.ignore_case) && !fmod.off.ignore_case;
            first_literal_of_node(node, new_ic)
        }
        Node::Quantifier { node, range, .. } if range.min >= 1 => first_literal_of_node(node, ic),
        _ => None,
    }
}

/// This is used to compute a compile-time guard for `Fork`/`ForkNext`:
/// if `text[pos]` does not equal the returned char, the path starting at
/// `start_pc` is guaranteed to fail on its very first character match.
fn fork_guard_char(prog: &[Inst], mut pc: usize) -> Option<char> {
    loop {
        match prog.get(pc)? {
            Inst::Char(c) => return Some(*c),
            // Zero-width; keep looking
            Inst::Save(_) | Inst::KeepStart | Inst::NullCheckStart(_) => pc += 1,
            _ => return None,
        }
    }
}

/// Scan forward from `start_pc` (skipping zero-width instructions) and, if
/// the first consumed-character instruction is `Char(gc, false)`, replace it
/// with `CharFast(gc)`.  A no-op when no such `Char` is found (e.g. when the
/// primary path starts with another Fork rather than a direct Char).
fn promote_to_char_fast(prog: &mut [Inst], mut pc: usize, gc: char) {
    loop {
        match prog.get(pc) {
            Some(Inst::Save(_) | Inst::KeepStart | Inst::NullCheckStart(_)) => pc += 1,
            Some(Inst::Char(c)) if *c == gc => {
                prog[pc] = Inst::CharFast(gc);
                return;
            }
            _ => return,
        }
    }
}

/// Post-processing pass: fill in the guard character for every `ForkNext`
/// instruction that has a guaranteed first-char on its primary path.
/// `Fork` guards are already set syntactically during compilation.
///
/// After guards are set, a second sub-pass replaces each guarded `Char(c,
/// false)` on the fork's primary path with `CharFast(c)`, which skips both
/// the bounds check and the match check (the guard has already done both).
fn compute_fork_guards(prog: &mut [Inst]) {
    let len = prog.len();

    // Phase 1: set ForkNext guards via instruction-level walk.
    // (Fork guards were set syntactically at compilation time.)
    let fork_next_guards: Vec<(usize, Option<char>)> = (0..len)
        .filter_map(|pc| match prog[pc] {
            Inst::ForkNext(alt, None) => Some((pc, fork_guard_char(prog, alt))),
            _ => None,
        })
        .collect();
    for (pc, guard) in fork_next_guards {
        if let Inst::ForkNext(_, ref mut g) = prog[pc] {
            *g = guard;
        }
    }

    // Phase 2: promote guarded Char(gc, false) → CharFast(gc) on each fork's
    // primary path (after any zero-width instructions).
    //
    // For `Fork(alt, Some(gc))`: `pc+1` (the body) is only reached when the
    // guard passes (text[pos] == gc), so CharFast there is always safe.
    //
    // For `ForkNext(alt, Some(gc))`: `alt` is only safe to promote when it
    // is EXCLUSIVELY reachable via the guard check. Two unsafe paths exist:
    //
    // 1. Fall-through from `alt-1`: if the instruction before `alt` is not a
    //    `Jump`, `alt` is the natural successor of the preceding instruction.
    //    Blocked by requiring `matches!(prog[alt-1], Inst::Jump(_))`.
    //
    // 2. `NullCheckEnd{exit_pc: alt}`: when the body of a lazy loop matches
    //    empty, `NullCheckEnd` explicitly jumps to `alt` without having
    //    verified the guard character.  Blocked by requiring that no
    //    `NullCheckEnd` in the program targets `alt`.
    let null_check_exits: std::collections::HashSet<usize> = prog
        .iter()
        .filter_map(|inst| match inst {
            Inst::NullCheckEnd { exit_pc, .. } => Some(*exit_pc),
            _ => None,
        })
        .collect();

    let promotions: Vec<(usize, char)> = (0..len)
        .filter_map(|pc| match prog[pc] {
            Inst::Fork(_, Some(gc)) => Some((pc + 1, gc)),
            Inst::ForkNext(alt, Some(gc))
                if alt > 0
                    && matches!(prog[alt - 1], Inst::Jump(_))
                    && !null_check_exits.contains(&alt) =>
            {
                Some((alt, gc))
            }
            _ => None,
        })
        .collect();
    for (start, gc) in promotions {
        promote_to_char_fast(prog, start, gc);
    }
}

// ---------------------------------------------------------------------------
// SpanChar/SpanClass post-processing pass
// ---------------------------------------------------------------------------

/// What the continuation's first character slot looks like (conservative).
enum ContFirst {
    /// End of program or unconsumed anchor (StringEnd / Match): no char needed.
    Terminal,
    /// The first consumed-character instruction is `Char(c)` / `CharFast(c)`.
    Char(char),
    /// The first consumed-character instruction is `Class(idx, false)`.
    Class(usize),
    /// Unknown / complex: conservatively block the optimisation.
    Unknown,
}

/// Scan forward from `pc` (skipping zero-width instructions) to determine
/// what character(s) the continuation can start with.
fn continuation_first(prog: &[Inst], mut pc: usize) -> ContFirst {
    loop {
        match prog.get(pc) {
            None => return ContFirst::Terminal,
            Some(Inst::Save(_) | Inst::KeepStart) => pc += 1,
            Some(Inst::Match) => return ContFirst::Terminal,
            Some(Inst::Anchor(AnchorKind::StringEnd, _)) => return ContFirst::Terminal,
            Some(Inst::Char(c) | Inst::CharFast(c)) => return ContFirst::Char(*c),
            Some(Inst::Class(idx, false)) => return ContFirst::Class(*idx),
            _ => return ContFirst::Unknown,
        }
    }
}

fn char_disjoint_from_continuation(c: char, first: &ContFirst, charsets: &[CharSet]) -> bool {
    match first {
        ContFirst::Terminal => true,
        ContFirst::Char(c2) => c != *c2,
        ContFirst::Class(idx) => !charsets[*idx].matches(c),
        ContFirst::Unknown => false,
    }
}

fn class_disjoint_from_continuation(idx: usize, first: &ContFirst, charsets: &[CharSet]) -> bool {
    let body_cs = &charsets[idx];
    match first {
        ContFirst::Terminal => true,
        ContFirst::Char(c) => !body_cs.matches(*c),
        ContFirst::Class(idx2) => {
            // Conservative: only declare disjoint when both are non-negated and
            // their ASCII bitmaps have no overlap, and neither has non-ASCII ranges.
            let cont_cs = &charsets[*idx2];
            if body_cs.negate || cont_cs.negate {
                return false;
            }
            let body_nonascii = body_cs.ranges.iter().any(|&(lo, _)| lo as u32 >= 128);
            let cont_nonascii = cont_cs.ranges.iter().any(|&(lo, _)| lo as u32 >= 128);
            if body_nonascii || cont_nonascii {
                return false;
            }
            body_cs.ascii_bits[0] & cont_cs.ascii_bits[0] == 0
                && body_cs.ascii_bits[1] & cont_cs.ascii_bits[1] == 0
        }
        ContFirst::Unknown => false,
    }
}

/// Post-processing pass: replace simple greedy loops with `SpanChar`/`SpanClass`.
///
/// A greedy loop `Fork(exit_pc) + Char/CharFast/Class + Jump(fork_pc)` where
/// `exit_pc == fork_pc + 3` is replaced by `SpanChar`/`SpanClass` when the
/// body character set is provably disjoint from the continuation's first-
/// character set.
///
/// Disjointness guarantees that backtracking into the span could never enable
/// the continuation to succeed, so the greedy loop is de-facto possessive.
/// This converts it into a tight Rust/JIT inner loop with no backtrack-stack
/// overhead.
///
/// Called after `compute_fork_guards` so the body may be `CharFast` rather
/// than `Char`.  The two instructions that follow the `SpanChar`/`SpanClass`
/// are replaced with `Jump(exit_pc)` (dead code, never reached).
fn spanify_greedy_loops(prog: &mut [Inst], charsets: &[CharSet]) {
    let n = prog.len();
    for fork_pc in 0..n.saturating_sub(2) {
        // Detect: Fork(exit_pc) where exit_pc == fork_pc + 3
        let exit_pc = match prog[fork_pc] {
            Inst::Fork(ep, _) if ep == fork_pc + 3 => ep,
            _ => continue,
        };
        // Body at fork_pc+1, back-edge at fork_pc+2
        let body = prog[fork_pc + 1].clone();
        if !matches!(prog[fork_pc + 2], Inst::Jump(j) if j == fork_pc) {
            continue;
        }

        let first = continuation_first(prog, exit_pc);

        match body {
            Inst::Char(c) | Inst::CharFast(c) => {
                if char_disjoint_from_continuation(c, &first, charsets) {
                    prog[fork_pc] = Inst::SpanChar { c, exit_pc };
                    prog[fork_pc + 1] = Inst::Jump(exit_pc);
                    prog[fork_pc + 2] = Inst::Jump(exit_pc);
                }
            }
            Inst::Class(idx, false) => {
                if class_disjoint_from_continuation(idx, &first, charsets) {
                    prog[fork_pc] = Inst::SpanClass { idx, exit_pc };
                    prog[fork_pc + 1] = Inst::Jump(exit_pc);
                    prog[fork_pc + 2] = Inst::Jump(exit_pc);
                }
            }
            _ => {} // ic=true, FoldSeq, etc.: skip
        }
    }
}

pub struct CompiledProgram {
    pub prog: Vec<Inst>,
    pub charsets: Vec<CharSet>,
    pub alt_tries: Vec<ByteTrie>,
    pub named_groups: Vec<(String, u32)>,
    pub num_groups: usize,
    pub num_null_checks: usize,
    pub num_repeat_counters: usize,
    #[allow(dead_code)]
    pub subexp_starts: HashMap<u32, usize>,
}

pub fn compile(
    node: &Node,
    named_groups: Vec<(String, u32)>,
    opts: CompileOptions,
) -> Result<CompiledProgram, Error> {
    let base_flags = Flags {
        ignore_case: opts.ignore_case,
        multiline: opts.multiline,
        ..Default::default()
    };

    let mut compiler = Compiler::new(base_flags, named_groups.clone());
    compiler.compile_node(node, base_flags)?;
    compiler.emit(Inst::Match);
    compiler.backfill_calls()?;
    compute_fork_guards(&mut compiler.prog);
    spanify_greedy_loops(&mut compiler.prog, &compiler.charsets);

    let num_groups = compiler.num_groups as usize;
    let num_null_checks = compiler.num_null_checks as usize;
    let num_repeat_counters = compiler.num_repeat_counters as usize;
    Ok(CompiledProgram {
        prog: compiler.prog,
        charsets: compiler.charsets,
        alt_tries: compiler.alt_tries,
        named_groups,
        num_groups,
        num_null_checks,
        num_repeat_counters,
        subexp_starts: compiler.subexp_starts,
    })
}
