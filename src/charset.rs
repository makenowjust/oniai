/// Character set matching utilities.
use crate::ast::{PosixClass, Shorthand};
use crate::data::general_category_data::{
    GC_CC, GC_CF, GC_CN, GC_CO, GC_CS, GC_LL, GC_LM, GC_LO, GC_LT, GC_LU, GC_MC, GC_ME,
    GC_MN, GC_ND, GC_NL, GC_NO, GC_PC, GC_PD, GC_PE, GC_PF, GC_PI, GC_PO, GC_PS, GC_RANGES,
    GC_SC, GC_SK, GC_SM, GC_SO, GC_ZL, GC_ZP, GC_ZS,
};
use crate::data::unicode_prop_ranges_data::{
    ALPHABETIC_RANGES, ALPHANUMERIC_RANGES, LOWERCASE_RANGES, MATH_RANGES, NUMERIC_RANGES,
    UPPERCASE_RANGES, WHITESPACE_RANGES,
};
use crate::general_category::{GeneralCategory, get_general_category};

/// Test whether a character matches a POSIX class.
/// `ascii_range`: if true, only ASCII characters can match non-ASCII classes.
pub fn matches_posix(cls: PosixClass, ch: char, ascii_range: bool) -> bool {
    if ascii_range && ch as u32 > 127 {
        return false;
    }
    match cls {
        PosixClass::Alnum => ch.is_alphanumeric(),
        PosixClass::Alpha => ch.is_alphabetic(),
        PosixClass::Ascii => (ch as u32) <= 127,
        PosixClass::Blank => ch == ' ' || ch == '\t',
        PosixClass::Cntrl => (ch as u32) < 32 || ch as u32 == 127,
        PosixClass::Digit => ch.is_ascii_digit(),
        PosixClass::Graph => ch > ' ' && ch as u32 != 127 && !ch.is_control(),
        PosixClass::Lower => ch.is_lowercase(),
        PosixClass::Print => (ch >= ' ' && ch as u32 != 127) || (!ascii_range && !ch.is_control()),
        PosixClass::Punct => ch.is_ascii_punctuation(),
        PosixClass::Space => matches_shorthand(Shorthand::Space, ch, true),
        PosixClass::Upper => ch.is_uppercase(),
        PosixClass::XDigit => ch.is_ascii_hexdigit(),
        PosixClass::Word => matches_shorthand(Shorthand::Word, ch, ascii_range),
    }
}

/// Test whether a character matches a shorthand class.
pub fn matches_shorthand(sh: Shorthand, ch: char, ascii_range: bool) -> bool {
    match sh {
        Shorthand::Word => matches_word(ch, ascii_range),
        Shorthand::NonWord => !matches_word(ch, ascii_range),
        Shorthand::Digit => matches_digit(ch, ascii_range),
        Shorthand::NonDigit => !matches_digit(ch, ascii_range),
        Shorthand::Space => matches_space(ch, ascii_range),
        Shorthand::NonSpace => !matches_space(ch, ascii_range),
        Shorthand::HexDigit => ch.is_ascii_hexdigit(),
        Shorthand::NonHexDigit => !ch.is_ascii_hexdigit(),
    }
}

/// Binary-search a sorted `(lo, hi)` char range table.
pub fn in_range_table(table: &[(char, char)], ch: char) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if ch < lo {
                std::cmp::Ordering::Greater
            } else if ch > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

fn matches_word(ch: char, ascii_range: bool) -> bool {
    if ascii_range {
        ch.is_ascii_alphanumeric() || ch == '_'
    } else {
        in_range_table(ALPHANUMERIC_RANGES, ch) || ch == '_'
    }
}

fn matches_digit(ch: char, ascii_range: bool) -> bool {
    if ascii_range {
        ch.is_ascii_digit()
    } else {
        in_range_table(NUMERIC_RANGES, ch)
    }
}

fn matches_space(ch: char, _ascii_range: bool) -> bool {
    matches!(ch, '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ')
}

/// Normalize a Unicode property name: lowercase, strip `_`/`-`/` `.
fn normalize_prop_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Test a Unicode property name.
pub fn matches_unicode_prop(name: &str, ch: char, negate: bool) -> bool {
    let result = match_prop(&normalize_prop_name(name), ch);
    if negate { !result } else { result }
}

/// Return `true` if the given Unicode property name is recognized.
pub fn is_known_unicode_prop(name: &str) -> bool {
    match_prop_known(&normalize_prop_name(name))
}

/// Return the GC ids that make up a purely GC-based property, if applicable.
///
/// Returns `None` for properties that cannot be expressed as a set of General
/// Category IDs (POSIX-like, binary).  Special string `""` signals "any"
/// (all codepoints); `"!cn"` signals "assigned" (all except Unassigned).
fn gc_ids_for_prop(norm: &str) -> Option<&'static [u8]> {
    match norm {
        // ---- Letter (L) ----
        "l" | "letter" => Some(&[GC_LL, GC_LM, GC_LO, GC_LT, GC_LU]),
        "lu" | "uppercaseletter" => Some(&[GC_LU]),
        "ll" | "lowercaseletter" => Some(&[GC_LL]),
        "lt" | "titlecaseletter" => Some(&[GC_LT]),
        "lm" | "modifierletter" => Some(&[GC_LM]),
        "lo" | "otherletter" => Some(&[GC_LO]),
        // ---- Mark (M) ----
        "m" | "mark" | "combiningmark" => Some(&[GC_MC, GC_ME, GC_MN]),
        "mn" | "nonspacingmark" => Some(&[GC_MN]),
        "mc" | "spacingmark" | "spacingcombiningmark" => Some(&[GC_MC]),
        "me" | "enclosingmark" => Some(&[GC_ME]),
        // ---- Number (N) ----
        "n" | "number" => Some(&[GC_ND, GC_NL, GC_NO]),
        "nd" | "decimalnumber" | "decimaldigitnumber" => Some(&[GC_ND]),
        "nl" | "letternumber" => Some(&[GC_NL]),
        "no" | "othernumber" => Some(&[GC_NO]),
        // ---- Punctuation (P) ----
        "p" | "punctuation" => Some(&[GC_PC, GC_PD, GC_PE, GC_PF, GC_PI, GC_PO, GC_PS]),
        "pc" | "connectorpunctuation" => Some(&[GC_PC]),
        "pd" | "dashpunctuation" => Some(&[GC_PD]),
        "ps" | "openpunctuation" => Some(&[GC_PS]),
        "pe" | "closepunctuation" => Some(&[GC_PE]),
        "pi" | "initialpunctuation" => Some(&[GC_PI]),
        "pf" | "finalpunctuation" => Some(&[GC_PF]),
        "po" | "otherpunctuation" => Some(&[GC_PO]),
        // ---- Symbol (S) ----
        "s" | "symbol" => Some(&[GC_SC, GC_SK, GC_SM, GC_SO]),
        "sm" | "mathsymbol" => Some(&[GC_SM]),
        "sc" | "currencysymbol" => Some(&[GC_SC]),
        "sk" | "modifiersymbol" => Some(&[GC_SK]),
        "so" | "othersymbol" => Some(&[GC_SO]),
        // ---- Separator (Z) ----
        "z" | "separator" => Some(&[GC_ZL, GC_ZP, GC_ZS]),
        "zs" | "spaceseparator" => Some(&[GC_ZS]),
        "zl" | "lineseparator" => Some(&[GC_ZL]),
        "zp" | "paragraphseparator" => Some(&[GC_ZP]),
        // ---- Other (C) — but NOT Cn alone (unassigned has implicit gaps) ----
        "c" | "other" => Some(&[GC_CC, GC_CF, GC_CN, GC_CO, GC_CS]),
        "cc" | "control" => Some(&[GC_CC]),
        "cf" | "format" => Some(&[GC_CF]),
        "cs" | "surrogate" => Some(&[GC_CS]),
        "co" | "privateuse" => Some(&[GC_CO]),
        // Cn (unassigned): GC_RANGES has no gaps and covers 0..10FFFF completely,
        // so explicit GC_CN entries in the table enumerate all unassigned codepoints.
        "cn" | "unassigned" | "notassigned" => Some(&[GC_CN]),
        // ---- Special ----
        // "any" and "assigned" are handled separately in unicode_prop_direct_ranges.
        _ => None,
    }
}

/// Build codepoint ranges for a Unicode property directly from pre-computed
/// static tables, without iterating over all 1.1 M codepoints.
///
/// Returns `None` for properties that have no pre-computed table (currently
/// only `Cn`/unassigned), in which case the caller should fall back to
/// `codepoints_matching`.
pub fn unicode_prop_direct_ranges(name: &str) -> Option<Vec<(char, char)>> {
    let norm = normalize_prop_name(name);

    // "any" covers all valid Unicode codepoints.
    if norm == "any" {
        return Some(vec![('\0', '\u{10FFFF}')]);
    }

    // "assigned" = every codepoint whose GC is not Cn.
    // Since GC_RANGES lists only assigned (or explicitly categorised) codepoints
    // and unlisted codepoints default to Cn, we include all entries except CN.
    if norm == "assigned" {
        let mut ranges = Vec::new();
        for &(lo, hi, gc) in GC_RANGES {
            if gc != GC_CN {
                push_gc_range(lo, hi, &mut ranges);
            }
        }
        return Some(ranges);
    }

    // Pre-computed tables for stdlib-based binary / POSIX-like properties.
    // "word" = alphanumeric + '_'.
    if norm == "word" {
        let mut ranges = ALPHANUMERIC_RANGES.to_vec();
        ranges.push(('_', '_'));
        ranges.sort_unstable();
        return Some(ranges);
    }
    let static_table: Option<&'static [(char, char)]> = match norm.as_str() {
        "alpha" | "alphabetic" => Some(ALPHABETIC_RANGES),
        "upper" | "uppercase" => Some(UPPERCASE_RANGES),
        "lower" | "lowercase" => Some(LOWERCASE_RANGES),
        "alnum" => Some(ALPHANUMERIC_RANGES),
        "whitespace" => Some(WHITESPACE_RANGES),
        "numeric" => Some(NUMERIC_RANGES),
        "math" => Some(MATH_RANGES),
        // ASCII-range properties with trivially small range lists.
        "ascii" => return Some(vec![('\0', '\x7F')]),
        "digit" => return Some(vec![('0', '9')]),
        "blank" => return Some(vec![('\t', '\t'), (' ', ' ')]),
        "cntrl" => return Some(vec![('\0', '\x1F'), ('\x7F', '\x7F')]),
        "xdigit" | "hexdigit" => return Some(vec![('0', '9'), ('A', 'F'), ('a', 'f')]),
        "space" => {
            return Some(vec![
                ('\t', '\t'),
                ('\n', '\n'),
                ('\x0B', '\x0B'),
                ('\x0C', '\x0C'),
                ('\r', '\r'),
                (' ', ' '),
            ])
        }
        "punct" => {
            // ASCII punctuation: !"#$%&'()*+,-./:;<=>?@[\]^_`{|}~ and DEL-adjacent
            return Some(vec![
                ('!', '/'),
                (':', '@'),
                ('[', '`'),
                ('{', '~'),
            ])
        }
        _ => None,
    };
    if let Some(table) = static_table {
        return Some(table.to_vec());
    }

    // GC-based properties: filter GC_RANGES directly.
    let ids = gc_ids_for_prop(&norm)?;
    let mut ranges = Vec::new();
    for &(lo, hi, gc) in GC_RANGES {
        if ids.contains(&gc) {
            push_gc_range(lo, hi, &mut ranges);
        }
    }
    Some(ranges)
}

/// Push a codepoint range from GC_RANGES into `out`, skipping surrogates.
fn push_gc_range(lo: u32, hi: u32, out: &mut Vec<(char, char)>) {
    // GC_RANGES may contain the surrogate block (0xD800–0xDFFF, GC_CS=4).
    // char::from_u32 returns None for surrogates, so we skip any range that
    // overlaps the surrogate gap.
    if let (Some(lo_c), Some(hi_c)) = (char::from_u32(lo), char::from_u32(hi)) {
        out.push((lo_c, hi_c));
    }
    // If either endpoint is a surrogate we drop the whole range: surrogate
    // codepoints cannot appear in valid Rust str/char, so matching them is
    // meaningless.
}

fn match_prop(norm: &str, ch: char) -> bool {
    use GeneralCategory::*;
    let gc = get_general_category(ch);
    match norm {
        // ---- POSIX-like ----
        "alnum" => in_range_table(ALPHANUMERIC_RANGES, ch),
        "alpha" => in_range_table(ALPHABETIC_RANGES, ch),
        "blank" => ch == ' ' || ch == '\t',
        "cntrl" => (ch as u32) < 32 || ch as u32 == 127,
        "digit" => ch.is_ascii_digit(),
        "graph" => ch > ' ' && ch as u32 != 127 && !matches!(gc, Control | Surrogate),
        "lower" => in_range_table(LOWERCASE_RANGES, ch),
        "print" => (ch >= ' ' && ch as u32 != 127) || !matches!(gc, Control | Surrogate),
        "punct" => ch.is_ascii_punctuation(),
        "space" => matches!(ch, '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' '),
        "upper" => in_range_table(UPPERCASE_RANGES, ch),
        "xdigit" => ch.is_ascii_hexdigit(),
        "word" => in_range_table(ALPHANUMERIC_RANGES, ch) || ch == '_',
        "ascii" => (ch as u32) <= 127,

        // ---- Letter (L) ----
        "l" | "letter" => {
            matches!(
                gc,
                UppercaseLetter | LowercaseLetter | TitlecaseLetter | ModifierLetter | OtherLetter
            )
        }
        "lu" | "uppercaseletter" => gc == UppercaseLetter,
        "ll" | "lowercaseletter" => gc == LowercaseLetter,
        "lt" | "titlecaseletter" => gc == TitlecaseLetter,
        "lm" | "modifierletter" => gc == ModifierLetter,
        "lo" | "otherletter" => gc == OtherLetter,

        // ---- Mark (M) ----
        "m" | "mark" | "combiningmark" => {
            matches!(gc, NonspacingMark | SpacingMark | EnclosingMark)
        }
        "mn" | "nonspacingmark" => gc == NonspacingMark,
        "mc" | "spacingmark" | "spacingcombiningmark" => gc == SpacingMark,
        "me" | "enclosingmark" => gc == EnclosingMark,

        // ---- Number (N) ----
        "n" | "number" => matches!(gc, DecimalNumber | LetterNumber | OtherNumber),
        "nd" | "decimalnumber" | "decimaldigitnumber" => gc == DecimalNumber,
        "nl" | "letternumber" => gc == LetterNumber,
        "no" | "othernumber" => gc == OtherNumber,

        // ---- Punctuation (P) ----
        "p" | "punctuation" => {
            matches!(
                gc,
                ConnectorPunctuation
                    | DashPunctuation
                    | OpenPunctuation
                    | ClosePunctuation
                    | InitialPunctuation
                    | FinalPunctuation
                    | OtherPunctuation
            )
        }
        "pc" | "connectorpunctuation" => gc == ConnectorPunctuation,
        "pd" | "dashpunctuation" => gc == DashPunctuation,
        "ps" | "openpunctuation" => gc == OpenPunctuation,
        "pe" | "closepunctuation" => gc == ClosePunctuation,
        "pi" | "initialpunctuation" => gc == InitialPunctuation,
        "pf" | "finalpunctuation" => gc == FinalPunctuation,
        "po" | "otherpunctuation" => gc == OtherPunctuation,

        // ---- Symbol (S) ----
        "s" | "symbol" => matches!(
            gc,
            MathSymbol | CurrencySymbol | ModifierSymbol | OtherSymbol
        ),
        "sm" | "mathsymbol" => gc == MathSymbol,
        "sc" | "currencysymbol" => gc == CurrencySymbol,
        "sk" | "modifiersymbol" => gc == ModifierSymbol,
        "so" | "othersymbol" => gc == OtherSymbol,

        // ---- Separator (Z) ----
        "z" | "separator" => matches!(gc, SpaceSeparator | LineSeparator | ParagraphSeparator),
        "zs" | "spaceseparator" => gc == SpaceSeparator,
        "zl" | "lineseparator" => gc == LineSeparator,
        "zp" | "paragraphseparator" => gc == ParagraphSeparator,

        // ---- Other (C) ----
        "c" | "other" => matches!(gc, Control | Format | Surrogate | PrivateUse | Unassigned),
        "cc" | "control" => gc == Control,
        "cf" | "format" => gc == Format,
        "cs" | "surrogate" => gc == Surrogate,
        "co" | "privateuse" => gc == PrivateUse,
        "cn" | "unassigned" | "notassigned" => gc == Unassigned,

        // ---- Special ----
        "any" => true,
        "assigned" => gc != Unassigned,

        // ---- Binary properties ----
        "alphabetic" => in_range_table(ALPHABETIC_RANGES, ch),
        "uppercase" => in_range_table(UPPERCASE_RANGES, ch),
        "lowercase" => in_range_table(LOWERCASE_RANGES, ch),
        "whitespace" => in_range_table(WHITESPACE_RANGES, ch),
        "hexdigit" => ch.is_ascii_hexdigit(),
        "numeric" => in_range_table(NUMERIC_RANGES, ch),
        "math" => in_range_table(MATH_RANGES, ch),

        _ => false,
    }
}

fn match_prop_known(norm: &str) -> bool {
    matches!(
        norm,
        "alnum"
            | "alpha"
            | "blank"
            | "cntrl"
            | "digit"
            | "graph"
            | "lower"
            | "print"
            | "punct"
            | "space"
            | "upper"
            | "xdigit"
            | "word"
            | "ascii"
            | "l"
            | "letter"
            | "lu"
            | "uppercaseletter"
            | "ll"
            | "lowercaseletter"
            | "lt"
            | "titlecaseletter"
            | "lm"
            | "modifierletter"
            | "lo"
            | "otherletter"
            | "m"
            | "mark"
            | "combiningmark"
            | "mn"
            | "nonspacingmark"
            | "mc"
            | "spacingmark"
            | "spacingcombiningmark"
            | "me"
            | "enclosingmark"
            | "n"
            | "number"
            | "nd"
            | "decimalnumber"
            | "decimaldigitnumber"
            | "nl"
            | "letternumber"
            | "no"
            | "othernumber"
            | "p"
            | "punctuation"
            | "pc"
            | "connectorpunctuation"
            | "pd"
            | "dashpunctuation"
            | "ps"
            | "openpunctuation"
            | "pe"
            | "closepunctuation"
            | "pi"
            | "initialpunctuation"
            | "pf"
            | "finalpunctuation"
            | "po"
            | "otherpunctuation"
            | "s"
            | "symbol"
            | "sm"
            | "mathsymbol"
            | "sc"
            | "currencysymbol"
            | "sk"
            | "modifiersymbol"
            | "so"
            | "othersymbol"
            | "z"
            | "separator"
            | "zs"
            | "spaceseparator"
            | "zl"
            | "lineseparator"
            | "zp"
            | "paragraphseparator"
            | "c"
            | "other"
            | "cc"
            | "control"
            | "cf"
            | "format"
            | "cs"
            | "surrogate"
            | "co"
            | "privateuse"
            | "cn"
            | "unassigned"
            | "notassigned"
            | "any"
            | "assigned"
            | "alphabetic"
            | "uppercase"
            | "lowercase"
            | "whitespace"
            | "hexdigit"
            | "numeric"
            | "math"
    )
}
