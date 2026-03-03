//! Block merging pass (no-op initial implementation).

use crate::ir::IrRegion;

pub fn run(_region: &mut IrRegion) {
    // TODO: merge block A into B when A has exactly one successor B
    // and B has exactly one predecessor A, and A's terminator is Branch(B)
}
