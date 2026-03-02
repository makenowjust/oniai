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

