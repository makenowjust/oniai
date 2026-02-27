/// Compiler: transforms a parsed AST into a VM instruction sequence.

use std::collections::HashMap;
use crate::ast::*;
use crate::error::Error;
use crate::vm::{CharSet, CharSetItem, Inst};

// ---------------------------------------------------------------------------
// Options passed to the compiler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    /// ONIG_OPTION_IGNORE_CASE
    pub ignore_case: bool,
    /// ONIG_OPTION_MULTILINE (Ruby (?m): dot matches newline)
    pub multiline: bool,
}

// ---------------------------------------------------------------------------
// Compiler context
// ---------------------------------------------------------------------------

struct Compiler {
    prog: Vec<Inst>,
    charsets: Vec<CharSet>,
    /// Maps 1-based group index → start PC (for subexpression calls)
    subexp_starts: HashMap<u32, usize>,
    /// Groups that need their start PCs backfilled after compilation
    pending_calls: Vec<(usize, GroupRef)>,
    named_groups: Vec<(String, u32)>,
    base_flags: Flags,
    /// Total number of capture groups (1-based max)
    num_groups: u32,
}

impl Compiler {
    fn new(base_flags: Flags, named_groups: Vec<(String, u32)>) -> Self {
        Compiler {
            prog: Vec::new(),
            charsets: Vec::new(),
            subexp_starts: HashMap::new(),
            pending_calls: Vec::new(),
            named_groups,
            base_flags,
            num_groups: 0,
        }
    }

    fn emit(&mut self, inst: Inst) -> usize {
        let pc = self.prog.len();
        self.prog.push(inst);
        pc
    }

    fn pc(&self) -> usize {
        self.prog.len()
    }

    fn patch_jump(&mut self, pc: usize, target: usize) {
        match &mut self.prog[pc] {
            Inst::Jump(t) => *t = target,
            Inst::Fork(t) => *t = target,
            Inst::ForkNext(t) => *t = target,
            Inst::AtomicStart(t) => *t = target,
            Inst::LookStart { end_pc, .. } => *end_pc = target,
            Inst::AbsenceStart(t) => *t = target,
            Inst::CheckGroup { yes_pc, .. } => *yes_pc = target,
            other => panic!("patch_jump called on non-jump inst {:?}", other),
        }
    }

    fn patch_no_jump(&mut self, pc: usize, target: usize) {
        match &mut self.prog[pc] {
            Inst::CheckGroup { no_pc, .. } => *no_pc = target,
            other => panic!("patch_no_jump called on {:?}", other),
        }
    }

    fn add_charset(&mut self, cs: CharSet) -> usize {
        let idx = self.charsets.len();
        self.charsets.push(cs);
        idx
    }

    // -----------------------------------------------------------------------
    // Compile a node with current flags
    // -----------------------------------------------------------------------

    fn compile_node(&mut self, node: &Node, flags: Flags) -> Result<(), Error> {
        match node {
            Node::Empty => {}

            Node::Literal(c) => {
                if flags.ignore_case {
                    self.emit(Inst::Char(*c, true));
                } else {
                    self.emit(Inst::Char(*c, false));
                }
            }

            Node::AnyChar => {
                self.emit(Inst::AnyChar(flags.multiline));
            }

            Node::Shorthand(sh) => {
                self.emit(Inst::Shorthand(*sh, flags.ascii_range));
            }

            Node::UnicodeProp { name, negate } => {
                self.emit(Inst::Prop(name.clone(), *negate));
            }

            Node::Anchor(kind) => {
                self.emit(Inst::Anchor(*kind, flags));
            }

            Node::CharClass(cc) => {
                let cs = compile_charset(cc, flags.ignore_case, flags.ascii_range);
                let idx = self.add_charset(cs);
                self.emit(Inst::Class(idx, flags.ignore_case));
            }

            Node::Concat(nodes) => {
                for n in nodes {
                    self.compile_node(n, flags)?;
                }
            }

            Node::Alternation(alts) => {
                self.compile_alternation(alts, flags)?;
            }

            Node::Quantifier { node, range, kind } => {
                self.compile_quantifier(node, range, kind, flags)?;
            }

            Node::Capture { index, node, flags: inner_flags } => {
                let f = flags.apply_on(&FlagMod { on: *inner_flags, off: Flags::default() });
                let idx = *index;
                if idx > self.num_groups { self.num_groups = idx; }
                let start_pc = self.pc();
                self.subexp_starts.insert(idx, start_pc);
                let slot_open = ((idx - 1) * 2) as usize;
                let slot_close = slot_open + 1;
                self.emit(Inst::Save(slot_open));
                self.compile_node(node, f)?;
                self.emit(Inst::Save(slot_close));
                self.emit(Inst::RetIfCalled);
            }

            Node::NamedCapture { name: _, index, node, flags: inner_flags } => {
                let f = flags.apply_on(&FlagMod { on: *inner_flags, off: Flags::default() });
                let idx = *index;
                if idx > self.num_groups { self.num_groups = idx; }
                let start_pc = self.pc();
                self.subexp_starts.insert(idx, start_pc);
                let slot_open = ((idx - 1) * 2) as usize;
                let slot_close = slot_open + 1;
                self.emit(Inst::Save(slot_open));
                self.compile_node(node, f)?;
                self.emit(Inst::Save(slot_close));
                self.emit(Inst::RetIfCalled);
            }

            Node::Group { node, flags: inner_flags } => {
                let f = flags.apply_on(&FlagMod { on: *inner_flags, off: Flags::default() });
                self.compile_node(node, f)?;
            }

            Node::Atomic(node) => {
                let atomic_start = self.emit(Inst::AtomicStart(0)); // patch later
                self.compile_node(node, flags)?;
                let end_pc = self.pc();
                self.emit(Inst::AtomicEnd);
                self.patch_jump(atomic_start, end_pc);
            }

            Node::LookAround { dir, pol, node } => {
                let behind_lens = if *dir == LookDir::Behind {
                    compute_widths(node)
                } else {
                    Some(Vec::new())
                };
                let positive = *pol == LookPol::Positive;
                let look_start = self.emit(Inst::LookStart {
                    positive,
                    dir: *dir,
                    end_pc: 0,
                    behind_lens,
                });
                self.compile_node(node, flags)?;
                let end_pc = self.pc();
                self.emit(Inst::LookEnd);
                self.patch_jump(look_start, end_pc);
            }

            Node::Keep => {
                self.emit(Inst::KeepStart);
            }

            Node::BackRef { target, level } => {
                let ignore_case = flags.ignore_case;
                let inst = match target {
                    GroupRef::Index(n) => Inst::BackRef(*n, ignore_case, *level),
                    GroupRef::Name(name) => {
                        // Resolve name to group index
                        let idx = self.resolve_name(name)?;
                        Inst::BackRef(idx, ignore_case, *level)
                    }
                    GroupRef::RelativeBack(n) => Inst::BackRefRelBack(*n, ignore_case),
                    GroupRef::RelativeFwd(_) => {
                        return Err(Error::Compile("relative-forward backreference not supported".into()));
                    }
                    GroupRef::Whole => return Err(Error::Compile("\\k<0> backreference to whole pattern not supported".into())),
                };
                self.emit(inst);
            }

            Node::SubexpCall(target) => {
                let call_pc = self.emit(Inst::Call(0)); // patch later
                self.pending_calls.push((call_pc, target.clone()));
            }

            Node::InlineFlags { flags: flag_mod, node } => {
                let new_flags = flags.apply_on(flag_mod);
                self.compile_node(node, new_flags)?;
            }

            Node::Absence(inner) => {
                self.compile_absence(inner, flags)?;
            }

            Node::Conditional { cond, yes, no } => {
                self.compile_conditional(cond, yes, no, flags)?;
            }
        }
        Ok(())
    }

    fn resolve_name(&self, name: &str) -> Result<u32, Error> {
        // Find last group with this name (Onigmo semantics)
        self.named_groups.iter().rev()
            .find(|(n, _)| n == name)
            .map(|(_, idx)| *idx)
            .ok_or_else(|| Error::Compile(format!("undefined group name {:?}", name)))
    }

    // -----------------------------------------------------------------------
    // Alternation
    // -----------------------------------------------------------------------

    fn compile_alternation(&mut self, alts: &[Node], flags: Flags) -> Result<(), Error> {
        if alts.is_empty() { return Ok(()); }
        if alts.len() == 1 { return self.compile_node(&alts[0], flags); }

        // For N alternatives: emit Fork chain
        let mut fork_pcs = Vec::new();
        let mut jump_pcs = Vec::new();

        for (i, alt) in alts.iter().enumerate() {
            if i < alts.len() - 1 {
                // Fork: try next alternative if this one fails
                let fork_pc = self.emit(Inst::Fork(0)); // patch to next alt
                fork_pcs.push(fork_pc);
            }
            self.compile_node(alt, flags)?;
            if i < alts.len() - 1 {
                let jump_pc = self.emit(Inst::Jump(0)); // patch to after all alts
                jump_pcs.push(jump_pc);
                // Patch the fork to point to just after the Jump (= start of next alt)
                let next_alt_pc = self.pc();
                self.patch_jump(*fork_pcs.last().unwrap(), next_alt_pc);
            }
        }
        // Patch all jumps to after everything
        let end_pc = self.pc();
        for jpc in jump_pcs {
            self.patch_jump(jpc, end_pc);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Quantifiers
    // -----------------------------------------------------------------------

    fn compile_quantifier(
        &mut self,
        node: &Node,
        range: &QuantRange,
        kind: &QuantKind,
        flags: Flags,
    ) -> Result<(), Error> {
        let min = range.min;
        let max = range.max;

        // Emit `min` mandatory copies
        for _ in 0..min {
            self.compile_node(node, flags)?;
        }

        match (max, kind) {
            // {n} — exactly n
            (Some(m), _) if m == min => {} // done above

            // Greedy / reluctant {n,}
            (None, QuantKind::Greedy) => {
                // loop: Fork(exit), body, Jump(Fork)
                let fork_pc = self.emit(Inst::Fork(0));
                self.compile_node(node, flags)?;
                let jump_pc = self.emit(Inst::Jump(fork_pc));
                let _ = jump_pc;
                self.patch_jump(fork_pc, self.pc());
            }
            (None, QuantKind::Reluctant) => {
                let fork_pc = self.emit(Inst::ForkNext(0)); // try exit first (lazy)
                let body_start = self.pc();
                self.compile_node(node, flags)?;
                self.emit(Inst::Jump(fork_pc));
                let end_pc = self.pc();
                self.patch_jump(fork_pc, body_start); // on failure: try body
                // Actually ForkNext(end_pc): try end_pc first (skip body), else continue
                // Rewrite: ForkNext means "try pc+1 first (body), else jump"
                // Let's redefine: ForkNext(alt) = reluctant: try alt first, else pc+1
                // So ForkNext(exit): try exit (skip) first, then on re-try go to body
                // Hmm, actually for reluctant we want: first attempt = zero iterations.
                // Lazy *? : try 0 first, then 1, then 2...
                // Code: ForkNext(exit_pc); [body]; Jump(ForkNext)
                // ForkNext(exit): saves (ForkNext+1) as retry, tries exit first
                // On retry: tries pc+1 (body)
                // This needs careful re-examination.
                // Let me re-clarify the semantics:
                // Fork(alt): greedy — try pc+1 (continue), on failure retry via alt
                // ForkNext(alt): lazy — try alt (skip/exit) first, on failure retry via pc+1
                // For lazy *?: ForkNext(exit); body; Jump(ForkNext)
                // At ForkNext: we try exit first (0 iterations). If outer fails, we retry
                // and try body (1 iteration). After body, jump back to ForkNext for next iteration.
                // This is correct! The patch above is wrong. Let me redo.
                // We already emitted: ForkNext(0-placeholder), body, Jump(fork_pc)
                // We need ForkNext to point to exit (after the jump), which is self.pc()
                self.patch_jump(fork_pc, end_pc);
            }
            (None, QuantKind::Possessive) => {
                // Atomic loop
                let atomic_start = self.emit(Inst::AtomicStart(0));
                let fork_pc = self.emit(Inst::Fork(0));
                self.compile_node(node, flags)?;
                self.emit(Inst::Jump(fork_pc));
                let loop_end = self.pc();
                self.emit(Inst::AtomicEnd);
                self.patch_jump(fork_pc, loop_end);
                let after_atomic = self.pc();
                self.patch_jump(atomic_start, loop_end);
                // Wait, AtomicStart needs end_pc which is the AtomicEnd instruction's pc
                // The loop_end (before AtomicEnd) is where Fork jumps on "no more"
                // AtomicEnd is at loop_end, after_atomic = loop_end + 1
                // Fix: AtomicStart(atomic_end_pc) where atomic_end_pc = index of AtomicEnd
                let _ = after_atomic;
                self.patch_jump(atomic_start, loop_end);
            }

            // Greedy {n,m}
            (Some(m), QuantKind::Greedy) => {
                let extra = m - min;
                let fork_pcs: Vec<usize> = (0..extra).map(|_| {
                    let fp = self.emit(Inst::Fork(0));
                    fp
                }).collect();
                // We need to interleave: Fork, body, Fork, body, ...
                // But we've emitted all forks first which is wrong.
                // Redo: emit each fork then body
                // Let me rewrite this properly.
                // Actually I jumped the gun. The prog already has the mandatory iterations.
                // For {n,m}: after n mandatory, emit (m-n) optional ones:
                //   Fork(exit), body, Fork(exit), body, ..., Fork(exit), body
                // Each Fork jumps to the position after all optional iterations.
                // But we need to emit them in sequence.
                // Since I already started emitting fork_pcs (which is wrong), let me fix:
                // Remove the emitted Fork instructions and redo.
                // Actually we haven't emitted them yet (the code above is in the `map` closure
                // but only executed when we call collect). Wait, we DID call collect. Oops.
                // This code is wrong. Let me restructure.
                // The issue: I collect all fork PCs first, then need to interleave bodies.
                // Instead, remove those fork pcs and redo:
                let _ = fork_pcs; // ignore incorrectly emitted forks
                // Note: the forks we just emitted are wrong (they're all grouped together
                // with no bodies between them). We need to truncate and redo.
                // This is a design mistake. Let me truncate and redo properly.
                // ... we can't easily truncate. Let me just not use this approach.
                // The fix: don't pre-allocate fork_pcs. Instead compile iteratively.
                // Since this branch is already wrong, let's replace it entirely.
                // I'll fall through to a different implementation.
                // 
                // REAL FIX: This needs to be rewritten. See below.
                // For now, emit a hacky version: since we already emitted `extra` Fork(0)
                // instructions, we need to fill them in. Let's insert bodies between them...
                // This is too complex to fix inline. I'll write a separate helper.
                return self.compile_counted_optional(node, min, m, kind, flags, &fork_pcs);
            }

            (Some(m), QuantKind::Reluctant) => {
                for _ in min..m {
                    let fork_pc = self.emit(Inst::ForkNext(0));
                    self.compile_node(node, flags)?;
                    let after = self.pc();
                    self.patch_jump(fork_pc, after);
                }
            }

            (Some(m), QuantKind::Possessive) => {
                let extra = m - min;
                // Atomic wrapper around optional iterations
                let atomic_start = self.emit(Inst::AtomicStart(0));
                for _ in 0..extra {
                    let fork_pc = self.emit(Inst::Fork(0));
                    self.compile_node(node, flags)?;
                    let after = self.pc();
                    self.patch_jump(fork_pc, after); // hmm, all fork to same exit
                }
                let atom_end = self.pc();
                self.emit(Inst::AtomicEnd);
                self.patch_jump(atomic_start, atom_end);
            }
        }
        Ok(())
    }

    fn compile_counted_optional(
        &mut self,
        node: &Node,
        min: u32,
        max: u32,
        kind: &QuantKind,
        flags: Flags,
        pre_emitted_forks: &[usize],
    ) -> Result<(), Error> {
        // Remove pre-emitted (wrong) Fork instructions by truncating
        let truncate_to = if pre_emitted_forks.is_empty() {
            self.prog.len()
        } else {
            pre_emitted_forks[0]
        };
        self.prog.truncate(truncate_to);

        let extra = max - min;
        // All optional Fork instructions need to point to the same "exit" location.
        // We'll collect their pcs and patch them all at the end.
        let mut fork_pcs = Vec::new();
        for _ in 0..extra {
            let fp = self.emit(Inst::Fork(0));
            fork_pcs.push(fp);
            self.compile_node(node, flags)?;
        }
        let exit_pc = self.pc();
        for fp in fork_pcs {
            self.patch_jump(fp, exit_pc);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Absence operator (?~X)
    // -----------------------------------------------------------------------

    fn compile_absence(&mut self, inner: &Node, flags: Flags) -> Result<(), Error> {
        // Compile inner pattern as a sub-program stored inline.
        // Structure:
        //   AbsenceStart(inner_end_pc)  ← absence instruction; starts the greedy loop
        //   [inner pattern]             ← sub-program for "what to avoid"
        //   AbsenceEnd                  ← terminates inner (like Match for inner exec)
        // The VM interprets AbsenceStart by running the greedy loop.
        let absence_pc = self.emit(Inst::AbsenceStart(0)); // patch inner_end
        self.compile_node(inner, flags)?;
        let inner_end = self.pc();
        self.emit(Inst::AbsenceEnd);
        self.patch_jump(absence_pc, inner_end);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Conditional group
    // -----------------------------------------------------------------------

    fn compile_conditional(
        &mut self,
        cond: &Condition,
        yes: &Node,
        no: &Node,
        flags: Flags,
    ) -> Result<(), Error> {
        // CheckGroup(slot_pair, yes_pc, no_pc)
        // slot_pair: 0-based index into groups (group_num - 1) * 2
        let slot_pair = match cond {
            Condition::GroupNum(n) => ((*n - 1) * 2) as usize,
            Condition::GroupName(name) => {
                let idx = self.resolve_name(name).unwrap_or(1);
                ((idx - 1) * 2) as usize
            }
        };
        let check_pc = self.emit(Inst::CheckGroup { slot: slot_pair, yes_pc: 0, no_pc: 0 });
        // yes branch starts here
        let yes_pc = self.pc();
        self.compile_node(yes, flags)?;
        let jump_pc = self.emit(Inst::Jump(0));
        // no branch
        let no_pc = self.pc();
        self.compile_node(no, flags)?;
        let end_pc = self.pc();
        // patch
        self.patch_jump(check_pc, yes_pc);
        self.patch_no_jump(check_pc, no_pc);
        self.patch_jump(jump_pc, end_pc);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Backfill subexpression calls
    // -----------------------------------------------------------------------

    fn backfill_calls(&mut self) -> Result<(), Error> {
        let pending = std::mem::take(&mut self.pending_calls);
        for (call_pc, target) in pending {
            let target_pc = match &target {
                GroupRef::Index(n) => {
                    self.subexp_starts.get(n).copied()
                        .ok_or_else(|| Error::Compile(format!("undefined group {} for subexpr call", n)))?
                }
                GroupRef::Name(name) => {
                    let idx = self.named_groups.iter().rev()
                        .find(|(n, _)| n == name)
                        .map(|(_, i)| *i)
                        .ok_or_else(|| Error::Compile(format!("undefined group name {:?}", name)))?;
                    self.subexp_starts.get(&idx).copied()
                        .ok_or_else(|| Error::Compile(format!("group {:?} has no start PC", name)))?
                }
                GroupRef::Whole => 0, // whole pattern starts at 0
                GroupRef::RelativeBack(_) | GroupRef::RelativeFwd(_) => {
                    return Err(Error::Compile("relative subexpression calls not yet supported".into()));
                }
            };
            match &mut self.prog[call_pc] {
                Inst::Call(t) => *t = target_pc,
                _ => panic!("expected Call at {}", call_pc),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CharSet construction from AST CharClass
// ---------------------------------------------------------------------------

pub fn compile_charset(cc: &CharClass, ignore_case: bool, ascii_range: bool) -> CharSet {
    let items = cc.items.iter().map(|item| compile_class_item(item, ascii_range, ignore_case)).collect();
    let intersections = cc.intersections.iter().map(|ic| compile_charset(ic, ignore_case, ascii_range)).collect();
    CharSet { negate: cc.negate, items, intersections }
}

fn compile_class_item(item: &ClassItem, ascii_range: bool, ignore_case: bool) -> CharSetItem {
    match item {
        ClassItem::Char(c) => CharSetItem::Char(*c),
        ClassItem::Range(lo, hi) => CharSetItem::Range(*lo, *hi),
        ClassItem::Shorthand(sh) => CharSetItem::Shorthand(*sh, ascii_range),
        ClassItem::Posix(cls, neg) => CharSetItem::Posix(*cls, *neg),
        ClassItem::Unicode(name, neg) => CharSetItem::Unicode(name.clone(), *neg),
        ClassItem::Nested(inner) => CharSetItem::Nested(compile_charset(inner, ignore_case, ascii_range)),
    }
}

// ---------------------------------------------------------------------------
// Width computation for lookbehind
// ---------------------------------------------------------------------------

/// Returns `true` if `node` can only match strings of bounded (finite) byte length.
fn is_finite_width(node: &Node) -> bool {
    match node {
        Node::Quantifier { range, node, .. } => {
            range.max.is_some() && is_finite_width(node)
        }
        Node::Concat(nodes) => nodes.iter().all(is_finite_width),
        Node::Alternation(alts) => alts.iter().all(is_finite_width),
        Node::Group { node, .. } | Node::Capture { node, .. } | Node::NamedCapture { node, .. }
        | Node::Atomic(node) | Node::InlineFlags { node, .. } => is_finite_width(node),
        _ => true,
    }
}

/// Compute the set of possible byte widths for a node.
/// Returns `None` if the node can match strings of unbounded length (e.g. `a*`).
/// Used for look-behind to determine how far to step back.
fn compute_widths(node: &Node) -> Option<Vec<usize>> {
    if !is_finite_width(node) {
        return None;
    }
    let mut set = std::collections::BTreeSet::new();
    collect_widths(node, 0, &mut set);
    Some(set.into_iter().collect())
}

fn collect_widths(node: &Node, base: usize, out: &mut std::collections::BTreeSet<usize>) {
    match node {
        Node::Empty | Node::Anchor(_) | Node::Keep => { out.insert(base); }
        Node::Literal(_) | Node::AnyChar | Node::Shorthand(_)
        | Node::UnicodeProp { .. } | Node::CharClass(_) => {
            // Approximate: 1 char = 1-4 bytes; use byte count range.
            // For simplicity, use 1..=4 for non-ASCII capable nodes.
            // For ASCII-only patterns this is 1.
            // We'll insert base+1 to base+4 as possible widths.
            for bw in 1..=4usize { out.insert(base + bw); }
        }
        Node::Concat(nodes) => {
            let mut cur = std::collections::BTreeSet::new();
            cur.insert(base);
            for n in nodes {
                let mut next = std::collections::BTreeSet::new();
                for &w in &cur {
                    collect_widths(n, w, &mut next);
                }
                cur = next;
            }
            out.extend(cur);
        }
        Node::Alternation(alts) => {
            for alt in alts {
                collect_widths(alt, base, out);
            }
        }
        Node::Group { node, .. } | Node::Capture { node, .. } | Node::NamedCapture { node, .. }
        | Node::Atomic(node) | Node::InlineFlags { node, .. } => {
            collect_widths(node, base, out);
        }
        Node::Quantifier { node, range, .. } => {
            let min = range.min as usize;
            let max = range.max.expect("collect_widths called on unbounded quantifier") as usize;
            for count in min..=max {
                let mut sub = std::collections::BTreeSet::new();
                sub.insert(base);
                for _ in 0..count {
                    let mut next = std::collections::BTreeSet::new();
                    for &w in &sub {
                        collect_widths(node, w, &mut next);
                    }
                    sub = next;
                }
                out.extend(sub);
            }
        }
        // Lookarounds are zero-width
        Node::LookAround { .. } => { out.insert(base); }
        _ => { out.insert(base); }
    }
}

// ---------------------------------------------------------------------------
// Public compile function
// ---------------------------------------------------------------------------

pub struct CompiledProgram {
    pub prog: Vec<Inst>,
    pub charsets: Vec<CharSet>,
    pub named_groups: Vec<(String, u32)>,
    pub num_groups: usize,
    pub subexp_starts: HashMap<u32, usize>,
}

pub fn compile(
    node: &Node,
    named_groups: Vec<(String, u32)>,
    opts: CompileOptions,
) -> Result<CompiledProgram, Error> {
    let base_flags = Flags {
        ignore_case: opts.ignore_case,
        multiline: opts.multiline,
        ..Default::default()
    };

    let mut compiler = Compiler::new(base_flags, named_groups.clone());
    compiler.compile_node(node, base_flags)?;
    compiler.emit(Inst::Match);
    compiler.backfill_calls()?;

    let num_groups = compiler.num_groups as usize;
    Ok(CompiledProgram {
        prog: compiler.prog,
        charsets: compiler.charsets,
        named_groups,
        num_groups,
        subexp_starts: compiler.subexp_starts,
    })
}
