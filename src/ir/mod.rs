//! Intermediate Representation (IR) for the Oniai regex engine.
//!
//! The IR sits between the parsed AST and the flat `Vec<Inst>` that the VM
//! executes:
//!
//! ```text
//! AST → IrBuilder → IrProgram → [passes] → IrLower → Vec<Inst> → vm.rs
//! ```

pub mod build;
pub mod lower;
pub mod pass;
pub mod prefilter;
pub mod verify;
#[cfg(feature = "jit")]
pub mod jit;

use crate::ast::{AnchorKind, Flags, LookDir, LookPol};
use crate::bytetrie::ByteTrie;
use crate::vm::CharSet;

pub type BlockId = usize;
pub type RegionId = usize;

/// Bit set for tracking live capture slots.
#[derive(Debug, Clone, Default)]
pub struct LiveSlots {
    words: Vec<u64>,
}

impl PartialEq for LiveSlots {
    fn eq(&self, other: &Self) -> bool {
        let len = self.words.len().max(other.words.len());
        (0..len).all(|i| {
            self.words.get(i).copied().unwrap_or(0) == other.words.get(i).copied().unwrap_or(0)
        })
    }
}

impl LiveSlots {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn set(&mut self, slot: usize) {
        let (w, b) = (slot / 64, slot % 64);
        while self.words.len() <= w {
            self.words.push(0);
        }
        self.words[w] |= 1u64 << b;
    }

    pub fn get(&self, slot: usize) -> bool {
        let (w, b) = (slot / 64, slot % 64);
        if w >= self.words.len() {
            return false;
        }
        (self.words[w] >> b) & 1 != 0
    }

    pub fn union_with(&mut self, o: &LiveSlots) {
        while self.words.len() < o.words.len() {
            self.words.push(0);
        }
        for (a, &b) in self.words.iter_mut().zip(o.words.iter()) {
            *a |= b;
        }
    }

}

/// A candidate in an IR fork (guard + target block).
#[derive(Debug, Clone)]
pub struct IrForkCandidate {
    pub guard: IrGuard,
    pub block: BlockId,
}

/// A guard condition evaluated at the current position (zero-width or peek).
#[derive(Debug, Clone)]
pub enum IrGuard {
    /// Always true — unconditional candidate.
    Always,
    /// Peek: `text[pos] == c` (does not advance pos).
    Char(char),
    /// Peek: `charsets[id].matches(text[pos])`.
    #[allow(dead_code)]
    Class { id: usize, ignore_case: bool },
    /// True iff capture slot `slot` has matched.
    GroupMatched(usize),
    /// Run sub-region as zero-width lookaround guard.
    LookAround {
        pol: LookPol,
        #[allow(dead_code)]
        dir: LookDir,
        body: RegionId,
    },
}

/// A non-branching IR instruction inside a basic block.
#[derive(Debug, Clone)]
pub enum IrStmt {
    // Forward matching
    MatchChar(char),
    MatchAnyChar {
        dotall: bool,
    },
    MatchClass {
        id: usize,
        ignore_case: bool,
    },
    // Backward matching (lookbehind bodies)
    MatchCharBack(char),
    MatchAnyCharBack {
        dotall: bool,
    },
    MatchClassBack {
        id: usize,
        ignore_case: bool,
    },
    // Case-fold sequences
    MatchFoldSeq(Vec<char>),
    MatchFoldSeqBack(Vec<char>),
    // Trie-based literal alternation
    MatchAltTrie(usize),
    MatchAltTrieBack(usize),
    // Zero-width assertions
    CheckAnchor(AnchorKind, Flags),
    // Backreferences
    CheckBackRef {
        group: u32,
        ignore_case: bool,
        level: Option<i32>,
    },
    // State side-effects
    SaveCapture(usize),
    KeepStart,
    CounterInit(usize),
    NullCheckBegin(usize),
}

/// The terminator of a basic block — the only branching instruction.
#[derive(Debug, Clone)]
pub enum IrTerminator {
    /// Pattern matched (main region only).
    Match,
    /// Sub-program succeeded; return to invoking terminator.
    RegionEnd,
    /// Unconditional branch to block.
    Branch(BlockId),
    /// N-way fork.
    Fork {
        candidates: Vec<IrForkCandidate>,
        disjoint: bool,
        live_slots: LiveSlots,
    },
    /// Advance pos while text[pos] == c; then jump to exit.
    SpanChar { c: char, exit: BlockId },
    /// Advance pos while charsets[id] matches; then jump to exit.
    SpanClass { id: usize, exit: BlockId },
    /// Null-check end: if pos == saved_pos → exit, else → cont.
    NullCheckEnd {
        slot: usize,
        exit: BlockId,
        cont: BlockId,
    },
    /// Increment counter; if new value < count → body, else fall through to exit.
    CounterNext {
        slot: usize,
        count: u32,
        body: BlockId,
        exit: BlockId,
    },
    /// Push ret onto call stack; jump to target.
    Call { target: BlockId, ret: BlockId },
    /// If call stack non-empty, pop and return; else fall through.
    RetIfCalled { fallthrough: BlockId },
    /// Run atomic body region; on success drain bt stack to barrier, then next.
    Atomic { body: RegionId, next: BlockId },
    /// Run absence inner region; if inner never matches, continue to next.
    Absence { inner: RegionId, next: BlockId },
}

/// A basic block: straight-line stmts + one terminator.
#[derive(Debug, Clone)]
pub struct IrBlock {
    pub stmts: Vec<IrStmt>,
    pub term: IrTerminator,
}

/// The kind of a region, determining its execution context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionKind {
    Main,
    LookAhead {
        positive: bool,
    },
    LookBehind {
        positive: bool,
    },
    Atomic,
    Absence,
    #[allow(dead_code)]
    Subroutine {
        group: u32,
    },
}

/// A self-contained sub-CFG.
#[derive(Debug, Clone)]
pub struct IrRegion {
    pub blocks: Vec<IrBlock>,
    pub entry: BlockId,
    pub kind: RegionKind,
}

/// The top-level IR program.
#[derive(Debug, Clone)]
pub struct IrProgram {
    /// regions[0] is always the main pattern.
    pub regions: Vec<IrRegion>,
    pub charsets: Vec<CharSet>,
    pub alt_tries: Vec<ByteTrie>,
    pub num_captures: usize,
    pub num_counters: usize,
    pub num_null_checks: usize,
    pub use_memo: bool,
    pub named_groups: Vec<(String, u32)>,
}
