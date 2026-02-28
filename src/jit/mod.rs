//! JIT compilation for Aigumo.
//!
//! Enabled with `--features jit`.  Compiles a `Vec<Inst>` to native machine
//! code via Cranelift.  Transparent fallback to the interpreter for ineligible
//! patterns (back-references, absence operator, `FoldSeq`, subexpression calls).

mod builder;
pub(crate) mod helpers;

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};

use crate::vm::{CharSet, ExecScratch, Inst, JitExecCtx, MemoState};

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
    fork_pc_indices: &[Option<u32>],
) -> Option<JitModule> {
    if !is_eligible(prog) {
        return None;
    }

    let module = match build_module(prog, charsets, use_memo, fork_pc_indices) {
        Ok(m) => m,
        Err(_e) => {
            #[cfg(debug_assertions)]
            {
                eprintln!("[aigumo jit] build_module error: {_e}");
                for (i, inst) in prog.iter().enumerate() {
                    eprintln!("[aigumo jit]   prog[{i}] = {inst:?}");
                }
            }
            return None;
        }
    };
    Some(module)
}

fn build_module(
    prog: &[Inst],
    charsets: &[CharSet],
    use_memo: bool,
    fork_pc_indices: &[Option<u32>],
) -> Result<JitModule, String> {
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
    let func_id = builder::build(&mut module, prog, charsets, use_memo, fork_pc_indices)?;

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
    num_null_checks: usize,
    use_memo: bool,
    memo: &mut MemoState,
    scratch: &mut ExecScratch,
    fork_pc_indices: &[Option<u32>],
    fork_pc_count: u32,
) -> Option<(usize, usize, Vec<Option<usize>>)> {
    let _ = fork_pc_indices; // used only at compile time (build_module)
    let _ = fork_pc_count; // used only at compile time (build_module)

    // Reuse scratch buffers — clear bt and reset slots instead of allocating fresh.
    // On the first call slots is empty (len=0, no heap alloc yet); resize allocates
    // exactly once.  On subsequent calls it avoids the allocation entirely.
    scratch.bt.clear();
    let slot_count = num_groups * 2;
    scratch.slots.clear();
    scratch.slots.resize(slot_count, u64::MAX);
    scratch.null_check.clear();
    scratch.null_check.resize(num_null_checks * 2, u64::MAX);

    // "Lend" the bt allocation to ctx so JIT helpers can work with raw pointers.
    // Take ownership of the internal allocation via mem::forget to prevent double-free
    // if helpers reallocate. After exec, reconstruct scratch.bt from ctx's raw parts.
    let bt_raw_data = scratch.bt.as_mut_ptr();
    let bt_raw_cap = scratch.bt.capacity();
    let old_bt = std::mem::take(&mut scratch.bt);
    std::mem::forget(old_bt);

    // Lend fork_memo raw parts to ctx.  No take/forget needed because ExecScratch
    // owns the allocation via raw parts (not a Vec), so there is no Vec destructor
    // to accidentally double-free on realloc.  We simply read the current raw parts,
    // pass them to ctx, and update scratch after the call if they changed.
    let fork_memo_raw_ptr = scratch.fork_memo_ptr;
    let fork_memo_raw_cap = scratch.fork_memo_cap;

    // Safety: null_check Vec is sized to num_null_checks and lives for the duration of this call.
    let null_check_ptr = scratch.null_check.as_mut_ptr();

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
        slots_ptr: scratch.slots.as_mut_ptr(),
        slots_len: slot_count as u64,
        keep_pos: u64::MAX,
        bt_data_ptr: bt_raw_data,
        bt_len: 0,
        bt_cap: bt_raw_cap as u64,
        memo_ptr: memo as *mut MemoState as *mut (),
        // Enable the inline fast-fail check when either the interpreter HashMap OR the
        // JIT's own dense array already has recorded failures.  The JIT never writes to
        // `memo.fork_failures`, so without the `scratch.fork_memo_len` term the flag
        // would be stuck at 0 for every call even when the dense array is populated,
        // losing the cross-position memoization benefit.
        memo_has_failures: if memo.fork_failures.is_empty() && scratch.fork_memo_len == 0 {
            0
        } else {
            1
        },
        atomic_depth: 0,
        bt_retry_count: 0,
        fork_memo_data_ptr: fork_memo_raw_ptr,
        fork_memo_len: scratch.fork_memo_len as u64,
        fork_memo_cap: fork_memo_raw_cap as u64,
        null_check_ptr,
        null_check_len: num_null_checks as u64,
    };

    let result = unsafe { (jit.func_ptr)(&mut ctx as *mut JitExecCtx as i64, start_pos as i64) };

    // Reconstruct scratch.bt from ctx's raw parts (may have changed due to realloc).
    // SAFETY: ctx.bt_data_ptr/bt_len/bt_cap are maintained correctly by all helpers.
    scratch.bt =
        unsafe { Vec::from_raw_parts(ctx.bt_data_ptr, ctx.bt_len as usize, ctx.bt_cap as usize) };
    scratch.bt.clear(); // prepare for next use

    // Update scratch's fork_memo raw parts if the JIT helper grew the allocation.
    // SAFETY: ctx.fork_memo_data_ptr/len/cap are maintained correctly by all helpers.
    // No reconstruction needed (scratch already holds the raw parts directly).
    if use_memo && (ctx.fork_memo_cap != fork_memo_raw_cap as u64) {
        // Allocation was reallocated by jit_fork_memo_record: update scratch raw parts.
        scratch.fork_memo_ptr = ctx.fork_memo_data_ptr;
        scratch.fork_memo_len = ctx.fork_memo_len as usize;
        scratch.fork_memo_cap = ctx.fork_memo_cap as usize;
    } else if use_memo {
        // No reallocation, but len may have increased (new failures recorded).
        scratch.fork_memo_len = ctx.fork_memo_len as usize;
    }

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
    let opt_slots: Vec<Option<usize>> = scratch
        .slots
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
