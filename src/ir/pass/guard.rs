//! Fork guard analysis pass.
//!
//! For each `Fork` terminator, for each candidate whose block's first
//! consuming stmt is `MatchChar(c)` (and guard is currently `Always`),
//! set `guard = IrGuard::Char(c)`.

use crate::ir::{IrBlock, IrForkCandidate, IrGuard, IrRegion, IrStmt, IrTerminator};

pub fn run(region: &mut IrRegion) {
    for bid in 0..region.blocks.len() {
        let term = region.blocks[bid].term.clone();
        if let IrTerminator::Fork {
            candidates,
            disjoint,
            live_slots,
        } = term
        {
            let n = candidates.len();
            let new_candidates: Vec<IrForkCandidate> = candidates
                .into_iter()
                .enumerate()
                .map(|(i, mut cand)| {
                    // Only set guards on non-last candidates: the last candidate is
                    // lowered inline without a Fork instruction, so its guard is
                    // never used by the VM. Setting guards on it would prevent other
                    // passes (e.g. span detection) from firing.
                    if i + 1 < n
                        && matches!(cand.guard, IrGuard::Always)
                        && let Some(c) = first_char_of_block(&region.blocks[cand.block])
                    {
                        cand.guard = IrGuard::Char(c);
                    }
                    cand
                })
                .collect();
            region.blocks[bid].term = IrTerminator::Fork {
                candidates: new_candidates,
                disjoint,
                live_slots,
            };
        }
    }
}

fn first_char_of_block(block: &IrBlock) -> Option<char> {
    for stmt in &block.stmts {
        match stmt {
            IrStmt::MatchChar(c) => return Some(*c),
            // Zero-width: skip over these
            IrStmt::SaveCapture(_)
            | IrStmt::KeepStart
            | IrStmt::NullCheckBegin(_)
            | IrStmt::CounterInit(_)
            | IrStmt::CheckAnchor(..) => continue,
            // Anything else: stop
            _ => return None,
        }
    }
    None
}
