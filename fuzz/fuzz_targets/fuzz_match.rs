#![no_main]

use libfuzzer_sys::fuzz_target;

// Split fuzz input into a pattern and a subject string.
// The first byte encodes what fraction of the remaining bytes become the pattern.
// Goal: find panics or incorrect behaviour in the full VM pipeline (JIT enabled).
fuzz_target!(|data: &[u8]| {
    let Some((&split_byte, rest)) = data.split_first() else {
        return;
    };

    // split_byte / 256 determines how much of `rest` is the pattern.
    let pat_len = (rest.len() * split_byte as usize) / 256;
    let pattern = String::from_utf8_lossy(&rest[..pat_len]);
    let subject = String::from_utf8_lossy(&rest[pat_len..]);

    let Ok(re) = aigumo::Regex::new(&pattern) else {
        return;
    };
    let _ = re.find(&subject);
});
