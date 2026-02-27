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

use crate::ast::{AnchorKind, Shorthand};
use crate::vm::{CharSet, CharSetItem, Inst};

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

// Compile-time layout verification (will fail to compile if offsets are wrong)
const _: () = {
    use crate::vm::JitExecCtx;
    assert!(std::mem::offset_of!(JitExecCtx, text_ptr) == CTX_TEXT_PTR as usize);
    assert!(std::mem::offset_of!(JitExecCtx, text_len) == CTX_TEXT_LEN as usize);
    assert!(std::mem::offset_of!(JitExecCtx, slots_ptr) == CTX_SLOTS_PTR as usize);
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
        emit_function(&mut builder, module, prog, charsets, use_memo)?;
        builder.finalize();
    }

    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| format!("define jit_exec: {e}"))?;
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
    _use_memo: bool,
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
    let h_match_shorthand = decl_helper!(module, builder, "jit_match_shorthand",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_prop = decl_helper!(module, builder, "jit_match_prop",
        [types::I64, types::I64, types::I64, types::I64, types::I32] => [types::I64]);
    let h_match_char_back = decl_helper!(module, builder, "jit_match_char_back",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_any_char_back = decl_helper!(module, builder, "jit_match_any_char_back",
        [types::I64, types::I64, types::I32] => [types::I64]);
    let h_match_class_back = decl_helper!(module, builder, "jit_match_class_back",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_shorthand_back = decl_helper!(module, builder, "jit_match_shorthand_back",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_prop_back = decl_helper!(module, builder, "jit_match_prop_back",
        [types::I64, types::I64, types::I64, types::I64, types::I32] => [types::I64]);
    let h_check_anchor = decl_helper!(module, builder, "jit_check_anchor",
        [types::I64, types::I64, types::I32, types::I32] => [types::I32]);
    let _h_save = decl_helper!(module, builder, "jit_save",
        [types::I64, types::I32, types::I64] => []);
    let h_keep_start = decl_helper!(module, builder, "jit_keep_start",
        [types::I64, types::I64] => []);
    let h_check_group = decl_helper!(module, builder, "jit_check_group",
        [types::I64, types::I32] => [types::I32]);
    let h_fork = decl_helper!(module, builder, "jit_fork",
        [types::I64, types::I32, types::I32, types::I64] => [types::I32]);
    let h_fork_next = decl_helper!(module, builder, "jit_fork_next",
        [types::I64, types::I32, types::I32, types::I64] => [types::I32]);
    let h_bt_pop = decl_helper!(module, builder, "jit_bt_pop",
        [types::I64, types::I64, types::I64] => [types::I32]);
    let h_atomic_start = decl_helper!(module, builder, "jit_atomic_start",
        [types::I64] => []);
    let h_atomic_end = decl_helper!(module, builder, "jit_atomic_end",
        [types::I64] => []);
    let h_lookaround = decl_helper!(module, builder, "jit_lookaround",
        [types::I64, types::I32, types::I32, types::I64, types::I32] => [types::I32]);

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
                if !ic && is_simple_ascii_charset(&charsets[*idx]) {
                    inline_charclass_fwd(
                        builder,
                        &inst_blocks,
                        bt_resume_block,
                        var_pos,
                        var_ctx,
                        ctx_v,
                        pos_v,
                        pc,
                        &charsets[*idx],
                    );
                } else {
                    let i = builder.ins().iconst(types::I32, *idx as i64);
                    let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                    let call = builder.ins().call(h_match_class, &[ctx_v, pos_v, i, ic_v]);
                    emit_match_call!(call, pc + 1);
                }
            }
            Inst::Shorthand(sh, ar) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                inline_shorthand_fwd(
                    builder,
                    &inst_blocks,
                    bt_resume_block,
                    h_match_shorthand,
                    var_pos,
                    var_ctx,
                    ctx_v,
                    pos_v,
                    pc,
                    *sh,
                    *ar,
                );
            }
            Inst::Prop(name, neg) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let (ptr, len) = string_const(builder, name);
                let neg_v = builder.ins().iconst(types::I32, *neg as i64);
                let call = builder
                    .ins()
                    .call(h_match_prop, &[ctx_v, pos_v, ptr, len, neg_v]);
                emit_match_call!(call, pc + 1);
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
                if !ic && is_simple_ascii_charset(&charsets[*idx]) {
                    inline_charclass_back(
                        builder,
                        &inst_blocks,
                        bt_resume_block,
                        var_pos,
                        var_ctx,
                        ctx_v,
                        pos_v,
                        pc,
                        &charsets[*idx],
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
            Inst::ShorthandBack(sh, ar) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let s = builder.ins().iconst(types::I32, shorthand_code(*sh) as i64);
                let ar_v = builder.ins().iconst(types::I32, *ar as i64);
                let call = builder
                    .ins()
                    .call(h_match_shorthand_back, &[ctx_v, pos_v, s, ar_v]);
                emit_match_call!(call, pc + 1);
            }
            Inst::PropBack(name, neg) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let (ptr, len) = string_const(builder, name);
                let neg_v = builder.ins().iconst(types::I32, *neg as i64);
                let call = builder
                    .ins()
                    .call(h_match_prop_back, &[ctx_v, pos_v, ptr, len, neg_v]);
                emit_match_call!(call, pc + 1);
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
            Inst::Fork(alt) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let fork_pc = builder.ins().iconst(types::I32, pc as i64);
                let alt_v = builder.ins().iconst(types::I32, *alt as i64);
                let call = builder.ins().call(h_fork, &[ctx_v, fork_pc, alt_v, pos_v]);
                let ok = builder.inst_results(call)[0];
                emit_cond!(ok, pc + 1);
            }
            Inst::ForkNext(alt) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                let fork_pc = builder.ins().iconst(types::I32, pc as i64);
                let main_v = builder.ins().iconst(types::I32, (pc + 1) as i64);
                let call = builder
                    .ins()
                    .call(h_fork_next, &[ctx_v, fork_pc, main_v, pos_v]);
                let ok = builder.inst_results(call)[0];
                builder
                    .ins()
                    .brif(ok, inst_blocks[*alt], &[], bt_resume_block, &[]);
            }

            // ----------------------------------------------------------------
            // Capture slots — INLINED
            // ----------------------------------------------------------------
            Inst::Save(slot) => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                inline_save(builder, &inst_blocks, ctx_v, pos_v, pc, *slot);
            }
            Inst::KeepStart => {
                let ctx_v = builder.use_var(var_ctx);
                let pos_v = builder.use_var(var_pos);
                builder.ins().call(h_keep_start, &[ctx_v, pos_v]);
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
            | Inst::FoldSeq(_)
            | Inst::FoldSeqBack(_)
            | Inst::BackRef(..)
            | Inst::BackRefRelBack(..) => {
                builder.ins().trap(TrapCode::unwrap_user(0));
            }
        }
    }

    // ---- bt_resume_block ----
    builder.switch_to_block(bt_resume_block);
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

/// Inline forward shorthand match.
///
/// ASCII bytes: evaluated with inline range checks.
/// Non-ASCII bytes (when `ar=false`): fall back to the `jit_match_shorthand` helper.
/// Creates 2 sub-blocks (read_block + ascii_check_block) plus optionally a
/// unicode_block for the non-ASCII fallback path.
#[allow(clippy::too_many_arguments)]
fn inline_shorthand_fwd(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    bt_resume: Block,
    h_shorthand: FuncRef,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    pc: usize,
    sh: Shorthand,
    ar: bool,
) {
    // --- bounds check ---
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], bt_resume, &[]);

    // --- read_block: load byte, branch ASCII vs non-ASCII ---
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

    if ar {
        // ASCII-only mode: non-ASCII bytes simply don't match.
        let c80 = builder.ins().iconst(types::I32, 0x80);
        let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);
        builder
            .ins()
            .brif(is_ascii, ascii_check_block, &[], bt_resume, &[]);
    } else {
        // Unicode mode: non-ASCII → call helper for correctness.
        let unicode_block = builder.create_block();
        let c80 = builder.ins().iconst(types::I32, 0x80);
        let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);
        builder
            .ins()
            .brif(is_ascii, ascii_check_block, &[], unicode_block, &[]);

        // unicode_block: call jit_match_shorthand
        builder.switch_to_block(unicode_block);
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);
        let sh_code = builder.ins().iconst(types::I32, shorthand_code(sh) as i64);
        let ar_v = builder.ins().iconst(types::I32, ar as i64);
        let call = builder
            .ins()
            .call(h_shorthand, &[ctx_v, pos_v, sh_code, ar_v]);
        let result = builder.inst_results(call)[0];
        let neg1 = builder.ins().iconst(types::I64, -1_i64);
        let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
        builder.def_var(var_pos, result);
        builder
            .ins()
            .brif(is_fail, bt_resume, &[], inst_blocks[pc + 1], &[]);
    }

    // --- ascii_check_block: inline range test, advance pos by 1 ---
    builder.switch_to_block(ascii_check_block);
    let pos_v = builder.use_var(var_pos);
    let ctx_v = builder.use_var(var_ctx);
    let text_ptr = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_PTR);
    let byte_ptr = builder.ins().iadd(text_ptr, pos_v);
    let byte = builder
        .ins()
        .uload8(types::I32, MemFlags::trusted(), byte_ptr, 0);

    let ok = shorthand_ascii_check(builder, sh, byte);
    let new_pos = builder.ins().iadd_imm(pos_v, 1);
    builder.def_var(var_pos, new_pos);
    builder
        .ins()
        .brif(ok, inst_blocks[pc + 1], &[], bt_resume, &[]);
}

/// Emit an inline ASCII range check for a shorthand class.
/// `byte` is an I32 (zero-extended u8).  Returns an I8 boolean value.
fn shorthand_ascii_check(builder: &mut FunctionBuilder<'_>, sh: Shorthand, byte: Value) -> Value {
    // Build constants up-front to avoid double-borrow in arg position.
    macro_rules! range_check {
        ($lo:expr, $span:expr) => {{
            let lo_v = builder.ins().iconst(types::I32, $lo as i64);
            let adj = builder.ins().isub(byte, lo_v);
            let sp_v = builder.ins().iconst(types::I32, $span as i64);
            builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, adj, sp_v)
        }};
    }
    macro_rules! eq_check {
        ($ch:expr) => {{
            let cv = builder.ins().iconst(types::I32, $ch as i64);
            builder.ins().icmp(IntCC::Equal, byte, cv)
        }};
    }
    macro_rules! bool_not {
        ($a:expr) => {{
            let val = $a; // evaluate fully before any new borrow
            let zero = builder.ins().iconst(types::I8, 0);
            builder.ins().icmp(IntCC::Equal, val, zero)
        }};
    }

    match sh {
        Shorthand::Digit => range_check!(b'0', 9u8),
        Shorthand::NonDigit => bool_not!(range_check!(b'0', 9u8)),
        Shorthand::Word => {
            let lo = range_check!(b'a', 25u8);
            let up = range_check!(b'A', 25u8);
            let di = range_check!(b'0', 9u8);
            let us = eq_check!(b'_');
            let t1 = builder.ins().bor(lo, up);
            let t2 = builder.ins().bor(t1, di);
            builder.ins().bor(t2, us)
        }
        Shorthand::NonWord => {
            let lo = range_check!(b'a', 25u8);
            let up = range_check!(b'A', 25u8);
            let di = range_check!(b'0', 9u8);
            let us = eq_check!(b'_');
            let t1 = builder.ins().bor(lo, up);
            let t2 = builder.ins().bor(t1, di);
            let w = builder.ins().bor(t2, us);
            bool_not!(w)
        }
        Shorthand::Space => {
            // '\t'=9 .. '\r'=13  plus ' '=32
            let ctrl = range_check!(b'\t', 4u8);
            let sp = eq_check!(b' ');
            builder.ins().bor(ctrl, sp)
        }
        Shorthand::NonSpace => {
            let ctrl = range_check!(b'\t', 4u8);
            let sp = eq_check!(b' ');
            let s = builder.ins().bor(ctrl, sp);
            bool_not!(s)
        }
        Shorthand::HexDigit => {
            let di = range_check!(b'0', 9u8);
            let up = range_check!(b'A', 5u8);
            let lo = range_check!(b'a', 5u8);
            let t1 = builder.ins().bor(di, up);
            builder.ins().bor(t1, lo)
        }
        Shorthand::NonHexDigit => {
            let di = range_check!(b'0', 9u8);
            let up = range_check!(b'A', 5u8);
            let lo = range_check!(b'a', 5u8);
            let t1 = builder.ins().bor(di, up);
            let h = builder.ins().bor(t1, lo);
            bool_not!(h)
        }
    }
}

// ---------------------------------------------------------------------------
// CharClass inlining helpers (Phase 3)
// ---------------------------------------------------------------------------

/// Returns `true` when `cs` can be fully inlined as pure Cranelift IR:
/// - no negation, no intersections
/// - all items are ASCII `Char` or ASCII `Range`
fn is_simple_ascii_charset(cs: &CharSet) -> bool {
    if cs.negate || !cs.intersections.is_empty() {
        return false;
    }
    cs.items.iter().all(|item| match item {
        CharSetItem::Char(c) => c.is_ascii(),
        CharSetItem::Range(lo, hi) => lo.is_ascii() && hi.is_ascii(),
        _ => false,
    })
}

/// Emit I8 boolean: 1 if `byte` (I32) matches the simple ASCII charset.
fn charset_ascii_check(builder: &mut FunctionBuilder<'_>, cs: &CharSet, byte: Value) -> Value {
    let mut result = builder.ins().iconst(types::I8, 0);
    for item in &cs.items {
        let item_match: Value = match item {
            CharSetItem::Char(c) => {
                let cv = builder.ins().iconst(types::I32, *c as i64);
                builder.ins().icmp(IntCC::Equal, byte, cv)
            }
            CharSetItem::Range(lo, hi) => {
                let lo_v = builder.ins().iconst(types::I32, *lo as i64);
                let adj = builder.ins().isub(byte, lo_v);
                let span_v = builder.ins().iconst(types::I32, *hi as i64 - *lo as i64);
                builder
                    .ins()
                    .icmp(IntCC::UnsignedLessThanOrEqual, adj, span_v)
            }
            _ => unreachable!("caller must check is_simple_ascii_charset"),
        };
        result = builder.ins().bor(result, item_match);
    }
    result
}

/// Inline forward `CharClass` match for a simple ASCII charset.
/// Creates two sub-blocks.
#[allow(clippy::too_many_arguments)]
fn inline_charclass_fwd(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    bt_resume: Block,
    var_pos: Variable,
    var_ctx: Variable,
    ctx_v: Value,
    pos_v: Value,
    pc: usize,
    cs: &CharSet,
) {
    // --- bounds check ---
    let text_len = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_TEXT_LEN);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, pos_v, text_len);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(in_bounds, read_block, &[], bt_resume, &[]);

    // --- read_block: load byte, ASCII check ---
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

    let check_block = builder.create_block();
    let c80 = builder.ins().iconst(types::I32, 0x80);
    let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);
    // Pass byte as block param so check_block can use it.
    builder
        .ins()
        .brif(is_ascii, check_block, &[byte], bt_resume, &[]);

    // --- check_block: range/eq checks ---
    builder.append_block_param(check_block, types::I32);
    builder.switch_to_block(check_block);
    let byte_p = builder.block_params(check_block)[0];
    let pos_v = builder.use_var(var_pos);
    let ok = charset_ascii_check(builder, cs, byte_p);
    let new_pos = builder.ins().iadd_imm(pos_v, 1);
    builder.def_var(var_pos, new_pos);
    builder
        .ins()
        .brif(ok, inst_blocks[pc + 1], &[], bt_resume, &[]);
}

/// Inline backward `CharClass` match for a simple ASCII charset.
/// Creates two sub-blocks.
#[allow(clippy::too_many_arguments)]
fn inline_charclass_back(
    builder: &mut FunctionBuilder<'_>,
    inst_blocks: &[Block],
    bt_resume: Block,
    var_pos: Variable,
    var_ctx: Variable,
    _ctx_v: Value,
    pos_v: Value,
    pc: usize,
    cs: &CharSet,
) {
    // --- pos > 0 check ---
    let zero = builder.ins().iconst(types::I64, 0);
    let not_at_start = builder.ins().icmp(IntCC::UnsignedGreaterThan, pos_v, zero);
    let read_block = builder.create_block();
    builder
        .ins()
        .brif(not_at_start, read_block, &[], bt_resume, &[]);

    // --- read_block: load byte at pos-1, ASCII check ---
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

    let check_block = builder.create_block();
    let c80 = builder.ins().iconst(types::I32, 0x80);
    let is_ascii = builder.ins().icmp(IntCC::UnsignedLessThan, byte, c80);
    builder
        .ins()
        .brif(is_ascii, check_block, &[byte], bt_resume, &[]);

    // --- check_block: range/eq checks ---
    builder.append_block_param(check_block, types::I32);
    builder.switch_to_block(check_block);
    let byte_p = builder.block_params(check_block)[0];
    let pos_v = builder.use_var(var_pos);
    let ok = charset_ascii_check(builder, cs, byte_p);
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
) {
    let slots_ptr = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ctx_v, CTX_SLOTS_PTR);
    let offset = slot as i64 * 8; // each slot is u64 (8 bytes)
    if offset == 0 {
        builder
            .ins()
            .store(MemFlags::trusted(), pos_v, slots_ptr, 0);
    } else {
        let slot_addr = builder.ins().iadd_imm(slots_ptr, offset);
        builder
            .ins()
            .store(MemFlags::trusted(), pos_v, slot_addr, 0);
    }
    builder.ins().jump(inst_blocks[pc + 1], &[]);
}

// ---------------------------------------------------------------------------
// Codec helpers
// ---------------------------------------------------------------------------

fn shorthand_code(sh: Shorthand) -> u32 {
    match sh {
        Shorthand::Word => 0,
        Shorthand::NonWord => 1,
        Shorthand::Digit => 2,
        Shorthand::NonDigit => 3,
        Shorthand::Space => 4,
        Shorthand::NonSpace => 5,
        Shorthand::HexDigit => 6,
        Shorthand::NonHexDigit => 7,
    }
}

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
