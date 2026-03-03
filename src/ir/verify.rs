//! Minimal IR verifier (debug builds).

use crate::ir::{BlockId, IrGuard, IrProgram, IrTerminator, RegionId};

pub fn verify(prog: &IrProgram) -> Result<(), String> {
    let num_regions = prog.regions.len();

    if num_regions == 0 {
        return Err("IrProgram has no regions".into());
    }

    for (rid, region) in prog.regions.iter().enumerate() {
        let num_blocks = region.blocks.len();
        for (bid, block) in region.blocks.iter().enumerate() {
            check_term(rid, bid, &block.term, num_blocks, num_regions)?;
        }
    }
    Ok(())
}

fn check_block_ref(
    rid: RegionId,
    bid: BlockId,
    b: BlockId,
    num_blocks: usize,
) -> Result<(), String> {
    if b >= num_blocks {
        return Err(format!(
            "region {} block {}: invalid BlockId {} (num_blocks={})",
            rid, bid, b, num_blocks
        ));
    }
    Ok(())
}

fn check_region_ref(
    rid: RegionId,
    bid: BlockId,
    r: RegionId,
    num_regions: usize,
) -> Result<(), String> {
    if r >= num_regions {
        return Err(format!(
            "region {} block {}: invalid RegionId {} (num_regions={})",
            rid, bid, r, num_regions
        ));
    }
    Ok(())
}

fn check_term(
    rid: RegionId,
    bid: BlockId,
    term: &IrTerminator,
    num_blocks: usize,
    num_regions: usize,
) -> Result<(), String> {
    match term {
        IrTerminator::Match | IrTerminator::RegionEnd => {}
        IrTerminator::Branch(b) => check_block_ref(rid, bid, *b, num_blocks)?,
        IrTerminator::Fork { candidates, .. } => {
            for cand in candidates {
                check_block_ref(rid, bid, cand.block, num_blocks)?;
                if let IrGuard::LookAround { body, .. } = &cand.guard {
                    check_region_ref(rid, bid, *body, num_regions)?;
                }
            }
        }
        IrTerminator::SpanChar { exit, .. } => check_block_ref(rid, bid, *exit, num_blocks)?,
        IrTerminator::SpanClass { exit, .. } => check_block_ref(rid, bid, *exit, num_blocks)?,
        IrTerminator::NullCheckEnd { exit, cont, .. } => {
            check_block_ref(rid, bid, *exit, num_blocks)?;
            check_block_ref(rid, bid, *cont, num_blocks)?;
        }
        IrTerminator::CounterNext { body, exit, .. } => {
            check_block_ref(rid, bid, *body, num_blocks)?;
            check_block_ref(rid, bid, *exit, num_blocks)?;
        }
        IrTerminator::Call { target, ret } => {
            check_block_ref(rid, bid, *target, num_blocks)?;
            check_block_ref(rid, bid, *ret, num_blocks)?;
        }
        IrTerminator::RetIfCalled { fallthrough } => {
            check_block_ref(rid, bid, *fallthrough, num_blocks)?;
        }
        IrTerminator::Atomic { body, next } => {
            check_region_ref(rid, bid, *body, num_regions)?;
            check_block_ref(rid, bid, *next, num_blocks)?;
        }
        IrTerminator::Absence { inner, next } => {
            check_region_ref(rid, bid, *inner, num_regions)?;
            check_block_ref(rid, bid, *next, num_blocks)?;
        }
    }
    Ok(())
}
