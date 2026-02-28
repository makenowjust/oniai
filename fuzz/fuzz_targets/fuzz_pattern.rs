#![no_main]

use libfuzzer_sys::fuzz_target;

// Feed arbitrary bytes as a regex pattern.
// Goal: find panics or stack overflows in the parser and compiler.
fuzz_target!(|data: &[u8]| {
    let pattern = String::from_utf8_lossy(data);
    // We don't care about the result — only about not panicking.
    let _ = aigumo::Regex::new(&pattern);
});
