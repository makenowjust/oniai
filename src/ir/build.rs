//! IrBuilder: AST + CompileOptions → IrProgram.

use crate::ast::*;
use crate::bytetrie::ByteTrie;
use crate::casefold::{CaseFold, case_fold};
use crate::charset;
use crate::compile::{CompileOptions, compile_charset};
use crate::data::casefold_data::SIMPLE_CASE_FOLDS;
use crate::error::Error;
use crate::ir::{
    BlockId, IrBlock, IrForkCandidate, IrGuard, IrProgram, IrRegion, IrStmt, IrTerminator,
    LiveSlots, RegionId, RegionKind,
};
use crate::vm::CharSet;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Helpers mirrored from compile.rs
// ---------------------------------------------------------------------------

fn can_match_empty(node: &Node) -> bool {
    match node {
        Node::Empty => true,
        Node::Literal(_) | Node::AnyChar | Node::CharClass(_) | Node::Shorthand(_) => false,
        Node::UnicodeProp { .. } => false,
        Node::Anchor(_) | Node::Keep => true,
        Node::LookAround { .. } => true,
        Node::Concat(nodes) => nodes.iter().all(can_match_empty),
        Node::Alternation(nodes) => nodes.iter().any(can_match_empty),
        Node::Quantifier { node, range, .. } => range.min == 0 || can_match_empty(node),
        Node::Capture { node, .. }
        | Node::NamedCapture { node, .. }
        | Node::Group { node, .. }
        | Node::Atomic(node)
        | Node::InlineFlags { node, .. } => can_match_empty(node),
        Node::Absence(node) => !can_match_empty(node),
        Node::BackRef { .. } | Node::SubexpCall(_) => true,
        Node::Conditional { yes, no, .. } => can_match_empty(yes) || can_match_empty(no),
    }
}

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

const MAX_FOLD_VARIANTS: usize = 1024;

fn chars_folding_to(target: char) -> Vec<char> {
    let mut out = vec![target];
    for &(src, folded) in SIMPLE_CASE_FOLDS {
        if folded == target && src != target {
            out.push(src);
        }
    }
    out
}

fn expand_alts_case_folded(alts: &[String]) -> Option<Vec<String>> {
    let mut all_variants: Vec<String> = Vec::new();
    for s in alts {
        let mut per_char: Vec<Vec<char>> = Vec::new();
        for c in s.chars() {
            let fold_target = match case_fold(c) {
                CaseFold::Single(f) => f,
                CaseFold::Multi(_) => return None,
            };
            per_char.push(chars_folding_to(fold_target));
        }
        let n_variants: usize = per_char.iter().map(|v| v.len()).product();
        if all_variants.len() + n_variants > MAX_FOLD_VARIANTS {
            return None;
        }
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

fn merge_ranges(mut raw: Vec<(char, char)>) -> Vec<(char, char)> {
    if raw.is_empty() {
        return raw;
    }
    raw.sort_unstable_by_key(|&(lo, _)| lo as u32);
    let mut merged: Vec<(char, char)> = Vec::new();
    for (lo, hi) in raw {
        if let Some(last) = merged.last_mut()
            && lo as u32 <= last.1 as u32 + 1
        {
            if hi as u32 > last.1 as u32 {
                last.1 = hi;
            }
            continue;
        }
        merged.push((lo, hi));
    }
    merged
}

fn expand_case_folds(ranges: Vec<(char, char)>) -> Vec<(char, char)> {
    let mut raw = ranges.clone();
    for &(src, folded) in SIMPLE_CASE_FOLDS {
        for &(lo, hi) in &ranges {
            if src >= lo && src <= hi {
                raw.push((folded, folded));
                break;
            }
        }
        for &(lo, hi) in &ranges {
            if folded >= lo && folded <= hi {
                raw.push((src, src));
                break;
            }
        }
    }
    merge_ranges(raw)
}

fn shorthand_charset(sh: Shorthand, ascii_range: bool, ignore_case: bool) -> CharSet {
    let raw = charset::shorthand_direct_ranges(sh, ascii_range);
    let ranges = merge_ranges(raw);
    let ranges = if ignore_case {
        expand_case_folds(ranges)
    } else {
        ranges
    };
    CharSet::new(false, ranges)
}

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

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

struct BlockData {
    stmts: Vec<IrStmt>,
    term: Option<IrTerminator>,
}

struct PendingCall {
    region: RegionId,
    block: BlockId,
    target: GroupRef,
    current_group: u32,
}

struct IrBuilder {
    /// blocks[region][block]
    blocks: Vec<Vec<BlockData>>,
    region_kinds: Vec<RegionKind>,
    charsets: Vec<CharSet>,
    alt_tries: Vec<ByteTrie>,
    subexp_starts: HashMap<u32, BlockId>,
    pending_calls: Vec<PendingCall>,
    named_groups: Vec<(String, u32)>,
    num_groups: u32,
    current_group: u32,
    num_null_checks: u32,
    num_counters: u32,
    use_memo: bool,
}

impl IrBuilder {
    fn new(named_groups: Vec<(String, u32)>) -> Self {
        IrBuilder {
            blocks: Vec::new(),
            region_kinds: Vec::new(),
            charsets: Vec::new(),
            alt_tries: Vec::new(),
            subexp_starts: HashMap::new(),
            pending_calls: Vec::new(),
            named_groups,
            num_groups: 0,
            current_group: 0,
            num_null_checks: 0,
            num_counters: 0,
            use_memo: true,
        }
    }

    fn new_region(&mut self, kind: RegionKind) -> RegionId {
        let rid = self.blocks.len();
        self.blocks.push(Vec::new());
        self.region_kinds.push(kind);
        // Entry block is always block 0
        let _entry = self.new_block(rid);
        rid
    }

    fn new_block(&mut self, region: RegionId) -> BlockId {
        let bid = self.blocks[region].len();
        self.blocks[region].push(BlockData {
            stmts: Vec::new(),
            term: None,
        });
        bid
    }

    fn push_stmt(&mut self, region: RegionId, block: BlockId, stmt: IrStmt) {
        self.blocks[region][block].stmts.push(stmt);
    }

    fn set_term(&mut self, region: RegionId, block: BlockId, term: IrTerminator) {
        self.blocks[region][block].term = Some(term);
    }

    fn add_charset(&mut self, cs: CharSet) -> usize {
        let idx = self.charsets.len();
        self.charsets.push(cs);
        idx
    }

    fn alloc_null_check(&mut self) -> usize {
        let s = self.num_null_checks as usize;
        self.num_null_checks += 1;
        s
    }

    fn alloc_counter(&mut self) -> usize {
        let s = self.num_counters as usize;
        self.num_counters += 1;
        s
    }

    fn resolve_name(&self, name: &str) -> Result<u32, Error> {
        self.named_groups
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, idx)| *idx)
            .ok_or_else(|| Error::Compile(format!("undefined group name {:?}", name)))
    }

    // -----------------------------------------------------------------------
    // Core: build a node into `entry` block, continue to `cont` block
    // -----------------------------------------------------------------------

    fn build_node_into(
        &mut self,
        region: RegionId,
        entry: BlockId,
        node: &Node,
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        let ic = flags.ignore_case;
        match node {
            Node::Empty => {
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::Literal(c) => {
                if ic {
                    let folded: Vec<char> = case_fold(*c).chars().to_vec();
                    if backward {
                        self.push_stmt(region, entry, IrStmt::MatchFoldSeqBack(folded));
                    } else {
                        self.push_stmt(region, entry, IrStmt::MatchFoldSeq(folded));
                    }
                } else if backward {
                    self.push_stmt(region, entry, IrStmt::MatchCharBack(*c));
                } else {
                    self.push_stmt(region, entry, IrStmt::MatchChar(*c));
                }
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::AnyChar => {
                if backward {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchAnyCharBack {
                            dotall: flags.multiline,
                        },
                    );
                } else {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchAnyChar {
                            dotall: flags.multiline,
                        },
                    );
                }
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::Shorthand(sh) => {
                let cs = shorthand_charset(*sh, flags.ascii_range, ic);
                let id = self.add_charset(cs);
                if backward {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchClassBack {
                            id,
                            ignore_case: ic,
                        },
                    );
                } else {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchClass {
                            id,
                            ignore_case: ic,
                        },
                    );
                }
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::UnicodeProp { name, negate } => {
                let cs = unicode_prop_charset(name, *negate, ic)?;
                let id = self.add_charset(cs);
                if backward {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchClassBack {
                            id,
                            ignore_case: ic,
                        },
                    );
                } else {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchClass {
                            id,
                            ignore_case: ic,
                        },
                    );
                }
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::CharClass(cc) => {
                let cs = compile_charset(cc, ic, flags.ascii_range)?;
                let id = self.add_charset(cs);
                if backward {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchClassBack {
                            id,
                            ignore_case: ic,
                        },
                    );
                } else {
                    self.push_stmt(
                        region,
                        entry,
                        IrStmt::MatchClass {
                            id,
                            ignore_case: ic,
                        },
                    );
                }
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::Anchor(kind) => {
                self.push_stmt(region, entry, IrStmt::CheckAnchor(*kind, flags));
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::Keep => {
                self.push_stmt(region, entry, IrStmt::KeepStart);
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::BackRef { target, level } => {
                self.use_memo = false;
                let group = self.resolve_group_ref_to_index(target)?;
                self.push_stmt(
                    region,
                    entry,
                    IrStmt::CheckBackRef {
                        group,
                        ignore_case: ic,
                        level: *level,
                    },
                );
                self.set_term(region, entry, IrTerminator::Branch(cont));
            }

            Node::Concat(nodes) => {
                self.build_concat_into(region, entry, nodes, flags, backward, cont)?;
            }

            Node::Alternation(alts) => {
                self.build_alternation_into(region, entry, alts, flags, backward, cont)?;
            }

            Node::Quantifier { node, range, kind } => {
                self.build_quantifier_into(
                    region, entry, node, range, kind, flags, backward, cont,
                )?;
            }

            Node::Capture {
                index,
                node,
                flags: inner_flags,
            } => {
                self.build_capture(
                    region,
                    entry,
                    *index,
                    node,
                    inner_flags,
                    flags,
                    backward,
                    cont,
                )?;
            }

            Node::NamedCapture {
                index,
                node,
                flags: inner_flags,
                ..
            } => {
                self.build_capture(
                    region,
                    entry,
                    *index,
                    node,
                    inner_flags,
                    flags,
                    backward,
                    cont,
                )?;
            }

            Node::Group {
                node,
                flags: inner_flags,
            } => {
                let f = flags.apply_on(&FlagMod {
                    on: *inner_flags,
                    off: Flags::default(),
                });
                self.build_node_into(region, entry, node, f, backward, cont)?;
            }

            Node::InlineFlags {
                flags: flag_mod,
                node,
            } => {
                let new_flags = flags.apply_on(flag_mod);
                self.build_node_into(region, entry, node, new_flags, backward, cont)?;
            }

            Node::Atomic(node) => {
                let sub_rid = self.new_region(RegionKind::Atomic);
                let sub_cont = self.new_block(sub_rid);
                self.build_node_into(sub_rid, 0, node, flags, backward, sub_cont)?;
                self.set_term(sub_rid, sub_cont, IrTerminator::RegionEnd);
                self.set_term(
                    region,
                    entry,
                    IrTerminator::Atomic {
                        body: sub_rid,
                        next: cont,
                    },
                );
            }

            Node::LookAround { dir, pol, node } => {
                let inner_backward = *dir == LookDir::Behind;
                let sub_kind = match (dir, pol) {
                    (LookDir::Ahead, LookPol::Positive) => RegionKind::LookAhead { positive: true },
                    (LookDir::Ahead, LookPol::Negative) => {
                        RegionKind::LookAhead { positive: false }
                    }
                    (LookDir::Behind, LookPol::Positive) => {
                        RegionKind::LookBehind { positive: true }
                    }
                    (LookDir::Behind, LookPol::Negative) => {
                        RegionKind::LookBehind { positive: false }
                    }
                };
                let sub_rid = self.new_region(sub_kind);
                let sub_cont = self.new_block(sub_rid);
                self.build_node_into(sub_rid, 0, node, flags, inner_backward, sub_cont)?;
                self.set_term(sub_rid, sub_cont, IrTerminator::RegionEnd);

                // Lookaround is a single-candidate Fork with LookAround guard
                let after_look = self.new_block(region);
                self.set_term(region, after_look, IrTerminator::Branch(cont));
                self.set_term(
                    region,
                    entry,
                    IrTerminator::Fork {
                        candidates: vec![IrForkCandidate {
                            guard: IrGuard::LookAround {
                                pol: *pol,
                                dir: *dir,
                                body: sub_rid,
                            },
                            block: after_look,
                        }],
                        disjoint: true,
                        live_slots: LiveSlots::new(),
                    },
                );
            }

            Node::Absence(inner) => {
                let sub_rid = self.new_region(RegionKind::Absence);
                let sub_cont = self.new_block(sub_rid);
                self.build_node_into(sub_rid, 0, inner, flags, false, sub_cont)?;
                self.set_term(sub_rid, sub_cont, IrTerminator::RegionEnd);
                self.set_term(
                    region,
                    entry,
                    IrTerminator::Absence {
                        inner: sub_rid,
                        next: cont,
                    },
                );
            }

            Node::Conditional { cond, yes, no } => {
                self.use_memo = false;
                let slot_pair = match cond {
                    Condition::GroupNum(n) => ((*n - 1) * 2) as usize,
                    Condition::GroupName(name) => {
                        let idx = self.resolve_name(name).unwrap_or(1);
                        ((idx - 1) * 2) as usize
                    }
                };
                let yes_block = self.new_block(region);
                let no_block = self.new_block(region);
                self.build_node_into(region, yes_block, yes, flags, backward, cont)?;
                self.build_node_into(region, no_block, no, flags, backward, cont)?;
                self.set_term(
                    region,
                    entry,
                    IrTerminator::Fork {
                        candidates: vec![
                            IrForkCandidate {
                                guard: IrGuard::GroupMatched(slot_pair),
                                block: yes_block,
                            },
                            IrForkCandidate {
                                guard: IrGuard::Always,
                                block: no_block,
                            },
                        ],
                        disjoint: true,
                        live_slots: LiveSlots::new(),
                    },
                );
            }

            Node::SubexpCall(target) => {
                self.set_term(
                    region,
                    entry,
                    IrTerminator::Call {
                        target: 0,
                        ret: cont,
                    }, // target backfilled
                );
                self.pending_calls.push(PendingCall {
                    region,
                    block: entry,
                    target: target.clone(),
                    current_group: self.current_group,
                });
            }
        }
        Ok(())
    }

    fn resolve_group_ref_to_index(&self, target: &GroupRef) -> Result<u32, Error> {
        match target {
            GroupRef::Index(n) => Ok(*n),
            GroupRef::Name(name) => self.resolve_name(name),
            GroupRef::RelativeBack(n) => {
                let abs = self.current_group.checked_sub(*n).filter(|&x| x >= 1);
                abs.ok_or_else(|| {
                    Error::Compile(format!("relative backreference \\k<-{}> out of range", n))
                })
            }
            GroupRef::RelativeFwd(_) => Err(Error::Compile(
                "relative-forward backreference not supported".into(),
            )),
            GroupRef::Whole => Err(Error::Compile("\\k<0> backreference not supported".into())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn build_capture(
        &mut self,
        region: RegionId,
        entry: BlockId,
        index: u32,
        node: &Node,
        inner_flags: &Flags,
        outer_flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        let f = outer_flags.apply_on(&FlagMod {
            on: *inner_flags,
            off: Flags::default(),
        });
        if index > self.num_groups {
            self.num_groups = index;
        }
        let prev_group = self.current_group;
        self.current_group = index;
        // Register this block as the entry for subexpression calls
        self.subexp_starts.insert(index, entry);

        let slot_open = ((index - 1) * 2) as usize;
        let slot_close = slot_open + 1;
        let body_block = self.new_block(region);
        let after_block = self.new_block(region);

        if backward {
            self.push_stmt(region, entry, IrStmt::SaveCapture(slot_close));
            self.set_term(region, entry, IrTerminator::Branch(body_block));
            self.build_node_into(region, body_block, node, f, backward, after_block)?;
            self.push_stmt(region, after_block, IrStmt::SaveCapture(slot_open));
        } else {
            self.push_stmt(region, entry, IrStmt::SaveCapture(slot_open));
            self.set_term(region, entry, IrTerminator::Branch(body_block));
            self.build_node_into(region, body_block, node, f, backward, after_block)?;
            self.push_stmt(region, after_block, IrStmt::SaveCapture(slot_close));
        }
        self.set_term(
            region,
            after_block,
            IrTerminator::RetIfCalled { fallthrough: cont },
        );
        self.current_group = prev_group;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Concat
    // -----------------------------------------------------------------------

    fn build_concat_into(
        &mut self,
        region: RegionId,
        entry: BlockId,
        nodes: &[Node],
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        let ic = flags.ignore_case;

        if ic {
            // Merge consecutive case-insensitive literals into FoldSeq
            let iter_nodes: Vec<&Node> = if backward {
                nodes.iter().rev().collect()
            } else {
                nodes.iter().collect()
            };

            // Build segments: sequences of folds alternating with non-literal nodes
            enum Seg<'a> {
                Fold(Vec<char>),
                Node(&'a Node),
            }

            let mut segments: Vec<Seg<'_>> = Vec::new();
            let mut fold_accum: Vec<char> = Vec::new();
            for child in &iter_nodes {
                if let Node::Literal(c) = child {
                    fold_accum.extend(case_fold(*c).chars().iter().copied());
                } else {
                    if !fold_accum.is_empty() {
                        segments.push(Seg::Fold(std::mem::take(&mut fold_accum)));
                    }
                    segments.push(Seg::Node(child));
                }
            }
            if !fold_accum.is_empty() {
                segments.push(Seg::Fold(fold_accum));
            }

            if segments.is_empty() {
                self.set_term(region, entry, IrTerminator::Branch(cont));
                return Ok(());
            }

            let n = segments.len();
            let mut inter_blocks: Vec<BlockId> = Vec::with_capacity(n.saturating_sub(1));
            for _ in 1..n {
                inter_blocks.push(self.new_block(region));
            }

            for (i, seg) in segments.iter().enumerate() {
                let cur = if i == 0 { entry } else { inter_blocks[i - 1] };
                let next = if i + 1 < n { inter_blocks[i] } else { cont };
                match seg {
                    Seg::Fold(folded) => {
                        if backward {
                            self.push_stmt(region, cur, IrStmt::MatchFoldSeqBack(folded.clone()));
                        } else {
                            self.push_stmt(region, cur, IrStmt::MatchFoldSeq(folded.clone()));
                        }
                        self.set_term(region, cur, IrTerminator::Branch(next));
                    }
                    Seg::Node(n_node) => {
                        self.build_node_into(region, cur, n_node, flags, backward, next)?;
                    }
                }
            }
        } else {
            let iter_nodes: Vec<&Node> = if backward {
                nodes.iter().rev().collect()
            } else {
                nodes.iter().collect()
            };

            let n = iter_nodes.len();
            if n == 0 {
                self.set_term(region, entry, IrTerminator::Branch(cont));
                return Ok(());
            }

            let mut inter_blocks: Vec<BlockId> = Vec::with_capacity(n.saturating_sub(1));
            for _ in 1..n {
                inter_blocks.push(self.new_block(region));
            }

            for (i, child) in iter_nodes.iter().enumerate() {
                let cur = if i == 0 { entry } else { inter_blocks[i - 1] };
                let next = if i + 1 < n { inter_blocks[i] } else { cont };
                self.build_node_into(region, cur, child, flags, backward, next)?;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Alternation
    // -----------------------------------------------------------------------

    fn build_alternation_into(
        &mut self,
        region: RegionId,
        entry: BlockId,
        alts: &[Node],
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        if alts.is_empty() {
            self.set_term(region, entry, IrTerminator::Branch(cont));
            return Ok(());
        }
        if alts.len() == 1 {
            return self.build_node_into(region, entry, &alts[0], flags, backward, cont);
        }

        // AltTrie optimization
        {
            let strings: Option<Vec<String>> = alts.iter().map(try_extract_plain_string).collect();
            if let Some(strings) = strings {
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
                            self.push_stmt(region, entry, IrStmt::MatchAltTrieBack(idx));
                        } else {
                            self.alt_tries.push(trie);
                            self.push_stmt(region, entry, IrStmt::MatchAltTrie(idx));
                        }
                        self.set_term(region, entry, IrTerminator::Branch(cont));
                        return Ok(());
                    }
                }
            }
        }

        // Fork chain for N alternatives
        let mut candidates: Vec<IrForkCandidate> = Vec::new();
        for alt in alts {
            let alt_block = self.new_block(region);
            self.build_node_into(region, alt_block, alt, flags, backward, cont)?;
            candidates.push(IrForkCandidate {
                guard: IrGuard::Always,
                block: alt_block,
            });
        }
        self.set_term(
            region,
            entry,
            IrTerminator::Fork {
                candidates,
                disjoint: false,
                live_slots: LiveSlots::new(),
            },
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Quantifier
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn build_quantifier_into(
        &mut self,
        region: RegionId,
        entry: BlockId,
        node: &Node,
        range: &QuantRange,
        kind: &QuantKind,
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        let min = range.min;
        let max = range.max;
        const REPEAT_COUNTER_THRESHOLD: u32 = 4;

        // Emit `min` mandatory copies, return the block to use for optional part
        let tail_block = if min >= REPEAT_COUNTER_THRESHOLD {
            // Use counter loop: CounterInit → body_loop_start → body_loop_end → CounterNext
            let slot = self.alloc_counter();
            let body_loop_start = self.new_block(region);
            let body_loop_end = self.new_block(region);
            let exit_block = self.new_block(region);
            self.push_stmt(region, entry, IrStmt::CounterInit(slot));
            self.set_term(region, entry, IrTerminator::Branch(body_loop_start));
            self.build_node_into(
                region,
                body_loop_start,
                node,
                flags,
                backward,
                body_loop_end,
            )?;
            self.set_term(
                region,
                body_loop_end,
                IrTerminator::CounterNext {
                    slot,
                    count: min,
                    body: body_loop_start,
                    exit: exit_block,
                },
            );
            exit_block
        } else {
            // Inline min copies
            let mut cur = entry;
            for _ in 0..min {
                let next = self.new_block(region);
                self.build_node_into(region, cur, node, flags, backward, next)?;
                cur = next;
            }
            cur
        };

        match (max, kind) {
            (Some(m), _) if m == min => {
                // {n}: exactly n; tail just branches to cont
                self.set_term(region, tail_block, IrTerminator::Branch(cont));
            }

            (None, QuantKind::Greedy) => {
                self.build_greedy_star(region, tail_block, node, flags, backward, cont)?;
            }

            (None, QuantKind::Reluctant) => {
                self.build_lazy_star(region, tail_block, node, flags, backward, cont)?;
            }

            (None, QuantKind::Possessive) => {
                self.build_possessive_star(region, tail_block, node, flags, backward, cont)?;
            }

            (Some(m), QuantKind::Greedy) => {
                // {n,m}: extra = m-n optional copies with Fork chain
                let extra = m - min;
                let mut cur = tail_block;
                for i in 0..extra {
                    let body_block = self.new_block(region);
                    let next = if i + 1 < extra {
                        self.new_block(region)
                    } else {
                        cont
                    };
                    self.build_node_into(region, body_block, node, flags, backward, next)?;
                    self.set_term(
                        region,
                        cur,
                        IrTerminator::Fork {
                            candidates: vec![
                                IrForkCandidate {
                                    guard: IrGuard::Always,
                                    block: body_block,
                                },
                                IrForkCandidate {
                                    guard: IrGuard::Always,
                                    block: next,
                                },
                            ],
                            disjoint: false,
                            live_slots: LiveSlots::new(),
                        },
                    );
                    if i + 1 < extra {
                        cur = next;
                    }
                }
                if extra == 0 {
                    self.set_term(region, cur, IrTerminator::Branch(cont));
                }
            }

            (Some(m), QuantKind::Reluctant) => {
                let extra = m - min;
                let mut cur = tail_block;
                for i in 0..extra {
                    let body_block = self.new_block(region);
                    let next = if i + 1 < extra {
                        self.new_block(region)
                    } else {
                        cont
                    };
                    self.build_node_into(region, body_block, node, flags, backward, next)?;
                    // Lazy: exit first, body second
                    self.set_term(
                        region,
                        cur,
                        IrTerminator::Fork {
                            candidates: vec![
                                IrForkCandidate {
                                    guard: IrGuard::Always,
                                    block: next,
                                },
                                IrForkCandidate {
                                    guard: IrGuard::Always,
                                    block: body_block,
                                },
                            ],
                            disjoint: false,
                            live_slots: LiveSlots::new(),
                        },
                    );
                    if i + 1 < extra {
                        cur = next;
                    }
                }
                if extra == 0 {
                    self.set_term(region, cur, IrTerminator::Branch(cont));
                }
            }

            (Some(m), QuantKind::Possessive) => {
                let extra = m - min;
                if extra == 0 {
                    self.set_term(region, tail_block, IrTerminator::Branch(cont));
                    return Ok(());
                }
                let sub_rid = self.new_region(RegionKind::Atomic);
                let mut cur = 0usize; // entry of sub_rid
                for i in 0..extra {
                    let body_block = self.new_block(sub_rid);
                    let next = self.new_block(sub_rid);
                    self.build_node_into(sub_rid, body_block, node, flags, backward, next)?;
                    self.set_term(
                        sub_rid,
                        cur,
                        IrTerminator::Fork {
                            candidates: vec![
                                IrForkCandidate {
                                    guard: IrGuard::Always,
                                    block: body_block,
                                },
                                IrForkCandidate {
                                    guard: IrGuard::Always,
                                    block: next,
                                },
                            ],
                            disjoint: false,
                            live_slots: LiveSlots::new(),
                        },
                    );
                    cur = next;
                    if i + 1 == extra {
                        self.set_term(sub_rid, cur, IrTerminator::RegionEnd);
                    }
                }
                self.set_term(
                    region,
                    tail_block,
                    IrTerminator::Atomic {
                        body: sub_rid,
                        next: cont,
                    },
                );
            }
        }
        Ok(())
    }

    fn build_greedy_star(
        &mut self,
        region: RegionId,
        entry: BlockId,
        node: &Node,
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        if can_match_empty(node) {
            let slot = self.alloc_null_check();
            let null_check_block = self.new_block(region);
            let fork_block = self.new_block(region);
            let body_block = self.new_block(region);
            let nc_end_block = self.new_block(region);

            self.set_term(region, entry, IrTerminator::Branch(null_check_block));
            self.push_stmt(region, null_check_block, IrStmt::NullCheckBegin(slot));
            self.set_term(region, null_check_block, IrTerminator::Branch(fork_block));
            self.set_term(
                region,
                fork_block,
                IrTerminator::Fork {
                    candidates: vec![
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: body_block,
                        },
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: cont,
                        },
                    ],
                    disjoint: false,
                    live_slots: LiveSlots::new(),
                },
            );
            self.build_node_into(region, body_block, node, flags, backward, nc_end_block)?;
            self.set_term(
                region,
                nc_end_block,
                IrTerminator::NullCheckEnd {
                    slot,
                    exit: cont,
                    cont: null_check_block,
                },
            );
        } else {
            let fork_block = self.new_block(region);
            let body_block = self.new_block(region);

            self.set_term(region, entry, IrTerminator::Branch(fork_block));
            self.set_term(
                region,
                fork_block,
                IrTerminator::Fork {
                    candidates: vec![
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: body_block,
                        },
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: cont,
                        },
                    ],
                    disjoint: false,
                    live_slots: LiveSlots::new(),
                },
            );
            // body loops back to fork_block
            self.build_node_into(region, body_block, node, flags, backward, fork_block)?;
        }
        Ok(())
    }

    fn build_lazy_star(
        &mut self,
        region: RegionId,
        entry: BlockId,
        node: &Node,
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        if can_match_empty(node) {
            let slot = self.alloc_null_check();
            let null_check_block = self.new_block(region);
            let fork_block = self.new_block(region);
            let body_block = self.new_block(region);
            let nc_end_block = self.new_block(region);

            self.set_term(region, entry, IrTerminator::Branch(null_check_block));
            self.push_stmt(region, null_check_block, IrStmt::NullCheckBegin(slot));
            self.set_term(region, null_check_block, IrTerminator::Branch(fork_block));
            // Lazy: exit (cont) first, body second
            self.set_term(
                region,
                fork_block,
                IrTerminator::Fork {
                    candidates: vec![
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: cont,
                        },
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: body_block,
                        },
                    ],
                    disjoint: false,
                    live_slots: LiveSlots::new(),
                },
            );
            self.build_node_into(region, body_block, node, flags, backward, nc_end_block)?;
            self.set_term(
                region,
                nc_end_block,
                IrTerminator::NullCheckEnd {
                    slot,
                    exit: cont,
                    cont: null_check_block,
                },
            );
        } else {
            let fork_block = self.new_block(region);
            let body_block = self.new_block(region);

            self.set_term(region, entry, IrTerminator::Branch(fork_block));
            // Lazy: exit (cont) first, body second
            self.set_term(
                region,
                fork_block,
                IrTerminator::Fork {
                    candidates: vec![
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: cont,
                        },
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: body_block,
                        },
                    ],
                    disjoint: false,
                    live_slots: LiveSlots::new(),
                },
            );
            // body loops back to fork_block
            self.build_node_into(region, body_block, node, flags, backward, fork_block)?;
        }
        Ok(())
    }

    fn build_possessive_star(
        &mut self,
        region: RegionId,
        entry: BlockId,
        node: &Node,
        flags: Flags,
        backward: bool,
        cont: BlockId,
    ) -> Result<(), Error> {
        let sub_rid = self.new_region(RegionKind::Atomic);
        if can_match_empty(node) {
            let slot = self.alloc_null_check();
            let null_check_block = self.new_block(sub_rid);
            let fork_block = self.new_block(sub_rid);
            let body_block = self.new_block(sub_rid);
            let nc_end_block = self.new_block(sub_rid);
            let region_end_block = self.new_block(sub_rid);

            // sub_rid entry (0) → null_check_block
            self.set_term(sub_rid, 0, IrTerminator::Branch(null_check_block));
            self.push_stmt(sub_rid, null_check_block, IrStmt::NullCheckBegin(slot));
            self.set_term(sub_rid, null_check_block, IrTerminator::Branch(fork_block));
            self.set_term(
                sub_rid,
                fork_block,
                IrTerminator::Fork {
                    candidates: vec![
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: body_block,
                        },
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: region_end_block,
                        },
                    ],
                    disjoint: false,
                    live_slots: LiveSlots::new(),
                },
            );
            self.build_node_into(sub_rid, body_block, node, flags, backward, nc_end_block)?;
            self.set_term(
                sub_rid,
                nc_end_block,
                IrTerminator::NullCheckEnd {
                    slot,
                    exit: region_end_block,
                    cont: null_check_block,
                },
            );
            self.set_term(sub_rid, region_end_block, IrTerminator::RegionEnd);
        } else {
            let fork_block = self.new_block(sub_rid);
            let body_block = self.new_block(sub_rid);
            let region_end_block = self.new_block(sub_rid);

            // sub_rid entry (0) → fork_block
            self.set_term(sub_rid, 0, IrTerminator::Branch(fork_block));
            self.set_term(
                sub_rid,
                fork_block,
                IrTerminator::Fork {
                    candidates: vec![
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: body_block,
                        },
                        IrForkCandidate {
                            guard: IrGuard::Always,
                            block: region_end_block,
                        },
                    ],
                    disjoint: false,
                    live_slots: LiveSlots::new(),
                },
            );
            self.build_node_into(sub_rid, body_block, node, flags, backward, fork_block)?;
            self.set_term(sub_rid, region_end_block, IrTerminator::RegionEnd);
        }
        self.set_term(
            region,
            entry,
            IrTerminator::Atomic {
                body: sub_rid,
                next: cont,
            },
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Backfill subexpression calls
    // -----------------------------------------------------------------------

    fn backfill_calls(&mut self) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending_calls);
        for pc in pending {
            let target_block = match &pc.target {
                GroupRef::Index(n) => self.subexp_starts.get(n).copied().ok_or_else(|| {
                    Error::Compile(format!("undefined group {n} for subexpr call"))
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
                        Error::Compile(format!("group {:?} has no entry block", name))
                    })?
                }
                GroupRef::Whole => 0,
                GroupRef::RelativeBack(n) => {
                    let abs = pc.current_group.checked_sub(*n).filter(|&x| x >= 1);
                    let idx = abs.ok_or_else(|| {
                        Error::Compile(format!(
                            "relative \\g<-{}> out of range (current group {})",
                            n, pc.current_group
                        ))
                    })?;
                    self.subexp_starts.get(&idx).copied().ok_or_else(|| {
                        Error::Compile(format!("group {} (\\g<-{}>) has no entry", idx, n))
                    })?
                }
                GroupRef::RelativeFwd(n) => {
                    let idx = pc.current_group + n;
                    self.subexp_starts.get(&idx).copied().ok_or_else(|| {
                        Error::Compile(format!("group {} (\\g<+{}>) not compiled", idx, n))
                    })?
                }
            };
            match &mut self.blocks[pc.region][pc.block].term {
                Some(IrTerminator::Call { target, .. }) => *target = target_block,
                _ => panic!("expected Call terminator at backfill site"),
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Finalize
    // -----------------------------------------------------------------------

    fn finish(self) -> IrProgram {
        let regions: Vec<IrRegion> = self
            .blocks
            .into_iter()
            .zip(self.region_kinds)
            .map(|(blocks, kind)| {
                let ir_blocks: Vec<IrBlock> = blocks
                    .into_iter()
                    .map(|bd| IrBlock {
                        stmts: bd.stmts,
                        term: bd.term.expect("block missing terminator"),
                    })
                    .collect();
                IrRegion {
                    blocks: ir_blocks,
                    entry: 0,
                    kind,
                }
            })
            .collect();

        IrProgram {
            regions,
            charsets: self.charsets,
            alt_tries: self.alt_tries,
            num_captures: self.num_groups as usize * 2,
            num_counters: self.num_counters as usize,
            num_null_checks: self.num_null_checks as usize,
            use_memo: self.use_memo,
            named_groups: self.named_groups,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn build(
    node: &Node,
    named_groups: Vec<(String, u32)>,
    opts: CompileOptions,
) -> Result<IrProgram, Error> {
    let base_flags = Flags {
        ignore_case: opts.ignore_case,
        multiline: opts.multiline,
        ..Default::default()
    };

    let mut builder = IrBuilder::new(named_groups);
    let main_rid = builder.new_region(RegionKind::Main);
    debug_assert_eq!(main_rid, 0);

    let match_block = builder.new_block(main_rid);
    builder.set_term(main_rid, match_block, IrTerminator::Match);

    builder.build_node_into(main_rid, 0, node, base_flags, false, match_block)?;
    builder.backfill_calls()?;

    Ok(builder.finish())
}
