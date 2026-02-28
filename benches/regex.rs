use aigumo::Regex;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

// ---------------------------------------------------------------------------
// Benchmark corpus
// ---------------------------------------------------------------------------

/// Repeat `s` `n` times.
fn rep(s: &str, n: usize) -> String {
    s.repeat(n)
}

// ---------------------------------------------------------------------------
// Literal matching
// ---------------------------------------------------------------------------

fn bench_literal(c: &mut Criterion) {
    let haystack = rep("aaaaaaaaaa", 100); // 1000 'a's
    let re = Regex::new("abcde").unwrap();
    c.bench_function("literal/no_match_1k", |b| {
        b.iter(|| re.is_match(black_box(&haystack)))
    });

    let haystack2 = format!("{}abcde{}", rep("x", 500), rep("x", 500));
    c.bench_function("literal/match_mid_1k", |b| {
        b.iter(|| re.find(black_box(&haystack2)))
    });
}

// ---------------------------------------------------------------------------
// Anchored literal
// ---------------------------------------------------------------------------

fn bench_anchored(c: &mut Criterion) {
    let haystack = rep("a", 1000);
    let re = Regex::new(r"\Aabc").unwrap();
    c.bench_function("anchored/no_match_1k", |b| {
        b.iter(|| re.is_match(black_box(&haystack)))
    });
}

// ---------------------------------------------------------------------------
// Alternation
// ---------------------------------------------------------------------------

fn bench_alternation(c: &mut Criterion) {
    let haystack = format!("{}baz{}", rep("x", 200), rep("x", 200));
    let re = Regex::new("foo|bar|baz|qux").unwrap();
    c.bench_function("alternation/4_alts_match", |b| {
        b.iter(|| re.find(black_box(&haystack)))
    });

    let haystack2 = rep("x", 500);
    c.bench_function("alternation/4_alts_no_match", |b| {
        b.iter(|| re.is_match(black_box(&haystack2)))
    });
}

// ---------------------------------------------------------------------------
// Greedy quantifiers
// ---------------------------------------------------------------------------

fn bench_quantifier(c: &mut Criterion) {
    let haystack = rep("a", 500);
    let re = Regex::new(r"a*b").unwrap();
    c.bench_function("quantifier/greedy_no_match_500", |b| {
        b.iter(|| re.is_match(black_box(&haystack)))
    });

    let re2 = Regex::new(r"a+").unwrap();
    c.bench_function("quantifier/greedy_match_500", |b| {
        b.iter(|| re2.find(black_box(&haystack)))
    });
}

// ---------------------------------------------------------------------------
// Capture groups
// ---------------------------------------------------------------------------

fn bench_captures(c: &mut Criterion) {
    let haystack = "John Smith, Jane Doe, Bob Jones, Alice Brown";
    let re = Regex::new(r"(\w+)\s+(\w+)").unwrap();
    c.bench_function("captures/two_groups", |b| {
        b.iter(|| re.captures(black_box(haystack)))
    });

    let re2 = Regex::new(r"(\w+)\s+(\w+)").unwrap();
    c.bench_function("captures/iter_all", |b| {
        b.iter(|| re2.captures_iter(black_box(haystack)).count())
    });
}

// ---------------------------------------------------------------------------
// Email-like pattern
// ---------------------------------------------------------------------------

fn bench_email(c: &mut Criterion) {
    let haystack = "contact us at hello@example.com or support@test.org for help";
    let re = Regex::new(r"\w+@\w+\.\w+").unwrap();
    c.bench_function("email/find_all", |b| {
        b.iter(|| re.find_iter(black_box(haystack)).count())
    });
}

// ---------------------------------------------------------------------------
// Character classes
// ---------------------------------------------------------------------------

fn bench_charclass(c: &mut Criterion) {
    let haystack = rep("aAbBcC123", 100); // 900 chars
    let re = Regex::new(r"[a-zA-Z]+").unwrap();
    c.bench_function("charclass/alpha_iter", |b| {
        b.iter(|| re.find_iter(black_box(&haystack)).count())
    });

    let re2 = Regex::new(r"[[:digit:]]+").unwrap();
    c.bench_function("charclass/posix_digit_iter", |b| {
        b.iter(|| re2.find_iter(black_box(&haystack)).count())
    });
}

// ---------------------------------------------------------------------------
// Case-insensitive
// ---------------------------------------------------------------------------

fn bench_case_insensitive(c: &mut Criterion) {
    let haystack = format!("{}HELLO{}", rep("x", 300), rep("x", 300));
    let re = Regex::new(r"(?i)hello").unwrap();
    c.bench_function("case_insensitive/match", |b| {
        b.iter(|| re.find(black_box(&haystack)))
    });
}

// ---------------------------------------------------------------------------
// Scaling: find_iter on growing input
// ---------------------------------------------------------------------------

fn bench_find_iter_scale(c: &mut Criterion) {
    let re = Regex::new(r"\d+").unwrap();
    let mut group = c.benchmark_group("find_iter_scale");
    for size in [100usize, 500, 1000, 5000] {
        // interleave digits and letters
        let haystack: String = (0..size)
            .map(|i| if i % 3 == 0 { '1' } else { 'x' })
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(size), &haystack, |b, h| {
            b.iter(|| re.find_iter(black_box(h)).count())
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Pathological backtracking (a?^n a^n)
// ---------------------------------------------------------------------------

fn bench_pathological(c: &mut Criterion) {
    let mut group = c.benchmark_group("pathological");
    for n in [10usize, 15, 20] {
        let pattern = format!("{}{}", "a?".repeat(n), "a".repeat(n));
        let haystack = "a".repeat(n);
        let re = Regex::new(&pattern).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(n), &haystack, |b, h| {
            b.iter(|| re.is_match(black_box(h)))
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Pathological backtracking — cross-position reuse
//
// `find_iter` reuses the same ExecScratch across all start positions, so
// memoization data accumulated at position k is available at position k+1.
// With the `memo_has_failures` fix this benchmark shows a significant
// reduction in work compared to the baseline (fresh scratch each call).
// ---------------------------------------------------------------------------

fn bench_pathological_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("pathological_iter");
    for n in [10usize, 15, 20] {
        let pattern = format!("{}{}", "a?".repeat(n), "a".repeat(n));
        // Haystack: many 'b's (no match) followed by one match at the end.
        // find_iter must scan every position, exercising cross-position memo reuse.
        let haystack = format!("{}{}", "b".repeat(n * 10), "a".repeat(n));
        let re = Regex::new(&pattern).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(n), &haystack, |b, h| {
            b.iter(|| re.find_iter(black_box(h)).count())
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Real-world text: "A Study in Scarlet"
// ---------------------------------------------------------------------------

fn bench_real_world(c: &mut Criterion) {
    let text = include_str!("fixtures/stud.txt");
    let mut group = c.benchmark_group("real_world");

    // Simple literal: count occurrences of "Holmes"
    let re_holmes = Regex::new("Holmes").unwrap();
    group.bench_function("literal_count", |b| {
        b.iter(|| re_holmes.find_iter(black_box(text)).count())
    });

    // Capitalized words: [A-Z][a-z]+ (exercises inline charclass)
    let re_cap = Regex::new(r"[A-Z][a-z]+").unwrap();
    group.bench_function("capitalized_words", |b| {
        b.iter(|| re_cap.find_iter(black_box(text)).count())
    });

    // POSIX digit sequences: [[:digit:]]+ (exercises POSIX inline charclass)
    let re_digits = Regex::new(r"[[:digit:]]+").unwrap();
    group.bench_function("posix_digits", |b| {
        b.iter(|| re_digits.find_iter(black_box(text)).count())
    });

    // Quoted strings: "[^"]*" (negated charclass, helper path)
    let re_quotes = Regex::new("\"[^\"]*\"").unwrap();
    group.bench_function("quoted_strings", |b| {
        b.iter(|| re_quotes.find_iter(black_box(text)).count())
    });

    // Name pattern: Mr. / Mrs. followed by a capitalized word
    let re_title = Regex::new(r"Mrs?\. [A-Z][a-z]+").unwrap();
    group.bench_function("title_name", |b| {
        b.iter(|| re_title.find_iter(black_box(text)).count())
    });

    group.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_literal,
    bench_anchored,
    bench_alternation,
    bench_quantifier,
    bench_captures,
    bench_email,
    bench_charclass,
    bench_case_insensitive,
    bench_find_iter_scale,
    bench_pathological,
    bench_pathological_iter,
    bench_real_world,
);
criterion_main!(benches);
