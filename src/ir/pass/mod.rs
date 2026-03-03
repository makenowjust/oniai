//! IR optimization pass pipeline.

pub mod dce;
pub mod merge;

use crate::ir::IrProgram;

#[allow(dead_code)]
pub fn run_passes(prog: &mut IrProgram) {
    for region in &mut prog.regions {
        dce::run(region);
        merge::run(region);
    }
}
