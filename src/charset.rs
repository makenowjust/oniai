/// Character set matching utilities.
use crate::ast::{PosixClass, Shorthand};
use unicode_general_category::{GeneralCategory, get_general_category};

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

fn match_prop(norm: &str, ch: char) -> bool {
    use GeneralCategory::*;
    let gc = get_general_category(ch);
    match norm {
        // ---- POSIX-like ----
        "alnum" => ch.is_alphanumeric(),
        "alpha" => ch.is_alphabetic(),
        "blank" => ch == ' ' || ch == '\t',
        "cntrl" => (ch as u32) < 32 || ch as u32 == 127,
        "digit" => ch.is_ascii_digit(),
        "graph" => ch > ' ' && ch as u32 != 127 && !ch.is_control(),
        "lower" => ch.is_lowercase(),
        "print" => (ch >= ' ' && ch as u32 != 127) || !ch.is_control(),
        "punct" => ch.is_ascii_punctuation(),
        "space" => matches!(ch, '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' '),
        "upper" => ch.is_uppercase(),
        "xdigit" => ch.is_ascii_hexdigit(),
        "word" => ch.is_alphanumeric() || ch == '_',
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
        "alphabetic" => ch.is_alphabetic(),
        "uppercase" => ch.is_uppercase(),
        "lowercase" => ch.is_lowercase(),
        "whitespace" => ch.is_whitespace(),
        "hexdigit" => ch.is_ascii_hexdigit(),
        "numeric" => ch.is_numeric(),
        "math" => gc == MathSymbol,

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
