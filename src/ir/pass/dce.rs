//! Dead block elimination pass (no-op initial implementation).

use crate::ir::IrRegion;

pub fn run(_region: &mut IrRegion) {
    // TODO: remove blocks with no predecessors
}
