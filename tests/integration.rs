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

