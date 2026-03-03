/// Character set matching utilities.
use crate::ast::{PosixClass, Shorthand};
use crate::data::general_category_data::{
    GC_CC, GC_CF, GC_CN, GC_CO, GC_CS, GC_LL, GC_LM, GC_LO, GC_LT, GC_LU, GC_MC, GC_ME,
    GC_MN, GC_ND, GC_NL, GC_NO, GC_PC, GC_PD, GC_PE, GC_PF, GC_PI, GC_PO, GC_PS, GC_RANGES,
    GC_SC, GC_SK, GC_SM, GC_SO, GC_ZL, GC_ZP, GC_ZS,
};
use crate::data::script_data::{SCRIPT_BY_NAME, SCRIPT_EXT_BY_NAME};
use crate::data::unicode_prop_ranges_data::{
    ALPHABETIC_RANGES, ALPHANUMERIC_RANGES, LOWERCASE_RANGES, MATH_RANGES, NUMERIC_RANGES,
    UPPERCASE_RANGES, WHITESPACE_RANGES,
};

/// Normalize a Unicode property name: lowercase, strip `_`/`-`/` `.
fn normalize_prop_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect()
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

    // Script / Script_Extensions properties.
    // Accepted forms (all case-insensitive, ignoring `_`/`-`/` `):
    //   \p{Script=Latin}          \p{sc=Latin}
    //   \p{Script_Extensions=Latin}  \p{scx=Latin}
    //   \p{Latin}  (bare script name, treated as Script=Latin)
    if let Some(ranges) = script_prop_ranges(&norm) {
        return Some(ranges);
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

/// Look up Script or Script_Extensions ranges for a normalized property name.
///
/// Handles `script=<value>`, `sc=<value>`, `scriptextensions=<value>`,
/// `scx=<value>`, and bare script names (e.g. `latin`).
/// Returns `None` if the name is not recognized as a Script property.
fn script_prop_ranges(norm: &str) -> Option<Vec<(char, char)>> {
    // Property=value form: split on '='.
    if let Some(eq) = norm.find('=') {
        let prop = &norm[..eq];
        let val = &norm[eq + 1..];
        let table = match prop {
            "script" | "sc" => SCRIPT_BY_NAME,
            "scriptextensions" | "scx" => SCRIPT_EXT_BY_NAME,
            _ => return None,
        };
        return lookup_script_ranges(table, val);
    }

    // Bare script name: try Script first (most common intent).
    lookup_script_ranges(SCRIPT_BY_NAME, norm)
}

/// Binary-search `table` (sorted by normalized name) for `norm_val` and return
/// a copy of the matched ranges, or `None` if not found.
fn lookup_script_ranges(
    table: &[(&str, &[(char, char)])],
    norm_val: &str,
) -> Option<Vec<(char, char)>> {
    table
        .binary_search_by_key(&norm_val, |&(n, _)| n)
        .ok()
        .map(|i| table[i].1.to_vec())
}

// ---------------------------------------------------------------------------
// Range utility helpers (used by shorthand_direct_ranges / posix_direct_ranges)
// ---------------------------------------------------------------------------

/// Sort and merge an unsorted list of (possibly overlapping) char ranges.
fn sort_merge(mut v: Vec<(char, char)>) -> Vec<(char, char)> {
    if v.is_empty() {
        return v;
    }
    v.sort_unstable_by_key(|&(lo, _)| lo as u32);
    let mut merged: Vec<(char, char)> = Vec::with_capacity(v.len());
    for (lo, hi) in v {
        if let Some(last) = merged.last_mut() {
            let next = char::from_u32(last.1 as u32 + 1);
            if lo <= last.1 || next == Some(lo) {
                if hi > last.1 {
                    last.1 = hi;
                }
                continue;
            }
        }
        merged.push((lo, hi));
    }
    merged
}

/// Complement of sorted merged `ranges` within `['\0', '\u{10FFFF}']`,
/// skipping the surrogate block (U+D800–U+DFFF).
fn complement_full(ranges: &[(char, char)]) -> Vec<(char, char)> {
    const SUR_LO: u32 = 0xD800;
    const SUR_HI: u32 = 0xDFFF;

    fn push_valid(out: &mut Vec<(char, char)>, lo_u: u32, hi_u: u32) {
        if hi_u < lo_u {
            return;
        }
        if hi_u < SUR_LO || lo_u > SUR_HI {
            if let (Some(lo), Some(hi)) = (char::from_u32(lo_u), char::from_u32(hi_u)) {
                out.push((lo, hi));
            }
        } else {
            if lo_u < SUR_LO
                && let (Some(lo), Some(hi)) =
                    (char::from_u32(lo_u), char::from_u32(SUR_LO - 1))
                {
                    out.push((lo, hi));
                }
            if hi_u > SUR_HI
                && let (Some(lo), Some(hi)) =
                    (char::from_u32(SUR_HI + 1), char::from_u32(hi_u))
                {
                    out.push((lo, hi));
                }
        }
    }

    let mut result = Vec::new();
    let mut pos: u32 = 0;
    for &(lo, hi) in ranges {
        let lo_u = lo as u32;
        if pos < lo_u {
            push_valid(&mut result, pos, lo_u - 1);
        }
        pos = (hi as u32).saturating_add(1);
        if pos > 0x10FFFF {
            return result;
        }
    }
    push_valid(&mut result, pos, 0x10FFFF);
    result
}

// ---------------------------------------------------------------------------
// Direct range builders for shorthand and POSIX classes
// ---------------------------------------------------------------------------

/// Build codepoint ranges for a shorthand class (`\w`, `\d`, etc.) directly
/// from pre-computed static tables — no codepoint iteration.
pub fn shorthand_direct_ranges(sh: Shorthand, ascii_range: bool) -> Vec<(char, char)> {
    match sh {
        Shorthand::Word => {
            if ascii_range {
                vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')]
            } else {
                let mut v = ALPHANUMERIC_RANGES.to_vec();
                v.push(('_', '_'));
                sort_merge(v)
            }
        }
        Shorthand::NonWord => complement_full(&shorthand_direct_ranges(Shorthand::Word, ascii_range)),
        Shorthand::Digit => {
            if ascii_range {
                vec![('0', '9')]
            } else {
                NUMERIC_RANGES.to_vec()
            }
        }
        Shorthand::NonDigit => complement_full(&shorthand_direct_ranges(Shorthand::Digit, ascii_range)),
        Shorthand::Space => vec![
            ('\t', '\t'),
            ('\n', '\n'),
            ('\x0B', '\x0B'),
            ('\x0C', '\x0C'),
            ('\r', '\r'),
            (' ', ' '),
        ],
        Shorthand::NonSpace => complement_full(&shorthand_direct_ranges(Shorthand::Space, ascii_range)),
        Shorthand::HexDigit => vec![('0', '9'), ('A', 'F'), ('a', 'f')],
        Shorthand::NonHexDigit => complement_full(&shorthand_direct_ranges(Shorthand::HexDigit, ascii_range)),
    }
}

/// Build codepoint ranges for a POSIX character class directly from
/// pre-computed static tables — no codepoint iteration.
pub fn posix_direct_ranges(cls: PosixClass, ascii_range: bool) -> Vec<(char, char)> {
    match cls {
        PosixClass::Alnum => {
            if ascii_range {
                vec![('0', '9'), ('A', 'Z'), ('a', 'z')]
            } else {
                ALPHANUMERIC_RANGES.to_vec()
            }
        }
        PosixClass::Alpha => {
            if ascii_range {
                vec![('A', 'Z'), ('a', 'z')]
            } else {
                ALPHABETIC_RANGES.to_vec()
            }
        }
        PosixClass::Ascii => vec![('\0', '\x7F')],
        PosixClass::Blank => vec![('\t', '\t'), (' ', ' ')],
        PosixClass::Cntrl => vec![('\0', '\x1F'), ('\x7F', '\x7F')],
        PosixClass::Digit => vec![('0', '9')],
        PosixClass::Graph => {
            if ascii_range {
                // 0x21–0x7E: printable non-space ASCII
                vec![('!', '~')]
            } else {
                // All chars > ' ' (0x20) that are not GC_CC (Unicode control).
                // Build the exclusion set: GC_CC ranges ∪ {'\0'..=' '}.
                let mut excl: Vec<(char, char)> = vec![('\0', ' ')];
                for &(lo, hi, gc) in GC_RANGES {
                    if gc == GC_CC {
                        push_gc_range(lo, hi, &mut excl);
                    }
                }
                complement_full(&sort_merge(excl))
            }
        }
        PosixClass::Lower => {
            if ascii_range {
                vec![('a', 'z')]
            } else {
                LOWERCASE_RANGES.to_vec()
            }
        }
        PosixClass::Print => {
            if ascii_range {
                // 0x20–0x7E
                vec![(' ', '~')]
            } else {
                // All chars >= ' ' (0x20) except DEL (0x7F), minus surrogates.
                // The condition (ch >= ' ' && ch != 0x7F) || !ch.is_control()
                // simplifies to: ch >= 0x20 AND ch != 0x7F (minus surrogates),
                // because 0x80-0x9F satisfy ch >= 0x20 and are included.
                vec![
                    (' ', '\u{7E}'),
                    ('\u{80}', '\u{D7FF}'),
                    ('\u{E000}', '\u{10FFFF}'),
                ]
            }
        }
        PosixClass::Punct => vec![('!', '/'), (':', '@'), ('[', '`'), ('{', '~')],
        PosixClass::Space => shorthand_direct_ranges(Shorthand::Space, ascii_range),
        PosixClass::Upper => {
            if ascii_range {
                vec![('A', 'Z')]
            } else {
                UPPERCASE_RANGES.to_vec()
            }
        }
        PosixClass::XDigit => vec![('0', '9'), ('A', 'F'), ('a', 'f')],
        PosixClass::Word => shorthand_direct_ranges(Shorthand::Word, ascii_range),
    }
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
