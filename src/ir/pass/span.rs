//! Span detection pass.
//!
//! Converts eligible greedy loops of the form
//! `Fork{[{_, body_block}, {Always, exit_block}]}` to `SpanChar{c, exit}` or
//! `SpanClass{id, exit}` when the body block has exactly one consuming stmt
//! and the body character set is disjoint from the exit continuation.

use crate::ir::{IrBlock, IrGuard, IrRegion, IrStmt, IrTerminator};
use crate::vm::CharSet;

pub fn run(region: &mut IrRegion, charsets: &[CharSet]) {
    for bid in 0..region.blocks.len() {
        let term = region.blocks[bid].term.clone();
        let IrTerminator::Fork { ref candidates, .. } = term else {
            continue;
        };

        if candidates.len() != 2 {
            continue;
        }

        let body_cand = &candidates[0];
        let exit_cand = &candidates[1];

        // Exit candidate must be unconditional
        if !matches!(exit_cand.guard, IrGuard::Always) {
            continue;
        }

        let body_blk = body_cand.block;
        let exit_blk = exit_cand.block;

        // Body block must have exactly one stmt
        if region.blocks[body_blk].stmts.len() != 1 {
            continue;
        }

        // Body block must loop back to the current fork block
        if !matches!(region.blocks[body_blk].term, IrTerminator::Branch(b) if b == bid) {
            continue;
        }

        let body_stmt = region.blocks[body_blk].stmts[0].clone();
        let exit_first = first_consuming_stmt(&region.blocks[exit_blk]);

        match body_stmt {
            IrStmt::MatchChar(c) => {
                if char_disjoint(c, &exit_first, charsets) {
                    region.blocks[bid].term = IrTerminator::SpanChar { c, exit: exit_blk };
                }
            }
            IrStmt::MatchClass {
                id,
                ignore_case: false,
            } => {
                if class_disjoint(id, &exit_first, charsets) {
                    region.blocks[bid].term = IrTerminator::SpanClass { id, exit: exit_blk };
                }
            }
            _ => {}
        }
    }
}

enum ExitFirst {
    Terminal,
    Char(char),
    Class(usize),
    Unknown,
}

fn first_consuming_stmt(block: &IrBlock) -> ExitFirst {
    for stmt in &block.stmts {
        match stmt {
            IrStmt::MatchChar(c) => return ExitFirst::Char(*c),
            IrStmt::MatchClass {
                id,
                ignore_case: false,
            } => return ExitFirst::Class(*id),
            IrStmt::SaveCapture(_) | IrStmt::KeepStart => continue,
            _ => return ExitFirst::Unknown,
        }
    }
    match &block.term {
        IrTerminator::Match | IrTerminator::RegionEnd => ExitFirst::Terminal,
        _ => ExitFirst::Unknown,
    }
}

fn char_disjoint(c: char, exit_first: &ExitFirst, charsets: &[CharSet]) -> bool {
    match exit_first {
        ExitFirst::Terminal => true,
        ExitFirst::Char(c2) => c != *c2,
        ExitFirst::Class(idx) => !charsets[*idx].matches(c),
        ExitFirst::Unknown => false,
    }
}

fn class_disjoint(idx: usize, exit_first: &ExitFirst, charsets: &[CharSet]) -> bool {
    let body_cs = &charsets[idx];
    match exit_first {
        ExitFirst::Terminal => true,
        ExitFirst::Char(c) => !body_cs.matches(*c),
        ExitFirst::Class(idx2) => {
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
        ExitFirst::Unknown => false,
    }
}
