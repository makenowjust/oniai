//! Generates Unicode data tables committed to src/ in the oniai crate.
//!
//! Run from the repository root:
//!   cargo run --manifest-path scripts/gen_unicode_tables/Cargo.toml
//!
//! This reads:
//!   data/CaseFolding.txt
//!   data/extracted/DerivedGeneralCategory.txt
//!   data/DerivedCoreProperties.txt
//!   data/PropList.txt
//!   data/Scripts.txt
//!   data/ScriptExtensions.txt
//!   data/PropertyValueAliases.txt
//!
//! And writes:
//!   src/data/casefold_data.rs
//!   src/data/general_category_data.rs
//!   src/data/unicode_prop_ranges_data.rs
//!   src/data/script_data.rs
//!
//! Fetch fresh Unicode data first if needed:
//!   sh scripts/fetch_unicode_data.sh [VERSION]

use std::collections::HashMap;
use std::fs;

fn main() {
    gen_casefold();
    gen_general_category();
    gen_unicode_prop_ranges();
    gen_script_data();
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

// ─── unicode_prop_ranges_data.rs ────────────────────────────────────────────

/// Parse a Unicode data file (DerivedCoreProperties.txt or PropList.txt)
/// and return codepoint ranges for the given property name.
fn parse_ucd_property(src: &str, property: &str) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for line in src.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split(';');
        let range_part = fields.next().unwrap_or("").trim();
        let prop_part = fields.next().unwrap_or("").trim();
        if prop_part != property {
            continue;
        }
        let (lo, hi) = if let Some((a, b)) = range_part.split_once("..") {
            (parse_hex(a.trim()), parse_hex(b.trim()))
        } else {
            let cp = parse_hex(range_part);
            (cp, cp)
        };
        ranges.push((lo, hi));
    }
    ranges.sort_unstable_by_key(|&(lo, _)| lo);
    ranges
}

/// Merge a list of (u32,u32) codepoint ranges, skipping surrogates.
fn merge_u32_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(char, char)> {
    ranges.sort_unstable_by_key(|&(lo, _)| lo);
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (lo, hi) in ranges {
        if let Some(last) = merged.last_mut() {
            if lo <= last.1 + 1 {
                last.1 = last.1.max(hi);
                continue;
            }
        }
        merged.push((lo, hi));
    }
    // Convert to char ranges, splitting around the surrogate block.
    let surrogate_lo = 0xD800u32;
    let surrogate_hi = 0xDFFFu32;
    let mut result = Vec::new();
    for (lo, hi) in merged {
        // Clamp/skip surrogate halves.
        if hi < surrogate_lo || lo > surrogate_hi {
            if let (Some(lo_c), Some(hi_c)) = (char::from_u32(lo), char::from_u32(hi)) {
                result.push((lo_c, hi_c));
            }
        } else {
            // Split: [lo, D7FF] and [E000, hi]
            if lo < surrogate_lo {
                if let (Some(lo_c), Some(hi_c)) =
                    (char::from_u32(lo), char::from_u32(surrogate_lo - 1))
                {
                    result.push((lo_c, hi_c));
                }
            }
            if hi > surrogate_hi {
                if let (Some(lo_c), Some(hi_c)) =
                    (char::from_u32(surrogate_hi + 1), char::from_u32(hi))
                {
                    result.push((lo_c, hi_c));
                }
            }
        }
    }
    result
}

fn fmt_ranges(ranges: &[(char, char)]) -> String {
    ranges
        .iter()
        .map(|(lo, hi)| format!("    ('\\u{{{:04X}}}', '\\u{{{:04X}}}'),\n", *lo as u32, *hi as u32))
        .collect()
}

fn gen_unicode_prop_ranges() {
    let derived_core = fs::read_to_string("data/DerivedCoreProperties.txt")
        .expect("data/DerivedCoreProperties.txt not found; run: sh scripts/fetch_unicode_data.sh");
    let prop_list = fs::read_to_string("data/PropList.txt")
        .expect("data/PropList.txt not found; run: sh scripts/fetch_unicode_data.sh");
    let gc_data = fs::read_to_string("data/extracted/DerivedGeneralCategory.txt")
        .expect("data/extracted/DerivedGeneralCategory.txt not found");

    // Parse properties directly from Unicode data files.
    let alphabetic = merge_u32_ranges(parse_ucd_property(&derived_core, "Alphabetic"));
    let uppercase   = merge_u32_ranges(parse_ucd_property(&derived_core, "Uppercase"));
    let lowercase   = merge_u32_ranges(parse_ucd_property(&derived_core, "Lowercase"));
    let math        = merge_u32_ranges(parse_ucd_property(&derived_core, "Math"));
    let white_space = merge_u32_ranges(parse_ucd_property(&prop_list, "White_Space"));

    // Numeric = Nd + Nl + No (all N* GC categories) from DerivedGeneralCategory.
    let numeric_raw: Vec<(u32, u32)> = ["Nd", "Nl", "No"]
        .iter()
        .flat_map(|cat| parse_ucd_property(&gc_data, cat))
        .collect();
    let numeric = merge_u32_ranges(numeric_raw);

    // Alphanumeric = Alphabetic ∪ Numeric.
    let alnum_raw: Vec<(u32, u32)> = alphabetic
        .iter()
        .map(|&(lo, hi)| (lo as u32, hi as u32))
        .chain(numeric.iter().map(|&(lo, hi)| (lo as u32, hi as u32)))
        .collect();
    let alphanumeric = merge_u32_ranges(alnum_raw);

    let mut out = String::with_capacity(512 * 1024);
    out.push_str(DO_NOT_EDIT);
    out.push_str(
        "// Generated from DerivedCoreProperties.txt, PropList.txt, and\n\
         // DerivedGeneralCategory.txt by scripts/gen_unicode_tables.\n\
         // Pre-computed codepoint range tables for Unicode binary / POSIX-like\n\
         // properties that supplement GC_RANGES.\n\n",
    );

    let tables: &[(&str, &str, &[(char, char)])] = &[
        ("ALPHABETIC", "Unicode Alphabetic property (DerivedCoreProperties.txt)", &alphabetic),
        ("UPPERCASE",  "Unicode Uppercase property (DerivedCoreProperties.txt)",  &uppercase),
        ("LOWERCASE",  "Unicode Lowercase property (DerivedCoreProperties.txt)",  &lowercase),
        ("MATH",       "Unicode Math property (DerivedCoreProperties.txt)",       &math),
        ("WHITESPACE", "Unicode White_Space property (PropList.txt)",             &white_space),
        ("NUMERIC",    "Unicode Numeric (Nd+Nl+No) from DerivedGeneralCategory",  &numeric),
        ("ALPHANUMERIC","Unicode Alphabetic ∪ Numeric",                           &alphanumeric),
    ];

    for &(name, doc, ranges) in tables {
        out.push_str(&format!(
            "/// Sorted `(lo, hi)` char ranges for {} ({} ranges).\n\
             pub const {}_RANGES: &[(char, char)] = &[\n",
            doc, ranges.len(), name,
        ));
        out.push_str(&fmt_ranges(ranges));
        out.push_str("];\n\n");
        eprintln!("  {}_RANGES: {} ranges", name, ranges.len());
    }

    fs::write("src/data/unicode_prop_ranges_data.rs", &out)
        .expect("failed to write src/data/unicode_prop_ranges_data.rs");
    eprintln!("Wrote src/data/unicode_prop_ranges_data.rs");
}

// ─── Scripts.txt / ScriptExtensions.txt ─────────────────────────────────────

/// Normalize a property value name for use as a match key:
/// lowercase, strip `_`, `-`, and ` `.
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Parse `PropertyValueAliases.txt` and return a map from abbreviated Script
/// code (e.g. `"Latn"`) to the canonical long name (e.g. `"Latin"`).
fn parse_script_aliases(src: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in src.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(4, ';');
        let prop = fields.next().unwrap_or("").trim();
        if prop != "sc" {
            continue;
        }
        let abbrev = fields.next().unwrap_or("").trim().to_string();
        let long = fields.next().unwrap_or("").trim().to_string();
        if !abbrev.is_empty() && !long.is_empty() {
            map.insert(abbrev, long);
        }
    }
    map
}

/// Parse `Scripts.txt` and return a map from full script name (e.g. `"Latin"`)
/// to a sorted list of `(lo, hi)` codepoint ranges.
fn parse_scripts(src: &str) -> HashMap<String, Vec<(u32, u32)>> {
    let mut map: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    for line in src.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(2, ';');
        let range_part = fields.next().unwrap_or("").trim();
        let rest = fields.next().unwrap_or("").trim();
        // Script name is the first token in `rest` (before any whitespace).
        let script_name = rest.split_whitespace().next().unwrap_or("").to_string();
        if script_name.is_empty() {
            continue;
        }
        let (lo, hi) = if let Some((a, b)) = range_part.split_once("..") {
            (parse_hex(a.trim()), parse_hex(b.trim()))
        } else {
            let cp = parse_hex(range_part);
            (cp, cp)
        };
        map.entry(script_name).or_default().push((lo, hi));
    }
    map
}

/// Parse `ScriptExtensions.txt` and return:
/// - A map from full script name to the codepoint ranges that explicitly list
///   that script as an extension (from the file, using `aliases` to resolve
///   abbreviations to full names).
/// - A sorted, merged list of **all** codepoints mentioned anywhere in the
///   file (i.e. every codepoint whose Script_Extensions entry overrides its
///   Script value).
///
/// `aliases` maps abbreviation → full name (from PropertyValueAliases.txt).
fn parse_script_extensions(
    src: &str,
    aliases: &HashMap<String, String>,
) -> (HashMap<String, Vec<(u32, u32)>>, Vec<(u32, u32)>) {
    let mut map: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut all_mentioned: Vec<(u32, u32)> = Vec::new();
    for line in src.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(2, ';');
        let range_part = fields.next().unwrap_or("").trim();
        let abbrevs_part = fields.next().unwrap_or("").trim();
        let (lo, hi) = if let Some((a, b)) = range_part.split_once("..") {
            (parse_hex(a.trim()), parse_hex(b.trim()))
        } else {
            let cp = parse_hex(range_part);
            (cp, cp)
        };
        all_mentioned.push((lo, hi));
        for abbrev in abbrevs_part.split_whitespace() {
            if let Some(full) = aliases.get(abbrev) {
                map.entry(full.clone()).or_default().push((lo, hi));
            }
        }
    }
    (map, merge_u32_only(all_mentioned))
}

/// Merge a sorted-or-unsorted list of `(lo, hi)` u32 ranges into a canonical
/// sorted, non-overlapping, non-adjacent form.  Returns u32 pairs (no char
/// conversion; used for intermediate range arithmetic).
fn merge_u32_only(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable_by_key(|&(lo, _)| lo);
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (lo, hi) in ranges {
        if let Some(last) = merged.last_mut() {
            if lo <= last.1.saturating_add(1) {
                last.1 = last.1.max(hi);
                continue;
            }
        }
        merged.push((lo, hi));
    }
    merged
}

/// Compute `a \ b` (set difference) for sorted, merged u32 range lists.
fn subtract_u32_ranges(a: &[(u32, u32)], b: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut result = Vec::new();
    let mut bi = 0;
    for &(mut lo, hi) in a {
        // Advance past b ranges that end strictly before lo.
        while bi < b.len() && b[bi].1 < lo {
            bi += 1;
        }
        // Subtract overlapping b ranges.
        let mut j = bi;
        while j < b.len() && b[j].0 <= hi {
            let (blo, bhi) = b[j];
            if blo > lo {
                result.push((lo, blo - 1));
            }
            lo = bhi.saturating_add(1);
            if lo > hi {
                break;
            }
            j += 1;
        }
        if lo <= hi {
            result.push((lo, hi));
        }
    }
    result
}

fn gen_script_data() {
    let aliases_src = fs::read_to_string("data/PropertyValueAliases.txt")
        .expect("data/PropertyValueAliases.txt not found; run: sh scripts/fetch_unicode_data.sh");
    let scripts_src = fs::read_to_string("data/Scripts.txt")
        .expect("data/Scripts.txt not found; run: sh scripts/fetch_unicode_data.sh");
    let scx_src = fs::read_to_string("data/ScriptExtensions.txt")
        .expect("data/ScriptExtensions.txt not found; run: sh scripts/fetch_unicode_data.sh");

    let aliases = parse_script_aliases(&aliases_src);
    let script_ranges = parse_scripts(&scripts_src);
    let (scx_explicit, scx_mentioned) = parse_script_extensions(&scx_src, &aliases);

    // Collect all script names in sorted order (by normalized name for
    // binary search in charset.rs).
    let mut script_names: Vec<String> = script_ranges.keys().cloned().collect();
    script_names.sort_unstable_by(|a, b| normalize_name(a).cmp(&normalize_name(b)));

    let mut out = String::with_capacity(4 * 1024 * 1024);
    out.push_str(DO_NOT_EDIT);
    out.push_str(
        "// Generated from data/Scripts.txt, data/ScriptExtensions.txt, and\n\
         // data/PropertyValueAliases.txt by scripts/gen_unicode_tables.\n\
         // Pre-computed codepoint range tables for the Script and Script_Extensions\n\
         // Unicode properties.\n\n",
    );

    // For each script emit two const slices:
    //   Script ranges  — direct from Scripts.txt
    //   Script_Extensions ranges — per UAX #24:
    //     scx(X) = {codepoints in ScriptExtensions.txt with X listed}
    //              ∪ {codepoints with Script=X NOT in ScriptExtensions.txt}
    // Equivalently: scx(X) = scx_explicit(X) ∪ (Script(X) \ scx_mentioned)
    // This correctly excludes Common/Inherited codepoints that were reassigned
    // to specific scripts by ScriptExtensions.txt.
    let mut script_table_entries: Vec<(String, String)> = Vec::new();
    let mut scx_table_entries: Vec<(String, String)> = Vec::new();

    for script in &script_names {
        let norm = normalize_name(script);
        let sc_const = format!(
            "SCRIPT_{}_RANGES",
            script.to_uppercase().replace(' ', "_")
        );
        let scx_const = format!(
            "SCRIPT_EXT_{}_RANGES",
            script.to_uppercase().replace(' ', "_")
        );

        // Script ranges (from Scripts.txt).
        let sc_ranges = merge_u32_ranges(script_ranges[script].clone());

        // Script_Extensions = explicitly listed + (Script \ scx_mentioned).
        let sc_not_mentioned =
            subtract_u32_ranges(&merge_u32_only(script_ranges[script].clone()), &scx_mentioned);
        let mut scx_raw = sc_not_mentioned;
        if let Some(extra) = scx_explicit.get(script) {
            scx_raw.extend_from_slice(extra);
        }
        let scx_ranges = merge_u32_ranges(scx_raw);

        // Emit Script const.
        out.push_str(&format!("const {}: &[(char, char)] = &[\n", sc_const));
        out.push_str(&fmt_ranges(&sc_ranges));
        out.push_str("];\n");

        // Emit Script_Extensions const.
        out.push_str(&format!("const {}: &[(char, char)] = &[\n", scx_const));
        out.push_str(&fmt_ranges(&scx_ranges));
        out.push_str("];\n");

        script_table_entries.push((norm.clone(), sc_const));
        scx_table_entries.push((norm, scx_const));
    }

    // Add abbreviated-name aliases to the lookup tables so that e.g.
    // \p{sc=Hira} resolves to the same ranges as \p{sc=Hiragana}.
    // (Skip aliases where normalize(abbrev) == normalize(full) — already present.)
    for (abbrev, full) in &aliases {
        let norm_abbrev = normalize_name(abbrev);
        let norm_full = normalize_name(full);
        if norm_abbrev == norm_full {
            continue; // e.g. Ahom ; Ahom — no extra entry needed
        }
        // Find the existing entry for this script's full name.
        if let Ok(pos) = script_table_entries.binary_search_by_key(&&*norm_full, |(n, _)| n.as_str()) {
            let sc_const = script_table_entries[pos].1.clone();
            let scx_const = scx_table_entries[pos].1.clone();
            script_table_entries.push((norm_abbrev.clone(), sc_const));
            scx_table_entries.push((norm_abbrev, scx_const));
        }
    }

    // Re-sort after inserting abbreviation aliases.
    script_table_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    scx_table_entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    // Emit lookup tables.
    out.push_str(
        "\n/// Lookup table: normalized script name or alias → Script property ranges.\n\
         /// Sorted by name for binary search.  \"Normalized\" = lowercase, no `_`/`-`/` `.\n\
         pub const SCRIPT_BY_NAME: &[(&str, &[(char, char)])] = &[\n",
    );
    for (norm, sc_const) in &script_table_entries {
        out.push_str(&format!("    (\"{norm}\", {sc_const}),\n"));
    }
    out.push_str("];\n\n");

    out.push_str(
        "/// Lookup table: normalized script name or alias → Script_Extensions property ranges.\n\
         /// Sorted by name for binary search.  \"Normalized\" = lowercase, no `_`/`-`/` `.\n\
         pub const SCRIPT_EXT_BY_NAME: &[(&str, &[(char, char)])] = &[\n",
    );
    for (norm, scx_const) in &scx_table_entries {
        out.push_str(&format!("    (\"{norm}\", {scx_const}),\n"));
    }
    out.push_str("];\n");

    fs::write("src/data/script_data.rs", &out)
        .expect("failed to write src/data/script_data.rs");
    eprintln!(
        "Wrote src/data/script_data.rs ({} scripts, {} table entries)",
        script_names.len(),
        script_table_entries.len(),
    );
}

