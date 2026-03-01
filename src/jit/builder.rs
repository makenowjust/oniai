//! Cranelift IR builder: translates a `Vec<Inst>` into a JIT-compiled native
//! function.
//!
//! # Generated function signature
//!
//! ```text
//! fn jit_exec(ctx_ptr: i64, start_pos: i64) -> i64
//! ```
//!
//! Returns the end position on match, -1 on no-match.
//!
//! # Inlining strategy
//!
//! Common instructions are emitted as inline Cranelift IR to avoid the
//! overhead of `extern "C"` helper calls:
//!
//! | Instruction | Inline |
//! |---|---|
//! | `Char(ch, false)` ASCII | bounds check + `uload8` + compare |
//! | `AnyChar(dotall)` | bounds check + UTF-8 length + optional newline test |
//! | `Shorthand(sh, ar)` | bounds check + ASCII range checks; non-ASCII falls back to helper |
//! | `Save(slot)` | direct `store` into `ctx.slots_ptr[slot]` |
//! | `Anchor(StringStart\|StringEnd)` | `icmp` on `pos` vs 0 / `text_len` |
//! | `Jump(target)` | direct block jump |
//!
//! Everything else calls the corresponding `jit_*` extern helper.
//!
//! # Control-flow model
//!
//! * One **entry block** per instruction (`inst_blocks[pc]`), targeted by
//!   `br_table` dispatch.  Inlined instructions may create private sub-blocks.
//! * `bt_resume_block` → `jit_bt_pop` → success or `return_fail_block`.
//! * `bt_dispatch_block` → reads `(block_id, pos)` from stack-slots and
//!   dispatches with `br_table`.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::instructions::BlockCall;
use cranelift_codegen::ir::{
    AbiParam, Block, InstBuilder, JumpTableData, MemFlags, StackSlotData, StackSlotKind, TrapCode,
    UserFuncName,
};
use cranelift_codegen::ir::{FuncRef, Value, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::AnchorKind;
use crate::vm::{CharSet, Inst};

// ---------------------------------------------------------------------------
// Variable indices
// ---------------------------------------------------------------------------

const VAR_POS: u32 = 0;
const VAR_CTX: u32 = 1;

// ---------------------------------------------------------------------------
// JitExecCtx field byte offsets (must match vm.rs JitExecCtx, #[repr(C)])
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
const CTX_FORK_MEMO_CAP: i32 = 168;

// BtJit layout constants (repr(C) struct, size=24)
// offset 0: tag (u32), offset 4: a (u32), offset 8: b (u64), offset 16: c (u64)
const BTJIT_SIZE: i64 = 24;
const BTJIT_TAG_RETRY: i64 = 0;
const BTJIT_TAG_MEMO_MARK: i64 = 3;
const BTJIT_OFF_A: i32 = 4; // block_id / slot / fork_block_id
const BTJIT_OFF_B: i32 = 8; // pos / old_value / fork_pos
const BTJIT_OFF_C: i32 = 16; // keep_pos (Retry only)

// Compile-time layout verification (will fail to compile if offsets are wrong)
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
    assert!(std::mem::offset_of!(JitExecCtx, bt_retry_count) == CTX_BT_RETRY_COUNT as usize);
    assert!(std::mem::offset_of!(JitExecCtx, atomic_depth) == CTX_ATOMIC_DEPTH as usize);
    assert!(
        std::mem::offset_of!(JitExecCtx, fork_memo_data_ptr) == CTX_FORK_MEMO_DATA_PTR as usize
    );
    assert!(std::mem::offset_of!(JitExecCtx, fork_memo_len) == CTX_FORK_MEMO_LEN as usize);
    assert!(std::mem::offset_of!(JitExecCtx, fork_memo_cap) == CTX_FORK_MEMO_CAP as usize);
};

// ---------------------------------------------------------------------------
// Helper-function declaration macro
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
// Public entry point
// ---------------------------------------------------------------------------

pub(super) fn build(
    module: &mut cranelift_jit::JITModule,
    prog: &[Inst],
    charsets: &[CharSet],
    use_memo: bool,
    fork_pc_indices: &[Option<u32>],
) -> Result<FuncId, String> {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // ctx_ptr
    sig.params.push(AbiParam::new(types::I64)); // start_pos
    sig.returns.push(AbiParam::new(types::I64));

    let func_id = module
        .declare_function("jit_exec", Linkage::Local, &sig)
        .map_err(|e| format!("declare jit_exec: {e}"))?;

    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    ctx.func.name = UserFuncName::user(0, func_id.as_u32());

    let mut fb_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
        emit_function(
            &mut builder,
            module,
            prog,
            charsets,
            use_memo,
            fork_pc_indices,
        )?;
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
                eprintln!("[oniai jit] IR:\n{ir_text}");
                for err in &errs.0 {
                    eprintln!("[oniai jit] verifier: {} -- {}", err.location, err.message);
                }
            }
        }
        format!("define jit_exec: {e}")
    })?;
    module.clear_context(&mut ctx);
    Ok(func_id)
}

// ---------------------------------------------------------------------------
// Function body emitter
// ---------------------------------------------------------------------------

fn emit_function(
    builder: &mut FunctionBuilder<'_>,
    module: &mut cranelift_jit::JITModule,
    prog: &[Inst],
    charsets: &[CharSet],
    use_memo: bool,
    fork_pc_indices: &[Option<u32>],
) -> Result<(), String> {
    let n = prog.len();

    let var_pos = Variable::from_u32(VAR_POS);
    let var_ctx = Variable::from_u32(VAR_CTX);
    builder.declare_var(var_pos, types::I64);
    builder.declare_var(var_ctx, types::I64);

    let entry_block = builder.create_block();
    let inst_blocks: Vec<Block> = (0..n).map(|_| builder.create_block()).collect();
    let bt_resume_block = builder.create_block();
    let bt_dispatch_block = builder.create_block();
    let return_fail_block = builder.create_block();

    let slot_block_id =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 4, 0));
    let slot_pos_out =
        builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 0));

    // ---- entry block ----
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    let ctx_ptr = builder.block_params(entry_block)[0];
    let start_pos = builder.block_params(entry_block)[1];
    builder.def_var(var_ctx, ctx_ptr);
    builder.def_var(var_pos, start_pos);
    if n == 0 {
        builder.ins().jump(return_fail_block, &[]);
    } else {
        builder.ins().jump(inst_blocks[0], &[]);
    }

    // ---- declare helpers (all needed regardless of inlining) ----
    let h_match_char = decl_helper!(module, builder, "jit_match_char",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let _h_match_any_char = decl_helper!(module, builder, "jit_match_any_char",
        [types::I64, types::I64, types::I32] => [types::I64]);
    let h_match_class = decl_helper!(module, builder, "jit_match_class",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_char_back = decl_helper!(module, builder, "jit_match_char_back",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_any_char_back = decl_helper!(module, builder, "jit_match_any_char_back",
        [types::I64, types::I64, types::I32] => [types::I64]);
    let h_match_class_back = decl_helper!(module, builder, "jit_match_class_back",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_check_anchor = decl_helper!(module, builder, "jit_check_anchor",
        [types::I64, types::I64, types::I32, types::I32] => [types::I32]);
    let _h_save = decl_helper!(module, builder, "jit_save",
        [types::I64, types::I32, types::I64] => []);
    let h_push_save_undo = decl_helper!(module, builder, "jit_push_save_undo",
        [types::I64, types::I32, types::I64] => []);
    let _h_keep_start = decl_helper!(module, builder, "jit_keep_start",
        [types::I64, types::I64] => []);
    let h_check_group = decl_helper!(module, builder, "jit_check_group",
        [types::I64, types::I32] => [types::I32]);
    let h_fork = decl_helper!(module, builder, "jit_fork",
        [types::I64, types::I32, types::I32, types::I32, types::I64] => [types::I32]);
    let h_fork_next = decl_helper!(module, builder, "jit_fork_next",
        [types::I64, types::I32, types::I32, types::I32, types::I64] => [types::I32]);
    let h_bt_pop = decl_helper!(module, builder, "jit_bt_pop",
        [types::I64, types::I64, types::I64] => [types::I32]);
    let h_fork_memo_record = decl_helper!(module, builder, "jit_fork_memo_record",
        [types::I64, types::I32, types::I64] => []);
    let h_atomic_start = decl_helper!(module, builder, "jit_atomic_start",
        [types::I64] => []);
    let h_atomic_end = decl_helper!(module, builder, "jit_atomic_end",
        [types::I64] => []);
    let h_lookaround = decl_helper!(module, builder, "jit_lookaround",
        [types::I64, types::I32, types::I32, types::I64, types::I32] => [types::I32]);
    let h_fold_seq = decl_helper!(module, builder, "jit_fold_seq",
        [types::I64, types::I64, types::I64, types::I64] => [types::I64]);
    let h_fold_seq_back = decl_helper!(module, builder, "jit_fold_seq_back",
        [types::I64, types::I64, types::I64, types::I64] => [types::I64]);
    let h_null_check_start = decl_helper!(module, builder, "jit_null_check_start",
        [types::I64, types::I32, types::I64] => []);
    let h_null_check_end = decl_helper!(module, builder, "jit_null_check_end",
        [types::I64, types::I32, types::I64] => [types::I32]);

    // ---- per-instruction IR emission ----
    for (pc, inst) in prog.iter().enumerate() {
        builder.switch_to_block(inst_blocks[pc]);

        // Helper macro: call a "match helper" (i64 result, -1 = fail).
        // Defines var_pos=result (harmless on fail path: bt_dispatch redefines it).
        macro_rules! emit_match_call {
            ($call:expr, $next:expr) => {{
                let result = builder.inst_results($call)[0];
                let neg1 = builder.ins().iconst(types::I64, -1_i64);
                let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
                builder.def_var(var_pos, result);
                builder
                    .ins()
                    .brif(is_fail, bt_resume_block, &[], inst_blocks[$next], &[]);
            }};
        }

        // Helper macro: u32 0/1 condition → next or fail.
        macro_rules! emit_cond {
            ($ok:expr, $next:expr) => {
                builder
                    .ins()
                    .brif($ok, inst_blocks[$next], &[], bt_resume_block, &[])
            };
        }

        match inst {
            // ----------------------------------------------------------------
            // Terminators
            // ----------------------------------------------------------------
            Inst::Match => {
                let pos_v = builder.use_var(var_pos);
                builder.ins().return_(&[pos_v]);
            }
            Inst::LookEnd | Inst::AbsenceEnd => {
                let pos_v = builder.use_var(var_pos);
                builder.ins().return_(&[pos_v]);
            }

            // ----------------------------------------------------------------
            // Forward character matching — INLINED for ASCII / common cases
            // ----------------------------------------------------------------
            Inst::Char(ch, false) if ch.is_ascii() => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                inline_char_fwd(
                    builder,
                    &inst_blocks,
                    bt_resume_block,
                    var_pos,
                    var_ctx,
                    ctx_v,
                    pos_v,
                    pc,
                    *ch as u8,
                );
            }
            Inst::Char(ch, ic) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let c = builder.ins().iconst(types::I32, *ch as i64);
                let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                let call = builder.ins().call(h_match_char, &[ctx_v, pos_v, c, ic_v]);
                emit_match_call!(call, pc + 1);
            }
            Inst::AnyChar(dotall) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                inline_any_char_fwd(
                    builder,
                    &inst_blocks,
                    bt_resume_block,
                    var_pos,
                    var_ctx,
                    ctx_v,
                    pos_v,
                    pc,
                    *dotall,
                );
            }
            Inst::Class(idx, ic) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                if !ic {
                    inline_charclass_fwd(
                        builder,
                        module,
                        &inst_blocks,
                        bt_resume_block,
                        h_match_class,
                        var_pos,
                        var_ctx,
                        ctx_v,
                        pos_v,
                        pc,
                        &charsets[*idx],
                        *idx,
                    );
                } else {
                    let i = builder.ins().iconst(types::I32, *idx as i64);
                    let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                    let call = builder.ins().call(h_match_class, &[ctx_v, pos_v, i, ic_v]);
                    emit_match_call!(call, pc + 1);
                }
            }
            // ----------------------------------------------------------------
            // Backward character matching (lookbehind) — helper calls
            // ----------------------------------------------------------------
            Inst::CharBack(ch, ic) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let c = builder.ins().iconst(types::I32, *ch as i64);
                let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                let call = builder
                    .ins()
                    .call(h_match_char_back, &[ctx_v, pos_v, c, ic_v]);
                emit_match_call!(call, pc + 1);
            }
            Inst::AnyCharBack(dotall) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let d = builder.ins().iconst(types::I32, *dotall as i64);
                let call = builder
                    .ins()
                    .call(h_match_any_char_back, &[ctx_v, pos_v, d]);
                emit_match_call!(call, pc + 1);
            }
            Inst::ClassBack(idx, ic) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                if !ic {
                    inline_charclass_back(
                        builder,
                        module,
                        &inst_blocks,
                        bt_resume_block,
                        h_match_class_back,
                        var_pos,
                        var_ctx,
                        ctx_v,
                        pos_v,
                        pc,
                        &charsets[*idx],
                        *idx,
                    );
                } else {
                    let i = builder.ins().iconst(types::I32, *idx as i64);
                    let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                    let call = builder
                        .ins()
                        .call(h_match_class_back, &[ctx_v, pos_v, i, ic_v]);
                    emit_match_call!(call, pc + 1);
                }
            }
            // ----------------------------------------------------------------
            // Anchor — inline for StringStart/StringEnd; helper for others
            // ----------------------------------------------------------------
            Inst::Anchor(AnchorKind::StringStart, _) => {
                let pos_v = builder.use_var(var_pos);
                let zero = builder.ins().iconst(types::I64, 0);
                let ok = builder.ins().icmp(IntCC::Equal, pos_v, zero);
                emit_cond!(ok, pc + 1);
            }
            Inst::Anchor(AnchorKind::StringEnd, _) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let ok = builder.ins().icmp(IntCC::Equal, pos_v, text_len);
                emit_cond!(ok, pc + 1);
            }
            Inst::Anchor(kind, flags) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let k = builder.ins().iconst(types::I32, anchor_code(*kind) as i64);
                let f = builder.ins().iconst(types::I32, flags_bits(*flags) as i64);
                let call = builder.ins().call(h_check_anchor, &[ctx_v, pos_v, k, f]);
                let ok = builder.inst_results(call)[0];
                emit_cond!(ok, pc + 1);
            }

            // ----------------------------------------------------------------
            // Unconditional jump
            // ----------------------------------------------------------------
            Inst::Jump(target) => {
                builder.ins().jump(inst_blocks[*target], &[]);
            }

            // ----------------------------------------------------------------
            // Fork (greedy) / ForkNext (lazy)
            // ----------------------------------------------------------------
            Inst::Fork(alt, guard) => {
                let fork_idx = fork_pc_indices[pc].unwrap();
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                // Guard: if text[pos] != gc, jump directly to alt (skip push).
                if let Some(gc) = guard
                    && gc.is_ascii()
                {
                    let guard_pass_block = builder.create_block();
                    emit_fork_guard(
                        builder,
                        var_ctx,
                        var_pos,
                        ctx_v,
                        pos_v,
                        *gc as u8,
                        inst_blocks[*alt],
                        guard_pass_block,
                    );
                    builder.switch_to_block(guard_pass_block);
                    let ctx_v = builder.use_var(var_ctx);
                    let pos_v = builder.use_var(var_pos);
                    inline_fork(
                        builder,
                        &inst_blocks,
                        bt_resume_block,
                        var_ctx,
                        var_pos,
                        ctx_v,
                        pos_v,
                        pc,
                        *alt,
                        false,
                        use_memo,
                        h_fork,
                        fork_idx,
                    );
                } else {
                    inline_fork(
                        builder,
                        &inst_blocks,
                        bt_resume_block,
                        var_ctx,
                        var_pos,
                        ctx_v,
                        pos_v,
                        pc,
                        *alt,
                        false,
                        use_memo,
                        h_fork,
                        fork_idx,
                    );
                }
            }
            Inst::ForkNext(alt, guard) => {
                let fork_idx = fork_pc_indices[pc].unwrap();
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                // Guard: if text[pos] != gc, jump directly to pc+1 (skip push).
                if let Some(gc) = guard
                    && gc.is_ascii()
                {
                    let guard_pass_block = builder.create_block();
                    emit_fork_guard(
                        builder,
                        var_ctx,
                        var_pos,
                        ctx_v,
                        pos_v,
                        *gc as u8,
                        inst_blocks[pc + 1],
                        guard_pass_block,
                    );
                    builder.switch_to_block(guard_pass_block);
                    let ctx_v = builder.use_var(var_ctx);
                    let pos_v = builder.use_var(var_pos);
                    inline_fork(
                        builder,
                        &inst_blocks,
                        bt_resume_block,
                        var_ctx,
                        var_pos,
                        ctx_v,
                        pos_v,
                        pc,
                        *alt,
                        true,
                        use_memo,
                        h_fork_next,
                        fork_idx,
                    );
                } else {
                    inline_fork(
                        builder,
                        &inst_blocks,
                        bt_resume_block,
                        var_ctx,
                        var_pos,
                        ctx_v,
                        pos_v,
                        pc,
                        *alt,
                        true,
                        use_memo,
                        h_fork_next,
                        fork_idx,
                    );
                }
            }

            // ----------------------------------------------------------------
            // CharFast — guard-verified advance (no bounds/match check needed)
            // ----------------------------------------------------------------
            Inst::CharFast(c) => {
                let pos_v = builder.use_var(var_pos);
                let new_pos = builder.ins().iadd_imm(pos_v, c.len_utf8() as i64);
                builder.def_var(var_pos, new_pos);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }

            // ----------------------------------------------------------------
            // Capture slots — INLINED
            // ----------------------------------------------------------------
            Inst::Save(slot) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                inline_save(
                    builder,
                    &inst_blocks,
                    ctx_v,
                    pos_v,
                    pc,
                    *slot,
                    h_push_save_undo,
                );
            }
            Inst::KeepStart => {
                // Inline: store pos directly into ctx.keep_pos (eliminates a C call).
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                builder
                    .ins()
                    .store(MemFlags::trusted(), pos_v, ctx_v, CTX_KEEP_POS);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }

            // ----------------------------------------------------------------
            // Atomic groups
            // ----------------------------------------------------------------
            Inst::AtomicStart(_end_pc) => {
                let ctx_v = builder.use_var(var_ctx);
                builder.ins().call(h_atomic_start, &[ctx_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }
            Inst::AtomicEnd => {
                let ctx_v = builder.use_var(var_ctx);
                builder.ins().call(h_atomic_end, &[ctx_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }

            // ----------------------------------------------------------------
            // Null-loop guard (prevent infinite loops on empty-matching bodies)
            // ----------------------------------------------------------------
            Inst::NullCheckStart(slot) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let slot_v = builder.ins().iconst(types::I32, *slot as i64);
                builder
                    .ins()
                    .call(h_null_check_start, &[ctx_v, slot_v, pos_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }
            Inst::NullCheckEnd { slot, exit_pc } => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let slot_v = builder.ins().iconst(types::I32, *slot as i64);
                let call = builder
                    .ins()
                    .call(h_null_check_end, &[ctx_v, slot_v, pos_v]);
                let null_flag = builder.inst_results(call)[0];
                // null_flag == 1: null iteration detected, bt truncated, jump to exit_pc
                // null_flag == 0: pos advanced, continue to pc+1 (loop body → Jump)
                builder.ins().brif(
                    null_flag,
                    inst_blocks[*exit_pc],
                    &[],
                    inst_blocks[pc + 1],
                    &[],
                );
            }

            // ----------------------------------------------------------------
            // Lookaround
            // ----------------------------------------------------------------
            Inst::LookStart { positive, end_pc } => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let lk_pc_v = builder.ins().iconst(types::I32, pc as i64);
                let body_pc_v = builder.ins().iconst(types::I32, (pc + 1) as i64);
                let p = builder.ins().iconst(types::I32, *positive as i64);
                let call = builder
                    .ins()
                    .call(h_lookaround, &[ctx_v, lk_pc_v, body_pc_v, pos_v, p]);
                let ok = builder.inst_results(call)[0];
                builder
                    .ins()
                    .brif(ok, inst_blocks[end_pc + 1], &[], bt_resume_block, &[]);
            }

            // ----------------------------------------------------------------
            // Conditional group
            // ----------------------------------------------------------------
            Inst::CheckGroup {
                slot,
                yes_pc,
                no_pc,
            } => {
                let ctx_v = builder.use_var(var_ctx);
                let s = builder.ins().iconst(types::I32, *slot as i64);
                let call = builder.ins().call(h_check_group, &[ctx_v, s]);
                let has = builder.inst_results(call)[0];
                builder
                    .ins()
                    .brif(has, inst_blocks[*yes_pc], &[], inst_blocks[*no_pc], &[]);
            }

            // ----------------------------------------------------------------
            // Ineligible instructions (unreachable)
            // ----------------------------------------------------------------
            Inst::Call(_)
            | Inst::Ret
            | Inst::RetIfCalled
            | Inst::AbsenceStart(_)
            | Inst::BackRef(..)
            | Inst::BackRefRelBack(..) => {
                builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
            }

            // ----------------------------------------------------------------
            // Case-folding sequence match (forward / backward)
            // ----------------------------------------------------------------
            Inst::FoldSeq(folded) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let ptr_v = builder.ins().iconst(types::I64, folded.as_ptr() as i64);
                let len_v = builder.ins().iconst(types::I64, folded.len() as i64);
                let call = builder
                    .ins()
                    .call(h_fold_seq, &[ctx_v, pos_v, ptr_v, len_v]);
                emit_match_call!(call, pc + 1);
            }
            Inst::FoldSeqBack(folded) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let ptr_v = builder.ins().iconst(types::I64, folded.as_ptr() as i64);
                let len_v = builder.ins().iconst(types::I64, folded.len() as i64);
                let call = builder
                    .ins()
                    .call(h_fold_seq_back, &[ctx_v, pos_v, ptr_v, len_v]);
                emit_match_call!(call, pc + 1);
            }
        }
    }

    // ---- bt_resume_block ----
    builder.switch_to_block(bt_resume_block);
    {
        // Fast path: if top of stack is Retry, pop it inline (no function call).
        // If top is MemoMark, record inline in dense array and loop back.
        // Slow path (SaveUndo/AtomicBarrier on top): fall back to h_bt_pop.
        let ctx_v = builder.use_var(var_ctx);
        let bt_len = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);

        let check_top_block = builder.create_block();
        let fast_retry_block = builder.create_block();
        let check_memo_block = builder.create_block();
        let memo_pop_block = builder.create_block();
        let memo_pop_slow_block = builder.create_block();
        // Declare block params before any branch to these blocks
        builder.append_block_param(memo_pop_slow_block, types::I32); // fork_idx
        builder.append_block_param(memo_pop_slow_block, types::I64); // fork_pos
        let memo_pop_after_block = builder.create_block();
        builder.append_block_param(memo_pop_after_block, types::I64); // idx
        let slow_bt_block = builder.create_block();

        // If stack is empty, fail immediately.
        builder
            .ins()
            .brif(bt_len, check_top_block, &[], return_fail_block, &[]);

        // check_top_block: peek at top entry's tag; if Retry → fast_retry, else → check_memo
        builder.switch_to_block(check_top_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len = builder
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
            let is_retry = builder.ins().icmp_imm(IntCC::Equal, tag, BTJIT_TAG_RETRY);
            builder
                .ins()
                .brif(is_retry, fast_retry_block, &[], check_memo_block, &[]);
        }

        // check_memo_block: if top is MemoMark → memo_pop, else → slow_bt
        builder.switch_to_block(check_memo_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len = builder
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
            let is_memo = builder
                .ins()
                .icmp_imm(IntCC::Equal, tag, BTJIT_TAG_MEMO_MARK);
            builder
                .ins()
                .brif(is_memo, memo_pop_block, &[], slow_bt_block, &[]);
        }

        // fast_retry_block: top is Retry — pop inline
        builder.switch_to_block(fast_retry_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len = builder
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
            let ret_pos = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), top_ptr, BTJIT_OFF_B);
            let kpos = builder
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
            builder.ins().jump(bt_dispatch_block, &[]);
        }

        // memo_pop_block: pop MemoMark; inline record if in bounds, else call slow helper
        builder.switch_to_block(memo_pop_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let bt_len = builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
            let bt_data =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
            let new_len = builder.ins().iadd_imm(bt_len, -1);
            let entry_off = builder.ins().imul_imm(new_len, BTJIT_SIZE);
            let top_ptr = builder.ins().iadd(bt_data, entry_off);

            // Pop the entry (decrement bt_len) before branching
            builder
                .ins()
                .store(MemFlags::trusted(), new_len, ctx_v, CTX_BT_LEN);

            if use_memo {
                // Load fork_idx (u32) and fork_pos (u64) from the MemoMark entry
                let fork_idx_u32 =
                    builder
                        .ins()
                        .load(types::I32, MemFlags::trusted(), top_ptr, BTJIT_OFF_A);
                let fork_pos =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), top_ptr, BTJIT_OFF_B);

                // Compute idx = fork_idx * stride + fork_pos
                // stride = text_len + 1 (no separate field needed; text_len is in ctx)
                let fork_idx_64 = builder.ins().uextend(types::I64, fork_idx_u32);
                let text_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
                let stride = builder.ins().iadd_imm(text_len, 1);
                let idx = builder.ins().imul(fork_idx_64, stride);
                let idx = builder.ins().iadd(idx, fork_pos);

                // Bounds check: if idx < fork_memo_len → inline record, else → slow helper
                let fork_memo_len =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_FORK_MEMO_LEN);
                let in_bounds = builder
                    .ins()
                    .icmp(IntCC::UnsignedLessThan, idx, fork_memo_len);
                builder.ins().brif(
                    in_bounds,
                    memo_pop_after_block,
                    &[idx],
                    memo_pop_slow_block,
                    &[fork_idx_u32, fork_pos],
                );
            } else {
                builder.ins().jump(bt_resume_block, &[]);
            }
        }

        // memo_pop_after_block (param idx: i64): inline record — array already in bounds
        // memo_pop_slow_block (params fork_idx: i32, fork_pos: i64): grow + record via helper
        // These blocks are only reachable when use_memo=true.
        if use_memo {
            builder.switch_to_block(memo_pop_after_block);
            {
                let idx = builder.block_params(memo_pop_after_block)[0];
                let ctx_v = builder.use_var(var_ctx);

                // Compute depth_bit = 1u32 << (atomic_depth & 7)
                // Use i32 throughout — Cranelift doesn't support ishl/bor/icmp_imm on i8.
                let atomic_depth =
                    builder
                        .ins()
                        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_ATOMIC_DEPTH);
                let depth_u32 = builder.ins().ireduce(types::I32, atomic_depth);
                let one_i32 = builder.ins().iconst(types::I32, 1);
                let depth_bit = builder.ins().ishl(one_i32, depth_u32);

                // Load data_ptr, OR and store back (istore8 truncates to 8 bits)
                let data_ptr = builder.ins().load(
                    types::I64,
                    MemFlags::trusted(),
                    ctx_v,
                    CTX_FORK_MEMO_DATA_PTR,
                );
                let byte_ptr = builder.ins().iadd(data_ptr, idx);
                let old_byte = builder
                    .ins()
                    .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
                let new_byte = builder.ins().bor(old_byte, depth_bit);
                builder
                    .ins()
                    .istore8(MemFlags::trusted(), new_byte, byte_ptr, 0);

                // Mark that a failure has been recorded (enables dense array check in forks)
                let one_i64 = builder.ins().iconst(types::I64, 1);
                builder
                    .ins()
                    .store(MemFlags::trusted(), one_i64, ctx_v, CTX_MEMO_HAS_FAILURES);

                builder.ins().jump(bt_resume_block, &[]);
            }

            builder.switch_to_block(memo_pop_slow_block);
            {
                let ctx_v = builder.use_var(var_ctx);
                let fork_idx_p = builder.block_params(memo_pop_slow_block)[0];
                let fork_pos_p = builder.block_params(memo_pop_slow_block)[1];
                builder
                    .ins()
                    .call(h_fork_memo_record, &[ctx_v, fork_idx_p, fork_pos_p]);
                builder.ins().jump(bt_resume_block, &[]);
            }
        } else {
            // Unreachable when !use_memo — MemoMark is never pushed, so these blocks
            // are dead code. Add required terminators to satisfy Cranelift IR validity.
            builder.switch_to_block(memo_pop_after_block);
            builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
            builder.switch_to_block(memo_pop_slow_block);
            builder.ins().trap(TrapCode::INTEGER_OVERFLOW);
        }

        // slow_bt_block: non-Retry, non-MemoMark on top, use h_bt_pop
        builder.switch_to_block(slow_bt_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let addr_block_id = builder.ins().stack_addr(types::I64, slot_block_id, 0);
            let addr_pos_out = builder.ins().stack_addr(types::I64, slot_pos_out, 0);
            let call = builder
                .ins()
                .call(h_bt_pop, &[ctx_v, addr_block_id, addr_pos_out]);
            let ok = builder.inst_results(call)[0];
            builder
                .ins()
                .brif(ok, bt_dispatch_block, &[], return_fail_block, &[]);
        }
    }

    // ---- bt_dispatch_block ----
    builder.switch_to_block(bt_dispatch_block);
    {
        let restored_block_id = builder.ins().stack_load(types::I32, slot_block_id, 0);
        let restored_pos = builder.ins().stack_load(types::I64, slot_pos_out, 0);
        builder.def_var(var_pos, restored_pos);
        let mut table_entries: Vec<BlockCall> = Vec::with_capacity(n);
        for &b in &inst_blocks {
            let bc = BlockCall::new(b, &[], &mut builder.func.dfg.value_lists);
            table_entries.push(bc);
        }
        let default_bc = BlockCall::new(return_fail_block, &[], &mut builder.func.dfg.value_lists);
        let jt_data = JumpTableData::new(default_bc, &table_entries);
        let jt = builder.create_jump_table(jt_data);
        builder.ins().br_table(restored_block_id, jt);
    }

    // ---- return_fail_block ----
    builder.switch_to_block(return_fail_block);
    {
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        builder.ins().return_(&[neg1]);
    }

    builder.seal_all_blocks();
    Ok(())
}

// ---------------------------------------------------------------------------
// Inline IR helpers
// ---------------------------------------------------------------------------

/// Inline forward match of a single ASCII byte (case-sensitive).
///
/// Creates one sub-block (`cmp_block`).  On entry `inst_blocks[pc]` must be
/// the current block.  Terminates `inst_blocks[pc]` with a bounds-check brif.
#[allow(clippy::too_many_arguments)]
fn inline_char_fwd(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    bt_resume: Block,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    pc: usize,
    ch: u8,
) {
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let cmp_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, cmp_block, &[], bt_resume, &[]);

    builder.switch_to_block(cmp_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr = builder
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
    builder
        .ins()
        .brif(ok, inst_blocks[pc + 1], &[], bt_resume, &[]);
}

/// Emit a fork guard check for an ASCII guard character.
///
/// On entry the current block must be the `inst_blocks[pc]` for the Fork/ForkNext
/// instruction.  Terminates the current block with:
///   `pos < text_len && text[pos] == gc`  → falls through to `pass_block`
///   otherwise                             → jumps to `fail_block`
///
/// Called only when `gc.is_ascii()`.  The caller switches to `pass_block` after
/// this function returns and emits the normal `inline_fork` IR there.
#[allow(clippy::too_many_arguments)]
fn emit_fork_guard(
    builder: &mut FunctionBuilder<'_>,
    var_ctx: Variable,
    var_pos: Variable,
    ctx_v: Value,
    pos_v: Value,
    gc: u8,
    fail_block: Block,
    pass_block: Block,
) {
    // Bounds check: pos < text_len.
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], fail_block, &[]);

    // read_block: load byte and compare.
    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
    let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
    let byte = builder
        .ins()
        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);
    let gc_v = builder.ins().iconst(types::I32, gc as i64);
    let ok = builder.ins().icmp(IntCC::Equal, byte, gc_v);
    builder.ins().brif(ok, pass_block, &[], fail_block, &[]);
}

/// Inline forward AnyChar match — correct for full UTF-8.
///
/// Creates one sub-block (`read_block`).
#[allow(clippy::too_many_arguments)]
fn inline_any_char_fwd(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    bt_resume: Block,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    pc: usize,
    dotall: bool,
) {
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], bt_resume, &[]);

    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr = builder
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
        builder.ins().jump(inst_blocks[pc + 1], &[]);
    } else {
        let nl = builder.ins().iconst(types::I32, b'\n' as i64);
        let is_nl = builder.ins().icmp(IntCC::Equal, b0, nl);
        builder
            .ins()
            .brif(is_nl, bt_resume, &[], inst_blocks[pc + 1], &[]);
    }
}

/// Compute the UTF-8 byte width of a character from its leading byte.
/// `b0` is an I32 value (zero-extended u8); returns I64.
fn utf8_char_len(builder: &mut FunctionBuilder<'_>, b0: Value) -> Value {
    // < 0x80 → 1 byte; < 0xE0 → 2; < 0xF0 → 3; else 4
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
// CharClass inlining helpers — inversion list based
// ---------------------------------------------------------------------------

/// Returns `true` when no non-ASCII char can ever match `cs`
/// (so non-ASCII bytes can be rejected inline without calling the helper).
fn is_ascii_only_charset(cs: &CharSet) -> bool {
    if cs.negate {
        return false; // negation of ASCII-only items matches all non-ASCII chars
    }
    cs.ranges.iter().all(|&(_, hi)| (hi as u32) < 128)
}

/// Extract ASCII sub-ranges from a `CharSet`'s inversion list.
fn charset_ascii_ranges(cs: &CharSet) -> Vec<(u8, u8)> {
    cs.ranges
        .iter()
        .filter_map(|&(lo, hi)| {
            let lo_u = lo as u32;
            if lo_u >= 128 {
                return None;
            }
            let hi_u = (hi as u32).min(127) as u8;
            Some((lo_u as u8, hi_u))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Low-level byte-check emitters (avoid double-borrow in argument position)
// ---------------------------------------------------------------------------

/// `(byte - lo) <= (hi - lo)`  — unsigned range check on an I32 byte value.
fn emit_range_check(builder: &mut FunctionBuilder<'_>, byte: Value, lo: u8, hi: u8) -> Value {
    let lo_v = builder.ins().iconst(types::I32, lo as i64);
    let adj = builder.ins().isub(byte, lo_v);
    let span_v = builder.ins().iconst(types::I32, (hi - lo) as i64);
    builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, adj, span_v)
}

/// `byte == ch` — equality check on an I32 byte value.
fn emit_eq_check(builder: &mut FunctionBuilder<'_>, byte: Value, ch: u8) -> Value {
    let cv = builder.ins().iconst(types::I32, ch as i64);
    builder.ins().icmp(IntCC::Equal, byte, cv)
}

/// Emit an I8 boolean: 1 if `byte` (I32, zero-extended u8) matches any
/// precomputed ASCII range, 0 otherwise.
///
/// Emits a cascade of `(byte - lo) <= (hi - lo)` unsigned range checks
/// OR-ed together.  For an empty slice emits the constant 0.
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

/// Inline forward `CharClass` match using precomputed `ascii_ranges`.
///
/// For ASCII bytes: emits precomputed range checks (no helper call).
/// For non-ASCII bytes: either fails inline (ASCII-only charset) or calls
/// the `jit_match_class` helper for correct Unicode handling.
///
/// Creates 2–3 sub-blocks.
#[allow(clippy::too_many_arguments)]
fn inline_charclass_fwd(
    builder: &mut FunctionBuilder<'_>,
    _module: &mut cranelift_jit::JITModule,
    inst_blocks: &[Block],
    bt_resume: Block,
    h_match_class: FuncRef,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    pc: usize,
    cs: &CharSet,
    idx: usize,
) {
    let ascii_ranges = charset_ascii_ranges(cs);
    let ascii_ranges = ascii_ranges.as_slice();

    // --- bounds check ---
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], bt_resume, &[]);

    // --- read_block: load leading byte ---
    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr = builder
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
        // Non-ASCII bytes never match: fail immediately without helper.
        builder
            .ins()
            .brif(is_ascii, ascii_check_block, &[byte], bt_resume, &[]);
    } else {
        // Non-ASCII bytes may match: call jit_match_class for correct handling.
        let nonascii_block = builder.create_block();
        builder
            .ins()
            .brif(is_ascii, ascii_check_block, &[byte], nonascii_block, &[]);

        builder.switch_to_block(nonascii_block);
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let i = builder.ins().iconst(types::I32, idx as i64);
        let ic_v = builder.ins().iconst(types::I32, 0_i64); // !ic path
        let call = builder.ins().call(h_match_class, &[ctx_v, pos_v, i, ic_v]);
        let result = builder.inst_results(call)[0];
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
        builder.def_var(var_pos, result);
        builder
            .ins()
            .brif(is_fail, bt_resume, &[], inst_blocks[pc + 1], &[]);
    }

    // --- ascii_check_block: precomputed range checks ---
    builder.append_block_param(ascii_check_block, types::I32);
    builder.switch_to_block(ascii_check_block);
    let byte_p = builder.block_params(ascii_check_block)[0];
    let pos_v = builder.use_var(var_pos);
    let raw_ok = emit_ascii_ranges_check(builder, ascii_ranges, byte_p);
    // For negated charsets the ASCII match is: byte is NOT in ascii_ranges.
    let ok = if cs.negate {
        let one = builder.ins().iconst(types::I8, 1);
        builder.ins().bxor(raw_ok, one)
    } else {
        raw_ok
    };
    let new_pos = builder.ins().iadd_imm(pos_v, 1);
    builder.def_var(var_pos, new_pos);
    builder
        .ins()
        .brif(ok, inst_blocks[pc + 1], &[], bt_resume, &[]);
}

/// Inline backward `CharClass` match using precomputed `ascii_ranges`.
///
/// Mirrors `inline_charclass_fwd` but reads `pos - 1` and advances backward.
#[allow(clippy::too_many_arguments)]
fn inline_charclass_back(
    builder: &mut FunctionBuilder<'_>,
    _module: &mut cranelift_jit::JITModule,
    inst_blocks: &[Block],
    bt_resume: Block,
    h_match_class_back: FuncRef,
    var_pos: Variable,
    var_ctx: Variable,
    _ctx_v: Value,
    pos_v: Value,
    pc: usize,
    cs: &CharSet,
    idx: usize,
) {
    let ascii_ranges = charset_ascii_ranges(cs);
    let ascii_ranges = ascii_ranges.as_slice();

    // --- pos > 0 check ---
    let zero = builder.ins().iconst(types::I64, 0);
    let not_at_start = builder.ins().icmp(IntCC::UnsignedGreaterThan, pos_v, zero);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(not_at_start, read_block, &[], bt_resume, &[]);

    // --- read_block: load byte at pos-1 ---
    builder.switch_to_block(read_block);
    let ctx_v = builder.use_var(var_ctx);
    let pos_v = builder.use_var(var_pos);
    let text_ptr = builder
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
        builder
            .ins()
            .brif(is_ascii, ascii_check_block, &[byte], bt_resume, &[]);
    } else {
        let nonascii_block = builder.create_block();
        builder
            .ins()
            .brif(is_ascii, ascii_check_block, &[byte], nonascii_block, &[]);

        builder.switch_to_block(nonascii_block);
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let i = builder.ins().iconst(types::I32, idx as i64);
        let ic_v = builder.ins().iconst(types::I32, 0_i64);
        let call = builder
            .ins()
            .call(h_match_class_back, &[ctx_v, pos_v, i, ic_v]);
        let result = builder.inst_results(call)[0];
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
        builder.def_var(var_pos, result);
        builder
            .ins()
            .brif(is_fail, bt_resume, &[], inst_blocks[pc + 1], &[]);
    }

    // --- ascii_check_block: precomputed range checks ---
    builder.append_block_param(ascii_check_block, types::I32);
    builder.switch_to_block(ascii_check_block);
    let byte_p = builder.block_params(ascii_check_block)[0];
    let pos_v = builder.use_var(var_pos);
    let raw_ok = emit_ascii_ranges_check(builder, ascii_ranges, byte_p);
    // For negated charsets the ASCII match is: byte is NOT in ascii_ranges.
    let ok = if cs.negate {
        let one = builder.ins().iconst(types::I8, 1);
        builder.ins().bxor(raw_ok, one)
    } else {
        raw_ok
    };
    let new_pos = builder.ins().iadd_imm(pos_v, -1_i64);
    builder.def_var(var_pos, new_pos);
    builder
        .ins()
        .brif(ok, inst_blocks[pc + 1], &[], bt_resume, &[]);
}

fn inline_save(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    ctx_v: Value,
    pos_v: Value,
    pc: usize,
    slot: usize,
    h_push_save_undo: FuncRef,
) {
    let slots_ptr = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_SLOTS_PTR);
    let offset = slot as i64 * 8; // each slot is u64 (8 bytes)
    let slot_addr = if offset == 0 {
        slots_ptr
    } else {
        builder.ins().iadd_imm(slots_ptr, offset)
    };

    // Guard: only push a SaveUndo entry when bt_retry_count > 0.
    // When there are no active Retry entries on the bt stack, backtracking is
    // impossible and the undo entry would never be consumed — skip it entirely
    // to avoid the helper call overhead on the fast (no-backtrack) path.
    let bt_retry_count =
        builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_RETRY_COUNT);
    let zero = builder.ins().iconst(types::I64, 0);
    let needs_undo = builder.ins().icmp(IntCC::NotEqual, bt_retry_count, zero);

    let undo_block = builder.create_block();
    let write_block = builder.create_block();
    builder
        .ins()
        .brif(needs_undo, undo_block, &[], write_block, &[]);

    builder.switch_to_block(undo_block);
    builder.seal_block(undo_block);
    let old_value = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), slot_addr, 0);
    let slot_imm = builder.ins().iconst(types::I32, slot as i64);
    builder
        .ins()
        .call(h_push_save_undo, &[ctx_v, slot_imm, old_value]);
    builder.ins().jump(write_block, &[]);

    builder.switch_to_block(write_block);
    builder.seal_block(write_block);
    // Write new value.
    builder
        .ins()
        .store(MemFlags::trusted(), pos_v, slot_addr, 0);
    builder.ins().jump(inst_blocks[pc + 1], &[]);
}

// ---------------------------------------------------------------------------
// Codec helpers
// ---------------------------------------------------------------------------

/// Emit Cranelift IR for a Fork or ForkNext instruction.
///
/// For `Fork(alt)` (greedy):   try `pc+1` first, save `alt` as the retry block.
/// For `ForkNext(alt)` (lazy): try `alt` first, save `pc+1` as the retry block.
///
/// Fast path (inline push, no function call) when bt has enough capacity AND
/// (for memo mode) `memo_has_failures == 0`.  Falls back to the `h_fork` /
/// `h_fork_next` extern helper otherwise.
#[allow(clippy::too_many_arguments)]
fn inline_fork(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    bt_resume_block: Block,
    var_ctx: Variable,
    var_pos: Variable,
    ctx_v: Value,
    _pos_v: Value,
    pc: usize,
    alt: usize,
    is_fork_next: bool,
    use_memo: bool,
    h_fork: FuncRef,
    fork_idx: u32,
) {
    // retry_block_id: what gets stored as the Retry entry's block_id.
    // For Fork:     save alt  (alternative) as retry; fall through to pc+1.
    // For ForkNext: save pc+1 (main)        as retry; jump to alt.
    let (retry_id, success_pc) = if is_fork_next {
        (pc + 1, alt)
    } else {
        (alt, pc + 1)
    };
    let fork_pc_v = builder.ins().iconst(types::I32, pc as i64);
    let retry_id_v = builder.ins().iconst(types::I32, retry_id as i64);
    let fork_idx_v = builder.ins().iconst(types::I32, fork_idx as i64);

    let bt_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
    let bt_cap = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_CAP);

    let fast_block = builder.create_block();
    let slow_block = builder.create_block();
    let next_block = inst_blocks[success_pc];

    // Capacity needed: 2 entries for memo (MemoMark + Retry), 1 for no-memo.
    let entries_needed = if use_memo { 2i64 } else { 1i64 };
    let bt_after = builder.ins().iadd_imm(bt_len, entries_needed);
    let has_room = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, bt_after, bt_cap);

    // Routing:
    //  • !has_room                                → slow_block (C helper handles realloc)
    //  • has_room && !use_memo                    → fast_block
    //  • has_room && memo_has_failures == 0       → fast_block (no failures yet)
    //  • has_room && memo_has_failures == 1
    //      && atomic_depth <= 1                   → inline_check_block (dense-array read in IR)
    //      && atomic_depth >= 2                   → slow_block (rare; C helper is fine)
    //
    // The inline check emits ~8 IR ops vs a C call; it eliminates jit_fork overhead
    // for the entire backtracking hot loop once any failure has been recorded.
    // Restricting to atomic_depth <= 1 avoids a complex depth-mask computation;
    // atomic_depth >= 2 (multiple nested atomic groups) is extremely rare.
    if use_memo {
        let memo_hf = builder.ins().load(
            types::I64,
            MemFlags::trusted(),
            ctx_v,
            CTX_MEMO_HAS_FAILURES,
        );
        let no_fail = builder.ins().icmp_imm(IntCC::Equal, memo_hf, 0);
        let atomic_depth =
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), ctx_v, CTX_ATOMIC_DEPTH);

        // can_fast: has room AND no failures recorded yet
        let can_fast = builder.ins().band(has_room, no_fail);

        // check_block: reached when can_fast == 0 (either !has_room or failures exist)
        let check_block = builder.create_block();
        builder
            .ins()
            .brif(can_fast, fast_block, &[], check_block, &[]);

        // check_block: If has_room AND atomic_depth <= 1: inline dense-array check in IR.
        // Otherwise fall to slow_block (C helper: handles realloc or deep atomics).
        builder.switch_to_block(check_block);
        let depth_le1 = builder
            .ins()
            .icmp_imm(IntCC::UnsignedLessThanOrEqual, atomic_depth, 1);
        let can_inline = builder.ins().band(has_room, depth_le1);
        // inline_check_block: has_room=true, atomic_depth<=1, memo_has_failures=1
        // Compute idx = fork_idx * (text_len + 1) + pos, read dense array byte,
        // apply depth mask, branch.
        // do_check_block receives idx as a block param to avoid cross-block SSA.
        let inline_check_block = builder.create_block();
        let do_check_block = builder.create_block();
        builder.append_block_param(do_check_block, types::I64); // idx
        builder
            .ins()
            .brif(can_inline, inline_check_block, &[], slow_block, &[]);

        builder.switch_to_block(inline_check_block);
        {
            let ctx_v = builder.use_var(var_ctx);
            let pos_v = builder.use_var(var_pos);
            let text_len = builder
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
            // Out of bounds → no failure recorded; push normally
            builder
                .ins()
                .brif(in_bounds, do_check_block, &[idx], fast_block, &[]);
        }

        // do_check_block(idx): idx is in bounds; read one byte and check mask.
        // atomic_depth is re-loaded from ctx (avoids cross-block SSA use).
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
            // Use i32 for byte load and mask arithmetic — Cranelift doesn't support
            // ishl/band/icmp_imm on i8.  uload8(i32) zero-extends to i32.
            let data = builder
                .ins()
                .uload8(types::I32, MemFlags::trusted(), data_ptr, 0);
            // depth_mask = (1 << (atomic_depth + 1)) - 1
            //   depth=0 → mask=0b00000001, depth=1 → mask=0b00000011
            // Re-load atomic_depth here to avoid using a cross-block SSA value.
            let atomic_depth =
                builder
                    .ins()
                    .load(types::I64, MemFlags::trusted(), ctx_v, CTX_ATOMIC_DEPTH);
            let depth_i32 = builder.ins().ireduce(types::I32, atomic_depth);
            let shift_amt = builder.ins().iadd_imm(depth_i32, 1);
            let one_i32 = builder.ins().iconst(types::I32, 1);
            let mask = builder.ins().ishl(one_i32, shift_amt);
            let mask = builder.ins().iadd_imm(mask, -1);
            let and_result = builder.ins().band(data, mask);
            let is_failure = builder.ins().icmp_imm(IntCC::NotEqual, and_result, 0);
            // Failure cached → bt_resume_block (skip push, continue backtracking)
            // No failure     → fast_block (push inline, continue matching)
            builder
                .ins()
                .brif(is_failure, bt_resume_block, &[], fast_block, &[]);
        }
    } else {
        builder
            .ins()
            .brif(has_room, fast_block, &[], slow_block, &[]);
    }

    // ---- fast_block ----
    builder.switch_to_block(fast_block);
    {
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let bt_len = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_LEN);
        let bt_data = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_DATA_PTR);
        let keep_pos = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_KEEP_POS);
        let bt_rc = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), ctx_v, CTX_BT_RETRY_COUNT);

        let mut next_len = bt_len;

        if use_memo {
            // Write MemoMark: { tag=3, a=fork_idx (compact index), b=pos }
            // NOTE: store fork_idx (not fork_pc) so bt_resume can index the dense array
            // directly without a pc→idx lookup.
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

        // Write Retry at bt_data[next_len]:
        // { tag=0 at +0, block_id=retry_id at +4, pos at +8, keep_pos at +16 }
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

    // ---- slow_block: fall back to h_fork / h_fork_next ----
    builder.switch_to_block(slow_block);
    {
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let call = builder
            .ins()
            .call(h_fork, &[ctx_v, fork_pc_v, fork_idx_v, retry_id_v, pos_v]);
        let ok = builder.inst_results(call)[0];
        if is_fork_next {
            builder
                .ins()
                .brif(ok, inst_blocks[alt], &[], bt_resume_block, &[]);
        } else {
            builder
                .ins()
                .brif(ok, inst_blocks[pc + 1], &[], bt_resume_block, &[]);
        }
    }
}

// ---------------------------------------------------------------------------
// Codec helpers
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

/// Embed a Rust string as `(ptr: i64, len: i64)` constant pair.
/// The string data lives in the compiled `Vec<Inst>` which outlives all JIT calls.
fn string_const(
    builder: &mut FunctionBuilder<'_>,
    s: &str,
) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
    let ptr_v = builder.ins().iconst(types::I64, s.as_ptr() as i64);
    let len_v = builder.ins().iconst(types::I64, s.len() as i64);
    (ptr_v, len_v)
}
