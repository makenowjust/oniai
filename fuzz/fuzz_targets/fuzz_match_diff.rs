#![no_main]

use libfuzzer_sys::fuzz_target;

// Differential fuzz target: run the same input through both the JIT executor
// and the pure interpreter and panic if their results disagree.
// Goal: find semantic divergence between the two execution engines.
fuzz_target!(|data: &[u8]| {
    let Some((&split_byte, rest)) = data.split_first() else {
        return;
    };

    let pat_len = (rest.len() * split_byte as usize) / 256;
    let pattern = String::from_utf8_lossy(&rest[..pat_len]);
    let subject = String::from_utf8_lossy(&rest[pat_len..]);

    let Ok(re) = aigumo::Regex::new(&pattern) else {
        return;
    };

    let jit_result = re.find(&subject);
    let interp_result = re.find_interp(&subject);

    // The two engines must agree on whether there is a match and on its range.
    match (jit_result, interp_result) {
        (Some(jit), Some(interp)) => {
            assert_eq!(
                (jit.start(), jit.end()),
                (interp.start(), interp.end()),
                "JIT and interpreter disagree on match range for pattern {:?} against {:?}",
                pattern,
                subject,
            );
        }
        (None, None) => {}
        (jit, interp) => {
            panic!(
                "JIT and interpreter disagree on match presence: jit={:?} interp={:?} \
                 pattern={:?} subject={:?}",
                jit.is_some(),
                interp.is_some(),
                pattern,
                subject,
            );
        }
    }
});
