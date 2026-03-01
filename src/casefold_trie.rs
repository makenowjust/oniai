/// Compile-time case-fold expansion utilities.
///
/// `fold_seq_to_trie` builds a [`ByteTrie`] containing every UTF-8 byte
/// sequence that Unicode-case-folds **to** the given target sequence.
/// At match time the engine just walks the trie over raw bytes — no
/// `case_fold()` calls, no UTF-8 decoding.
///
/// `charset_to_bytetrie` builds a [`ByteTrie`] from a `CharSet` by
/// enumerating all matching Unicode codepoints and inserting their UTF-8
/// encodings.
///
/// # Algorithm
///
/// For a target sequence `T = [c0, c1, …, cn]` we recursively find all
/// Unicode codepoints `ch` such that `ch.case_fold()` produces some
/// non-empty prefix `T[0..k]`, then recurse on `T[k..]`.  The base case
/// (empty target) inserts the accumulated byte sequence into the output trie.
///
/// The inner loop scans U+0000..=U+10FFFF once per unique target prefix
/// length — expensive but paid once at `Regex::new()` time.
use crate::bytetrie::ByteTrie;
use crate::vm::CharSet;
use unicode_casefold::UnicodeCaseFold;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Build a `ByteTrie` containing all UTF-8 byte sequences whose Unicode case
/// fold equals `target`.
///
/// For example, `fold_seq_to_trie(&['s'])` produces a trie accepting
/// `"s"`, `"S"`, `"ſ"` (U+017F), `"ẛ"` (U+1E9B) etc.
///
/// `fold_seq_to_trie(&['s', 's'])` produces a trie accepting `"ss"`, `"sS"`,
/// `"Ss"`, `"SS"`, `"ß"` (U+00DF), combinations with ſ, etc.
pub fn fold_seq_to_trie(target: &[char]) -> ByteTrie {
    let mut trie = ByteTrie::new();
    let mut buf = Vec::new();
    enumerate_inputs(target, &mut buf, &mut trie);
    trie
}

/// Build a *reversed* `ByteTrie` for backward matching (used by
/// `Inst::FoldSeqBack`).
pub fn fold_seq_to_trie_back(target: &[char]) -> ByteTrie {
    fold_seq_to_trie(target).reversed()
}

// ---------------------------------------------------------------------------
// Internal enumeration
// ---------------------------------------------------------------------------

/// Recursively enumerate all input byte sequences that fold to `remaining`,
/// appending discovered bytes to `buf` and inserting complete sequences into
/// `out`.
fn enumerate_inputs(remaining: &[char], buf: &mut Vec<u8>, out: &mut ByteTrie) {
    if remaining.is_empty() {
        out.insert(buf);
        return;
    }

    // Try every split length k (1..=remaining.len()):
    // find all codepoints whose case_fold == remaining[..k], then recurse.
    for k in 1..=remaining.len() {
        let prefix = &remaining[..k];
        let suffix = &remaining[k..];

        for ch in codepoints_with_fold(prefix) {
            let start = buf.len();
            let mut tmp = [0u8; 4];
            let encoded = ch.encode_utf8(&mut tmp);
            buf.extend_from_slice(encoded.as_bytes());
            enumerate_inputs(suffix, buf, out);
            buf.truncate(start);
        }
    }
}

/// Yield all Unicode codepoints whose full case fold equals `target`.
fn codepoints_with_fold(target: &[char]) -> impl Iterator<Item = char> {
    // We keep a small stack-friendly copy because we scan all codepoints and
    // need to compare against the target inside the iterator.
    let target: Vec<char> = target.to_vec();

    // Scan U+0000..=U+10FFFF
    (0u32..=0x10_FFFF).filter_map(move |cp| {
        let ch = char::from_u32(cp)?;
        let fold: Vec<char> = ch.case_fold().collect();
        if fold == target { Some(ch) } else { None }
    })
}

// ---------------------------------------------------------------------------
// CharSet → ByteTrie
// ---------------------------------------------------------------------------

/// Build a `ByteTrie` from a `CharSet` by scanning all Unicode codepoints and
/// inserting the UTF-8 encoding of every codepoint that the charset matches.
///
/// This is the compile-time cost paid at `Regex::new()`.  The resulting trie
/// can be used in place of the `CharSet` in the VM hot path with no calls to
/// `case_fold()` or Unicode property functions at match time.
///
/// For very large charsets (e.g. `\w` Unicode) the trie may be large; callers
/// should fall back to the existing `CharSet::matches` path if the trie exceeds
/// a practical size threshold.
pub fn charset_to_bytetrie(cs: &CharSet, ignore_case: bool, ascii_range: bool) -> ByteTrie {
    let mut trie = ByteTrie::new();
    let mut buf = [0u8; 4];
    for cp in 0u32..=0x10_FFFF {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        if cs.matches(ch, ascii_range, ignore_case) {
            trie.insert(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    trie
}

/// Build a reversed `ByteTrie` from a `CharSet` (for backward `ClassBack`).
pub fn charset_to_bytetrie_back(cs: &CharSet, ignore_case: bool, ascii_range: bool) -> ByteTrie {
    charset_to_bytetrie(cs, ignore_case, ascii_range).reversed()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(trie: &ByteTrie, s: &str) -> bool {
        trie.advance(s.as_bytes(), 0) == Some(s.len())
    }

    // --- Single-codepoint folds ---

    #[test]
    fn fold_trie_ascii_s() {
        let t = fold_seq_to_trie(&['s']);
        assert!(accepts(&t, "s"), "s");
        assert!(accepts(&t, "S"), "S");
        assert!(accepts(&t, "ſ"), "ſ U+017F"); // Latin small letter long s (folds to 's')
        assert!(!accepts(&t, "ẛ"), "ẛ U+1E9B folds to ṡ, not s");
        assert!(!accepts(&t, "x"), "x should not match");
        assert!(!accepts(&t, "ß"), "ß has 2-char fold ss, not s");
    }

    #[test]
    fn fold_trie_kelvin() {
        let t = fold_seq_to_trie(&['k']);
        assert!(accepts(&t, "k"), "k");
        assert!(accepts(&t, "K"), "K");
        assert!(accepts(&t, "\u{212A}"), "Kelvin sign U+212A");
        assert!(!accepts(&t, "x"), "x");
    }

    #[test]
    fn fold_trie_ascii_a() {
        let t = fold_seq_to_trie(&['a']);
        assert!(accepts(&t, "a"), "a");
        assert!(accepts(&t, "A"), "A");
        assert!(!accepts(&t, "b"), "b");
    }

    // --- Multi-codepoint folds ---

    #[test]
    fn fold_trie_sharp_s() {
        // target = ['s','s']
        let t = fold_seq_to_trie(&['s', 's']);
        assert!(accepts(&t, "ss"), "ss");
        assert!(accepts(&t, "sS"), "sS");
        assert!(accepts(&t, "Ss"), "Ss");
        assert!(accepts(&t, "SS"), "SS");
        assert!(accepts(&t, "ß"), "ß U+00DF");
        // ſ folds to ['s'], so ſs should match too
        assert!(accepts(&t, "ſs"), "ſs");
        assert!(accepts(&t, "ſS"), "ſS");
        // single s/S: fold is just ['s'], not ['s','s']
        assert!(!accepts(&t, "s"), "single s");
        assert!(!accepts(&t, "S"), "single S");
    }

    #[test]
    fn fold_trie_fi_ligature() {
        // target = ['f', 'i']  (fold of ﬁ U+FB01)
        let t = fold_seq_to_trie(&['f', 'i']);
        assert!(accepts(&t, "fi"), "fi");
        assert!(accepts(&t, "fI"), "fI");
        assert!(accepts(&t, "Fi"), "Fi");
        assert!(accepts(&t, "FI"), "FI");
        assert!(accepts(&t, "ﬁ"), "ﬁ U+FB01");
        assert!(!accepts(&t, "f"), "incomplete");
        assert!(!accepts(&t, "x"), "x");
    }

    // --- Backward trie ---

    #[test]
    fn fold_trie_back_ascii() {
        let t = fold_seq_to_trie_back(&['s']);
        // advance_back on "xs" from pos 2 should give pos 1
        assert_eq!(t.advance_back(b"xs", 2), Some(1));
        assert_eq!(t.advance_back(b"xS", 2), Some(1));
        let s = "x\u{017F}".to_string();
        assert_eq!(
            t.advance_back(s.as_bytes(), s.len()),
            Some(1),
            "xſ backward"
        );
    }

    #[test]
    fn fold_trie_back_sharp_s() {
        let t = fold_seq_to_trie_back(&['s', 's']);
        // "xss" backward from 3 → 1
        assert_eq!(t.advance_back(b"xss", 3), Some(1));
        // "xß" backward (ß is 2 bytes)
        let s = "x\u{00DF}".to_string();
        assert_eq!(t.advance_back(s.as_bytes(), s.len()), Some(1));
    }

    // --- CharSet → ByteTrie ---

    fn make_range_charset(lo: char, hi: char) -> CharSet {
        use crate::vm::CharSetItem;
        let mut cs = CharSet {
            negate: false,
            items: vec![CharSetItem::Range(lo, hi)],
            intersections: vec![],
            ascii_ranges: None,
        };
        cs.compute_ascii_ranges();
        cs
    }

    #[test]
    fn charset_trie_az_case_insensitive() {
        let cs = make_range_charset('a', 'z');
        let t = charset_to_bytetrie(&cs, true, false);
        // ASCII uppercase matched
        assert!(accepts(&t, "A"), "A");
        assert!(accepts(&t, "Z"), "Z");
        assert!(accepts(&t, "a"), "a");
        assert!(accepts(&t, "z"), "z");
        // ſ (U+017F) simple-folds to 's' → should be in class
        assert!(accepts(&t, "ſ"), "ſ U+017F");
        // ß has no simple fold (multi-codepoint) → should NOT be in class
        assert!(
            !accepts(&t, "ß"),
            "ß should not match [a-z] case-insensitive"
        );
        // Kelvin sign (U+212A) folds to 'k'
        assert!(accepts(&t, "\u{212A}"), "Kelvin sign U+212A");
        // digit outside range
        assert!(!accepts(&t, "1"), "digit");
    }

    #[test]
    fn charset_trie_az_case_sensitive() {
        let cs = make_range_charset('a', 'z');
        let t = charset_to_bytetrie(&cs, false, false);
        assert!(accepts(&t, "a"), "a");
        assert!(accepts(&t, "z"), "z");
        assert!(!accepts(&t, "A"), "A case-sensitive");
        assert!(!accepts(&t, "ſ"), "ſ case-sensitive");
    }
}
