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
//! # Control-flow model
//!
//! * One Cranelift `Block` per instruction (index == VM program counter).
//! * `pos` is managed via a Cranelift **variable** (SSA with automatic phi
//!   insertion).  Every predecessor that changes `pos` calls `def_var`; every
//!   consumer calls `use_var`.
//! * Forward control flow: each instruction block jumps directly to the next
//!   (or target) block — no dispatch table.
//! * Backtrack resumption goes through two extra blocks:
//!   - `bt_resume_block`: calls `jit_bt_pop`, branches to fail or dispatch.
//!   - `bt_dispatch_block`: reads restored `(block_id, pos)`, updates the
//!     `pos` variable, then uses `br_table` to jump to any instruction block.
//! * All instruction blocks whose fail path needs to invoke backtracking jump
//!   to `bt_resume_block`.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::instructions::BlockCall;
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    AbiParam, Block, InstBuilder, JumpTableData, StackSlotData, StackSlotKind, TrapCode,
    UserFuncName,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};

use crate::ast::{AnchorKind, Shorthand};
use crate::vm::{CharSet, Inst};

// ---------------------------------------------------------------------------
// Variable indices
// ---------------------------------------------------------------------------

const VAR_POS: u32 = 0;
const VAR_CTX: u32 = 1;

// ---------------------------------------------------------------------------
// Helper function declaration inside a function body
// ---------------------------------------------------------------------------

/// Declare an external helper inside `builder.func` and return its `FuncRef`.
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

/// Build and compile a JIT function for the given program.
///
/// Returns the function ID on success.  The caller must subsequently call
/// `module.finalize_definitions()` before calling the function.
pub(super) fn build(
    module: &mut cranelift_jit::JITModule,
    prog: &[Inst],
    _charsets: &[CharSet],
    _use_memo: bool,
) -> Result<FuncId, String> {
    // ---- function signature: (i64, i64) -> i64 ----
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
        emit_function(&mut builder, module, prog)?;
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
) -> Result<(), String> {
    let n = prog.len();

    // ---- declare variables ----
    let var_pos = Variable::from_u32(VAR_POS);
    let var_ctx = Variable::from_u32(VAR_CTX);
    builder.declare_var(var_pos, types::I64);
    builder.declare_var(var_ctx, types::I64);

    // ---- create all blocks upfront ----
    let entry_block = builder.create_block();
    // One block per instruction
    let inst_blocks: Vec<Block> = (0..n).map(|_| builder.create_block()).collect();
    // Backtrack resume/dispatch blocks
    let bt_resume_block = builder.create_block();
    let bt_dispatch_block = builder.create_block();
    let return_fail_block = builder.create_block();

    // ---- stack slots for jit_bt_pop output parameters ----
    let slot_block_id = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        4, // u32
        0,
    ));
    let slot_pos_out = builder.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        8, // u64
        0,
    ));

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

    // ---- declare all helpers we may need ----
    let h_match_char = decl_helper!(module, builder, "jit_match_char",
        [types::I64, types::I64, types::I32, types::I32] => [types::I64]);
    let h_match_any_char = decl_helper!(module, builder, "jit_match_any_char",
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
    let h_save = decl_helper!(module, builder, "jit_save",
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

    // ---- emit instruction blocks ----
    for (pc, inst) in prog.iter().enumerate() {
        builder.switch_to_block(inst_blocks[pc]);
        let ctx_v = builder.use_var(var_ctx);
        let pos_v = builder.use_var(var_pos);

        // Helper: emit a call to a "match helper" that returns i64 (-1 = fail).
        // Defines var_pos = result unconditionally (harmless on failure path because
        // bt_dispatch_block will overwrite it before any use).
        // Then branches: result != -1 → next_pc, result == -1 → bt_resume_block.
        macro_rules! emit_match {
            ($call:expr, $next_pc:expr) => {{
                let result = builder.inst_results($call)[0];
                let neg1 = builder.ins().iconst(types::I64, -1_i64);
                let is_fail = builder.ins().icmp(IntCC::Equal, result, neg1);
                builder.def_var(var_pos, result);
                builder
                    .ins()
                    .brif(is_fail, bt_resume_block, &[], inst_blocks[$next_pc], &[]);
            }};
        }

        // Helper: emit a "conditional proceed" where a u32 0/1 result drives
        // the branch.  1 = proceed to `next_pc`, 0 = fail.
        macro_rules! emit_cond {
            ($ok:expr, $next_pc:expr) => {{
                builder
                    .ins()
                    .brif($ok, inst_blocks[$next_pc], &[], bt_resume_block, &[]);
            }};
        }

        match inst {
            // ---- terminator ----
            Inst::Match => {
                builder.ins().return_(&[pos_v]);
            }

            // ---- LookEnd / AbsenceEnd: sub-execution terminators ----
            // These are dead code in the JIT (the interpreter handles them),
            // but we must emit a valid terminator.
            Inst::LookEnd | Inst::AbsenceEnd => {
                builder.ins().return_(&[pos_v]);
            }

            // ---- forward character matching ----
            Inst::Char(ch, ic) => {
                let c = builder.ins().iconst(types::I32, *ch as i64);
                let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                let call = builder.ins().call(h_match_char, &[ctx_v, pos_v, c, ic_v]);
                emit_match!(call, pc + 1);
            }
            Inst::AnyChar(dotall) => {
                let d = builder.ins().iconst(types::I32, *dotall as i64);
                let call = builder.ins().call(h_match_any_char, &[ctx_v, pos_v, d]);
                emit_match!(call, pc + 1);
            }
            Inst::Class(idx, ic) => {
                let i = builder.ins().iconst(types::I32, *idx as i64);
                let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                let call = builder.ins().call(h_match_class, &[ctx_v, pos_v, i, ic_v]);
                emit_match!(call, pc + 1);
            }
            Inst::Shorthand(sh, ar) => {
                let s = builder.ins().iconst(types::I32, shorthand_code(*sh) as i64);
                let ar_v = builder.ins().iconst(types::I32, *ar as i64);
                let call = builder
                    .ins()
                    .call(h_match_shorthand, &[ctx_v, pos_v, s, ar_v]);
                emit_match!(call, pc + 1);
            }
            Inst::Prop(name, neg) => {
                let (ptr, len) = string_const(builder, name);
                let neg_v = builder.ins().iconst(types::I32, *neg as i64);
                let call = builder
                    .ins()
                    .call(h_match_prop, &[ctx_v, pos_v, ptr, len, neg_v]);
                emit_match!(call, pc + 1);
            }

            // ---- backward character matching (lookbehind) ----
            Inst::CharBack(ch, ic) => {
                let c = builder.ins().iconst(types::I32, *ch as i64);
                let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                let call = builder
                    .ins()
                    .call(h_match_char_back, &[ctx_v, pos_v, c, ic_v]);
                emit_match!(call, pc + 1);
            }
            Inst::AnyCharBack(dotall) => {
                let d = builder.ins().iconst(types::I32, *dotall as i64);
                let call = builder
                    .ins()
                    .call(h_match_any_char_back, &[ctx_v, pos_v, d]);
                emit_match!(call, pc + 1);
            }
            Inst::ClassBack(idx, ic) => {
                let i = builder.ins().iconst(types::I32, *idx as i64);
                let ic_v = builder.ins().iconst(types::I32, *ic as i64);
                let call = builder
                    .ins()
                    .call(h_match_class_back, &[ctx_v, pos_v, i, ic_v]);
                emit_match!(call, pc + 1);
            }
            Inst::ShorthandBack(sh, ar) => {
                let s = builder.ins().iconst(types::I32, shorthand_code(*sh) as i64);
                let ar_v = builder.ins().iconst(types::I32, *ar as i64);
                let call = builder
                    .ins()
                    .call(h_match_shorthand_back, &[ctx_v, pos_v, s, ar_v]);
                emit_match!(call, pc + 1);
            }
            Inst::PropBack(name, neg) => {
                let (ptr, len) = string_const(builder, name);
                let neg_v = builder.ins().iconst(types::I32, *neg as i64);
                let call = builder
                    .ins()
                    .call(h_match_prop_back, &[ctx_v, pos_v, ptr, len, neg_v]);
                emit_match!(call, pc + 1);
            }

            // ---- anchor ----
            Inst::Anchor(kind, flags) => {
                let k = builder.ins().iconst(types::I32, anchor_code(*kind) as i64);
                let f = builder.ins().iconst(types::I32, flags_bits(*flags) as i64);
                let call = builder.ins().call(h_check_anchor, &[ctx_v, pos_v, k, f]);
                let ok = builder.inst_results(call)[0];
                emit_cond!(ok, pc + 1);
            }

            // ---- unconditional jump ----
            Inst::Jump(target) => {
                builder.ins().jump(inst_blocks[*target], &[]);
            }

            // ---- greedy fork ----
            Inst::Fork(alt) => {
                let fork_pc = builder.ins().iconst(types::I32, pc as i64);
                let alt_v = builder.ins().iconst(types::I32, *alt as i64);
                let call = builder.ins().call(h_fork, &[ctx_v, fork_pc, alt_v, pos_v]);
                let ok = builder.inst_results(call)[0];
                emit_cond!(ok, pc + 1);
            }

            // ---- lazy fork ----
            Inst::ForkNext(alt) => {
                let fork_pc = builder.ins().iconst(types::I32, pc as i64);
                let main_v = builder.ins().iconst(types::I32, (pc + 1) as i64);
                let call = builder
                    .ins()
                    .call(h_fork_next, &[ctx_v, fork_pc, main_v, pos_v]);
                let ok = builder.inst_results(call)[0];
                // On success jump to alt, on fail backtrack.
                builder
                    .ins()
                    .brif(ok, inst_blocks[*alt], &[], bt_resume_block, &[]);
            }

            // ---- save capture slot ----
            Inst::Save(slot) => {
                let s = builder.ins().iconst(types::I32, *slot as i64);
                builder.ins().call(h_save, &[ctx_v, s, pos_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }

            // ---- \K ----
            Inst::KeepStart => {
                builder.ins().call(h_keep_start, &[ctx_v, pos_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }

            // ---- atomic groups ----
            Inst::AtomicStart(_end_pc) => {
                builder.ins().call(h_atomic_start, &[ctx_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }
            Inst::AtomicEnd => {
                builder.ins().call(h_atomic_end, &[ctx_v]);
                builder.ins().jump(inst_blocks[pc + 1], &[]);
            }

            // ---- lookaround ----
            Inst::LookStart { positive, end_pc } => {
                let lk_pc_v = builder.ins().iconst(types::I32, pc as i64);
                let body_pc_v = builder.ins().iconst(types::I32, (pc + 1) as i64);
                let p = builder.ins().iconst(types::I32, *positive as i64);
                let call = builder
                    .ins()
                    .call(h_lookaround, &[ctx_v, lk_pc_v, body_pc_v, pos_v, p]);
                let ok = builder.inst_results(call)[0];
                // proceed → skip past LookEnd to end_pc+1
                builder
                    .ins()
                    .brif(ok, inst_blocks[end_pc + 1], &[], bt_resume_block, &[]);
            }

            // ---- conditional group ----
            Inst::CheckGroup {
                slot,
                yes_pc,
                no_pc,
            } => {
                let s = builder.ins().iconst(types::I32, *slot as i64);
                let call = builder.ins().call(h_check_group, &[ctx_v, s]);
                let has = builder.inst_results(call)[0];
                builder
                    .ins()
                    .brif(has, inst_blocks[*yes_pc], &[], inst_blocks[*no_pc], &[]);
            }

            // ---- ineligible instructions (should never be reached) ----
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

    // ---- bt_resume_block: call jit_bt_pop ----
    builder.switch_to_block(bt_resume_block);
    {
        let ctx_v2 = builder.use_var(var_ctx);
        let addr_block_id = builder.ins().stack_addr(types::I64, slot_block_id, 0);
        let addr_pos_out = builder.ins().stack_addr(types::I64, slot_pos_out, 0);
        let call = builder
            .ins()
            .call(h_bt_pop, &[ctx_v2, addr_block_id, addr_pos_out]);
        let ok = builder.inst_results(call)[0];
        builder
            .ins()
            .brif(ok, bt_dispatch_block, &[], return_fail_block, &[]);
    }

    // ---- bt_dispatch_block: read restored pos + block_id, dispatch ----
    builder.switch_to_block(bt_dispatch_block);
    {
        let restored_block_id = builder.ins().stack_load(types::I32, slot_block_id, 0);
        let restored_pos = builder.ins().stack_load(types::I64, slot_pos_out, 0);
        builder.def_var(var_pos, restored_pos);

        // Build jump table: index i → inst_blocks[i], default → return_fail_block.
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
// Codec helpers: enum variants → stable u32 discriminants
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

/// Embed a string literal as an `iconst` pointer + length pair.
/// The string data is in static memory owned by the compiled `Vec<Inst>`,
/// which outlives all JIT calls.
fn string_const(
    builder: &mut FunctionBuilder<'_>,
    s: &str,
) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
    let ptr = s.as_ptr() as i64;
    let len = s.len() as i64;
    let ptr_v = builder.ins().iconst(types::I64, ptr);
    let len_v = builder.ins().iconst(types::I64, len);
    (ptr_v, len_v)
}
