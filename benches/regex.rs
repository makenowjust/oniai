use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use oniai::Regex;

// ---------------------------------------------------------------------------
// Benchmark corpus
// ---------------------------------------------------------------------------

fn rep(s: &str, n: usize) -> String {
    s.repeat(n)
}

// ---------------------------------------------------------------------------
// Helper macros
//
//   bench_all!     — all five engines (patterns supported by all)
//     1: oniai/jit
//     2: oniai/interp  (jit feature only)
//     3: regex         (standard NFA crate)
//     4: fancy-regex   (NFA + backtracking; supports lookarounds/backrefs)
//     5: pcre2         (PCRE2 C library via Rust bindings)
//
//   bench_advanced! — four engines, excluding `regex`
//     (for patterns regex does not support: lookarounds, backrefs, atomic groups)
// ---------------------------------------------------------------------------

macro_rules! bench_all {
    ($group:expr, $jit:expr, $interp:expr, $std:expr, $fancy:expr, $pcre2:expr) => {
        $group.bench_function("oniai/jit", |b| b.iter(|| $jit));
        #[cfg(feature = "jit")]
        $group.bench_function("oniai/interp", |b| b.iter(|| $interp));
        $group.bench_function("regex", |b| b.iter(|| $std));
        $group.bench_function("fancy-regex", |b| b.iter(|| $fancy));
        $group.bench_function("pcre2", |b| b.iter(|| $pcre2));
    };
}

macro_rules! bench_advanced {
    ($group:expr, $jit:expr, $interp:expr, $fancy:expr, $pcre2:expr) => {
        $group.bench_function("oniai/jit", |b| b.iter(|| $jit));
        #[cfg(feature = "jit")]
        $group.bench_function("oniai/interp", |b| b.iter(|| $interp));
        $group.bench_function("fancy-regex", |b| b.iter(|| $fancy));
        $group.bench_function("pcre2", |b| b.iter(|| $pcre2));
    };
}

// ---------------------------------------------------------------------------
// Literal matching
// ---------------------------------------------------------------------------

fn bench_literal(c: &mut Criterion) {
    let haystack = rep("aaaaaaaaaa", 100); // 1000 'a's
    let o = Regex::new("abcde").unwrap();
    let s = regex::Regex::new("abcde").unwrap();
    let f = fancy_regex::Regex::new("abcde").unwrap();
    let p = pcre2::bytes::Regex::new("abcde").unwrap();

    let mut g = c.benchmark_group("literal/no_match_1k");
    bench_all!(
        g,
        o.is_match(black_box(&haystack)),
        o.find_iter_interp(black_box(&haystack)).next().is_some(),
        s.is_match(black_box(&haystack)),
        f.is_match(black_box(&haystack)).unwrap(),
        p.is_match(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();

    let haystack2 = format!("{}abcde{}", rep("x", 500), rep("x", 500));
    let mut g = c.benchmark_group("literal/match_mid_1k");
    bench_all!(
        g,
        o.find(black_box(&haystack2)),
        o.find_iter_interp(black_box(&haystack2)).next(),
        s.find(black_box(&haystack2)),
        f.find(black_box(&haystack2)).unwrap(),
        p.find(black_box(haystack2.as_bytes())).unwrap()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Anchored literal
// ---------------------------------------------------------------------------

fn bench_anchored(c: &mut Criterion) {
    let haystack = rep("a", 1000);
    let o = Regex::new(r"\Aabc").unwrap();
    let s = regex::Regex::new(r"\Aabc").unwrap();
    let f = fancy_regex::Regex::new(r"\Aabc").unwrap();
    let p = pcre2::bytes::Regex::new(r"\Aabc").unwrap();

    let mut g = c.benchmark_group("anchored/no_match_1k");
    bench_all!(
        g,
        o.is_match(black_box(&haystack)),
        o.find_iter_interp(black_box(&haystack)).next().is_some(),
        s.is_match(black_box(&haystack)),
        f.is_match(black_box(&haystack)).unwrap(),
        p.is_match(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Alternation
// ---------------------------------------------------------------------------

fn bench_alternation(c: &mut Criterion) {
    let haystack = format!("{}baz{}", rep("x", 200), rep("x", 200));
    let haystack2 = rep("x", 500);
    let o = Regex::new("foo|bar|baz|qux").unwrap();
    let s = regex::Regex::new("foo|bar|baz|qux").unwrap();
    let f = fancy_regex::Regex::new("foo|bar|baz|qux").unwrap();
    let p = pcre2::bytes::Regex::new("foo|bar|baz|qux").unwrap();

    let mut g = c.benchmark_group("alternation/4_alts_match");
    bench_all!(
        g,
        o.find(black_box(&haystack)),
        o.find_iter_interp(black_box(&haystack)).next(),
        s.find(black_box(&haystack)),
        f.find(black_box(&haystack)).unwrap(),
        p.find(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();

    let mut g = c.benchmark_group("alternation/4_alts_no_match");
    bench_all!(
        g,
        o.is_match(black_box(&haystack2)),
        o.find_iter_interp(black_box(&haystack2)).next().is_some(),
        s.is_match(black_box(&haystack2)),
        f.is_match(black_box(&haystack2)).unwrap(),
        p.is_match(black_box(haystack2.as_bytes())).unwrap()
    );
    g.finish();

    // Benchmark for many-string alternations — measures AltTrie benefit.
    // Pattern has 10 alternatives that don't share a common prefix.
    let pat10 = "alpha|bravo|charlie|delta|echo|foxtrot|golf|hotel|india|juliet";
    let haystack3 = format!("{}juliet{}", rep("x", 200), rep("x", 200));
    let haystack4 = rep("x", 500);
    let o10 = Regex::new(pat10).unwrap();
    let s10 = regex::Regex::new(pat10).unwrap();
    let f10 = fancy_regex::Regex::new(pat10).unwrap();
    let p10 = pcre2::bytes::Regex::new(pat10).unwrap();

    let mut g = c.benchmark_group("alternation/10_alts_match");
    bench_all!(
        g,
        o10.find(black_box(&haystack3)),
        o10.find_iter_interp(black_box(&haystack3)).next(),
        s10.find(black_box(&haystack3)),
        f10.find(black_box(&haystack3)).unwrap(),
        p10.find(black_box(haystack3.as_bytes())).unwrap()
    );
    g.finish();

    let mut g = c.benchmark_group("alternation/10_alts_no_match");
    bench_all!(
        g,
        o10.is_match(black_box(&haystack4)),
        o10.find_iter_interp(black_box(&haystack4)).next().is_some(),
        s10.is_match(black_box(&haystack4)),
        f10.is_match(black_box(&haystack4)).unwrap(),
        p10.is_match(black_box(haystack4.as_bytes())).unwrap()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Greedy quantifiers
// ---------------------------------------------------------------------------

fn bench_quantifier(c: &mut Criterion) {
    let haystack = rep("a", 500);
    let o1 = Regex::new(r"a*b").unwrap();
    let s1 = regex::Regex::new(r"a*b").unwrap();
    let f1 = fancy_regex::Regex::new(r"a*b").unwrap();
    let p1 = pcre2::bytes::Regex::new(r"a*b").unwrap();

    let mut g = c.benchmark_group("quantifier/greedy_no_match_500");
    bench_all!(
        g,
        o1.is_match(black_box(&haystack)),
        o1.find_iter_interp(black_box(&haystack)).next().is_some(),
        s1.is_match(black_box(&haystack)),
        f1.is_match(black_box(&haystack)).unwrap(),
        p1.is_match(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();

    let o2 = Regex::new(r"a+").unwrap();
    let s2 = regex::Regex::new(r"a+").unwrap();
    let f2 = fancy_regex::Regex::new(r"a+").unwrap();
    let p2 = pcre2::bytes::Regex::new(r"a+").unwrap();

    let mut g = c.benchmark_group("quantifier/greedy_match_500");
    bench_all!(
        g,
        o2.find(black_box(&haystack)),
        o2.find_iter_interp(black_box(&haystack)).next(),
        s2.find(black_box(&haystack)),
        f2.find(black_box(&haystack)).unwrap(),
        p2.find(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Capture groups
// ---------------------------------------------------------------------------

fn bench_captures(c: &mut Criterion) {
    let haystack = "John Smith, Jane Doe, Bob Jones, Alice Brown";
    let o = Regex::new(r"(\w+)\s+(\w+)").unwrap();
    let s = regex::Regex::new(r"(\w+)\s+(\w+)").unwrap();
    let f = fancy_regex::Regex::new(r"(\w+)\s+(\w+)").unwrap();
    let p = pcre2::bytes::Regex::new(r"(\w+)\s+(\w+)").unwrap();

    let mut g = c.benchmark_group("captures/two_groups");
    bench_all!(
        g,
        o.captures(black_box(haystack)),
        o.find_iter_interp(black_box(haystack)).next(),
        s.captures(black_box(haystack)),
        f.captures(black_box(haystack)).unwrap(),
        p.captures(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();

    let mut g = c.benchmark_group("captures/iter_all");
    bench_all!(
        g,
        o.captures_iter(black_box(haystack)).count(),
        o.find_iter_interp(black_box(haystack)).count(),
        s.captures_iter(black_box(haystack)).count(),
        f.captures_iter(black_box(haystack))
            .map(|r| r.unwrap())
            .count(),
        p.captures_iter(black_box(haystack.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Email-like pattern
// ---------------------------------------------------------------------------

fn bench_email(c: &mut Criterion) {
    let haystack = "contact us at hello@example.com or support@test.org for help";
    let o = Regex::new(r"\w+@\w+\.\w+").unwrap();
    let s = regex::Regex::new(r"\w+@\w+\.\w+").unwrap();
    let f = fancy_regex::Regex::new(r"\w+@\w+\.\w+").unwrap();
    let p = pcre2::bytes::Regex::new(r"\w+@\w+\.\w+").unwrap();

    let mut g = c.benchmark_group("email/find_all");
    bench_all!(
        g,
        o.find_iter(black_box(haystack)).count(),
        o.find_iter_interp(black_box(haystack)).count(),
        s.find_iter(black_box(haystack)).count(),
        f.find_iter(black_box(haystack)).map(|r| r.unwrap()).count(),
        p.find_iter(black_box(haystack.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Character classes
// ---------------------------------------------------------------------------

fn bench_charclass(c: &mut Criterion) {
    let haystack = rep("aAbBcC123", 100); // 900 chars
    let o1 = Regex::new(r"[a-zA-Z]+").unwrap();
    let s1 = regex::Regex::new(r"[a-zA-Z]+").unwrap();
    let f1 = fancy_regex::Regex::new(r"[a-zA-Z]+").unwrap();
    let p1 = pcre2::bytes::Regex::new(r"[a-zA-Z]+").unwrap();

    let mut g = c.benchmark_group("charclass/alpha_iter");
    bench_all!(
        g,
        o1.find_iter(black_box(&haystack)).count(),
        o1.find_iter_interp(black_box(&haystack)).count(),
        s1.find_iter(black_box(&haystack)).count(),
        f1.find_iter(black_box(&haystack))
            .map(|r| r.unwrap())
            .count(),
        p1.find_iter(black_box(haystack.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();

    let o2 = Regex::new(r"[[:digit:]]+").unwrap();
    let s2 = regex::Regex::new(r"[[:digit:]]+").unwrap();
    let f2 = fancy_regex::Regex::new(r"[[:digit:]]+").unwrap();
    let p2 = pcre2::bytes::Regex::new(r"[[:digit:]]+").unwrap();

    let mut g = c.benchmark_group("charclass/posix_digit_iter");
    bench_all!(
        g,
        o2.find_iter(black_box(&haystack)).count(),
        o2.find_iter_interp(black_box(&haystack)).count(),
        s2.find_iter(black_box(&haystack)).count(),
        f2.find_iter(black_box(&haystack))
            .map(|r| r.unwrap())
            .count(),
        p2.find_iter(black_box(haystack.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Case-insensitive
// ---------------------------------------------------------------------------

fn bench_case_insensitive(c: &mut Criterion) {
    let haystack = format!("{}HELLO{}", rep("x", 300), rep("x", 300));
    let o = Regex::new(r"(?i)hello").unwrap();
    let s = regex::Regex::new(r"(?i)hello").unwrap();
    let f = fancy_regex::Regex::new(r"(?i)hello").unwrap();
    let p = pcre2::bytes::Regex::new(r"(?i)hello").unwrap();

    let mut g = c.benchmark_group("case_insensitive/match");
    bench_all!(
        g,
        o.find(black_box(&haystack)),
        o.find_iter_interp(black_box(&haystack)).next(),
        s.find(black_box(&haystack)),
        f.find(black_box(&haystack)).unwrap(),
        p.find(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Lookahead — `regex` does not support; oniai / fancy-regex / pcre2 only
// Pattern: \w+(?=,)  — words immediately followed by a comma
// ---------------------------------------------------------------------------

fn bench_lookahead(c: &mut Criterion) {
    let text = include_str!("fixtures/stud.txt");
    let o = Regex::new(r"\w+(?=,)").unwrap();
    let f = fancy_regex::Regex::new(r"\w+(?=,)").unwrap();
    let p = pcre2::bytes::Regex::new(r"\w+(?=,)").unwrap();

    let mut g = c.benchmark_group("lookahead/word_before_comma");
    bench_advanced!(
        g,
        o.find_iter(black_box(text)).count(),
        o.find_iter_interp(black_box(text)).count(),
        f.find_iter(black_box(text)).map(|r| r.unwrap()).count(),
        p.find_iter(black_box(text.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Lookbehind — `regex` does not support; oniai / fancy-regex / pcre2 only
// Pattern: (?<=\. )[A-Z]\w+  — capitalized word after end of sentence
// ---------------------------------------------------------------------------

fn bench_lookbehind(c: &mut Criterion) {
    let text = include_str!("fixtures/stud.txt");
    let o = Regex::new(r"(?<=\. )[A-Z]\w+").unwrap();
    let f = fancy_regex::Regex::new(r"(?<=\. )[A-Z]\w+").unwrap();
    let p = pcre2::bytes::Regex::new(r"(?<=\. )[A-Z]\w+").unwrap();

    let mut g = c.benchmark_group("lookbehind/word_after_period");
    bench_advanced!(
        g,
        o.find_iter(black_box(text)).count(),
        o.find_iter_interp(black_box(text)).count(),
        f.find_iter(black_box(text)).map(|r| r.unwrap()).count(),
        p.find_iter(black_box(text.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Backreference — `regex` does not support; oniai / fancy-regex / pcre2 only
// Pattern: (\b\w+\b) \1  — doubled word (e.g. "the the")
// ---------------------------------------------------------------------------

fn bench_backreference(c: &mut Criterion) {
    let text = include_str!("fixtures/stud.txt");
    let o = Regex::new(r"(\b\w+\b) \1").unwrap();
    let f = fancy_regex::Regex::new(r"(\b\w+\b) \1").unwrap();
    let p = pcre2::bytes::Regex::new(r"(\b\w+\b) \1").unwrap();

    let mut g = c.benchmark_group("backreference/doubled_word");
    bench_advanced!(
        g,
        o.find_iter(black_box(text)).count(),
        o.find_iter_interp(black_box(text)).count(),
        f.find_iter(black_box(text)).map(|r| r.unwrap()).count(),
        p.find_iter(black_box(text.as_bytes()))
            .map(|r| r.unwrap())
            .count()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Atomic groups — `regex` does not support; oniai / fancy-regex / pcre2 only
// Pattern: (?>a+)b on 500 'a's (no-match): atomic group prevents backtracking
// ---------------------------------------------------------------------------

fn bench_atomic_group(c: &mut Criterion) {
    let haystack = rep("a", 500);
    let o = Regex::new(r"(?>a+)b").unwrap();
    let f = fancy_regex::Regex::new(r"(?>a+)b").unwrap();
    let p = pcre2::bytes::Regex::new(r"(?>a+)b").unwrap();

    let mut g = c.benchmark_group("atomic_group/no_match_500");
    bench_advanced!(
        g,
        o.is_match(black_box(&haystack)),
        o.find_iter_interp(black_box(&haystack)).next().is_some(),
        f.is_match(black_box(&haystack)).unwrap(),
        p.is_match(black_box(haystack.as_bytes())).unwrap()
    );
    g.finish();
}

// ---------------------------------------------------------------------------
// Scaling: find_iter on growing input
// ---------------------------------------------------------------------------

fn bench_find_iter_scale(c: &mut Criterion) {
    let o = Regex::new(r"\d+").unwrap();
    let s = regex::Regex::new(r"\d+").unwrap();
    let f = fancy_regex::Regex::new(r"\d+").unwrap();
    let p = pcre2::bytes::Regex::new(r"\d+").unwrap();
    let sizes = [100usize, 500, 1000, 5000];
    let haystacks: Vec<String> = sizes
        .iter()
        .map(|&n| (0..n).map(|i| if i % 3 == 0 { '1' } else { 'x' }).collect())
        .collect();

    let mut group = c.benchmark_group("find_iter_scale");
    for (&size, haystack) in sizes.iter().zip(haystacks.iter()) {
        group.bench_with_input(BenchmarkId::new("oniai/jit", size), haystack, |b, h| {
            b.iter(|| o.find_iter(black_box(h)).count())
        });
        #[cfg(feature = "jit")]
        group.bench_with_input(BenchmarkId::new("oniai/interp", size), haystack, |b, h| {
            b.iter(|| o.find_iter_interp(black_box(h)).count())
        });
        group.bench_with_input(BenchmarkId::new("regex", size), haystack, |b, h| {
            b.iter(|| s.find_iter(black_box(h)).count())
        });
        group.bench_with_input(BenchmarkId::new("fancy-regex", size), haystack, |b, h| {
            b.iter(|| f.find_iter(black_box(h)).map(|r| r.unwrap()).count())
        });
        group.bench_with_input(BenchmarkId::new("pcre2", size), haystack, |b, h| {
            b.iter(|| {
                p.find_iter(black_box(h.as_bytes()))
                    .map(|r| r.unwrap())
                    .count()
            })
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
        let o = Regex::new(&pattern).unwrap();
        let s = regex::Regex::new(&pattern).unwrap();
        let f = fancy_regex::Regex::new(&pattern).unwrap();
        let p = pcre2::bytes::Regex::new(&pattern).unwrap();
        group.bench_with_input(BenchmarkId::new("oniai/jit", n), &haystack, |b, h| {
            b.iter(|| o.is_match(black_box(h)))
        });
        #[cfg(feature = "jit")]
        group.bench_with_input(BenchmarkId::new("oniai/interp", n), &haystack, |b, h| {
            b.iter(|| o.find_iter_interp(black_box(h)).next().is_some())
        });
        group.bench_with_input(BenchmarkId::new("regex", n), &haystack, |b, h| {
            b.iter(|| s.is_match(black_box(h)))
        });
        group.bench_with_input(BenchmarkId::new("fancy-regex", n), &haystack, |b, h| {
            b.iter(|| f.is_match(black_box(h)).unwrap())
        });
        group.bench_with_input(BenchmarkId::new("pcre2", n), &haystack, |b, h| {
            b.iter(|| p.is_match(black_box(h.as_bytes())).unwrap())
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Pathological backtracking — cross-position reuse
// ---------------------------------------------------------------------------

fn bench_pathological_iter(c: &mut Criterion) {
    let mut group = c.benchmark_group("pathological_iter");
    for n in [10usize, 15, 20] {
        let pattern = format!("{}{}", "a?".repeat(n), "a".repeat(n));
        let haystack = format!("{}{}", "b".repeat(n * 10), "a".repeat(n));
        let o = Regex::new(&pattern).unwrap();
        let s = regex::Regex::new(&pattern).unwrap();
        let f = fancy_regex::Regex::new(&pattern).unwrap();
        let p = pcre2::bytes::Regex::new(&pattern).unwrap();
        group.bench_with_input(BenchmarkId::new("oniai/jit", n), &haystack, |b, h| {
            b.iter(|| o.find_iter(black_box(h)).count())
        });
        #[cfg(feature = "jit")]
        group.bench_with_input(BenchmarkId::new("oniai/interp", n), &haystack, |b, h| {
            b.iter(|| o.find_iter_interp(black_box(h)).count())
        });
        group.bench_with_input(BenchmarkId::new("regex", n), &haystack, |b, h| {
            b.iter(|| s.find_iter(black_box(h)).count())
        });
        group.bench_with_input(BenchmarkId::new("fancy-regex", n), &haystack, |b, h| {
            b.iter(|| f.find_iter(black_box(h)).map(|r| r.unwrap()).count())
        });
        group.bench_with_input(BenchmarkId::new("pcre2", n), &haystack, |b, h| {
            b.iter(|| {
                p.find_iter(black_box(h.as_bytes()))
                    .map(|r| r.unwrap())
                    .count()
            })
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Real-world text: "A Study in Scarlet"
// ---------------------------------------------------------------------------

fn bench_real_world(c: &mut Criterion) {
    let text = include_str!("fixtures/stud.txt");

    macro_rules! rw {
        ($c:expr, $name:expr, $pat:expr, $iter:expr) => {{
            let o = Regex::new($pat).unwrap();
            let s = regex::Regex::new($pat).unwrap();
            let f = fancy_regex::Regex::new($pat).unwrap();
            let p = pcre2::bytes::Regex::new($pat).unwrap();
            let mut g = $c.benchmark_group(concat!("real_world/", $name));
            if $iter {
                bench_all!(
                    g,
                    o.find_iter(black_box(text)).count(),
                    o.find_iter_interp(black_box(text)).count(),
                    s.find_iter(black_box(text)).count(),
                    f.find_iter(black_box(text)).map(|r| r.unwrap()).count(),
                    p.find_iter(black_box(text.as_bytes()))
                        .map(|r| r.unwrap())
                        .count()
                );
            } else {
                bench_all!(
                    g,
                    o.find(black_box(text)),
                    o.find_iter_interp(black_box(text)).next(),
                    s.find(black_box(text)),
                    f.find(black_box(text)).unwrap(),
                    p.find(black_box(text.as_bytes())).unwrap()
                );
            }
            g.finish();
        }};
    }

    rw!(c, "literal_count", "Holmes", true);
    rw!(c, "capitalized_words", r"[A-Z][a-z]+", true);
    rw!(c, "posix_digits", r"[[:digit:]]+", true);
    rw!(c, "quoted_strings", "\"[^\"]*\"", true);
    rw!(c, "title_name", r"Mrs?\. [A-Z][a-z]+", true);
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
    bench_lookahead,
    bench_lookbehind,
    bench_backreference,
    bench_atomic_group,
    bench_find_iter_scale,
    bench_pathological,
    bench_pathological_iter,
    bench_real_world,
);
criterion_main!(benches);
