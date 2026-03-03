//! IrProgram → Vec<Inst> lowering (compatibility path).
//!
//! All regions are lowered INLINE into a single flat `Vec<Inst>`. Sub-regions
//! (atomic bodies, lookaround bodies, absence inner programs) are emitted
//! immediately after their start markers within the same flat stream.

use crate::compile::CompiledProgram;
use crate::ir::{BlockId, IrGuard, IrProgram, IrStmt, IrTerminator, RegionId, RegionKind};
use crate::vm::Inst;
use std::collections::HashMap;

const NO_PC: usize = usize::MAX;

// ---------------------------------------------------------------------------
// Deferred patches
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Patch {
    /// prog[pc] is a Jump(0); patch to block_pcs[region][block]
    #[allow(dead_code)]
    Jump {
        pc: usize,
        region: RegionId,
        block: BlockId,
    },
    /// prog[pc] is NullCheckEnd{exit_pc:0}; patch exit_pc to block_pcs[region][block]
    NullCheckExit {
        pc: usize,
        region: RegionId,
        block: BlockId,
    },
    /// prog[pc] is Call(0); patch to block_pcs[region][block]
    CallTarget {
        pc: usize,
        region: RegionId,
        block: BlockId,
    },
}

// ---------------------------------------------------------------------------
// Lowerer
// ---------------------------------------------------------------------------

struct Lowerer<'a> {
    prog: Vec<Inst>,
    program: &'a IrProgram,
    /// block_pcs[region][block] = start PC of this block (NO_PC if not yet visited)
    block_pcs: Vec<Vec<usize>>,
    visited: Vec<Vec<bool>>,
    patches: Vec<Patch>,
}

impl<'a> Lowerer<'a> {
    fn new(program: &'a IrProgram) -> Self {
        let block_pcs: Vec<Vec<usize>> = program
            .regions
            .iter()
            .map(|r| vec![NO_PC; r.blocks.len()])
            .collect();
        let visited: Vec<Vec<bool>> = program
            .regions
            .iter()
            .map(|r| vec![false; r.blocks.len()])
            .collect();
        Lowerer {
            prog: Vec::new(),
            program,
            block_pcs,
            visited,
            patches: Vec::new(),
        }
    }

    fn lower_all(&mut self) {
        self.lower_region(0);
    }

    /// Lower all reachable blocks of a region starting from its entry.
    /// Returns the PC of the last reachable instruction (the region-end
    /// marker: `AtomicEnd`, `LookEnd`, `AbsenceEnd`, or `Match`).
    ///
    /// Unreachable blocks (DCE-tagged dead code) are NOT emitted here:
    /// emitting them between the region-end marker and the parent continuation
    /// would corrupt `pc += 1` fall-through semantics for `AtomicEnd`/`LookEnd`.
    fn lower_region(&mut self, rid: RegionId) -> usize {
        let entry = self.program.regions[rid].entry;
        self.lower_block(rid, entry);
        self.prog.len().saturating_sub(1)
    }

    fn lower_block(&mut self, rid: RegionId, bid: BlockId) {
        if self.visited[rid][bid] {
            return;
        }
        self.visited[rid][bid] = true;
        self.block_pcs[rid][bid] = self.prog.len();

        // Emit statements
        let block = &self.program.regions[rid].blocks[bid];
        let stmts = block.stmts.clone();
        let term = block.term.clone();
        for stmt in &stmts {
            self.emit_stmt(stmt);
        }
        self.lower_term(rid, &term);
    }

    fn emit_stmt(&mut self, stmt: &IrStmt) {
        match stmt {
            IrStmt::MatchChar(c) => self.prog.push(Inst::Char(*c)),
            IrStmt::MatchAnyChar { dotall } => self.prog.push(Inst::AnyChar(*dotall)),
            IrStmt::MatchClass { id, ignore_case } => {
                self.prog.push(Inst::Class(*id, *ignore_case))
            }
            IrStmt::MatchCharBack(c) => self.prog.push(Inst::CharBack(*c)),
            IrStmt::MatchAnyCharBack { dotall } => self.prog.push(Inst::AnyCharBack(*dotall)),
            IrStmt::MatchClassBack { id, ignore_case } => {
                self.prog.push(Inst::ClassBack(*id, *ignore_case))
            }
            IrStmt::MatchFoldSeq(folded) => self.prog.push(Inst::FoldSeq(folded.clone())),
            IrStmt::MatchFoldSeqBack(folded) => self.prog.push(Inst::FoldSeqBack(folded.clone())),
            IrStmt::MatchAltTrie(idx) => self.prog.push(Inst::AltTrie(*idx)),
            IrStmt::MatchAltTrieBack(idx) => self.prog.push(Inst::AltTrieBack(*idx)),
            IrStmt::CheckAnchor(kind, flags) => self.prog.push(Inst::Anchor(*kind, *flags)),
            IrStmt::CheckBackRef {
                group,
                ignore_case,
                level,
            } => self.prog.push(Inst::BackRef(*group, *ignore_case, *level)),
            IrStmt::SaveCapture(slot) => self.prog.push(Inst::Save(*slot)),
            IrStmt::KeepStart => self.prog.push(Inst::KeepStart),
            IrStmt::CounterInit(slot) => self.prog.push(Inst::RepeatInit { slot: *slot }),
            IrStmt::NullCheckBegin(slot) => self.prog.push(Inst::NullCheckStart(*slot)),
        }
    }

    fn lower_term(&mut self, rid: RegionId, term: &IrTerminator) {
        match term {
            IrTerminator::Match => {
                self.prog.push(Inst::Match);
            }

            IrTerminator::RegionEnd => {
                // Emit the appropriate end marker based on the region kind
                let kind = self.program.regions[rid].kind;
                match kind {
                    RegionKind::LookAhead { .. } | RegionKind::LookBehind { .. } => {
                        self.prog.push(Inst::LookEnd);
                    }
                    RegionKind::Atomic => {
                        self.prog.push(Inst::AtomicEnd);
                    }
                    RegionKind::Absence => {
                        self.prog.push(Inst::AbsenceEnd);
                    }
                    _ => {
                        // Main / Subroutine: shouldn't have RegionEnd normally
                        self.prog.push(Inst::Match);
                    }
                }
            }

            IrTerminator::Branch(b) => {
                if !self.visited[rid][*b] {
                    // Target block is not yet emitted: lower it inline (fallthrough).
                    // No Jump needed — the target will be the very next instruction.
                    self.lower_block(rid, *b);
                } else {
                    // Target block was already emitted elsewhere: need a Jump.
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let target_pc = self.block_pcs[rid][*b];
                    debug_assert_ne!(target_pc, NO_PC);
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = target_pc,
                        _ => unreachable!(),
                    }
                }
            }

            IrTerminator::Fork { candidates, .. } => {
                self.lower_fork(rid, candidates);
            }

            IrTerminator::SpanChar { c, exit } => {
                let span_pc = self.prog.len();
                self.prog.push(Inst::SpanChar { c: *c, exit_pc: 0 });
                if !self.visited[rid][*exit] {
                    self.lower_block(rid, *exit);
                }
                let exit_pc = self.block_pcs[rid][*exit];
                match &mut self.prog[span_pc] {
                    Inst::SpanChar { exit_pc: ep, .. } => *ep = exit_pc,
                    _ => unreachable!(),
                }
            }

            IrTerminator::SpanClass { id, exit } => {
                let span_pc = self.prog.len();
                self.prog.push(Inst::SpanClass {
                    idx: *id,
                    exit_pc: 0,
                });
                if !self.visited[rid][*exit] {
                    self.lower_block(rid, *exit);
                }
                let exit_pc = self.block_pcs[rid][*exit];
                match &mut self.prog[span_pc] {
                    Inst::SpanClass { exit_pc: ep, .. } => *ep = exit_pc,
                    _ => unreachable!(),
                }
            }

            IrTerminator::NullCheckEnd {
                slot,
                exit,
                cont: nc_cont,
            } => {
                let nc_end_pc = self.prog.len();
                self.prog.push(Inst::NullCheckEnd {
                    slot: *slot,
                    exit_pc: 0,
                });
                // Jump back to null_check_block (cont); it must already be visited
                let nc_pc = self.block_pcs[rid][*nc_cont];
                debug_assert_ne!(nc_pc, NO_PC, "null_check cont block not yet visited");
                self.prog.push(Inst::Jump(nc_pc));
                // exit block will be lowered later (it's the Fork's second candidate);
                // register deferred patch
                self.patches.push(Patch::NullCheckExit {
                    pc: nc_end_pc,
                    region: rid,
                    block: *exit,
                });
                // Don't lower exit here — it will be lowered as the Fork's c1
            }

            IrTerminator::CounterNext {
                slot,
                count,
                body,
                exit,
            } => {
                // body must already be visited (it's the loop body, lowered before the loop end)
                let body_pc = self.block_pcs[rid][*body];
                debug_assert_ne!(body_pc, NO_PC, "counter body block not yet visited");
                self.prog.push(Inst::RepeatNext {
                    slot: *slot,
                    count: *count,
                    body_pc,
                });
                // Fall through to exit block
                if !self.visited[rid][*exit] {
                    self.lower_block(rid, *exit);
                } else {
                    // emit jump to exit
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let ep = self.block_pcs[rid][*exit];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = ep,
                        _ => unreachable!(),
                    }
                }
            }

            IrTerminator::Call { target, ret } => {
                let call_pc = self.prog.len();
                self.prog.push(Inst::Call(0));
                // target may not be visited yet — use deferred patch
                if self.visited[rid][*target] {
                    let tpc = self.block_pcs[rid][*target];
                    match &mut self.prog[call_pc] {
                        Inst::Call(t) => *t = tpc,
                        _ => unreachable!(),
                    }
                } else {
                    self.patches.push(Patch::CallTarget {
                        pc: call_pc,
                        region: rid,
                        block: *target,
                    });
                }
                // Continue with ret block inline
                if !self.visited[rid][*ret] {
                    self.lower_block(rid, *ret);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let rp = self.block_pcs[rid][*ret];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = rp,
                        _ => unreachable!(),
                    }
                }
            }

            IrTerminator::RetIfCalled { fallthrough } => {
                self.prog.push(Inst::RetIfCalled);
                if !self.visited[rid][*fallthrough] {
                    self.lower_block(rid, *fallthrough);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let fp = self.block_pcs[rid][*fallthrough];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = fp,
                        _ => unreachable!(),
                    }
                }
            }

            IrTerminator::Atomic {
                body: body_rid,
                next,
            } => {
                let atomic_start_pc = self.prog.len();
                self.prog.push(Inst::AtomicStart(0));
                // Lower atomic body region inline; get the PC of AtomicEnd.
                let atomic_end_pc = self.lower_region(*body_rid);
                // Patch AtomicStart.end_pc = atomic_end_pc
                match &mut self.prog[atomic_start_pc] {
                    Inst::AtomicStart(ep) => *ep = atomic_end_pc,
                    _ => unreachable!(),
                }
                // Continue with next block
                if !self.visited[rid][*next] {
                    self.lower_block(rid, *next);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let np = self.block_pcs[rid][*next];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = np,
                        _ => unreachable!(),
                    }
                }
            }

            IrTerminator::Absence {
                inner: inner_rid,
                next,
            } => {
                let absence_start_pc = self.prog.len();
                self.prog.push(Inst::AbsenceStart(0));
                let absence_end_pc = self.lower_region(*inner_rid);
                match &mut self.prog[absence_start_pc] {
                    Inst::AbsenceStart(ep) => *ep = absence_end_pc,
                    _ => unreachable!(),
                }
                if !self.visited[rid][*next] {
                    self.lower_block(rid, *next);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let np = self.block_pcs[rid][*next];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = np,
                        _ => unreachable!(),
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Fork chain lowering
    // -----------------------------------------------------------------------

    fn lower_fork(&mut self, rid: RegionId, candidates: &[crate::ir::IrForkCandidate]) {
        let n = candidates.len();
        if n == 0 {
            return;
        }
        if n == 1 {
            let cand = &candidates[0];
            // Single candidate — could be LookAround, GroupMatched, or Always
            self.lower_single_candidate(rid, cand);
            return;
        }

        // Check for CheckGroup pattern: [{GroupMatched(s), yes}, {Always, no}]
        if n == 2
            && let IrGuard::GroupMatched(slot) = candidates[0].guard
            && matches!(candidates[1].guard, IrGuard::Always)
        {
            self.lower_check_group(rid, slot, candidates[0].block, candidates[1].block);
            return;
        }

        // General N-way fork chain using Fork(alt, None) instructions
        // For each candidate 0..n-2: emit Fork(0) then lower inline; patch fork's alt
        // For candidate n-1: lower inline directly
        let (non_last, last) = candidates.split_at(n - 1);

        for cand in non_last {
            let fork_pc = self.prog.len();
            let gc = extract_char_guard(&cand.guard);
            self.prog.push(Inst::Fork(0, gc)); // placeholder alt
            let bid = cand.block;
            if !self.visited[rid][bid] {
                self.lower_block(rid, bid);
            } else {
                // Already visited: jump to it
                let jump_pc = self.prog.len();
                self.prog.push(Inst::Jump(0));
                let tp = self.block_pcs[rid][bid];
                match &mut self.prog[jump_pc] {
                    Inst::Jump(t) => *t = tp,
                    _ => unreachable!(),
                }
            }
            // Patch Fork's alt to current PC (= start of next candidate)
            let alt_pc = self.prog.len();
            match &mut self.prog[fork_pc] {
                Inst::Fork(alt, _) => *alt = alt_pc,
                _ => unreachable!(),
            }
        }

        // Last candidate: no Fork header
        let bid = last[0].block;
        if !self.visited[rid][bid] {
            self.lower_block(rid, bid);
        } else {
            let jump_pc = self.prog.len();
            self.prog.push(Inst::Jump(0));
            let tp = self.block_pcs[rid][bid];
            match &mut self.prog[jump_pc] {
                Inst::Jump(t) => *t = tp,
                _ => unreachable!(),
            }
        }
    }

    fn lower_single_candidate(&mut self, rid: RegionId, cand: &crate::ir::IrForkCandidate) {
        match &cand.guard {
            IrGuard::LookAround { pol, dir: _, body } => {
                let positive = *pol == crate::ast::LookPol::Positive;
                let look_start_pc = self.prog.len();
                self.prog.push(Inst::LookStart {
                    positive,
                    end_pc: 0,
                });
                // Lower lookaround body region inline; get the PC of LookEnd.
                let look_end_pc = self.lower_region(*body);
                match &mut self.prog[look_start_pc] {
                    Inst::LookStart { end_pc, .. } => *end_pc = look_end_pc,
                    _ => unreachable!(),
                }
                // After lookaround, lower the candidate block
                if !self.visited[rid][cand.block] {
                    self.lower_block(rid, cand.block);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let tp = self.block_pcs[rid][cand.block];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = tp,
                        _ => unreachable!(),
                    }
                }
            }
            IrGuard::GroupMatched(slot) => {
                // Single GroupMatched: unconditional check
                let check_pc = self.prog.len();
                self.prog.push(Inst::CheckGroup {
                    slot: *slot,
                    yes_pc: 0,
                    no_pc: 0,
                });
                let yes_pc = self.prog.len();
                self.lower_block(rid, cand.block);
                match &mut self.prog[check_pc] {
                    Inst::CheckGroup {
                        yes_pc: yp,
                        no_pc: np,
                        ..
                    } => {
                        *yp = yes_pc;
                        *np = yes_pc; // no path = same as yes for single candidate
                    }
                    _ => unreachable!(),
                }
            }
            IrGuard::Always => {
                // Unconditional: just lower the block
                if !self.visited[rid][cand.block] {
                    self.lower_block(rid, cand.block);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let tp = self.block_pcs[rid][cand.block];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = tp,
                        _ => unreachable!(),
                    }
                }
            }
            _ => {
                // Other guards: lower as Fork with guard, then lower the block
                let fork_pc = self.prog.len();
                let guard_char = extract_char_guard(&cand.guard);
                self.prog.push(Inst::Fork(0, guard_char)); // placeholder alt
                if !self.visited[rid][cand.block] {
                    self.lower_block(rid, cand.block);
                } else {
                    let jump_pc = self.prog.len();
                    self.prog.push(Inst::Jump(0));
                    let tp = self.block_pcs[rid][cand.block];
                    match &mut self.prog[jump_pc] {
                        Inst::Jump(t) => *t = tp,
                        _ => unreachable!(),
                    }
                }
                // No alt — this fork has no fallthrough; patch alt to current PC
                let alt_pc = self.prog.len();
                match &mut self.prog[fork_pc] {
                    Inst::Fork(alt, _) => *alt = alt_pc,
                    _ => unreachable!(),
                }
            }
        }
    }

    fn lower_check_group(
        &mut self,
        rid: RegionId,
        slot: usize,
        yes_block: BlockId,
        no_block: BlockId,
    ) {
        let check_pc = self.prog.len();
        self.prog.push(Inst::CheckGroup {
            slot,
            yes_pc: 0,
            no_pc: 0,
        });

        let yes_start = self.prog.len();
        if !self.visited[rid][yes_block] {
            self.lower_block(rid, yes_block);
        } else {
            let jump_pc = self.prog.len();
            self.prog.push(Inst::Jump(0));
            let tp = self.block_pcs[rid][yes_block];
            match &mut self.prog[jump_pc] {
                Inst::Jump(t) => *t = tp,
                _ => unreachable!(),
            }
        }

        // Jump after no branch (from yes branch)
        let jump_after_yes_pc = self.prog.len();
        self.prog.push(Inst::Jump(0));

        let no_start = self.prog.len();
        if !self.visited[rid][no_block] {
            self.lower_block(rid, no_block);
        } else {
            let jump_pc = self.prog.len();
            self.prog.push(Inst::Jump(0));
            let tp = self.block_pcs[rid][no_block];
            match &mut self.prog[jump_pc] {
                Inst::Jump(t) => *t = tp,
                _ => unreachable!(),
            }
        }

        let end_pc = self.prog.len();

        match &mut self.prog[check_pc] {
            Inst::CheckGroup { yes_pc, no_pc, .. } => {
                *yes_pc = yes_start;
                *no_pc = no_start;
            }
            _ => unreachable!(),
        }
        match &mut self.prog[jump_after_yes_pc] {
            Inst::Jump(t) => *t = end_pc,
            _ => unreachable!(),
        }
    }

    // -----------------------------------------------------------------------
    // Apply deferred patches
    // -----------------------------------------------------------------------

    fn apply_patches(&mut self) {
        let patches = std::mem::take(&mut self.patches);
        for patch in patches {
            match patch {
                Patch::Jump { pc, region, block } => {
                    let target_pc = self.block_pcs[region][block];
                    debug_assert_ne!(target_pc, NO_PC, "Jump patch: block not lowered");
                    match &mut self.prog[pc] {
                        Inst::Jump(t) => *t = target_pc,
                        _ => panic!("Jump patch on non-jump inst at {pc}"),
                    }
                }
                Patch::NullCheckExit { pc, region, block } => {
                    let exit_pc = self.block_pcs[region][block];
                    debug_assert_ne!(exit_pc, NO_PC, "NullCheckExit patch: block not lowered");
                    match &mut self.prog[pc] {
                        Inst::NullCheckEnd { exit_pc: ep, .. } => *ep = exit_pc,
                        _ => panic!("NullCheckExit patch on wrong inst at {pc}"),
                    }
                }
                Patch::CallTarget { pc, region, block } => {
                    let target_pc = self.block_pcs[region][block];
                    debug_assert_ne!(target_pc, NO_PC, "CallTarget patch: block not lowered");
                    match &mut self.prog[pc] {
                        Inst::Call(t) => *t = target_pc,
                        _ => panic!("CallTarget patch on non-call inst at {pc}"),
                    }
                }
            }
        }
    }
}

fn extract_char_guard(guard: &IrGuard) -> Option<char> {
    match guard {
        IrGuard::Char(c) => Some(*c),
        _ => None,
    }
}

/// Scan forward from `start` (skipping zero-width instructions) and promote
/// the first `Inst::Char(gc)` to `Inst::CharFast(gc)`.
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn lower(program: &IrProgram) -> CompiledProgram {
    let mut l = Lowerer::new(program);
    l.lower_all();
    l.apply_patches();

    // Promote Char(gc) → CharFast(gc) on the primary path of every guarded Fork,
    // but only when the position immediately after the Fork is exclusively
    // reachable via the Fork's fall-through (i.e. no other Jump targets it).
    // If anything else can reach fork_pc+1 without passing the guard, CharFast
    // would fire without verification — causing incorrect pos advances.
    let jump_targets: std::collections::HashSet<usize> = l
        .prog
        .iter()
        .flat_map(|inst| -> Box<dyn Iterator<Item = usize>> {
            match inst {
                Inst::Jump(t) => Box::new(std::iter::once(*t)),
                Inst::Fork(alt, _) | Inst::ForkNext(alt, _) => Box::new(std::iter::once(*alt)),
                Inst::NullCheckEnd { exit_pc, .. } => Box::new(std::iter::once(*exit_pc)),
                Inst::RepeatNext { body_pc, .. } => Box::new(std::iter::once(*body_pc)),
                Inst::LookStart { end_pc, .. } => Box::new(std::iter::once(*end_pc)),
                Inst::AtomicStart(ep) | Inst::AbsenceStart(ep) => Box::new(std::iter::once(*ep)),
                Inst::CheckGroup { yes_pc, no_pc, .. } => Box::new([*yes_pc, *no_pc].into_iter()),
                _ => Box::new(std::iter::empty()),
            }
        })
        .collect();

    let promotions: Vec<(usize, char)> = l
        .prog
        .iter()
        .enumerate()
        .filter_map(|(pc, inst)| match inst {
            Inst::Fork(_, Some(gc)) => Some((pc + 1, *gc)),
            _ => None,
        })
        .collect();
    for (start, gc) in promotions {
        // Only promote if start is exclusively reachable via the Fork fall-through.
        if !jump_targets.contains(&start) {
            promote_to_char_fast(&mut l.prog, start, gc);
        }
    }

    let num_groups = program.num_captures / 2;
    let named_groups: Vec<(String, u32)> = program.named_groups.clone();

    CompiledProgram {
        prog: l.prog,
        charsets: program.charsets.clone(),
        alt_tries: program.alt_tries.clone(),
        named_groups,
        num_groups,
        num_null_checks: program.num_null_checks,
        num_repeat_counters: program.num_counters,
        subexp_starts: HashMap::new(), // not needed after lowering
    }
}
