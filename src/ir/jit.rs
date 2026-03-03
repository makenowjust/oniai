//! Direct IR → Cranelift JIT compiler.
//!
//! Compiles an `IrProgram` to native machine code without the round-trip
//! through `Vec<Inst>`.  The generated function has the same signature as the
//! `Vec<Inst>`-based JIT (`fn(ctx_ptr: i64, start_pos: i64) -> i64`) so that
//! the existing `exec_jit` driver in `jit/mod.rs` can run it unchanged.

use std::collections::{HashMap, VecDeque};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::instructions::{BlockArg, BlockCall};
use cranelift_codegen::ir::{
    AbiParam, Block, FuncRef, InstBuilder, JumpTableData, MemFlags, StackSlotData, StackSlotKind,
    TrapCode, UserFuncName, Value, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::AnchorKind;
use crate::ir::{BlockId, IrGuard, IrProgram, IrStmt, IrTerminator, RegionId, RegionKind};
use crate::vm::CharSet;

// ---------------------------------------------------------------------------
// JitExecCtx field byte offsets (must match vm.rs JitExecCtx, #[repr(C)])
// These are identical to the offsets in jit/builder.rs.
// ---------------------------------------------------------------------------

const CTX_TEXT_PTR: i32 = 0;
const CTX_TEXT_LEN: i32 = 8;
const CTX_SLOTS_PTR: i32 = 72;
const CTX_KEEP_POS: i32 = 88;
const CTX_BT_DATA_PTR: i32 = 96;
const CTX_BT_LEN: i32 = 104;
const CTX_BT_CAP: i32 = 112;
const CTX_MEMO_HAS_FAILURES: i32 = 128;
const CTX_ATOMIC_DEPTH: i32 = 136;
const CTX_BT_RETRY_COUNT: i32 = 144;
const CTX_FORK_MEMO_DATA_PTR: i32 = 152;
const CTX_FORK_MEMO_LEN: i32 = 160;
#[allow(dead_code)]
const CTX_FORK_MEMO_CAP: i32 = 168;

// BtJit layout constants (repr(C) struct, size=24)
const BTJIT_SIZE: i64 = 24;
const BTJIT_TAG_RETRY: i64 = 0;
const BTJIT_TAG_MEMO_MARK: i64 = 3;
const BTJIT_OFF_A: i32 = 4;
const BTJIT_OFF_B: i32 = 8;
const BTJIT_OFF_C: i32 = 16;

// Compile-time layout verification
const _: () = {
    use crate::vm::JitExecCtx;
    assert!(std::mem::offset_of!(JitExecCtx, text_ptr) == CTX_TEXT_PTR as usize);
    assert!(std::mem::offset_of!(JitExecCtx, text_len) == CTX_TEXT_LEN as usize);
    assert!(std::mem::offset_of!(JitExecCtx, slots_ptr) == CTX_SLOTS_PTR as usize);
    assert!(std::mem::offset_of!(JitExecCtx, keep_pos) == CTX_KEEP_POS as usize);
    assert!(std::mem::offset_of!(JitExecCtx, bt_data_ptr) == CTX_BT_DATA_PTR as usize);
    assert!(std::mem::offset_of!(JitExecCtx, bt_len) == CTX_BT_LEN as usize);
    assert!(std::mem::offset_of!(JitExecCtx, bt_cap) == CTX_BT_CAP as usize);
    assert!(std::mem::offset_of!(JitExecCtx, memo_has_failures) == CTX_MEMO_HAS_FAILURES as usize);
    assert!(std::mem::offset_of!(JitExecCtx, atomic_depth) == CTX_ATOMIC_DEPTH as usize);
    assert!(std::mem::offset_of!(JitExecCtx, bt_retry_count) == CTX_BT_RETRY_COUNT as usize);
    assert!(
        std::mem::offset_of!(JitExecCtx, fork_memo_data_ptr) == CTX_FORK_MEMO_DATA_PTR as usize
    );
    assert!(std::mem::offset_of!(JitExecCtx, fork_memo_len) == CTX_FORK_MEMO_LEN as usize);
};

// ---------------------------------------------------------------------------
// Helper-function declaration macro (identical to jit/builder.rs)
// ---------------------------------------------------------------------------

macro_rules! decl_helper {
    ($module:expr, $builder:expr, $name:literal, [ $($param:expr),* ] => [ $($ret:expr),* ]) => {{
        let mut sig = $module.make_signature();
        $(sig.params.push(AbiParam::new($param));)*
        $(sig.returns.push(AbiParam::new($ret));)*
        let fid = $module
            .declare_function($name, Linkage::Import, &sig)
            .expect(concat!("declare ", $name));
        $module.declare_func_in_func(fid, &mut $builder.func)
    }};
}

// ---------------------------------------------------------------------------
// Eligibility check
// ---------------------------------------------------------------------------

/// Returns `true` if the `IrProgram` can be compiled by this IR JIT.
///
/// Ineligible programs fall back to the `Vec<Inst>` JIT or interpreter.
pub(crate) fn is_eligible(prog: &IrProgram) -> bool {
    for region in &prog.regions {
        match region.kind {
            RegionKind::Absence | RegionKind::Subroutine { .. } => return false,
            _ => {}
        }
        for block in &region.blocks {
            for stmt in &block.stmts {
                if matches!(stmt, IrStmt::CheckBackRef { .. }) {
                    return false;
                }
            }
            match &block.term {
                IrTerminator::Call { .. }
                | IrTerminator::RetIfCalled { .. }
                | IrTerminator::Absence { .. } => return false,
                IrTerminator::Fork { candidates, .. } => {
                    for cand in candidates {
                        if matches!(cand.guard, IrGuard::LookAround { .. }) {
                            return false;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Public compilation entry point
// ---------------------------------------------------------------------------

pub(crate) fn build_from_ir(
    module: &mut cranelift_jit::JITModule,
    prog: &IrProgram,
) -> Result<FuncId, String> {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // ctx_ptr
    sig.params.push(AbiParam::new(types::I64)); // start_pos
    sig.returns.push(AbiParam::new(types::I64));

    let func_id = module
        .declare_function("jit_ir_exec", Linkage::Local, &sig)
        .map_err(|e| format!("declare jit_ir_exec: {e}"))?;

    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    ctx.func.name = UserFuncName::user(0, func_id.as_u32());

    let mut fb_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        emit_ir_function(&mut builder, module, prog)?;
        builder.finalize();
    }

    module.define_function(func_id, &mut ctx).map_err(|e| {
        #[cfg(debug_assertions)]
        {
            use cranelift_codegen::CodegenError;
            use cranelift_codegen::write::write_function;
            if let cranelift_module::ModuleError::Compilation(CodegenError::Verifier(ref errs)) = e
            {
                let mut ir_text = String::new();
                write_function(&mut ir_text, &ctx.func).ok();
                eprintln!("[oniai ir-jit] IR:\n{ir_text}");
                for err in &errs.0 {
                    eprintln!("[oniai ir-jit] verifier: {} -- {}", err.location, err.message);
                }
            }
        }
        format!("define jit_ir_exec: {e}")
    })?;
    module.clear_context(&mut ctx);
    Ok(func_id)
}

// ---------------------------------------------------------------------------
// Function body emitter
// ---------------------------------------------------------------------------

/// All helpers and context needed to emit one IR block's stmts/terminator.
/// Passed by reference through the emission helpers to avoid huge argument lists.
#[allow(clippy::too_many_arguments)]
struct EmitHelpers {
    h_match_char: FuncRef,
    h_match_class: FuncRef,
    h_match_char_back: FuncRef,
    h_match_any_char_back: FuncRef,
    h_match_class_back: FuncRef,
    h_check_anchor: FuncRef,
    h_push_save_undo: FuncRef,
    h_fork: FuncRef,
    h_bt_pop: FuncRef,
    h_fork_memo_record: FuncRef,
    h_atomic_start: FuncRef,
    h_atomic_end: FuncRef,
    h_null_check_start: FuncRef,
    h_null_check_end: FuncRef,
    h_fold_seq: FuncRef,
    h_fold_seq_back: FuncRef,
    h_match_alt_trie: FuncRef,
    h_match_alt_trie_back: FuncRef,
    h_span_char: FuncRef,
    h_span_class_ascii: FuncRef,
}

fn emit_ir_function(
    builder: &mut FunctionBuilder<'_>,
    module: &mut cranelift_jit::JITModule,
    prog: &IrProgram,
) -> Result<(), String> {
    // ===== Pre-pass: collect compiled regions =====
    //
    // Start from region 0 (Main).  BFS over Atomic sub-regions only — LookAround
    // bodies are excluded by is_eligible, so they cannot be reached here.

    let mut compiled_regions: Vec<RegionId> = vec![0];
    let mut visited = vec![false; prog.regions.len()];
    visited[0] = true;
    let mut queue: VecDeque<RegionId> = VecDeque::from([0]);
    while let Some(rid) = queue.pop_front() {
        for block in &prog.regions[rid].blocks {
            if let IrTerminator::Atomic { body, .. } = block.term
                && !visited[body] {
                    visited[body] = true;
                    compiled_regions.push(body);
                    queue.push_back(body);
                }
        }
    }

    // Assign flat block IDs: flat_offset[rid] = first flat id for that region.
    let mut flat_offset = vec![0usize; prog.regions.len()];
    let mut total_ir_blocks = 0usize;
    for &rid in &compiled_regions {
        flat_offset[rid] = total_ir_blocks;
        total_ir_blocks += prog.regions[rid].blocks.len();
    }

    // Assign fork indices: one compact index per non-first Fork candidate.
    // fork_indices[rid][bid][cand_idx - 1] = fork index (u32).
    let mut fork_indices: Vec<Vec<Vec<u32>>> = prog
        .regions
        .iter()
        .map(|r| r.blocks.iter().map(|_| Vec::new()).collect())
        .collect();
    let mut fork_pc_count: u32 = 0;
    for &rid in &compiled_regions {
        for (bid, block) in prog.regions[rid].blocks.iter().enumerate() {
            if let IrTerminator::Fork { ref candidates, .. } = block.term {
                for _ in 1..candidates.len() {
                    fork_indices[rid][bid].push(fork_pc_count);
                    fork_pc_count += 1;
                }
            }
        }
    }
    let _ = fork_pc_count;

    // atomic_exits: sub_rid → (parent_rid, parent_bid)
    // When an Atomic body's RegionEnd fires, we call h_atomic_end and jump to
    // ir_cl_blocks[parent_rid][parent_bid].
    let mut atomic_exits: HashMap<RegionId, (RegionId, BlockId)> = HashMap::new();
    for &rid in &compiled_regions {
        for block in prog.regions[rid].blocks.iter() {
            if let IrTerminator::Atomic { body, next } = block.term {
                atomic_exits.insert(body, (rid, next));
            }
        }
    }

    // ===== Create Cranelift blocks =====

    let var_pos: Variable = builder.declare_var(types::I64);
    let var_ctx: Variable = builder.declare_var(types::I64);

    let entry_block = builder.create_block();

    // One Cranelift block per (rid, bid) in every region (including non-compiled
    // ones — they'll just never be emitted/reached, which is fine).
    let ir_cl_blocks: Vec<Vec<Block>> = prog
        .regions
        .iter()
        .map(|r| (0..r.blocks.len()).map(|_| builder.create_block()).collect())
        .collect();

    let bt_resume_block = builder.create_block();
    let bt_dispatch_block = builder.create_block();
    let return_fail_block = builder.create_block();

    // Stack slots for bt dispatch (same role as in jit/builder.rs).
    let slot_block_id =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 4, 0));
    let slot_pos_out =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 0));

    // One 8-byte stack slot per IR counter (CounterInit / CounterNext).
    let counter_slots: Vec<_> = (0..prog.num_counters)
        .map(|_| {
            builder
                .create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 0))
        })
        .collect();

    // ===== Entry block =====

    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    let ctx_ptr = builder.block_params(entry_block)[0];
    let start_pos = builder.block_params(entry_block)[1];
    builder.def_var(var_ctx, ctx_ptr);
    builder.def_var(var_pos, start_pos);
    if prog.regions.is_empty() || prog.regions[0].blocks.is_empty() {
        builder.ins().jump(return_fail_block, &[]);
    } else {
        builder
            .ins()
            .jump(ir_cl_blocks[0][prog.regions[0].entry], &[]);
    }

    // ===== Declare helpers =====
    // Signatures are identical to those in jit/builder.rs.

    let h = EmitHelpers {
        h_match_char: decl_helper!(module, builder, "jit_match_char",
            [types::I64, types::I64, types::I32] => [types::I64]),
        h_match_class: decl_helper!(module, builder, "jit_match_class",
            [types::I64, types::I64, types::I32, types::I32] => [types::I64]),
        h_match_char_back: decl_helper!(module, builder, "jit_match_char_back",
            [types::I64, types::I64, types::I32] => [types::I64]),
        h_match_any_char_back: decl_helper!(module, builder, "jit_match_any_char_back",
            [types::I64, types::I64, types::I32] => [types::I64]),
        h_match_class_back: decl_helper!(module, builder, "jit_match_class_back",
            [types::I64, types::I64, types::I32, types::I32] => [types::I64]),
        h_check_anchor: decl_helper!(module, builder, "jit_check_anchor",
            [types::I64, types::I64, types::I32, types::I32] => [types::I32]),
        h_push_save_undo: decl_helper!(module, builder, "jit_push_save_undo",
            [types::I64, types::I32, types::I64] => []),
        h_fork: decl_helper!(module, builder, "jit_fork",
            [types::I64, types::I32, types::I32, types::I32, types::I64] => [types::I32]),
        h_bt_pop: decl_helper!(module, builder, "jit_bt_pop",
            [types::I64, types::I64, types::I64] => [types::I32]),
        h_fork_memo_record: decl_helper!(module, builder, "jit_fork_memo_record",
            [types::I64, types::I32, types::I64] => []),
        h_atomic_start: decl_helper!(module, builder, "jit_atomic_start",
            [types::I64] => []),
        h_atomic_end: decl_helper!(module, builder, "jit_atomic_end",
            [types::I64] => []),
        h_null_check_start: decl_helper!(module, builder, "jit_null_check_start",
            [types::I64, types::I32, types::I64] => []),
        h_null_check_end: decl_helper!(module, builder, "jit_null_check_end",
            [types::I64, types::I32, types::I64] => [types::I32]),
        h_fold_seq: decl_helper!(module, builder, "jit_fold_seq",
            [types::I64, types::I64, types::I64, types::I64] => [types::I64]),
        h_fold_seq_back: decl_helper!(module, builder, "jit_fold_seq_back",
            [types::I64, types::I64, types::I64, types::I64] => [types::I64]),
        h_match_alt_trie: decl_helper!(module, builder, "jit_match_alt_trie",
            [types::I64, types::I64, types::I32] => [types::I64]),
        h_match_alt_trie_back: decl_helper!(module, builder, "jit_match_alt_trie_back",
            [types::I64, types::I64, types::I32] => [types::I64]),
        h_span_char: decl_helper!(module, builder, "jit_span_char_len",
            [types::I64, types::I64, types::I64, types::I32] => [types::I64]),
        h_span_class_ascii: decl_helper!(module, builder, "jit_span_class_ascii_len",
            [types::I64, types::I64, types::I64, types::I64] => [types::I64]),
    };
    // h_match_any_char is only used via inline expansion; declare to satisfy the
    // symbol table but don't bind to a named variable.
    let _ = decl_helper!(module, builder, "jit_match_any_char",
        [types::I64, types::I64, types::I32] => [types::I64]);
    // jit_check_group is registered in helpers but not needed by IR JIT.
    let _ = decl_helper!(module, builder, "jit_check_group",
        [types::I64, types::I32] => [types::I32]);

    let use_memo = prog.use_memo;

    // ===== Block emission loop =====

    for &cur_rid in &compiled_regions {
        let n_blocks = prog.regions[cur_rid].blocks.len();
        for bid in 0..n_blocks {
            let block = &prog.regions[cur_rid].blocks[bid];
            let stmts = &block.stmts;
            let n_stmts = stmts.len();

            // Build the continuation block chain:
            //   block_seq[0]       = ir_cl_blocks[cur_rid][bid]  (stmt-0 entry)
            //   block_seq[1..n]    = new blocks for stmts 1..n-1
            //   block_seq[n_stmts] = new block for the terminator
            //
            // When n_stmts == 0, block_seq = [ir_cl_blocks[cur_rid][bid],
            // term_block] so the terminator runs in a fresh block immediately.
            // We don't use block_seq[0] == ir_cl_blocks directly for the
            // terminator to avoid emitting into the same block twice if the
            // terminator creates sub-blocks.
            // SPECIAL CASE n_stmts == 0: emit terminator directly in ir_cl_blocks
            // to avoid a useless empty block (and an unreachable jump).

            if n_stmts == 0 {
                builder.switch_to_block(ir_cl_blocks[cur_rid][bid]);
                emit_term(
                    builder,
                    &block.term,
                    cur_rid,
                    bid,
                    &ir_cl_blocks,
                    &flat_offset,
                    &fork_indices,
                    &atomic_exits,
                    bt_resume_block,
                    var_pos,
                    var_ctx,
                    &h,
                    use_memo,
                    prog,
                    &counter_slots,
                );
                continue;
            }

            // n_stmts > 0 — create continuation blocks.
            // block_seq[i] is where stmt i begins.
            // block_seq[n_stmts] is where the terminator begins.
            let mut block_seq: Vec<Block> = Vec::with_capacity(n_stmts + 1);
            block_seq.push(ir_cl_blocks[cur_rid][bid]);
            for _ in 0..n_stmts {
                block_seq.push(builder.create_block());
            }

            for (i, stmt) in stmts.iter().enumerate() {
                let next_block = block_seq[i + 1];
                builder.switch_to_block(block_seq[i]);
                emit_stmt(
                    builder,
                    stmt,
                    next_block,
                    bt_resume_block,
                    var_pos,
                    var_ctx,
                    &h,
                    &prog.charsets,
                    &counter_slots,
                );
            }

            // Emit terminator in block_seq[n_stmts].
            builder.switch_to_block(block_seq[n_stmts]);
            emit_term(
                builder,
                &block.term,
                cur_rid,
                bid,
                &ir_cl_blocks,
                &flat_offset,
                &fork_indices,
                &atomic_exits,
                bt_resume_block,
                var_pos,
                var_ctx,
                &h,
                use_memo,
                prog,
                &counter_slots,
            );
        }
    }

    // ===== bt_resume_block =====
    //
    // Identical logic to jit/builder.rs: inline fast-path for Retry on top of
    // stack, inline memo-recording path for MemoMark, slow h_bt_pop fallback.

    builder.switch_to_block(bt_resume_block);
    {
        let ctx_v = builder.use_var(var_ctx);
        let bt_len =
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);

        let check_top_block = builder.create_block();
        let fast_retry_block = builder.create_block();
        let check_memo_block = builder.create_block();
        let memo_pop_block = builder.create_block();
        let memo_pop_slow_block = builder.create_block();
        builder.append_block_param(memo_pop_slow_block, types::I32); // fork_idx
        builder.append_block_param(memo_pop_slow_block, types::I64); // fork_pos
        let memo_pop_after_block = builder.create_block();
        builder.append_block_param(memo_pop_after_block, types::I64); // idx
        let slow_bt_block = builder.create_block();

        builder
            .ins()
            .brif(bt_len, check_top_block, &[], return_fail_block, &[]);

        // check_top_block: peek tag of top entry
        builder.switch_to_block(check_top_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
            let bt_data =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
            let new_len = builder.ins().iadd_imm(bt_len, -1);
            let entry_off = builder.ins().imul_imm(new_len, BTJIT_SIZE);
            let top_ptr = builder.ins().iadd(bt_data, entry_off);
            let tag = builder
                .ins()
                .load(types::I32, MemFlags::trusted(), top_ptr, 0);
            let is_retry = builder
                .ins()
                .icmp_imm(IntCC::Equal, tag, BTJIT_TAG_RETRY);
            builder
                .ins()
                .brif(is_retry, fast_retry_block, &[], check_memo_block, &[]);
        }

        // check_memo_block: MemoMark → memo_pop, else → slow_bt
        builder.switch_to_block(check_memo_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
            let bt_data =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
            let new_len = builder.ins().iadd_imm(bt_len, -1);
            let entry_off = builder.ins().imul_imm(new_len, BTJIT_SIZE);
            let top_ptr = builder.ins().iadd(bt_data, entry_off);
            let tag = builder
                .ins()
                .load(types::I32, MemFlags::trusted(), top_ptr, 0);
            let is_memo =
                builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, BTJIT_TAG_MEMO_MARK);
            builder
                .ins()
                .brif(is_memo, memo_pop_block, &[], slow_bt_block, &[]);
        }

        // fast_retry_block: pop Retry entry inline
        builder.switch_to_block(fast_retry_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
            let bt_data =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
            let new_len = builder.ins().iadd_imm(bt_len, -1);
            let entry_off = builder.ins().imul_imm(new_len, BTJIT_SIZE);
            let top_ptr = builder.ins().iadd(bt_data, entry_off);

            let block_id =
                builder
                    .ins()
                    .load(types::I32, MemFlags::trusted(), top_ptr, BTJIT_OFF_A);
            let ret_pos =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), top_ptr, BTJIT_OFF_B);
            let kpos =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), top_ptr, BTJIT_OFF_C);

            builder
                .ins()
                .store(MemFlags::trusted(), new_len, ctx_v, CTX_BT_LEN);
            builder
                .ins()
                .store(MemFlags::trusted(), kpos, ctx_v, CTX_KEEP_POS);
            let bt_rc =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_RETRY_COUNT);
            let new_rc = builder.ins().iadd_imm(bt_rc, -1);
            builder
                .ins()
                .store(MemFlags::trusted(), new_rc, ctx_v, CTX_BT_RETRY_COUNT);

            builder.ins().stack_store(block_id, slot_block_id, 0);
            builder.ins().stack_store(ret_pos, slot_pos_out, 0);
            builder.def_var(var_pos, ret_pos);
            builder.ins().jump(bt_dispatch_block, &[]);
        }

        // memo_pop_block: pop MemoMark; inline record if in bounds, else slow helper
        builder.switch_to_block(memo_pop_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
            let bt_data =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
            let new_len = builder.ins().iadd_imm(bt_len, -1);
            let entry_off = builder.ins().imul_imm(new_len, BTJIT_SIZE);
            let top_ptr = builder.ins().iadd(bt_data, entry_off);

            builder
                .ins()
                .store(MemFlags::trusted(), new_len, ctx_v, CTX_BT_LEN);

            if use_memo {
                let fork_idx_u32 =
                    builder
                        .ins()
                        .load(types::I32, MemFlags::trusted(), top_ptr, BTJIT_OFF_A);
                let fork_pos =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), top_ptr, BTJIT_OFF_B);

                let fork_idx_64 = builder.ins().uextend(types::I64, fork_idx_u32);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let stride = builder.ins().iadd_imm(text_len, 1);
                let idx = builder.ins().imul(fork_idx_64, stride);
                let idx = builder.ins().iadd(idx, fork_pos);

                let fork_memo_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_FORK_MEMO_LEN);
                let in_bounds =
                    builder
                        .ins()
                        .icmp(IntCC::UnsignedLessThan, idx, fork_memo_len);
                builder.ins().brif(
                    in_bounds,
                    memo_pop_after_block,
                    &[BlockArg::Value(idx)],
                    memo_pop_slow_block,
                    &[BlockArg::Value(fork_idx_u32), BlockArg::Value(fork_pos)],
                );
            } else {
                builder.ins().jump(bt_resume_block, &[]);
            }
        }

        if use_memo {
            builder.switch_to_block(memo_pop_after_block);
            {
                let idx = builder.block_params(memo_pop_after_block)[0];
                let ctx_v = builder.use_var(var_ctx);

                let atomic_depth = builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    ctx_v,
                    CTX_ATOMIC_DEPTH,
                );
                let depth_u32 = builder.ins().ireduce(types::I32, atomic_depth);
                let one_i32 = builder.ins().iconst(types::I32, 1);
                let depth_bit = builder.ins().ishl(one_i32, depth_u32);

                let data_ptr = builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    ctx_v,
                    CTX_FORK_MEMO_DATA_PTR,
                );
                let byte_ptr = builder.ins().iadd(data_ptr, idx);
                let old_byte =
                    builder
                        .ins()
                        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
                let new_byte = builder.ins().bor(old_byte, depth_bit);
                builder
                    .ins()
                    .istore8(MemFlags::trusted(), new_byte, byte_ptr, 0);

                let one_i64 = builder.ins().iconst(types::I64, 1);
                builder.ins().store(
                    MemFlags::trusted(),
                    one_i64,
                    ctx_v,
                    CTX_MEMO_HAS_FAILURES,
                );

                builder.ins().jump(bt_resume_block, &[]);
            }

            builder.switch_to_block(memo_pop_slow_block);
            {
                let ctx_v = builder.use_var(var_ctx);
                let fork_idx_p = builder.block_params(memo_pop_slow_block)[0];
                let fork_pos_p = builder.block_params(memo_pop_slow_block)[1];
                builder
                    .ins()
                    .call(h.h_fork_memo_record, &[ctx_v, fork_idx_p, fork_pos_p]);
                builder.ins().jump(bt_resume_block, &[]);
            }
        } else {
            builder.switch_to_block(memo_pop_after_block);
            builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
            builder.switch_to_block(memo_pop_slow_block);
            builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
        }

        // slow_bt_block: non-Retry/non-MemoMark on top; fall back to h_bt_pop
        builder.switch_to_block(slow_bt_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let addr_block_id = builder.ins().stack_addr(types::I64, slot_block_id, 0);
            let addr_pos_out = builder.ins().stack_addr(types::I64, slot_pos_out, 0);
            let call = builder
                .ins()
                .call(h.h_bt_pop, &[ctx_v, addr_block_id, addr_pos_out]);
            let ok = builder.inst_results(call)[0];
            // After h_bt_pop success: pos was written to slot_pos_out; restore it.
            let restored_pos = builder.ins().stack_load(types::I64, slot_pos_out, 0);
            builder.def_var(var_pos, restored_pos);
            builder
                .ins()
                .brif(ok, bt_dispatch_block, &[], return_fail_block, &[]);
        }
    }

    // ===== bt_dispatch_block =====
    //
    // Restores (block_id, pos) from stack slots and dispatches via br_table
    // over the flat IR block space.

    builder.switch_to_block(bt_dispatch_block);
    {
        let restored_block_id = builder.ins().stack_load(types::I32, slot_block_id, 0);
        let restored_pos = builder.ins().stack_load(types::I64, slot_pos_out, 0);
        builder.def_var(var_pos, restored_pos);

        // Build the jump table: flat_offset[rid] + bid → ir_cl_blocks[rid][bid].
        let mut table_entries: Vec<BlockCall> = Vec::with_capacity(total_ir_blocks);
        // Pre-fill with return_fail_block (default for unreachable flat IDs).
        for _ in 0..total_ir_blocks {
            let bc = BlockCall::new(
                return_fail_block,
                std::iter::empty::<BlockArg>(),
                &mut builder.func.dfg.value_lists,
            );
            table_entries.push(bc);
        }
        for &rid in &compiled_regions {
            for (bid, &cl_block) in ir_cl_blocks[rid].iter().enumerate() {
                let flat_id = flat_offset[rid] + bid;
                table_entries[flat_id] = BlockCall::new(
                    cl_block,
                    std::iter::empty::<BlockArg>(),
                    &mut builder.func.dfg.value_lists,
                );
            }
        }
        let default_bc = BlockCall::new(
            return_fail_block,
            std::iter::empty::<BlockArg>(),
            &mut builder.func.dfg.value_lists,
        );
        let jt_data = JumpTableData::new(default_bc, &table_entries);
        let jt = builder.create_jump_table(jt_data);
        builder.ins().br_table(restored_block_id, jt);
    }

    // ===== return_fail_block =====

    builder.switch_to_block(return_fail_block);
    {
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        builder.ins().return_(&[neg1]);
    }

    builder.seal_all_blocks();
    Ok(())
}

// ---------------------------------------------------------------------------
// IrStmt emitter
// ---------------------------------------------------------------------------

/// Emit Cranelift IR for one `IrStmt`.
///
/// On success → branches/jumps to `next_block`.
/// On failure → branches to `bt_resume_block`.
///
/// The builder must already be positioned inside the stmt's entry block.
/// After this call the builder will be inside some sub-block that has been
/// terminated; the caller must call `switch_to_block(next_block)` before
/// emitting the next stmt or the terminator.
#[allow(clippy::too_many_arguments)]
fn emit_stmt(
    builder: &mut FunctionBuilder<'_>,
    stmt: &IrStmt,
    next_block: Block,
    bt_resume_block: Block,
    var_pos: Variable,
    var_ctx: Variable,
    h: &EmitHelpers,
    charsets: &[CharSet],
    counter_slots: &[cranelift_codegen::ir::StackSlot],
) {
    // Convenience macro: call a helper that returns i64 (-1 = fail).
    macro_rules! emit_match_call {
        ($call:expr) => {{
            let result = builder.inst_results($call)[0];
            let neg1 = builder.ins().iconst(types::I64, -1_i64);
            let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
            builder.def_var(var_pos, result);
            builder
                .ins()
                .brif(is_fail, bt_resume_block, &[], next_block, &[]);
        }};
    }

    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);

    match stmt {
        // ---- Forward character matching ----

        IrStmt::MatchChar(c) if c.is_ascii() => {
            inline_char_fwd(
                builder,
                bt_resume_block,
                var_pos,
                var_ctx,
                ctx_v,
                pos_v,
                next_block,
                *c as u8,
            );
        }
        IrStmt::MatchChar(c) => {
            let c_v = builder.ins().iconst(types::I32, *c as i64);
            let call = builder.ins().call(h.h_match_char, &[ctx_v, pos_v, c_v]);
            emit_match_call!(call);
        }
        IrStmt::MatchAnyChar { dotall } => {
            inline_any_char_fwd(
                builder,
                bt_resume_block,
                var_pos,
                var_ctx,
                ctx_v,
                pos_v,
                next_block,
                *dotall,
            );
        }
        IrStmt::MatchClass { id, ignore_case } => {
            if !ignore_case {
                inline_charclass_fwd(
                    builder,
                    bt_resume_block,
                    h.h_match_class,
                    var_pos,
                    var_ctx,
                    ctx_v,
                    pos_v,
                    next_block,
                    &charsets[*id],
                    *id,
                );
            } else {
                let i_v = builder.ins().iconst(types::I32, *id as i64);
                let ic_v = builder.ins().iconst(types::I32, 1_i64);
                let call = builder
                    .ins()
                    .call(h.h_match_class, &[ctx_v, pos_v, i_v, ic_v]);
                emit_match_call!(call);
            }
        }

        // ---- Backward character matching (lookbehind bodies) ----

        IrStmt::MatchCharBack(c) => {
            let c_v = builder.ins().iconst(types::I32, *c as i64);
            let call = builder
                .ins()
                .call(h.h_match_char_back, &[ctx_v, pos_v, c_v]);
            emit_match_call!(call);
        }
        IrStmt::MatchAnyCharBack { dotall } => {
            let d_v = builder.ins().iconst(types::I32, *dotall as i64);
            let call = builder
                .ins()
                .call(h.h_match_any_char_back, &[ctx_v, pos_v, d_v]);
            emit_match_call!(call);
        }
        IrStmt::MatchClassBack { id, ignore_case } => {
            if !ignore_case {
                inline_charclass_back(
                    builder,
                    bt_resume_block,
                    h.h_match_class_back,
                    var_pos,
                    var_ctx,
                    pos_v,
                    next_block,
                    &charsets[*id],
                    *id,
                );
            } else {
                let i_v = builder.ins().iconst(types::I32, *id as i64);
                let ic_v = builder.ins().iconst(types::I32, 1_i64);
                let call = builder
                    .ins()
                    .call(h.h_match_class_back, &[ctx_v, pos_v, i_v, ic_v]);
                emit_match_call!(call);
            }
        }

        // ---- Case-fold sequences ----

        IrStmt::MatchFoldSeq(folded) => {
            let ptr_v = builder
                .ins()
                .iconst(types::I64, folded.as_ptr() as i64);
            let len_v = builder.ins().iconst(types::I64, folded.len() as i64);
            let call = builder
                .ins()
                .call(h.h_fold_seq, &[ctx_v, pos_v, ptr_v, len_v]);
            emit_match_call!(call);
        }
        IrStmt::MatchFoldSeqBack(folded) => {
            let ptr_v = builder
                .ins()
                .iconst(types::I64, folded.as_ptr() as i64);
            let len_v = builder.ins().iconst(types::I64, folded.len() as i64);
            let call = builder
                .ins()
                .call(h.h_fold_seq_back, &[ctx_v, pos_v, ptr_v, len_v]);
            emit_match_call!(call);
        }

        // ---- Alternation trie ----

        IrStmt::MatchAltTrie(idx) => {
            let i_v = builder.ins().iconst(types::I32, *idx as i64);
            let call = builder
                .ins()
                .call(h.h_match_alt_trie, &[ctx_v, pos_v, i_v]);
            emit_match_call!(call);
        }
        IrStmt::MatchAltTrieBack(idx) => {
            let i_v = builder.ins().iconst(types::I32, *idx as i64);
            let call = builder
                .ins()
                .call(h.h_match_alt_trie_back, &[ctx_v, pos_v, i_v]);
            emit_match_call!(call);
        }

        // ---- Anchors ----

        IrStmt::CheckAnchor(kind, flags) => {
            let k_v = builder
                .ins()
                .iconst(types::I32, anchor_code(*kind) as i64);
            let f_v = builder
                .ins()
                .iconst(types::I32, flags_bits(*flags) as i64);
            let call = builder
                .ins()
                .call(h.h_check_anchor, &[ctx_v, pos_v, k_v, f_v]);
            let ok = builder.inst_results(call)[0];
            builder
                .ins()
                .brif(ok, next_block, &[], bt_resume_block, &[]);
        }

        // ---- Capture state ----

        IrStmt::SaveCapture(slot) => {
            inline_save(builder, ctx_v, pos_v, *slot, h.h_push_save_undo, next_block);
        }
        IrStmt::KeepStart => {
            builder
                .ins()
                .store(MemFlags::trusted(), pos_v, ctx_v, CTX_KEEP_POS);
            builder.ins().jump(next_block, &[]);
        }

        // ---- Counter / null-check ----

        IrStmt::CounterInit(slot_idx) => {
            let zero = builder.ins().iconst(types::I64, 0);
            builder
                .ins()
                .stack_store(zero, counter_slots[*slot_idx], 0);
            builder.ins().jump(next_block, &[]);
        }
        IrStmt::NullCheckBegin(slot_idx) => {
            let slot_v = builder.ins().iconst(types::I32, *slot_idx as i64);
            builder
                .ins()
                .call(h.h_null_check_start, &[ctx_v, slot_v, pos_v]);
            builder.ins().jump(next_block, &[]);
        }

        // ---- Ineligible (excluded by is_eligible) ----
        IrStmt::CheckBackRef { .. } => {
            builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
        }
    }
}

// ---------------------------------------------------------------------------
// IrTerminator emitter
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn emit_term(
    builder: &mut FunctionBuilder<'_>,
    term: &IrTerminator,
    cur_rid: RegionId,
    bid: BlockId,
    ir_cl_blocks: &[Vec<Block>],
    flat_offset: &[usize],
    fork_indices: &[Vec<Vec<u32>>],
    atomic_exits: &HashMap<RegionId, (RegionId, BlockId)>,
    bt_resume_block: Block,
    var_pos: Variable,
    var_ctx: Variable,
    h: &EmitHelpers,
    use_memo: bool,
    prog: &IrProgram,
    counter_slots: &[cranelift_codegen::ir::StackSlot],
) {
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);

    match term {
        IrTerminator::Match => {
            builder.ins().return_(&[pos_v]);
        }

        IrTerminator::RegionEnd => {
            match prog.regions[cur_rid].kind {
                RegionKind::Atomic => {
                    // Commit the atomic group, then jump to the parent continuation.
                    builder.ins().call(h.h_atomic_end, &[ctx_v]);
                    if let Some(&(parent_rid, parent_bid)) = atomic_exits.get(&cur_rid) {
                        builder
                            .ins()
                            .jump(ir_cl_blocks[parent_rid][parent_bid], &[]);
                    } else {
                        // Orphaned atomic — shouldn't happen in a valid program.
                        builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
                    }
                }
                RegionKind::Main => {
                    // Shouldn't happen in valid programs.
                    builder.ins().return_(&[pos_v]);
                }
                _ => {
                    // LookAhead/LookBehind bodies return to their invoker.
                    builder.ins().return_(&[pos_v]);
                }
            }
        }

        IrTerminator::Branch(b) => {
            builder.ins().jump(ir_cl_blocks[cur_rid][*b], &[]);
        }

        IrTerminator::Fork { candidates, .. } => {
            let n = candidates.len();
            if n == 0 {
                builder.ins().jump(bt_resume_block, &[]);
                return;
            }

            let first_block = ir_cl_blocks[cur_rid][candidates[0].block];

            if n == 1 {
                builder.ins().jump(first_block, &[]);
                return;
            }

            // Push retries for candidates[1..] in reverse order so that
            // candidate[1] ends up on top of the stack (highest priority).
            // Each push calls jit_fork (which handles MemoMark + Retry).
            // We always continue to the next push regardless of jit_fork's
            // return value — correct for multi-candidate forks.

            let flat_fork_id = (flat_offset[cur_rid] + bid) as i32;
            let n_steps = n - 1; // number of push steps

            // Fast-path: if the FIRST candidate has an ASCII Char guard and this is a
            // 2-candidate fork, emit an early guard peek.  When text[pos] != c we
            // skip the retry push entirely and jump directly to the fallback block.
            // This mirrors `emit_fork_guard` in jit/builder.rs and is the key optimisation
            // for greedy quantifiers like `s?` where the body char is rarely present.
            if n == 2
                && let IrGuard::Char(gc) = candidates[0].guard
                    && gc.is_ascii() {
                        let ctx_v = builder.use_var(var_ctx);
                        let pos_v = builder.use_var(var_pos);
                        let fallback_block = ir_cl_blocks[cur_rid][candidates[1].block];
                        let text_len = builder.ins().load(
                            types::I64,
                            MemFlags::trusted(),
                            ctx_v,
                            CTX_TEXT_LEN,
                        );
                        let in_bounds =
                            builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
                        let cmp_block = builder.create_block();
                        let guard_ok_block = builder.create_block();
                        // OOB → fallback (body can't match)
                        builder
                            .ins()
                            .brif(in_bounds, cmp_block, &[], fallback_block, &[]);
                        builder.switch_to_block(cmp_block);
                        let ctx_v = builder.use_var(var_ctx);
                        let pos_v = builder.use_var(var_pos);
                        let text_ptr = builder.ins().load(
                            types::I64,
                            MemFlags::trusted(),
                            ctx_v,
                            CTX_TEXT_PTR,
                        );
                        let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
                        let byte = builder
                            .ins()
                            .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
                        let ch_v = builder.ins().iconst(types::I32, gc as u8 as i64);
                        let ok = builder.ins().icmp(IntCC::Equal, byte, ch_v);
                        // char mismatch → fallback (skip retry push)
                        builder
                            .ins()
                            .brif(ok, guard_ok_block, &[], fallback_block, &[]);
                        builder.switch_to_block(guard_ok_block);
                    }

            // inter[j] = entry block for step j+1 (j = 0..n-3).
            // step 0 runs in the already-active current block (no switch needed).
            // step n-1 jumps directly to first_block.
            let inter: Vec<Block> = (1..n_steps).map(|_| builder.create_block()).collect();

            for step in 0..n_steps {
                // Which candidate are we pushing? candidates[n-1-step] (reverse order).
                let cand_idx = n - 1 - step;
                let fi = fork_indices[cur_rid][bid][cand_idx - 1] as i32;
                let retry_id = (flat_offset[cur_rid] + candidates[cand_idx].block) as i32;

                // Determine the block to jump to after this push.
                let next_after_push = if step + 1 == n_steps {
                    first_block
                } else {
                    inter[step]
                };

                // Switch to this step's entry block (step 0: already active).
                if step > 0 {
                    builder.switch_to_block(inter[step - 1]);
                }

                // Emit an ASCII peek guard if the candidate has IrGuard::Char(c).
                // If text[pos] != c, skip the retry push entirely and jump to
                // next_after_push — matching the old JIT's emit_fork_guard behaviour.
                if let IrGuard::Char(gc) = candidates[cand_idx].guard
                    && gc.is_ascii() {
                        let ctx_v = builder.use_var(var_ctx);
                        let pos_v = builder.use_var(var_pos);
                        let text_len = builder.ins().load(
                            types::I64,
                            MemFlags::trusted(),
                            ctx_v,
                            CTX_TEXT_LEN,
                        );
                        let in_bounds =
                            builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
                        let cmp_block = builder.create_block();
                        let push_block = builder.create_block();
                        builder
                            .ins()
                            .brif(in_bounds, cmp_block, &[], next_after_push, &[]);
                        builder.switch_to_block(cmp_block);
                        let ctx_v = builder.use_var(var_ctx);
                        let pos_v = builder.use_var(var_pos);
                        let text_ptr = builder.ins().load(
                            types::I64,
                            MemFlags::trusted(),
                            ctx_v,
                            CTX_TEXT_PTR,
                        );
                        let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
                        let byte = builder
                            .ins()
                            .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
                        let ch_v = builder.ins().iconst(types::I32, gc as u8 as i64);
                        let ok = builder.ins().icmp(IntCC::Equal, byte, ch_v);
                        builder
                            .ins()
                            .brif(ok, push_block, &[], next_after_push, &[]);
                        builder.switch_to_block(push_block);
                    }

                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let fork_pc_v = builder.ins().iconst(types::I32, flat_fork_id as i64);
                let fork_idx_v = builder.ins().iconst(types::I32, fi as i64);
                let retry_id_v = builder.ins().iconst(types::I32, retry_id as i64);

                // Use the inline fast-path fork push for better performance.
                inline_fork_push(
                    builder,
                    next_after_push,
                    bt_resume_block,
                    var_ctx,
                    var_pos,
                    ctx_v,
                    pos_v,
                    fork_pc_v,
                    fork_idx_v,
                    retry_id_v,
                    use_memo,
                    h.h_fork,
                    fi as u32,
                );
            }
            // After the last step, control reaches first_block.
        }

        IrTerminator::SpanChar { c, exit } => {
            let exit_block = ir_cl_blocks[cur_rid][*exit];

            if c.is_ascii() {
                // Fast path: call jit_span_char_len helper for the entire span.
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_ptr =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let byte_v = builder.ins().iconst(types::I32, *c as u8 as i64);
                let call = builder
                    .ins()
                    .call(h.h_span_char, &[text_ptr, text_len, pos_v, byte_v]);
                let span_len = builder.inst_results(call)[0];
                let new_pos = builder.ins().iadd(pos_v, span_len);
                builder.def_var(var_pos, new_pos);
                builder.ins().jump(exit_block, &[]);
            } else {
                // Non-ASCII: keep the original byte-by-byte Cranelift loop.
                let loop_header = builder.create_block();
                let check_block = builder.create_block();

                builder.ins().jump(loop_header, &[]);

                // loop_header: bounds check
                builder.switch_to_block(loop_header);
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let in_bounds = builder
                    .ins()
                    .icmp(IntCC::UnsignedLessThan, pos_v, text_len);
                builder
                    .ins()
                    .brif(in_bounds, check_block, &[], exit_block, &[]);

                // check_block: load byte, check leading byte, call helper
                builder.switch_to_block(check_block);
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_ptr =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
                let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
                let byte =
                    builder
                        .ins()
                        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
                let mut utf8_buf = [0u8; 4];
                let leading = c.encode_utf8(&mut utf8_buf).as_bytes()[0] as u64;
                let leading_v = builder.ins().iconst(types::I32, leading as i64);
                let maybe_match = builder.ins().icmp(IntCC::Equal, byte, leading_v);
                let helper_block = builder.create_block();
                builder
                    .ins()
                    .brif(maybe_match, helper_block, &[], exit_block, &[]);

                builder.switch_to_block(helper_block);
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let c_v = builder.ins().iconst(types::I32, *c as i64);
                let call = builder.ins().call(h.h_match_char, &[ctx_v, pos_v, c_v]);
                let result = builder.inst_results(call)[0];
                let neg1 = builder.ins().iconst(types::I64, -1_i64);
                let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
                let new_pos = builder.ins().select(is_fail, pos_v, result);
                builder.def_var(var_pos, new_pos);
                builder
                    .ins()
                    .brif(is_fail, exit_block, &[], loop_header, &[]);
            }
        }

        IrTerminator::SpanClass { id, exit } => {
            let exit_block = ir_cl_blocks[cur_rid][*exit];
            let cs = &prog.charsets[*id];
            let ascii_ranges = charset_ascii_ranges(cs);

            if is_ascii_only_charset(cs) {
                // Fast path: call jit_span_class_ascii_len helper for the entire span.
                // Push the [u64; 2] bitmap onto a stack slot and pass its address.
                let bitmap_slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    16,
                    0,
                ));
                let bits0 = builder.ins().iconst(types::I64, cs.ascii_bits[0] as i64);
                let bits1 = builder.ins().iconst(types::I64, cs.ascii_bits[1] as i64);
                builder.ins().stack_store(bits0, bitmap_slot, 0);
                builder.ins().stack_store(bits1, bitmap_slot, 8);
                let bits_ptr = builder.ins().stack_addr(types::I64, bitmap_slot, 0);

                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_ptr =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let call = builder
                    .ins()
                    .call(h.h_span_class_ascii, &[text_ptr, text_len, pos_v, bits_ptr]);
                let span_len = builder.inst_results(call)[0];
                let new_pos = builder.ins().iadd(pos_v, span_len);
                builder.def_var(var_pos, new_pos);
                builder.ins().jump(exit_block, &[]);
            } else {
                // General path: keep the original byte-by-byte Cranelift loop.
                let loop_header = builder.create_block();
                let read_block = builder.create_block();
                let ascii_check_block = builder.create_block();
                let advance_block = builder.create_block();

                builder.ins().jump(loop_header, &[]);

                builder.switch_to_block(loop_header);
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let in_bounds = builder
                    .ins()
                    .icmp(IntCC::UnsignedLessThan, pos_v, text_len);
                builder
                    .ins()
                    .brif(in_bounds, read_block, &[], exit_block, &[]);

                builder.switch_to_block(read_block);
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_ptr =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
                let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
                let byte =
                    builder
                        .ins()
                        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
                let c80 = builder.ins().iconst(types::I32, 0x80);
                let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);

                let nonascii_block = builder.create_block();
                builder.ins().brif(
                    is_ascii,
                    ascii_check_block,
                    &[BlockArg::Value(byte)],
                    nonascii_block,
                    &[],
                );

                builder.switch_to_block(nonascii_block);
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let i_v = builder.ins().iconst(types::I32, *id as i64);
                let ic_v = builder.ins().iconst(types::I32, 0_i64);
                let call = builder
                    .ins()
                    .call(h.h_match_class, &[ctx_v, pos_v, i_v, ic_v]);
                let result = builder.inst_results(call)[0];
                let neg1 = builder.ins().iconst(types::I64, -1_i64);
                let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
                let new_pos = builder.ins().select(is_fail, pos_v, result);
                builder.def_var(var_pos, new_pos);
                builder
                    .ins()
                    .brif(is_fail, exit_block, &[], loop_header, &[]);

                builder.append_block_param(ascii_check_block, types::I32);
                builder.switch_to_block(ascii_check_block);
                let byte_p = builder.block_params(ascii_check_block)[0];
                let raw_ok = emit_ascii_check(builder, byte_p, cs, &ascii_ranges);
                let ok = if cs.negate {
                    let one = builder.ins().iconst(types::I8, 1);
                    builder.ins().bxor(raw_ok, one)
                } else {
                    raw_ok
                };
                builder
                    .ins()
                    .brif(ok, advance_block, &[], exit_block, &[]);

                builder.switch_to_block(advance_block);
                let pos_v = builder.use_var(var_pos);
                let new_pos = builder.ins().iadd_imm(pos_v, 1_i64);
                builder.def_var(var_pos, new_pos);
                builder.ins().jump(loop_header, &[]);
            }
        }

        IrTerminator::NullCheckEnd { slot, exit, cont } => {
            let slot_v = builder.ins().iconst(types::I32, *slot as i64);
            let call = builder
                .ins()
                .call(h.h_null_check_end, &[ctx_v, slot_v, pos_v]);
            let null_flag = builder.inst_results(call)[0];
            builder.ins().brif(
                null_flag,
                ir_cl_blocks[cur_rid][*exit],
                &[],
                ir_cl_blocks[cur_rid][*cont],
                &[],
            );
        }

        IrTerminator::CounterNext {
            slot,
            count,
            body,
            exit,
        } => {
            let val = builder
                .ins()
                .stack_load(types::I64, counter_slots[*slot], 0);
            let val1 = builder.ins().iadd_imm(val, 1);
            builder
                .ins()
                .stack_store(val1, counter_slots[*slot], 0);
            let count_v = builder.ins().iconst(types::I64, *count as i64);
            let cmp = builder
                .ins()
                .icmp(IntCC::UnsignedLessThan, val1, count_v);
            builder.ins().brif(
                cmp,
                ir_cl_blocks[cur_rid][*body],
                &[],
                ir_cl_blocks[cur_rid][*exit],
                &[],
            );
        }

        IrTerminator::Atomic { body, next } => {
            builder.ins().call(h.h_atomic_start, &[ctx_v]);
            builder
                .ins()
                .jump(ir_cl_blocks[*body][prog.regions[*body].entry], &[]);
            let _ = next; // next is stored in atomic_exits and used by RegionEnd
        }

        // Excluded by is_eligible — emit trap so Cranelift has a terminator.
        IrTerminator::Absence { .. }
        | IrTerminator::Call { .. }
        | IrTerminator::RetIfCalled { .. } => {
            builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
        }
    }
}



// ---------------------------------------------------------------------------
// Inline fork-push helper
// ---------------------------------------------------------------------------

/// Emit IR for a single retry-push (one non-first Fork candidate).
///
/// Uses the inline fast path when bt has capacity and memo allows; falls back
/// to `h_fork` otherwise.  After the push, always jumps to `next_block`
/// (ignores `h_fork`'s return value — correct for multi-candidate forks where
/// skipping one candidate doesn't mean the whole fork fails).
#[allow(clippy::too_many_arguments)]
fn inline_fork_push(
    builder: &mut FunctionBuilder<'_>,
    next_block: Block,
    bt_resume_block: Block,
    var_ctx: Variable,
    var_pos: Variable,
    ctx_v: Value,
    _pos_v: Value,
    fork_pc_v: Value,
    fork_idx_v: Value,
    retry_id_v: Value,
    use_memo: bool,
    h_fork: FuncRef,
    fork_idx: u32,
) {
    let bt_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
    let bt_cap = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_CAP);

    let fast_block = builder.create_block();
    let slow_block = builder.create_block();

    let entries_needed: i64 = if use_memo { 2 } else { 1 };
    let bt_after = builder.ins().iadd_imm(bt_len, entries_needed);
    let has_room = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bt_after, bt_cap);

    if use_memo {
        let memo_hf = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_v,
            CTX_MEMO_HAS_FAILURES,
        );
        let no_fail = builder.ins().icmp_imm(IntCC::Equal, memo_hf, 0);
        let atomic_depth = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_v,
            CTX_ATOMIC_DEPTH,
        );

        let can_fast = builder.ins().band(has_room, no_fail);
        let check_block = builder.create_block();
        builder
            .ins()
            .brif(can_fast, fast_block, &[], check_block, &[]);

        builder.switch_to_block(check_block);
        let depth_le1 = builder
            .ins()
            .icmp_imm(IntCC::UnsignedLessThanOrEqual, atomic_depth, 1);
        let can_inline = builder.ins().band(has_room, depth_le1);
        let inline_check_block = builder.create_block();
        let do_check_block = builder.create_block();
        builder.append_block_param(do_check_block, types::I64);
        builder
            .ins()
            .brif(can_inline, inline_check_block, &[], slow_block, &[]);

        builder.switch_to_block(inline_check_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let pos_v = builder.use_var(var_pos);
            let text_len =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
            let stride = builder.ins().iadd_imm(text_len, 1);
            let fork_idx_64 = builder.ins().iconst(types::I64, fork_idx as i64);
            let idx = builder.ins().imul(fork_idx_64, stride);
            let idx = builder.ins().iadd(idx, pos_v);
            let fork_memo_len =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_FORK_MEMO_LEN);
            let in_bounds = builder
                .ins()
                .icmp(IntCC::UnsignedLessThan, idx, fork_memo_len);
            builder.ins().brif(
                in_bounds,
                do_check_block,
                &[BlockArg::Value(idx)],
                fast_block,
                &[],
            );
        }

        builder.switch_to_block(do_check_block);
        {
            let idx = builder.block_params(do_check_block)[0];
            let ctx_v = builder.use_var(var_ctx);
            let fork_memo_data_ptr = builder.ins().load(
                types::I64,
                MemFlags::trusted(),
                ctx_v,
                CTX_FORK_MEMO_DATA_PTR,
            );
            let data_ptr = builder.ins().iadd(fork_memo_data_ptr, idx);
            let data = builder
                .ins()
                .uload8(types::I32, MemFlags::trusted(), data_ptr, 0);
            let atomic_depth = builder.ins().load(
                types::I64,
                MemFlags::trusted(),
                ctx_v,
                CTX_ATOMIC_DEPTH,
            );
            let depth_i32 = builder.ins().ireduce(types::I32, atomic_depth);
            let shift_amt = builder.ins().iadd_imm(depth_i32, 1);
            let one_i32 = builder.ins().iconst(types::I32, 1);
            let mask = builder.ins().ishl(one_i32, shift_amt);
            let mask = builder.ins().iadd_imm(mask, -1);
            let and_result = builder.ins().band(data, mask);
            let is_failure = builder.ins().icmp_imm(IntCC::NotEqual, and_result, 0);
            // Failure cached → bt_resume_block (memo says this candidate fails).
            // For multi-candidate forks, the caller should have handled this, but
            // as an optimization for 2-way forks this is also correct.
            builder
                .ins()
                .brif(is_failure, bt_resume_block, &[], fast_block, &[]);
        }
    } else {
        builder
            .ins()
            .brif(has_room, fast_block, &[], slow_block, &[]);
    }

    // fast_block: inline push (MemoMark + Retry)
    builder.switch_to_block(fast_block);
    {
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let bt_len =
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
        let bt_data =
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
        let keep_pos =
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_KEEP_POS);
        let bt_rc =
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_RETRY_COUNT);

        let mut next_len = bt_len;

        if use_memo {
            let off = builder.ins().imul_imm(bt_len, BTJIT_SIZE);
            let ptr = builder.ins().iadd(bt_data, off);
            let tag3 = builder.ins().iconst(types::I32, BTJIT_TAG_MEMO_MARK);
            builder.ins().store(MemFlags::trusted(), tag3, ptr, 0);
            builder
                .ins()
                .store(MemFlags::trusted(), fork_idx_v, ptr, BTJIT_OFF_A);
            builder
                .ins()
                .store(MemFlags::trusted(), pos_v, ptr, BTJIT_OFF_B);
            next_len = builder.ins().iadd_imm(bt_len, 1);
        }

        let off2 = builder.ins().imul_imm(next_len, BTJIT_SIZE);
        let ptr2 = builder.ins().iadd(bt_data, off2);
        let tag0 = builder.ins().iconst(types::I32, BTJIT_TAG_RETRY);
        builder.ins().store(MemFlags::trusted(), tag0, ptr2, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), retry_id_v, ptr2, BTJIT_OFF_A);
        builder
            .ins()
            .store(MemFlags::trusted(), pos_v, ptr2, BTJIT_OFF_B);
        builder
            .ins()
            .store(MemFlags::trusted(), keep_pos, ptr2, BTJIT_OFF_C);

        let final_len = builder.ins().iadd_imm(next_len, 1);
        builder
            .ins()
            .store(MemFlags::trusted(), final_len, ctx_v, CTX_BT_LEN);
        let new_rc = builder.ins().iadd_imm(bt_rc, 1);
        builder
            .ins()
            .store(MemFlags::trusted(), new_rc, ctx_v, CTX_BT_RETRY_COUNT);

        builder.ins().jump(next_block, &[]);
    }

    // slow_block: call h_fork, always jump to next_block (ignore return)
    builder.switch_to_block(slow_block);
    {
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        builder
            .ins()
            .call(h_fork, &[ctx_v, fork_pc_v, fork_idx_v, retry_id_v, pos_v]);
        builder.ins().jump(next_block, &[]);
    }
}

// ---------------------------------------------------------------------------
// Inline char/charclass forward helpers (adapted from jit/builder.rs)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn inline_char_fwd(
    builder: &mut FunctionBuilder<'_>,
    bt_resume: Block,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    next_block: Block,
    ch: u8,
) {
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let cmp_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, cmp_block, &[], bt_resume, &[]);

    builder.switch_to_block(cmp_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
    let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
    let byte = builder
        .ins()
        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
    let ch_v = builder.ins().iconst(types::I32, ch as i64);
    let ok = builder.ins().icmp(IntCC::Equal, byte, ch_v);
    let new_pos = builder.ins().iadd_imm(pos_v, 1);
    builder.def_var(var_pos, new_pos);
    builder.ins().brif(ok, next_block, &[], bt_resume, &[]);
}

#[allow(clippy::too_many_arguments)]
fn inline_any_char_fwd(
    builder: &mut FunctionBuilder<'_>,
    bt_resume: Block,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    next_block: Block,
    dotall: bool,
) {
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], bt_resume, &[]);

    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
    let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
    let b0 = builder
        .ins()
        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);

    let char_len = utf8_char_len(builder, b0);
    let new_pos = builder.ins().iadd(pos_v, char_len);
    builder.def_var(var_pos, new_pos);

    if dotall {
        builder.ins().jump(next_block, &[]);
    } else {
        let nl = builder.ins().iconst(types::I32, b'\n' as i64);
        let is_nl = builder.ins().icmp(IntCC::Equal, b0, nl);
        builder
            .ins()
            .brif(is_nl, bt_resume, &[], next_block, &[]);
    }
}

#[allow(clippy::too_many_arguments)]
fn inline_charclass_fwd(
    builder: &mut FunctionBuilder<'_>,
    bt_resume: Block,
    h_match_class: FuncRef,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    next_block: Block,
    cs: &CharSet,
    idx: usize,
) {
    let ascii_ranges = charset_ascii_ranges(cs);

    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder
        .ins()
        .icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], bt_resume, &[]);

    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
    let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
    let byte = builder
        .ins()
        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);

    let ascii_check_block = builder.create_block();
    let c80 = builder.ins().iconst(types::I32, 0x80);
    let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);

    if is_ascii_only_charset(cs) {
        builder.ins().brif(
            is_ascii,
            ascii_check_block,
            &[BlockArg::Value(byte)],
            bt_resume,
            &[],
        );
    } else {
        let nonascii_block = builder.create_block();
        builder.ins().brif(
            is_ascii,
            ascii_check_block,
            &[BlockArg::Value(byte)],
            nonascii_block,
            &[],
        );

        builder.switch_to_block(nonascii_block);
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let i_v = builder.ins().iconst(types::I32, idx as i64);
        let ic_v = builder.ins().iconst(types::I32, 0_i64);
        let call = builder
            .ins()
            .call(h_match_class, &[ctx_v, pos_v, i_v, ic_v]);
        let result = builder.inst_results(call)[0];
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
        builder.def_var(var_pos, result);
        builder
            .ins()
            .brif(is_fail, bt_resume, &[], next_block, &[]);
    }

    builder.append_block_param(ascii_check_block, types::I32);
    builder.switch_to_block(ascii_check_block);
    let byte_p = builder.block_params(ascii_check_block)[0];
    let pos_v = builder.use_var(var_pos);
    let raw_ok = emit_ascii_check(builder, byte_p, cs, &ascii_ranges);
    let ok = if cs.negate {
        let one = builder.ins().iconst(types::I8, 1);
        builder.ins().bxor(raw_ok, one)
    } else {
        raw_ok
    };
    let new_pos = builder.ins().iadd_imm(pos_v, 1);
    builder.def_var(var_pos, new_pos);
    builder.ins().brif(ok, next_block, &[], bt_resume, &[]);
}

#[allow(clippy::too_many_arguments)]
fn inline_charclass_back(
    builder: &mut FunctionBuilder<'_>,
    bt_resume: Block,
    h_match_class_back: FuncRef,
    var_pos: Variable,
    var_ctx: Variable,
    pos_v: Value,
    next_block: Block,
    cs: &CharSet,
    idx: usize,
) {
    let ascii_ranges = charset_ascii_ranges(cs);
    let zero = builder.ins().iconst(types::I64, 0);
    let not_at_start = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThan, pos_v, zero);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(not_at_start, read_block, &[], bt_resume, &[]);

    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
    let prev_idx = builder.ins().iadd_imm(pos_v, -1_i64);
    let byte_ptr = builder.ins().iadd(text_ptr, prev_idx);
    let byte = builder
        .ins()
        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);

    let ascii_check_block = builder.create_block();
    let c80 = builder.ins().iconst(types::I32, 0x80);
    let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);

    if is_ascii_only_charset(cs) {
        builder.ins().brif(
            is_ascii,
            ascii_check_block,
            &[BlockArg::Value(byte)],
            bt_resume,
            &[],
        );
    } else {
        let nonascii_block = builder.create_block();
        builder.ins().brif(
            is_ascii,
            ascii_check_block,
            &[BlockArg::Value(byte)],
            nonascii_block,
            &[],
        );

        builder.switch_to_block(nonascii_block);
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let i_v = builder.ins().iconst(types::I32, idx as i64);
        let ic_v = builder.ins().iconst(types::I32, 0_i64);
        let call = builder
            .ins()
            .call(h_match_class_back, &[ctx_v, pos_v, i_v, ic_v]);
        let result = builder.inst_results(call)[0];
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
        builder.def_var(var_pos, result);
        builder
            .ins()
            .brif(is_fail, bt_resume, &[], next_block, &[]);
    }

    builder.append_block_param(ascii_check_block, types::I32);
    builder.switch_to_block(ascii_check_block);
    let byte_p = builder.block_params(ascii_check_block)[0];
    let pos_v = builder.use_var(var_pos);
    let raw_ok = emit_ascii_check(builder, byte_p, cs, &ascii_ranges);
    let ok = if cs.negate {
        let one = builder.ins().iconst(types::I8, 1);
        builder.ins().bxor(raw_ok, one)
    } else {
        raw_ok
    };
    let new_pos = builder.ins().iadd_imm(pos_v, -1_i64);
    builder.def_var(var_pos, new_pos);
    builder.ins().brif(ok, next_block, &[], bt_resume, &[]);
}

fn inline_save(
    builder: &mut FunctionBuilder<'_>,
    ctx_v: Value,
    pos_v: Value,
    slot: usize,
    h_push_save_undo: FuncRef,
    next_block: Block,
) {
    let slots_ptr =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_SLOTS_PTR);
    let offset = slot as i64 * 8;
    let slot_addr = if offset == 0 {
        slots_ptr
    } else {
        builder.ins().iadd_imm(slots_ptr, offset)
    };

    let bt_retry_count =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_RETRY_COUNT);
    let zero = builder.ins().iconst(types::I64, 0);
    let needs_undo = builder
        .ins()
        .icmp(IntCC::NotEqual, bt_retry_count, zero);

    let undo_block = builder.create_block();
    let write_block = builder.create_block();
    builder
        .ins()
        .brif(needs_undo, undo_block, &[], write_block, &[]);

    builder.switch_to_block(undo_block);
    builder.seal_block(undo_block);
    let old_value =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), slot_addr, 0);
    let slot_imm = builder.ins().iconst(types::I32, slot as i64);
    builder
        .ins()
        .call(h_push_save_undo, &[ctx_v, slot_imm, old_value]);
    builder.ins().jump(write_block, &[]);

    builder.switch_to_block(write_block);
    builder.seal_block(write_block);
    builder
        .ins()
        .store(MemFlags::trusted(), pos_v, slot_addr, 0);
    builder.ins().jump(next_block, &[]);
}

// ---------------------------------------------------------------------------
// UTF-8 helpers (copied from jit/builder.rs)
// ---------------------------------------------------------------------------

fn utf8_char_len(builder: &mut FunctionBuilder<'_>, b0: Value) -> Value {
    let c80 = builder.ins().iconst(types::I32, 0x80);
    let ce0 = builder.ins().iconst(types::I32, 0xE0);
    let cf0 = builder.ins().iconst(types::I32, 0xF0);
    let lt_80 = builder.ins().icmp(IntCC::UnsignedLessThan, b0, c80);
    let lt_e0 = builder.ins().icmp(IntCC::UnsignedLessThan, b0, ce0);
    let lt_f0 = builder.ins().icmp(IntCC::UnsignedLessThan, b0, cf0);
    let v1 = builder.ins().iconst(types::I64, 1);
    let v2 = builder.ins().iconst(types::I64, 2);
    let v3 = builder.ins().iconst(types::I64, 3);
    let v4 = builder.ins().iconst(types::I64, 4);
    let s34 = builder.ins().select(lt_f0, v3, v4);
    let s234 = builder.ins().select(lt_e0, v2, s34);
    builder.ins().select(lt_80, v1, s234)
}

// ---------------------------------------------------------------------------
// ASCII charset check helpers (copied from jit/builder.rs)
// ---------------------------------------------------------------------------

fn is_ascii_only_charset(cs: &CharSet) -> bool {
    if cs.negate {
        return false;
    }
    cs.ranges.iter().all(|&(_, hi)| (hi as u32) < 128)
}

fn charset_ascii_ranges(cs: &CharSet) -> Vec<(u8, u8)> {
    cs.ranges
        .iter()
        .filter_map(|&(lo, hi)| {
            let lo_u = lo as u32;
            if lo_u >= 128 {
                return None;
            }
            let hi_u = (hi as u32).min(127) as u8;
            if hi_u < lo_u as u8 {
                return None;
            }
            Some((lo_u as u8, hi_u))
        })
        .collect()
}

fn emit_ascii_bitmap_check(builder: &mut FunctionBuilder<'_>, byte: Value, bits: [u64; 2]) -> Value {
    let shift6 = builder.ins().iconst(types::I32, 6);
    let word_idx = builder.ins().ushr(byte, shift6);
    let one_i32 = builder.ins().iconst(types::I32, 1);
    let is_hi = builder.ins().icmp(IntCC::Equal, word_idx, one_i32);
    let w0 = builder.ins().iconst(types::I64, bits[0] as i64);
    let w1 = builder.ins().iconst(types::I64, bits[1] as i64);
    let word = builder.ins().select(is_hi, w1, w0);
    let mask63 = builder.ins().iconst(types::I32, 63);
    let bit_idx = builder.ins().band(byte, mask63);
    let bit_idx64 = builder.ins().uextend(types::I64, bit_idx);
    let shifted = builder.ins().ushr(word, bit_idx64);
    let one_i64 = builder.ins().iconst(types::I64, 1);
    let result64 = builder.ins().band(shifted, one_i64);
    builder.ins().ireduce(types::I8, result64)
}

fn emit_range_check(builder: &mut FunctionBuilder<'_>, byte: Value, lo: u8, hi: u8) -> Value {
    let lo_v = builder.ins().iconst(types::I32, lo as i64);
    let adj = builder.ins().isub(byte, lo_v);
    let span_v = builder.ins().iconst(types::I32, (hi - lo) as i64);
    builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, adj, span_v)
}

fn emit_eq_check(builder: &mut FunctionBuilder<'_>, byte: Value, ch: u8) -> Value {
    let cv = builder.ins().iconst(types::I32, ch as i64);
    builder.ins().icmp(IntCC::Equal, byte, cv)
}

fn emit_ascii_ranges_check(
    builder: &mut FunctionBuilder<'_>,
    ranges: &[(u8, u8)],
    byte: Value,
) -> Value {
    let mut result = builder.ins().iconst(types::I8, 0);
    for &(lo, hi) in ranges {
        let check = if lo == hi {
            emit_eq_check(builder, byte, lo)
        } else {
            emit_range_check(builder, byte, lo, hi)
        };
        result = builder.ins().bor(result, check);
    }
    result
}

fn emit_ascii_check(
    builder: &mut FunctionBuilder<'_>,
    byte: Value,
    cs: &CharSet,
    ascii_ranges: &[(u8, u8)],
) -> Value {
    if ascii_ranges.len() >= 3 {
        emit_ascii_bitmap_check(builder, byte, cs.ascii_bits)
    } else {
        emit_ascii_ranges_check(builder, ascii_ranges, byte)
    }
}

// ---------------------------------------------------------------------------
// Codec helpers (copied from jit/builder.rs)
// ---------------------------------------------------------------------------

fn anchor_code(k: AnchorKind) -> u32 {
    match k {
        AnchorKind::Start => 0,
        AnchorKind::End => 1,
        AnchorKind::StringStart => 2,
        AnchorKind::StringEnd => 3,
        AnchorKind::StringEndOrNl => 4,
        AnchorKind::WordBoundary => 5,
        AnchorKind::NonWordBoundary => 6,
        AnchorKind::SearchStart => 7,
    }
}

fn flags_bits(f: crate::ast::Flags) -> u32 {
    let _ = f;
    0
}
