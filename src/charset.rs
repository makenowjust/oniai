/// Character set matching utilities.

use crate::ast::{PosixClass, Shorthand};

/// Test whether a character matches a POSIX class.
/// `ascii_range`: if true, only ASCII characters can match non-ASCII classes.
pub fn matches_posix(cls: PosixClass, ch: char, ascii_range: bool) -> bool {
    if ascii_range && ch as u32 > 127 {
        return false;
    }
    match cls {
        PosixClass::Alnum  => ch.is_alphanumeric(),
        PosixClass::Alpha  => ch.is_alphabetic(),
        PosixClass::Ascii  => (ch as u32) <= 127,
        PosixClass::Blank  => ch == ' ' || ch == '\t',
        PosixClass::Cntrl  => (ch as u32) < 32 || ch as u32 == 127,
        PosixClass::Digit  => ch.is_ascii_digit(),
        PosixClass::Graph  => ch > ' ' && ch as u32 != 127 && !ch.is_control(),
        PosixClass::Lower  => ch.is_lowercase(),
        PosixClass::Print  => (ch >= ' ' && ch as u32 != 127) || (!ascii_range && !ch.is_control()),
        PosixClass::Punct  => ch.is_ascii_punctuation(),
        PosixClass::Space  => matches_shorthand(Shorthand::Space, ch, true),
        PosixClass::Upper  => ch.is_uppercase(),
        PosixClass::XDigit => ch.is_ascii_hexdigit(),
        PosixClass::Word   => matches_shorthand(Shorthand::Word, ch, ascii_range),
    }
}

/// Test whether a character matches a shorthand class.
pub fn matches_shorthand(sh: Shorthand, ch: char, ascii_range: bool) -> bool {
    match sh {
        Shorthand::Word    => matches_word(ch, ascii_range),
        Shorthand::NonWord => !matches_word(ch, ascii_range),
        Shorthand::Digit   => matches_digit(ch, ascii_range),
        Shorthand::NonDigit=> !matches_digit(ch, ascii_range),
        Shorthand::Space   => matches_space(ch, ascii_range),
        Shorthand::NonSpace=> !matches_space(ch, ascii_range),
        Shorthand::HexDigit    => ch.is_ascii_hexdigit(),
        Shorthand::NonHexDigit => !ch.is_ascii_hexdigit(),
    }
}

fn matches_word(ch: char, ascii_range: bool) -> bool {
    if ascii_range {
        ch.is_ascii_alphanumeric() || ch == '_'
    } else {
        ch.is_alphanumeric() || ch == '_'
    }
}

fn matches_digit(ch: char, ascii_range: bool) -> bool {
    if ascii_range {
        ch.is_ascii_digit()
    } else {
        ch.is_numeric()
    }
}

fn matches_space(ch: char, _ascii_range: bool) -> bool {
    // Always use the full set; ascii_range controls non-ASCII Unicode spaces
    // For simplicity match the standard set
    matches!(ch, '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ')
}

/// Test a Unicode property name (basic POSIX-level properties).
pub fn matches_unicode_prop(name: &str, ch: char, negate: bool) -> bool {
    let result = match_prop(name, ch);
    if negate { !result } else { result }
}

fn match_prop(name: &str, ch: char) -> bool {
    match name {
        "Alnum"  => ch.is_alphanumeric(),
        "Alpha"  => ch.is_alphabetic(),
        "Blank"  => ch == ' ' || ch == '\t',
        "Cntrl"  => ch.is_control(),
        "Digit"  => ch.is_numeric(),
        "Graph"  => !ch.is_whitespace() && !ch.is_control(),
        "Lower"  => ch.is_lowercase(),
        "Print"  => !ch.is_control(),
        "Punct"  => ch.is_ascii_punctuation(),
        "Space"  => ch.is_whitespace(),
        "Upper"  => ch.is_uppercase(),
        "XDigit" => ch.is_ascii_hexdigit(),
        "Word"   => ch.is_alphanumeric() || ch == '_',
        "ASCII"  => (ch as u32) <= 127,
        // Unicode General Category approximations
        "L" | "Letter"             => ch.is_alphabetic(),
        "Lu" | "Uppercase_Letter"  => ch.is_uppercase(),
        "Ll" | "Lowercase_Letter"  => ch.is_lowercase(),
        "N" | "Number"             => ch.is_numeric(),
        "Nd" | "Decimal_Number"    => ch.is_ascii_digit(),
        "Z" | "Separator"         => ch.is_whitespace(),
        "Zs" | "Space_Separator"  => ch == ' ',
        "C" | "Other"             => ch.is_control(),
        "Cc" | "Control"          => ch.is_control(),
        "P" | "Punctuation"       => ch.is_ascii_punctuation(),
        _ => false,
    }
}
