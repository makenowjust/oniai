//! JIT compilation for Aigumo.
//!
//! Enabled with `--features jit`.  Compiles a `Vec<Inst>` to native machine
//! code via Cranelift.  Transparent fallback to the interpreter for ineligible
//! patterns (back-references, absence operator, `FoldSeq`, subexpression calls).

mod builder;
pub(crate) mod helpers;

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};

use crate::vm::{BtJit, CharSet, Inst, JitExecCtx, MemoState};

// ---------------------------------------------------------------------------
// JitModule
// ---------------------------------------------------------------------------

/// A JIT-compiled exec function for a single regex program.
///
/// Holds the `JITModule` (which owns the executable memory) and the raw
/// function pointer produced by Cranelift.
pub(crate) struct JitModule {
    /// Owns the executable memory; must outlive `func_ptr`.
    _module: JITModule,
    /// The compiled `fn(ctx_ptr: i64, start_pos: i64) -> i64` function.
    func_ptr: unsafe extern "C" fn(i64, i64) -> i64,
}

// SAFETY: the compiled function is stateless (all mutable state is accessed
// through the `ctx_ptr` argument) and the underlying JITModule memory is
// immutable after `finalize_definitions()`.
unsafe impl Send for JitModule {}
unsafe impl Sync for JitModule {}

// ---------------------------------------------------------------------------
// Eligibility check
// ---------------------------------------------------------------------------

/// Returns `true` if every instruction in `prog` can be handled by the Phase-1
/// JIT.  Ineligible patterns fall back to the interpreter transparently.
pub(crate) fn is_eligible(prog: &[Inst]) -> bool {
    prog.iter().all(|inst| {
        !matches!(
            inst,
            // Back-references — memo disabled; outcome depends on captured text.
            Inst::BackRef(..)
            | Inst::BackRefRelBack(..)
            // Multi-codepoint Unicode case folding — Phase 2.
            | Inst::FoldSeq(_)
            | Inst::FoldSeqBack(_)
            // Subexpression calls — Phase 2.
            | Inst::Call(_)
            | Inst::Ret
            | Inst::RetIfCalled
            // Absence operator — Phase 3.
            | Inst::AbsenceStart(_)
            | Inst::AbsenceEnd
        )
    })
}

// ---------------------------------------------------------------------------
// Compilation entry point
// ---------------------------------------------------------------------------

/// Attempt to JIT-compile `prog`.  Returns `None` on failure (e.g. the
/// program is ineligible or Cranelift encounters an error).
pub(crate) fn try_compile(
    prog: &[Inst],
    charsets: &[CharSet],
    use_memo: bool,
) -> Option<JitModule> {
    if !is_eligible(prog) {
        return None;
    }

    let module = build_module(prog, charsets, use_memo).ok()?;
    Some(module)
}

fn build_module(prog: &[Inst], charsets: &[CharSet], use_memo: bool) -> Result<JitModule, String> {
    // ---- ISA / flags ----
    let mut flag_builder = settings::builder();
    flag_builder.set("use_colocated_libcalls", "false").unwrap();
    flag_builder.set("is_pic", "false").unwrap();
    flag_builder.set("opt_level", "speed").unwrap();
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder().map_err(|e| format!("ISA builder: {e}"))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| format!("ISA finish: {e}"))?;

    // ---- JIT builder with helper symbols ----
    let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    helpers::register_symbols(&mut jit_builder);

    let mut module = JITModule::new(jit_builder);

    // ---- compile function ----
    let func_id = builder::build(&mut module, prog, charsets, use_memo)?;

    module
        .finalize_definitions()
        .map_err(|e| format!("finalize: {e}"))?;

    let raw_ptr = module.get_finalized_function(func_id);
    let func_ptr: unsafe extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(raw_ptr) };

    Ok(JitModule {
        _module: module,
        func_ptr,
    })
}

// ---------------------------------------------------------------------------
// JIT execution entry point (called from vm.rs try_at)
// ---------------------------------------------------------------------------

/// Execute the JIT-compiled function for one candidate start position.
///
/// Returns `Some((match_start, match_end, slots))` on success.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_jit(
    jit: &JitModule,
    prog: &[Inst],
    charsets: &[CharSet],
    text: &str,
    start_pos: usize,
    num_groups: usize,
    use_memo: bool,
    memo: &mut MemoState,
) -> Option<(usize, usize, Vec<Option<usize>>)> {
    // Initialise mutable per-exec state on the stack.
    let mut slots: Vec<u64> = vec![u64::MAX; num_groups * 2];
    let mut bt_stack: Vec<BtJit> = Vec::new();

    let mut ctx = JitExecCtx {
        text_ptr: text.as_ptr(),
        text_len: text.len() as u64,
        search_start: start_pos as u64,
        use_memo: use_memo as u64,
        charsets_ptr: charsets.as_ptr() as *const (),
        charsets_len: charsets.len() as u64,
        prog_ptr: prog.as_ptr() as *const (),
        prog_len: prog.len() as u64,
        num_groups: num_groups as u64,
        slots_ptr: slots.as_mut_ptr(),
        slots_len: slots.len() as u64,
        keep_pos: u64::MAX,
        bt_ptr: &mut bt_stack as *mut Vec<BtJit> as *mut (),
        memo_ptr: memo as *mut MemoState as *mut (),
        atomic_depth: 0,
    };

    let result = unsafe { (jit.func_ptr)(&mut ctx as *mut JitExecCtx as i64, start_pos as i64) };

    if result < 0 {
        return None;
    }

    let end_pos = result as usize;
    let match_start = if ctx.keep_pos == u64::MAX {
        start_pos
    } else {
        ctx.keep_pos as usize
    };

    // Convert slots from u64 (u64::MAX = None) to Option<usize>.
    let opt_slots: Vec<Option<usize>> = slots
        .iter()
        .map(|&v| {
            if v == u64::MAX {
                None
            } else {
                Some(v as usize)
            }
        })
        .collect();

    Some((match_start, end_pos, opt_slots))
}
