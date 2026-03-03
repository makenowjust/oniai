//! IR optimization pass pipeline.

pub mod dce;
pub mod guard;
pub mod liveness;
pub mod merge;
pub mod span;

use crate::ir::IrProgram;

pub fn run_passes(prog: &mut IrProgram) {
    let num_slots = prog.num_captures;
    // Clone charsets so we can borrow prog.regions mutably at the same time.
    let charsets = prog.charsets.clone();
    for region in &mut prog.regions {
        guard::run(region);
        liveness::run(region, num_slots);
        span::run(region, &charsets);
        dce::run(region);
        merge::run(region);
    }
}
