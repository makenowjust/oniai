//! Prefilter analysis: derive first-byte constraints from IrProgram.

use crate::vm::CharSet;
use super::{IrGuard, IrProgram, IrStmt, IrTerminator};

/// Walk the main region to extract the set of ASCII bytes that can appear at
/// `text[pos]` when the NFA starts at position `pos`.  Returns `None` if the
/// analysis cannot determine a restriction (conservative).
///
/// The returned `[u64; 2]` is the same 128-bit ASCII bitmap format used by
/// `CharSet::ascii_bits` and `StartStrategy::AsciiClassStart`.
pub fn first_byte_set(prog: &IrProgram) -> Option<[u64; 2]> {
    first_byte_set_block(prog, 0, prog.regions[0].entry, &mut 0)
}

fn char_bit(c: char) -> Option<[u64; 2]> {
    if !c.is_ascii() {
        return None;
    }
    let b = c as u8;
    let mut bits = [0u64; 2];
    bits[(b >> 6) as usize] |= 1u64 << (b & 63);
    Some(bits)
}

fn union(a: [u64; 2], b: [u64; 2]) -> [u64; 2] {
    [a[0] | b[0], a[1] | b[1]]
}

fn class_bits(cs: &CharSet) -> Option<[u64; 2]> {
    if cs.negate {
        return None; // negated class can match many bytes, bail out
    }
    // Non-ASCII ranges → bail out
    if cs.ranges.last().is_some_and(|&(_, hi)| hi as u32 >= 128) {
        return None;
    }
    Some(cs.ascii_bits)
}

fn first_byte_set_block(
    prog: &IrProgram,
    region_idx: usize,
    block_id: usize,
    depth: &mut u32,
) -> Option<[u64; 2]> {
    if *depth > 8 {
        return None;
    }
    *depth += 1;
    let block = &prog.regions[region_idx].blocks[block_id];

    // Walk stmts; skip zero-width ones, stop at first character-consuming stmt.
    for stmt in &block.stmts {
        match stmt {
            // Zero-width: skip.
            IrStmt::SaveCapture(_)
            | IrStmt::KeepStart
            | IrStmt::CounterInit(_)
            | IrStmt::NullCheckBegin(_)
            | IrStmt::CheckAnchor(_, _) => continue,
            // Character-consuming statements:
            IrStmt::MatchChar(c) => return char_bit(*c),
            IrStmt::MatchClass {
                id,
                ignore_case: false,
            } => return class_bits(&prog.charsets[*id]),
            // Anything else (AnyChar, FoldSeq, BackRef, Back matchers, AltTrie, ignore_case Class) → bail.
            _ => return None,
        }
    }

    // All stmts were zero-width; analyse the terminator.
    match &block.term {
        IrTerminator::Branch(b) => first_byte_set_block(prog, region_idx, *b, depth),
        IrTerminator::Fork { candidates, .. } => {
            let mut bits = [0u64; 2];
            for cand in candidates {
                let cand_bits = match &cand.guard {
                    IrGuard::Always => {
                        first_byte_set_block(prog, region_idx, cand.block, depth)?
                    }
                    IrGuard::Char(c) => char_bit(*c)?,
                    IrGuard::Class { id, ignore_case: false } => {
                        class_bits(&prog.charsets[*id])?
                    }
                    _ => return None,
                };
                bits = union(bits, cand_bits);
            }
            Some(bits)
        }
        // SpanChar/SpanClass can match zero chars; need first byte of exit too.
        IrTerminator::SpanChar { c, exit } => {
            let c_bits = char_bit(*c)?;
            let exit_bits = first_byte_set_block(prog, region_idx, *exit, depth)?;
            Some(union(c_bits, exit_bits))
        }
        IrTerminator::SpanClass { id, exit } => {
            let cs = &prog.charsets[*id];
            let cs_bits = class_bits(cs)?;
            let exit_bits = first_byte_set_block(prog, region_idx, *exit, depth)?;
            Some(union(cs_bits, exit_bits))
        }
        // Match at any position, or complex terminators → bail.
        _ => None,
    }
}
