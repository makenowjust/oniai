//! Generates Unicode data tables committed to src/ in the oniai crate.
//!
//! Run from the repository root:
//!   cargo run --manifest-path scripts/gen_unicode_tables/Cargo.toml
//!
//! This reads:
//!   data/CaseFolding.txt
//!   data/extracted/DerivedGeneralCategory.txt
//!
//! And writes:
//!   src/data/casefold_data.rs
//!   src/data/general_category_data.rs
//!
//! Fetch fresh Unicode data first if needed:
//!   sh scripts/fetch_unicode_data.sh [VERSION]

use std::fs;

fn main() {
    gen_casefold();
    gen_general_category();
    eprintln!("Done. Files written to src/.");
}

// ─── CaseFolding.txt ────────────────────────────────────────────────────────

fn gen_casefold() {
    let src = fs::read_to_string("data/CaseFolding.txt")
        .expect("data/CaseFolding.txt not found; run: sh scripts/fetch_unicode_data.sh");

    let mut simple: Vec<(u32, u32)> = Vec::new();
    let mut multi: Vec<(u32, Vec<u32>)> = Vec::new();

    for line in src.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(';');
        let src_hex = fields.next().unwrap_or("").trim();
        let status = fields.next().unwrap_or("").trim();
        let mapping = fields.next().unwrap_or("").trim();

        // Only process C (common single-char) and F (full multi-char) entries.
        if status != "C" && status != "F" {
            continue;
        }

        let src_cp = parse_hex(src_hex);
        let targets: Vec<u32> = mapping.split_whitespace().map(parse_hex).collect();

        match targets.len() {
            0 => {}
            1 => simple.push((src_cp, targets[0])),
            _ => multi.push((src_cp, targets)),
        }
    }

    simple.sort_unstable_by_key(|&(s, _)| s);
    multi.sort_unstable_by_key(|&(s, _)| s);

    let mut out = String::with_capacity(128 * 1024);
    out.push_str(DO_NOT_EDIT);
    out.push_str(
        "// Generated from data/CaseFolding.txt by scripts/gen_unicode_tables.\n\
         // Re-run the generator after updating the data file.\n\n",
    );

    out.push_str(
        "/// All non-trivial single-codepoint Unicode case folds: `(src, folded)`,\n\
         /// sorted by source codepoint.  Derived from data/CaseFolding.txt (status C).\n\
         pub const SIMPLE_CASE_FOLDS: &[(char, char)] = &[\n",
    );
    for &(src, dst) in &simple {
        let sc = char::from_u32(src).unwrap();
        let dc = char::from_u32(dst).unwrap();
        out.push_str(&format!(
            "    ('\\u{{{src:04X}}}', '\\u{{{dst:04X}}}'),  // {sc} -> {dc}\n"
        ));
    }
    out.push_str("];\n\n");

    out.push_str(
        "/// All Unicode multi-codepoint case folds: `(src, &[folded_chars])`,\n\
         /// sorted by source codepoint.  Derived from data/CaseFolding.txt (status F).\n\
         pub const MULTI_CASE_FOLDS: &[(char, &[char])] = &[\n",
    );
    for (src, fold) in &multi {
        let sc = char::from_u32(*src).unwrap();
        let fold_str: String = fold
            .iter()
            .map(|&c| format!("'\\u{{{c:04X}}}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let fold_display: String = fold.iter().map(|&c| char::from_u32(c).unwrap()).collect();
        out.push_str(&format!(
            "    ('\\u{{{src:04X}}}', &[{fold_str}]),  // {sc} -> {fold_display}\n"
        ));
    }
    out.push_str("];\n");

    fs::write("src/data/casefold_data.rs", &out).expect("failed to write src/data/casefold_data.rs");
    eprintln!("Wrote src/data/casefold_data.rs ({} simple, {} multi folds)", simple.len(), multi.len());
}

// ─── DerivedGeneralCategory.txt ─────────────────────────────────────────────

/// Canonical two-letter category codes in alphabetical order.
/// The index in this array is the u8 ID stored in GC_RANGES.
const GC_NAMES: &[&str] = &[
    "Cc", "Cf", "Cn", "Co", "Cs", // C*
    "Ll", "Lm", "Lo", "Lt", "Lu", // L*
    "Mc", "Me", "Mn",             // M*
    "Nd", "Nl", "No",             // N*
    "Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps", // P*
    "Sc", "Sk", "Sm", "So",       // S*
    "Zl", "Zp", "Zs",             // Z*
];

fn gc_id(code: &str) -> u8 {
    GC_NAMES
        .iter()
        .position(|&n| n == code)
        .unwrap_or_else(|| panic!("unknown GC code: {code}")) as u8
}

fn gen_general_category() {
    let src = fs::read_to_string("data/extracted/DerivedGeneralCategory.txt")
        .expect("data/extracted/DerivedGeneralCategory.txt not found; run: sh scripts/fetch_unicode_data.sh");

    let mut ranges: Vec<(u32, u32, u8)> = Vec::new();

    for line in src.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(';');
        let range_part = fields.next().unwrap_or("").trim();
        let cat_part = fields.next().unwrap_or("").trim();
        if cat_part.is_empty() {
            continue;
        }

        let (lo, hi) = if let Some((a, b)) = range_part.split_once("..") {
            (parse_hex(a.trim()), parse_hex(b.trim()))
        } else {
            let cp = parse_hex(range_part);
            (cp, cp)
        };

        ranges.push((lo, hi, gc_id(cat_part)));
    }

    ranges.sort_unstable_by_key(|&(lo, _, _)| lo);

    // Merge adjacent same-category ranges.
    let mut merged: Vec<(u32, u32, u8)> = Vec::with_capacity(ranges.len());
    for (lo, hi, cat) in ranges {
        if let Some(last) = merged.last_mut()
            && last.2 == cat
            && last.1 + 1 == lo
        {
            last.1 = hi;
            continue;
        }
        merged.push((lo, hi, cat));
    }

    let mut out = String::with_capacity(512 * 1024);
    out.push_str(DO_NOT_EDIT);
    out.push_str(
        "// Generated from data/extracted/DerivedGeneralCategory.txt by scripts/gen_unicode_tables.\n\
         // Re-run the generator after updating the data file.\n\n",
    );

    out.push_str(
        "/// Unicode General Category ID constants (index into GC_RANGES category field).\n\
         #[allow(dead_code)]\n",
    );
    for (i, &name) in GC_NAMES.iter().enumerate() {
        out.push_str(&format!("pub const GC_{}: u8 = {};\n", name.to_uppercase(), i));
    }
    out.push('\n');

    out.push_str(
        "/// Sorted Unicode General Category range table: `(lo, hi, category_id)`.\n\
         pub const GC_RANGES: &[(u32, u32, u8)] = &[\n",
    );
    for (lo, hi, cat) in &merged {
        out.push_str(&format!("    ({lo:#07X}, {hi:#07X}, {cat}),\n"));
    }
    out.push_str("];\n");

    fs::write("src/data/general_category_data.rs", &out)
        .expect("failed to write src/data/general_category_data.rs");
    eprintln!("Wrote src/data/general_category_data.rs ({} ranges)", merged.len());
}

// ─── helpers ────────────────────────────────────────────────────────────────

const DO_NOT_EDIT: &str = "\
// DO NOT EDIT — this file is generated by scripts/gen_unicode_tables.\n\
// To regenerate:\n\
//   sh scripts/fetch_unicode_data.sh   # update data/ if needed\n\
//   cargo run --manifest-path scripts/gen_unicode_tables/Cargo.toml\n\n";
fn strip_comment(line: &str) -> &str {
    if let Some(pos) = line.find('#') {
        &line[..pos]
    } else {
        line
    }
}

fn parse_hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 16).unwrap_or_else(|_| panic!("invalid hex: {s:?}"))
}
