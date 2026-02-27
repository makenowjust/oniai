//! Integration tests for the Aigumo regex engine.
//! Organized by feature section matching doc/RE.

use aigumo::Regex;

// Helper macros
macro_rules! assert_match {
    ($pat:expr, $text:expr) => {{
        let re = Regex::new($pat).expect(concat!("compile: ", $pat));
        assert!(re.is_match($text), "/{}/  should match {:?}", $pat, $text);
    }};
}
macro_rules! assert_no_match {
    ($pat:expr, $text:expr) => {{
        let re = Regex::new($pat).expect(concat!("compile: ", $pat));
        assert!(
            !re.is_match($text),
            "/{}/  should NOT match {:?}",
            $pat,
            $text
        );
    }};
}
macro_rules! assert_find {
    ($pat:expr, $text:expr, $expected:expr) => {{
        let re = Regex::new($pat).expect(concat!("compile: ", $pat));
        let m = re
            .find($text)
            .expect(concat!("find failed for /", $pat, "/"));
        assert_eq!(m.as_str(), $expected, "/{}/  on {:?}", $pat, $text);
    }};
}
macro_rules! assert_capture {
    ($pat:expr, $text:expr, $idx:expr, $expected:expr) => {{
        let re = Regex::new($pat).expect(concat!("compile: ", $pat));
        let caps = re
            .captures($text)
            .expect(concat!("captures failed for /", $pat, "/"));
        assert_eq!(
            caps.get($idx).map(|m| m.as_str()),
            $expected,
            "/{}/  capture {} on {:?}",
            $pat,
            $idx,
            $text
        );
    }};
}

// ---------------------------------------------------------------------------
// §2 Characters — escape sequences
// ---------------------------------------------------------------------------
#[test]
fn esc_tab() {
    assert_match!(r"\t", "\t");
}
#[test]
fn esc_newline() {
    assert_match!(r"\n", "\n");
}
#[test]
fn esc_cr() {
    assert_match!(r"\r", "\r");
}
#[test]
fn esc_hex() {
    assert_match!(r"\x41", "A");
}
#[test]
fn esc_hex_braces() {
    assert_match!(r"\x{41}", "A");
}
#[test]
fn esc_unicode_u() {
    assert_match!(r"\u0041", "A");
}
#[test]
fn esc_control() {
    assert_match!(r"\cA", "\x01");
}
#[test]
fn esc_octal() {
    assert_match!(r"\101", "A");
}

// ---------------------------------------------------------------------------
// §3 Character types
// ---------------------------------------------------------------------------
#[test]
fn any_char_no_newline() {
    assert_match!(r".", "a");
    assert_no_match!(r".", "\n");
}
#[test]
fn any_char_multiline_flag() {
    assert_match!(r"(?m:.)", "\n");
}
#[test]
fn shorthand_word() {
    assert_match!(r"\w", "a");
    assert_match!(r"\w", "0");
    assert_match!(r"\w", "_");
    assert_no_match!(r"\w", " ");
}
#[test]
fn shorthand_nonword() {
    assert_match!(r"\W", " ");
    assert_no_match!(r"\W", "a");
}
#[test]
fn shorthand_digit() {
    assert_match!(r"\d", "3");
    assert_no_match!(r"\d", "a");
}
#[test]
fn shorthand_space() {
    assert_match!(r"\s", " ");
    assert_match!(r"\s", "\t");
    assert_no_match!(r"\s", "a");
}
#[test]
fn shorthand_hex() {
    assert_match!(r"\h", "f");
    assert_match!(r"\h", "F");
    assert_match!(r"\h", "9");
    assert_no_match!(r"\h", "g");
}
#[test]
fn unicode_prop_alpha() {
    assert_match!(r"\p{Alpha}", "a");
    assert_no_match!(r"\p{Alpha}", "1");
}

// ---------------------------------------------------------------------------
// §4 Quantifiers
// ---------------------------------------------------------------------------
#[test]
fn quant_question_greedy() {
    assert_find!(r"colou?r", "colour", "colour");
    assert_find!(r"colou?r", "color", "color");
}
#[test]
fn quant_star_greedy() {
    assert_find!(r"a*", "aaa", "aaa");
    assert_find!(r"a*", "bbb", "");
}
#[test]
fn quant_plus_greedy() {
    assert_find!(r"a+", "aaa", "aaa");
    assert_no_match!(r"^a+$", "");
}
#[test]
fn quant_counted_exact() {
    assert_match!(r"a{3}", "aaa");
    assert_no_match!(r"^a{3}$", "aa");
}
#[test]
fn quant_counted_range() {
    assert_match!(r"^a{2,4}$", "aa");
    assert_match!(r"^a{2,4}$", "aaaa");
    assert_no_match!(r"^a{2,4}$", "aaaaa");
}
#[test]
fn quant_counted_min_only() {
    assert_match!(r"^a{2,}$", "aaaaa");
    assert_no_match!(r"^a{2,}$", "a");
}
#[test]
fn quant_counted_max_only() {
    assert_match!(r"^a{,3}$", "aaa");
    assert_no_match!(r"^a{,3}$", "aaaa");
}
#[test]
fn quant_reluctant_star() {
    // reluctant: match as few as possible
    let re = Regex::new(r"a.*?b").unwrap();
    assert_eq!(re.find("aXbYb").unwrap().as_str(), "aXb");
}
#[test]
fn quant_possessive_star() {
    // possessive: a*+ then must match b — no backtrack
    assert_no_match!(r"^a*+a$", "aa");
    assert_match!(r"^a*+b$", "aaab");
}

// ---------------------------------------------------------------------------
// §5 Anchors
// ---------------------------------------------------------------------------
#[test]
fn anchor_caret_start_of_line() {
    assert_match!(r"^foo", "foo bar");
    assert_no_match!(r"^foo", "bar foo");
}
#[test]
fn anchor_dollar_end_of_line() {
    assert_match!(r"foo$", "bar foo");
    assert_no_match!(r"foo$", "foo bar");
}
#[test]
fn anchor_string_start() {
    assert_match!(r"\Afoo", "foobar");
    assert_no_match!(r"\Afoo", "\nfoo");
}
#[test]
fn anchor_string_end() {
    assert_match!(r"foo\z", "foo");
    assert_no_match!(r"foo\z", "foo\n");
}
#[test]
fn anchor_string_end_or_nl() {
    assert_match!(r"foo\Z", "foo\n");
    assert_match!(r"foo\Z", "foo");
}
#[test]
fn anchor_word_boundary() {
    assert_match!(r"\bfoo\b", "foo");
    assert_match!(r"\bfoo\b", "say foo bar");
    assert_no_match!(r"\bfoo\b", "foobar");
}
#[test]
fn anchor_non_word_boundary() {
    assert_match!(r"\Boo\B", "foobar");
    assert_no_match!(r"\Bfoo\B", "foo bar");
}

// ---------------------------------------------------------------------------
// §6 Character classes
// ---------------------------------------------------------------------------
#[test]
fn charclass_basic() {
    assert_match!(r"[abc]", "b");
    assert_no_match!(r"[abc]", "d");
}
#[test]
fn charclass_range() {
    assert_match!(r"[a-z]", "m");
    assert_no_match!(r"[a-z]", "M");
}
#[test]
fn charclass_negate() {
    assert_match!(r"[^abc]", "d");
    assert_no_match!(r"[^abc]", "a");
}
#[test]
fn charclass_shorthand_inside() {
    assert_match!(r"[\d]", "5");
    assert_no_match!(r"[\d]", "a");
}
#[test]
fn charclass_posix() {
    assert_match!(r"[[:alpha:]]", "a");
    assert_no_match!(r"[[:alpha:]]", "1");
    assert_match!(r"[[:digit:]]", "7");
}
#[test]
fn charclass_intersection() {
    // [a-w&&[^c-g]z] ==> [abh-w]
    assert_match!(r"[a-w&&[^c-g]]", "a");
    assert_match!(r"[a-w&&[^c-g]]", "h");
    assert_no_match!(r"[a-w&&[^c-g]]", "d"); // in [c-g]
    assert_no_match!(r"[a-w&&[^c-g]]", "x"); // not in [a-w]
}
#[test]
fn charclass_nested() {
    assert_match!(r"[[abc][def]]", "a");
    assert_match!(r"[[abc][def]]", "e");
    assert_no_match!(r"[[abc][def]]", "g");
}

// ---------------------------------------------------------------------------
// §7 Extended groups
// ---------------------------------------------------------------------------
#[test]
fn group_comment() {
    assert_find!(r"foo(?#this is a comment)bar", "foobar", "foobar");
}
#[test]
fn group_noncapturing() {
    let re = Regex::new(r"(?:foo)(bar)").unwrap();
    let caps = re.captures("foobar").unwrap();
    assert_eq!(caps.get(1).unwrap().as_str(), "bar");
}
#[test]
fn group_capturing() {
    assert_capture!(r"(foo)(bar)", "foobar", 1, Some("foo"));
    assert_capture!(r"(foo)(bar)", "foobar", 2, Some("bar"));
}
#[test]
fn group_named() {
    let re = Regex::new(r"(?<year>\d{4})-(?<month>\d{2})-(?<day>\d{2})").unwrap();
    let caps = re.captures("2024-01-15").unwrap();
    assert_eq!(caps.name("year").unwrap().as_str(), "2024");
    assert_eq!(caps.name("month").unwrap().as_str(), "01");
    assert_eq!(caps.name("day").unwrap().as_str(), "15");
}
#[test]
fn group_named_alt_syntax() {
    let re = Regex::new(r"(?'word'\w+)").unwrap();
    let caps = re.captures("hello").unwrap();
    assert_eq!(caps.name("word").unwrap().as_str(), "hello");
}
#[test]
fn group_lookahead_pos() {
    assert_match!(r"foo(?=bar)", "foobar");
    assert_no_match!(r"foo(?=bar)", "foobaz");
    // lookahead does not consume
    assert_find!(r"foo(?=bar)", "foobar", "foo");
}
#[test]
fn group_lookahead_neg() {
    assert_match!(r"foo(?!bar)", "foobaz");
    assert_no_match!(r"foo(?!bar)", "foobar");
}
#[test]
fn group_lookbehind_pos() {
    assert_match!(r"(?<=foo)bar", "foobar");
    assert_no_match!(r"(?<=foo)bar", "bazbar");
}
#[test]
fn group_lookbehind_neg() {
    assert_match!(r"(?<!foo)bar", "bazbar");
    assert_no_match!(r"(?<!foo)bar", "foobar");
}
#[test]
fn group_lookbehind_variable_length() {
    // Unbounded quantifier inside lookbehind (was previously restricted).
    assert_match!(r"(?<=a+)b", "aaab");
    assert_no_match!(r"(?<=a+)b", "b");
    assert_match!(r"(?<=a*)b", "b"); // zero a's is allowed by a*
    assert_match!(r"(?<=a*)b", "aaab");
    // Alternation producing different lengths
    assert_match!(r"(?<=foo|fo)bar", "foobar");
    assert_match!(r"(?<=fo)bar", "fobar");
    // Negative variable-length lookbehind
    assert_no_match!(r"(?<!a+)b", "aaab");
    assert_match!(r"(?<!a+)b", "b");
}
#[test]
fn group_atomic() {
    // (?>a*) is possessive — no backtrack
    assert_no_match!(r"^(?>a*)a$", "aa");
    assert_match!(r"^(?>a*)b$", "aaab");
}
#[test]
fn group_keep() {
    // \K resets match start
    let re = Regex::new(r"foo\Kbar").unwrap();
    assert_eq!(re.find("foobar").unwrap().as_str(), "bar");
}
#[test]
fn inline_flags_case_insensitive() {
    assert_match!(r"(?i)foo", "FOO");
    assert_match!(r"(?i:foo)", "FOO");
    assert_no_match!(r"foo", "FOO");
}
#[test]
fn inline_flags_multiline() {
    // Ruby (?m): dot matches newline
    assert_match!(r"(?m)a.b", "a\nb");
    assert_no_match!(r"a.b", "a\nb");
}
#[test]
fn inline_flags_extended() {
    // (?x): whitespace ignored, # is comment
    assert_match!(r"(?x) f o o # match foo", "foo");
}
#[test]
fn group_absence_basic() {
    // (?~abc) matches strings not containing "abc"
    assert_match!(r"^(?~abc)$", "ab");
    assert_match!(r"^(?~abc)$", "");
    assert_no_match!(r"^(?~abc)$", "abc");
    assert_no_match!(r"^(?~abc)$", "xabcy");
}
#[test]
fn group_absence_c_comment() {
    // C-style comment: /* ... */ with no */ inside
    assert_match!(r"/\*(?~\*/)\*/", "/* foo */");
    // Non-anchored pattern finds "/* foo */" within the larger string
    assert_match!(r"/\*(?~\*/)\*/", "/* foo */ bar */");
    // Anchored version rejects text with multiple comment-like spans
    assert_no_match!(r"\A/\*(?~\*/)\*/\z", "/* foo */ bar */");
}
#[test]
fn group_conditional_num() {
    // (?(1)yes|no): if group 1 matched, use yes
    let re = Regex::new(r"(a)?(?(1)b|c)").unwrap();
    assert_eq!(re.find("ab").unwrap().as_str(), "ab");
    assert_eq!(re.find("c").unwrap().as_str(), "c");
}
#[test]
fn group_conditional_name() {
    let _re = Regex::new(r"(?<x>a)?(<x>b|c)").unwrap();
    // When group x matched: match b; else: match c
    // Note: using (?(cond)) syntax
    let re = Regex::new(r"(?<x>a)?(?(x)b|c)").unwrap();
    assert_eq!(re.find("ab").unwrap().as_str(), "ab");
    assert_eq!(re.find("c").unwrap().as_str(), "c");
}

// ---------------------------------------------------------------------------
// §8 Backreferences
// ---------------------------------------------------------------------------
#[test]
fn backref_numeric() {
    assert_match!(r"(foo)\1", "foofoo");
    assert_no_match!(r"(foo)\1", "foobar");
}
#[test]
fn backref_named() {
    assert_match!(r"(?<x>foo)\k<x>", "foofoo");
    assert_no_match!(r"(?<x>foo)\k<x>", "foobar");
}
#[test]
fn backref_quoted_name() {
    assert_match!(r"(?<x>foo)\k'x'", "foofoo");
}
#[test]
fn backref_case_insensitive() {
    assert_match!(r"(?i)(foo)\1", "fooFOO");
}

// ---------------------------------------------------------------------------
// §9 Subexpression calls
// ---------------------------------------------------------------------------
#[test]
fn subexp_call_named() {
    // Simple recursive palindrome
    let re = Regex::new(r"\A(?<a>|.|\g<a>)\z").unwrap();
    assert!(re.is_match("a") || re.is_match("racecar")); // basic smoke test
}
#[test]
fn subexp_call_whole() {
    // \g<0> calls whole pattern — basic smoke test (must not infinite loop)
    // We'll just test compile succeeds and simple non-recursive case works
    let re = Regex::new(r"a\g<0>?b").unwrap();
    assert!(re.is_match("ab"));
    assert!(re.is_match("aabb"));
}

// ---------------------------------------------------------------------------
// §10 find_iter / captures_iter
// ---------------------------------------------------------------------------
#[test]
fn find_iter_basic() {
    let re = Regex::new(r"\d+").unwrap();
    let matches: Vec<&str> = re
        .find_iter("one1two22three333")
        .map(|m| m.as_str())
        .collect();
    assert_eq!(matches, vec!["1", "22", "333"]);
}
#[test]
fn captures_iter_basic() {
    let re = Regex::new(r"(\w+)=(\d+)").unwrap();
    let pairs: Vec<(&str, &str)> = re
        .captures_iter("a=1 b=22 c=333")
        .map(|c| (c.get(1).unwrap().as_str(), c.get(2).unwrap().as_str()))
        .collect();
    assert_eq!(pairs, vec![("a", "1"), ("b", "22"), ("c", "333")]);
}
#[test]
fn find_iter_empty_matches() {
    let re = Regex::new(r"a*").unwrap();
    let matches: Vec<&str> = re.find_iter("xax").map(|m| m.as_str()).collect();
    // should not infinite-loop and should advance past empty matches
    assert!(matches.len() >= 3);
}

// ---------------------------------------------------------------------------
// Alternation
// ---------------------------------------------------------------------------
#[test]
fn alternation_basic() {
    assert_match!(r"cat|dog", "I have a cat");
    assert_match!(r"cat|dog", "I have a dog");
    assert_no_match!(r"cat|dog", "I have a fish");
}
#[test]
fn alternation_leftmost() {
    // leftmost alternative wins
    assert_find!(r"foo|foobar", "foobar", "foo");
}

// ---------------------------------------------------------------------------
// Case sensitivity
// ---------------------------------------------------------------------------
#[test]
fn case_sensitive_default() {
    assert_no_match!(r"foo", "FOO");
    assert_match!(r"foo", "foo");
}
#[test]
fn case_insensitive_flag() {
    assert_match!(r"(?i)foo", "FOO");
    assert_match!(r"(?i)foo", "Foo");
}

// ---------------------------------------------------------------------------
// Unicode
// ---------------------------------------------------------------------------
#[test]
fn unicode_literal() {
    assert_match!(r"日本語", "日本語のテキスト");
}
#[test]
fn unicode_word_boundary() {
    assert_match!(r"\bfoo\b", "foo");
}

// ---------------------------------------------------------------------------
// Memoization correctness
// ---------------------------------------------------------------------------

// Backreferences: memo must be disabled so that the same (pc, pos) can
// produce different outcomes depending on the current captured-group value.
#[test]
fn memo_disabled_for_backref() {
    // \1 is captured group 1; the fork at (a|aa) depends on what \1 matched.
    // Without disabling memo, the second alternative could be wrongly skipped.
    let _re = Regex::new(r"(a|aa)\1").unwrap();
    assert_match!(r"(a|aa)\1", "aaa"); // "aa" + \1="aa" doesn't work; "a" + \1="a" = "aa" ✓
    assert_no_match!(r"(a|aa)\1", "b");
    // This pattern requires memo to be off: if (a|aa) at pos 0 is memoized as
    // failure after trying "a" path, the "aa" path (which succeeds) would be skipped.
    let re2 = Regex::new(r"(a+)\1").unwrap();
    assert!(re2.is_match("aaaa")); // group 1 = "aa", \1 = "aa"
}

// Lookaround: failures from inside the lookahead body must be shared with
// the outer execution (Algorithm 6).  The pattern below has a pathological
// fork inside the lookahead body; with a shared memo it runs in linear time.
#[test]
fn memo_lookaround_correctness() {
    // Positive lookahead that itself has alternatives
    let _re = Regex::new(r"(?=(a|b))a").unwrap();
    assert_match!(r"(?=(a|b))a", "a");
    assert_no_match!(r"(?=(a|b))a", "b");

    // Negative lookahead
    let _re2 = Regex::new(r"(?!(a|b))c").unwrap();
    assert_match!(r"(?!(a|b))c", "c");
    assert_no_match!(r"(?!(a|b))c", "a");
}

// Atomic grouping + memo: a failure recorded inside an atomic group (at
// atomic_depth > 0) must NOT be reused outside the group (at depth 0),
// because outside the group there is less backtracking constraint.
#[test]
fn memo_atomic_depth_correctness() {
    // Pattern: (?>a|b)x | ax
    // At pos 0 on "ax":
    //   First alt: atomic group tries 'a' (ok), then 'x'… matches → done.
    // On "bx":
    //   First alt: atomic group tries 'a' (fail), tries 'b' (ok), then 'x'… matches.
    // On "cx":
    //   First alt: atomic tries 'a' (fail), 'b' (fail) → memo records failure
    //              at atomic_depth=1. Atomic group fails entirely.
    //   Second alt: 'a' (fail) → overall no match. Correct.
    assert_match!(r"(?>a|b)x|ax", "ax");
    assert_match!(r"(?>a|b)x|ax", "bx");
    assert_no_match!(r"(?>a|b)x|ax", "cx");

    // Trickier: (?>a|b) | (a|b)   on input "a" — second alt must not be
    // short-circuited even if inner fork fired inside atomic on a prior attempt.
    let _re = Regex::new(r"(?>a|b)c|(a|b)").unwrap();
    assert_match!(r"(?>a|b)c|(a|b)", "a"); // second alt matches
    assert_match!(r"(?>a|b)c|(a|b)", "b"); // second alt matches
    assert_match!(r"(?>a|b)c|(a|b)", "ac"); // first alt matches
}

// Lookaround success memoization: without Algorithm 6 success caching the
// same LookStart at (lk_pc, pos) is re-evaluated on every outer backtrack
// path, leading to exponential behaviour for patterns like (a?)^n (?=a^n).
// With success caching the sub-execution runs at most once per (lk_pc, pos).
#[test]
fn memo_lookaround_success_caching() {
    // Outer (a|ε)^5 with a lookahead containing its own alternatives.
    // The lookahead (?=(a|b)*) at a given position must only run once (cached).
    let re = Regex::new(r"(?:a|)(?:a|)(?:a|)(?:a|)(?:a|)(?=(a|b)*)$").unwrap();
    assert!(re.is_match("aaaaa"));
    assert!(re.is_match("aaabb"));
    assert!(re.is_match(""));

    // Positive lookahead: verify captured groups via the slot delta.
    // (?=(a+)) captures `a+` from current position.
    let re2 = Regex::new(r"(?:a|)(?:a|)(?:a|)(?=(a+))").unwrap();
    let caps = re2.captures("aaa").unwrap();
    // Group 1 must be set (the lookahead captured something).
    assert!(caps.get(1).is_some());

    // Positive lookbehind result must also be cached.
    let re3 = Regex::new(r"(?:a|)(?:a|)(?:a|)a*(?<=(a+))$").unwrap();
    assert!(re3.is_match("aaa"));
    // The lookbehind capture should be set.
    let caps3 = re3.captures("aaa").unwrap();
    assert!(caps3.get(1).is_some());
}

// ---------------------------------------------------------------------------
// Relative backreferences (\k<-n>)
// ---------------------------------------------------------------------------

#[test]
fn backref_relative_backward() {
    // \k<-1> inside group 2 refers to group 1
    assert_match!(r"(a)(\k<-1>)", "aa");
    assert_no_match!(r"(a)(\k<-1>)", "ab");

    // \k<-2> inside group 3 refers to group 1
    assert_match!(r"(a)(b)(\k<-2>)", "aba");
    assert_no_match!(r"(a)(b)(\k<-2>)", "abb");

    // \k<-1> inside group 3 refers to group 2
    assert_match!(r"(a)(b)(\k<-1>)", "abb");
    assert_no_match!(r"(a)(b)(\k<-1>)", "aba");
}

// ---------------------------------------------------------------------------
// Relative subexpression calls (\g<-n>, \g<+n>)
// ---------------------------------------------------------------------------

#[test]
fn subexp_call_relative_backward() {
    // (a)(\g<-1>) — group 2 calls group 1 (\g<-1> from inside group 2 = group 1)
    assert_match!(r"(a)(\g<-1>)", "aa");
    assert_no_match!(r"(a)(\g<-1>)", "ab");
}

#[test]
fn subexp_call_relative_forward() {
    // (\g<+1>)(abc) — group 1 calls group 2 ahead of it (fixed-width body, no backtracking)
    assert_match!(r"(\g<+1>)(abc)", "abcabc");
    assert_no_match!(r"(\g<+1>)(abc)", "abc");
}
