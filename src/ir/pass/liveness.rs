//! Capture liveness analysis pass.
//!
//! Removes dead `SaveCapture` stmts and tags `Fork` terminators with
//! `live_slots` (the union of live_in sets for all fork candidates).

use crate::ir::{BlockId, IrRegion, IrStmt, IrTerminator, LiveSlots};

pub fn run(region: &mut IrRegion, num_slots: usize) {
    if num_slots == 0 {
        return;
    }

    let n = region.blocks.len();
    let succs = compute_successors(region);
    let (gens, kill) = compute_gen_kill(region, num_slots);

    let all_live = {
        let mut s = LiveSlots::new();
        for i in 0..num_slots {
            s.set(i);
        }
        s
    };

    let is_terminal: Vec<bool> = (0..n)
        .map(|bid| {
            matches!(
                region.blocks[bid].term,
                IrTerminator::Match | IrTerminator::RegionEnd
            )
        })
        .collect();

    let mut live_in: Vec<LiveSlots> = vec![LiveSlots::new(); n];
    let mut live_out: Vec<LiveSlots> = (0..n)
        .map(|bid| {
            if is_terminal[bid] {
                all_live.clone()
            } else {
                LiveSlots::new()
            }
        })
        .collect();

    // Backward fixed-point iteration
    let mut changed = true;
    while changed {
        changed = false;
        for bid in (0..n).rev() {
            let new_out = if is_terminal[bid] {
                all_live.clone()
            } else {
                let mut out = LiveSlots::new();
                for &succ in &succs[bid] {
                    out.union_with(&live_in[succ]);
                }
                out
            };

            // live_in[bid] = gens[bid] | (live_out[bid] - kill[bid])
            let mut new_in = gens[bid].clone();
            for slot in 0..num_slots {
                if new_out.get(slot) && !kill[bid].get(slot) {
                    new_in.set(slot);
                }
            }

            if live_out[bid] != new_out || live_in[bid] != new_in {
                live_out[bid] = new_out;
                live_in[bid] = new_in;
                changed = true;
            }
        }
    }

    // Remove dead SaveCapture stmts via backward scan through each block
    for (bid, lo) in live_out.iter().enumerate() {
        let mut live = lo.clone();
        let stmts = std::mem::take(&mut region.blocks[bid].stmts);
        let mut kept: Vec<IrStmt> = Vec::with_capacity(stmts.len());
        for stmt in stmts.into_iter().rev() {
            match stmt {
                IrStmt::SaveCapture(s) => {
                    if live.get(s) {
                        kept.push(IrStmt::SaveCapture(s));
                    }
                    // Kill s: whether kept or dropped, s is defined here
                    live_kill(&mut live, s);
                }
                IrStmt::CheckBackRef {
                    group,
                    ignore_case,
                    level,
                } => {
                    let slot = ((group - 1) * 2) as usize;
                    live.set(slot);
                    kept.push(IrStmt::CheckBackRef {
                        group,
                        ignore_case,
                        level,
                    });
                }
                other => {
                    kept.push(other);
                }
            }
        }
        kept.reverse();
        region.blocks[bid].stmts = kept;
    }

    // Tag Fork terminators with live_slots
    for bid in 0..n {
        if let IrTerminator::Fork {
            ref mut live_slots,
            ref candidates,
            ..
        } = region.blocks[bid].term
        {
            let mut slots = LiveSlots::new();
            for cand in candidates {
                slots.union_with(&live_in[cand.block]);
            }
            *live_slots = slots;
        }
    }
}

fn live_kill(live: &mut LiveSlots, slot: usize) {
    let (w, b) = (slot / 64, slot % 64);
    if w < live.words.len() {
        live.words[w] &= !(1u64 << b);
    }
}

fn compute_successors(region: &IrRegion) -> Vec<Vec<BlockId>> {
    let n = region.blocks.len();
    let mut succs = vec![vec![]; n];
    for (bid, block) in region.blocks.iter().enumerate() {
        match &block.term {
            IrTerminator::Match | IrTerminator::RegionEnd => {}
            IrTerminator::Branch(b) => succs[bid].push(*b),
            IrTerminator::Fork { candidates, .. } => {
                for cand in candidates {
                    succs[bid].push(cand.block);
                }
            }
            IrTerminator::SpanChar { exit, .. } | IrTerminator::SpanClass { exit, .. } => {
                succs[bid].push(*exit)
            }
            IrTerminator::NullCheckEnd { exit, cont, .. } => {
                succs[bid].push(*exit);
                succs[bid].push(*cont);
            }
            IrTerminator::CounterNext { body, exit, .. } => {
                succs[bid].push(*body);
                succs[bid].push(*exit);
            }
            IrTerminator::Call { target, ret } => {
                succs[bid].push(*target);
                succs[bid].push(*ret);
            }
            IrTerminator::RetIfCalled { fallthrough } => succs[bid].push(*fallthrough),
            IrTerminator::Atomic { next, .. } | IrTerminator::Absence { next, .. } => {
                succs[bid].push(*next)
            }
        }
    }
    succs
}

fn compute_gen_kill(region: &IrRegion, _num_slots: usize) -> (Vec<LiveSlots>, Vec<LiveSlots>) {
    let n = region.blocks.len();
    let mut gens = vec![LiveSlots::new(); n];
    let mut kill = vec![LiveSlots::new(); n];
    for (bid, block) in region.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            match stmt {
                IrStmt::CheckBackRef { group, .. } => {
                    let slot = ((*group - 1) * 2) as usize;
                    if !kill[bid].get(slot) {
                        gens[bid].set(slot);
                    }
                }
                IrStmt::SaveCapture(s) => {
                    kill[bid].set(*s);
                }
                _ => {}
            }
        }
    }
    (gens, kill)
}
