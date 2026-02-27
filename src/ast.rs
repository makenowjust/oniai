//! Abstract Syntax Tree for Onigmo-compatible regular expressions.

/// Flags that can be set via inline options like (?imx)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    pub ignore_case: bool,
    pub multiline: bool, // dot matches newline (Ruby (?m))
    pub extended: bool,
    pub ascii_range: bool, // 'a' option: \w \d \s restricted to ASCII
}

impl Flags {
    pub fn apply_on(&self, other: &FlagMod) -> Flags {
        let mut f = *self;
        if other.on.ignore_case {
            f.ignore_case = true;
        }
        if other.on.multiline {
            f.multiline = true;
        }
        if other.on.extended {
            f.extended = true;
        }
        if other.on.ascii_range {
            f.ascii_range = true;
        }
        if other.off.ignore_case {
            f.ignore_case = false;
        }
        if other.off.multiline {
            f.multiline = false;
        }
        if other.off.extended {
            f.extended = false;
        }
        if other.off.ascii_range {
            f.ascii_range = false;
        }
        f
    }
}

#[derive(Debug, Clone, Default)]
pub struct FlagMod {
    pub on: Flags,
    pub off: Flags,
}

/// A quantifier range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantRange {
    pub min: u32,
    pub max: Option<u32>, // None = unbounded
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuantKind {
    Greedy,
    Reluctant,
    Possessive,
}

/// An element inside a character class `[...]`.
#[derive(Debug, Clone)]
pub enum ClassItem {
    Char(char),
    Range(char, char),
    Shorthand(Shorthand),
    Posix(PosixClass, bool /*negate*/),
    Unicode(String, bool /*negate*/),
    Nested(CharClass),
}

/// \w \d \s \h etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shorthand {
    Word,        // \w
    NonWord,     // \W
    Digit,       // \d
    NonDigit,    // \D
    Space,       // \s
    NonSpace,    // \S
    HexDigit,    // \h
    NonHexDigit, // \H
}

/// [:alnum:] etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PosixClass {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    XDigit,
    Word,
}

/// A character class `[...]`.
#[derive(Debug, Clone)]
pub struct CharClass {
    pub negate: bool,
    pub items: Vec<ClassItem>,
    /// For `[A&&B&&C]` — each element is AND-combined with the base items.
    pub intersections: Vec<CharClass>,
}

/// Condition in a conditional group (?(cond)yes|no)
#[derive(Debug, Clone)]
pub enum Condition {
    GroupNum(u32),
    GroupName(String),
}

/// The target of a backreference or subexpression call.
#[derive(Debug, Clone)]
pub enum GroupRef {
    Index(u32),
    Name(String),
    RelativeBack(u32), // \k<-n>
    #[allow(dead_code)]
    RelativeFwd(u32), // \g<+n>
    Whole,             // \g<0>
}

/// Anchor types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    Start,           // ^
    End,             // $
    StringStart,     // \A
    StringEnd,       // \z
    StringEndOrNl,   // \Z
    WordBoundary,    // \b
    NonWordBoundary, // \B
    SearchStart,     // \G
}

/// Lookaround direction/polarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookDir {
    Ahead,
    Behind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookPol {
    Positive,
    Negative,
}

/// The main AST node.
#[derive(Debug, Clone)]
pub enum Node {
    /// Empty pattern
    Empty,

    /// A literal character
    Literal(char),

    /// `.` any character
    AnyChar,

    /// A character class `[...]`
    CharClass(CharClass),

    /// A shorthand outside of a class (`\w`, `\d`, etc.)
    Shorthand(Shorthand),

    /// A Unicode property outside of a class
    UnicodeProp { name: String, negate: bool },

    /// An anchor (`^`, `$`, `\b`, etc.)
    Anchor(AnchorKind),

    /// Concatenation
    Concat(Vec<Node>),

    /// Alternation `a|b|c`
    Alternation(Vec<Node>),

    /// Quantified node
    Quantifier {
        node: Box<Node>,
        range: QuantRange,
        kind: QuantKind,
    },

    /// Capturing group
    Capture {
        index: u32, // 1-based
        node: Box<Node>,
        flags: Flags,
    },

    /// Named capturing group
    NamedCapture {
        #[allow(dead_code)]
        name: String,
        index: u32,
        node: Box<Node>,
        flags: Flags,
    },

    /// Non-capturing group
    Group { node: Box<Node>, flags: Flags },

    /// Atomic group (?>...)
    Atomic(Box<Node>),

    /// Look-around
    LookAround {
        dir: LookDir,
        pol: LookPol,
        node: Box<Node>,
    },

    /// \K — keep (reset match start)
    Keep,

    /// Backreference
    BackRef {
        target: GroupRef,
        /// Recursion level (None = current level, Some(0) = absolute, etc.)
        level: Option<i32>,
    },

    /// Subexpression call \g<n> \g<name>
    SubexpCall(GroupRef),

    /// Inline options that span to end of group: (?imx) without subexp
    InlineFlags { flags: FlagMod, node: Box<Node> },

    /// Absence operator (?~subexp)
    Absence(Box<Node>),

    /// Conditional (?(cond)yes|no)
    Conditional {
        cond: Condition,
        yes: Box<Node>,
        no: Box<Node>,
    },
}
