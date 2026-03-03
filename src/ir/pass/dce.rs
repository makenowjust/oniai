//! Dead block elimination pass.
//!
//! Marks all blocks reachable from the region entry via DFS and replaces
//! unreachable blocks with empty no-op stubs (stmts cleared, terminator set
//! to `Branch(entry)`).  This keeps the block list length stable while
//! preventing dead code from confusing later passes or the lowering.

use crate::ir::{IrRegion, IrTerminator};

pub fn run(region: &mut IrRegion) {
    let n = region.blocks.len();
    let mut reachable = vec![false; n];
    let mut stack = vec![region.entry];

    while let Some(bid) = stack.pop() {
        if reachable[bid] {
            continue;
        }
        reachable[bid] = true;
        match &region.blocks[bid].term {
            IrTerminator::Match | IrTerminator::RegionEnd => {}
            IrTerminator::Branch(b) => stack.push(*b),
            IrTerminator::Fork { candidates, .. } => {
                for cand in candidates {
                    stack.push(cand.block);
                }
            }
            IrTerminator::SpanChar { exit, .. } | IrTerminator::SpanClass { exit, .. } => {
                stack.push(*exit)
            }
            IrTerminator::NullCheckEnd { exit, cont, .. } => {
                stack.push(*exit);
                stack.push(*cont);
            }
            IrTerminator::CounterNext { body, exit, .. } => {
                stack.push(*body);
                stack.push(*exit);
            }
            IrTerminator::Call { target, ret } => {
                stack.push(*target);
                stack.push(*ret);
            }
            IrTerminator::RetIfCalled { fallthrough } => stack.push(*fallthrough),
            IrTerminator::Atomic { next, .. } | IrTerminator::Absence { next, .. } => {
                stack.push(*next)
            }
        }
    }

    let entry = region.entry;
    for (bid, reachable) in reachable.iter().enumerate() {
        if !reachable {
            region.blocks[bid].stmts.clear();
            region.blocks[bid].term = IrTerminator::Branch(entry);
        }
    }
}
