#[test]
#[cfg(feature = "jit")]
fn test_jit_fuzz_regression_1() {
    // Fuzz artifact: JIT/interpreter divergence for \u{FFFD}? vs \u{5}\u{FFFD}+1
    let re = oniai::Regex::new("\u{FFFD}?").unwrap();
    let subject = "\u{5}\u{FFFD}+1";
    let jit = re.find(subject);
    let interp = re.find_interp(subject);
    assert_eq!(
        jit.map(|m| (m.start(), m.end())),
        interp.map(|m| (m.start(), m.end())),
        "JIT/interp diverge for \\u{{FFFD}}? on \\u{{5}}\\u{{FFFD}}+1"
    );
}

#[test]
#[cfg(feature = "jit")]
fn test_fuzz_regression_2() {
    // Fuzz artifact: panic from CharFast at ForkNext alt being reached via
    // body fall-through without guard verification.
    // pattern="\u{FFFD}??\x02\u{FFFD}", subject="6\u{FFFD}\u{FFFD}\x02\x02\x02"
    let pattern = "\u{FFFD}??\x02\u{FFFD}";
    let subject = "6\u{FFFD}\u{FFFD}\x02\x02\x02";
    let re = oniai::Regex::new(pattern).unwrap();
    let jit = re.find(subject);
    let interp = re.find_interp(subject);
    assert_eq!(
        jit.map(|m| (m.start(), m.end())),
        interp.map(|m| (m.start(), m.end())),
        "JIT/interp diverge for fuzz regression 2"
    );
}

#[test]
fn test_fuzz_regression_3() {
    // Fuzz artifact: CharFast at ForkNext's alt reached via NullCheckEnd
    // when lazy loop body matched empty string.
    // pattern="(?:a|)(?:a|)(|)*?=...", subject="88\u{FFFD}*)$"
    let pattern = "(?:a|)(?:a|)(|)*?=\u{FFFD}";
    let subject = "88\u{FFFD}*)$";
    let re = oniai::Regex::new(pattern).unwrap();
    // Should not panic - both engines must agree
    let _ = re.find(subject);
}


#[test]
fn test_fuzz_regression_4() {
    // Fuzz artifact: panic in JIT `emit_range_check` (subtract overflow) when
    // a character class contains a range where lo > hi (e.g., `[a--]` where
    // 'a'=0x61 > '-'=0x2d). Parser now returns a parse error for such ranges.
    // pattern contains `[\x7fe-\x7fa--]` which has the invalid sub-range `a--`
    assert!(
        oniai::Regex::new("[\x7fe-\x7fa--]").is_err(),
        "invalid char class range lo > hi should be a parse error"
    );
}

#[test]
#[cfg(feature = "jit")]
fn test_fuzz_regression_5() {
    // Fuzz artifact (crash-1831897): JIT/interp divergence for `*\u{FFFD}*` pattern.
    // Root cause: SpanChar non-ASCII branch used `(*c as u32).to_le_bytes()[0]` as the
    // UTF-8 leading byte, giving the wrong value (LE codepoint byte != UTF-8 leading byte).
    // Fix: use `c.encode_utf8(&mut buf)[0]` for the correct leading byte.
    let pattern_bytes: &[u8] = &[0x2a, 0xff, 0x2a];
    let pattern = String::from_utf8_lossy(pattern_bytes);
    let subject_bytes: &[u8] = &[0x2a, 0xcf, 0x2a, 0xa3, 0x29, 0x9b, 0x00, 0x0c,
                                  0xff, 0xff, 0xff, 0x0e, 0xd4, 0x9b, 0x88, 0xc3,
                                  0xd4, 0x9b, 0x5c];
    let subject = String::from_utf8_lossy(subject_bytes);
    let re = oniai::Regex::new(&pattern).unwrap();
    assert_eq!(
        re.find(&subject).map(|m| (m.start(), m.end())),
        re.find_interp(&subject).map(|m| (m.start(), m.end())),
        "JIT/interp diverge for fuzz regression 5"
    );
}

#[test]
#[cfg(feature = "jit")]
fn test_fuzz_regression_6() {
    // Fuzz artifact (crash-752acad): JIT/interp divergence for `\u{FFFD}\u{FFFD}\u{FFFD}*`.
    // Same root cause as regression 5: wrong UTF-8 leading byte in SpanChar non-ASCII path.
    let re = oniai::Regex::new("\u{FFFD}\u{FFFD}\u{FFFD}*").unwrap();
    let subject = "\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}";
    assert_eq!(
        re.find(subject).map(|m| (m.start(), m.end())),
        re.find_interp(subject).map(|m| (m.start(), m.end())),
        "JIT/interp diverge for fuzz regression 6"
    );
}
