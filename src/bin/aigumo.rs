//! `aigumo` — a grep-like search tool backed by the Aigumo regex engine.
//!
//! Usage: aigumo [OPTIONS] PATTERN [FILE...]

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process;

use clap::Parser;

use aigumo::Regex;

// ---------------------------------------------------------------------------
// CLI argument definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "aigumo",
    version,
    about = "Search for PATTERN in each FILE (or stdin).\n\
             PATTERN is an Onigmo-compatible regular expression.",
    override_usage = "aigumo [OPTIONS] PATTERN [FILE]..."
)]
struct Args {
    /// Regular expression to search for
    pattern: String,

    /// Files to search (reads from stdin if none given)
    files: Vec<String>,

    /// Ignore case distinctions in PATTERN
    #[arg(short = 'i', long)]
    ignore_case: bool,

    /// Select lines that do NOT match PATTERN
    #[arg(short = 'v', long)]
    invert_match: bool,

    /// Prefix each output line with its 1-based line number
    #[arg(short = 'n', long)]
    line_number: bool,

    /// Print only a count of matching lines per file
    #[arg(short = 'c', long)]
    count: bool,

    /// Print only the names of files with at least one match
    #[arg(short = 'l', long, visible_alias = "files-with-matches")]
    list_files: bool,

    /// Print only the matched (non-empty) portion of each matching line
    #[arg(short = 'o', long)]
    only_matching: bool,

    /// Search directories recursively
    #[arg(short = 'r', long)]
    recursive: bool,

    /// Colorize output: always, never, or auto (default)
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    color: ColorWhen,
}

#[derive(Clone, clap::ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

// ---------------------------------------------------------------------------
// ANSI colour helpers
// ---------------------------------------------------------------------------

struct Colors {
    filename: &'static str,
    line_num: &'static str,
    matched: &'static str,
    reset: &'static str,
    sep: &'static str,
}

const COLOR_ON: Colors = Colors {
    filename: "\x1b[35m",  // magenta
    line_num: "\x1b[32m",  // green
    matched: "\x1b[1;31m", // bold red
    reset: "\x1b[0m",
    sep: "\x1b[36m", // cyan
};

const COLOR_OFF: Colors = Colors {
    filename: "",
    line_num: "",
    matched: "",
    reset: "",
    sep: "",
};

fn use_color(when: &ColorWhen) -> bool {
    match when {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            // Use colour when stdout is a terminal.
            // We detect this with a simple isatty check via libc-free heuristic:
            // check if TERM is set and not "dumb", and that NO_COLOR is absent.
            std::env::var("NO_COLOR").is_err() && std::env::var("TERM").is_ok_and(|t| t != "dumb")
        }
    }
}

// ---------------------------------------------------------------------------
// Core search logic
// ---------------------------------------------------------------------------

struct Searcher<'a, W: Write> {
    re: &'a Regex,
    args: &'a Args,
    colors: &'a Colors,
    out: W,
    /// Whether to print a filename prefix on each line.
    show_filename: bool,
    /// Overall exit status: 0 = match found, 1 = no match, 2 = error.
    status: i32,
}

impl<W: Write> Searcher<'_, W> {
    fn search_file(&mut self, path: Option<&Path>, reader: impl BufRead) {
        let filename = path.map(|p| p.display().to_string());
        let c = self.colors;

        let mut match_count: u64 = 0;
        let mut found_any = false;

        for (lineno, line_result) in reader.lines().enumerate() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    let name = filename.as_deref().unwrap_or("<stdin>");
                    eprintln!("aigumo: {name}: {e}");
                    self.status = 2;
                    return;
                }
            };

            let has_match = self.re.find(&line).is_some();
            let selected = has_match ^ self.args.invert_match;

            if !selected {
                continue;
            }

            found_any = true;
            match_count += 1;

            // -l: just record that this file matched
            if self.args.list_files {
                continue;
            }
            // -c: accumulate count, print later
            if self.args.count {
                continue;
            }

            // Build prefix
            let prefix = self.build_prefix(filename.as_deref(), lineno + 1, c);

            if self.args.only_matching && !self.args.invert_match {
                // Print each match on its own line
                for m in self.re.find_iter(&line) {
                    let _ = writeln!(
                        self.out,
                        "{prefix}{}{}{reset}",
                        c.matched,
                        m.as_str(),
                        reset = c.reset
                    );
                }
            } else {
                // Print the whole line, highlighting matches
                let highlighted = if !self.args.invert_match && !c.matched.is_empty() {
                    highlight_matches(self.re, &line, c)
                } else {
                    line.clone()
                };
                let _ = writeln!(self.out, "{prefix}{highlighted}");
            }
        }

        // Post-line output
        if self.args.list_files {
            if found_any {
                let name = filename.as_deref().unwrap_or("(stdin)");
                let _ = writeln!(self.out, "{}{name}{}", c.filename, c.reset);
            }
        } else if self.args.count {
            let prefix = self.build_prefix(filename.as_deref(), 0, c);
            let _ = writeln!(self.out, "{prefix}{match_count}");
        }

        if found_any {
            self.status = self.status.min(0);
        }
    }

    fn build_prefix(&self, filename: Option<&str>, lineno: usize, c: &Colors) -> String {
        let mut out = String::new();
        if self.show_filename
            && let Some(name) = filename
        {
            out.push_str(c.filename);
            out.push_str(name);
            out.push_str(c.reset);
            out.push_str(c.sep);
            out.push(':');
            out.push_str(c.reset);
        }
        if self.args.line_number && lineno > 0 {
            out.push_str(c.line_num);
            out.push_str(&lineno.to_string());
            out.push_str(c.reset);
            out.push_str(c.sep);
            out.push(':');
            out.push_str(c.reset);
        }
        out
    }
}

/// Return `line` with each regex match wrapped in colour escape sequences.
fn highlight_matches(re: &Regex, line: &str, c: &Colors) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    for m in re.find_iter(line) {
        let start = m.start();
        let end = m.end();
        out.push_str(&line[last..start]);
        out.push_str(c.matched);
        out.push_str(m.as_str());
        out.push_str(c.reset);
        last = end;
    }
    out.push_str(&line[last..]);
    out
}

// ---------------------------------------------------------------------------
// File / directory walking
// ---------------------------------------------------------------------------

fn collect_paths(args: &Args) -> Vec<PathBuf> {
    if args.files.is_empty() {
        return vec![];
    }
    let mut paths = Vec::new();
    for f in &args.files {
        let p = PathBuf::from(f);
        if p.is_dir() {
            if args.recursive {
                walk_dir(&p, &mut paths);
            } else {
                eprintln!("aigumo: {f}: Is a directory");
            }
        } else {
            paths.push(p);
        }
    }
    paths
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("aigumo: {}: {e}", dir.display());
            return;
        }
    };
    let mut children: Vec<_> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            walk_dir(&child, out);
        } else {
            out.push(child);
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let args = Args::parse();

    // Build pattern, injecting (?i) if requested.
    let pattern = if args.ignore_case {
        format!("(?i){}", args.pattern)
    } else {
        args.pattern.clone()
    };

    let re = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("aigumo: invalid pattern: {e}");
            process::exit(2);
        }
    };

    let colors = if use_color(&args.color) {
        &COLOR_ON
    } else {
        &COLOR_OFF
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let paths = collect_paths(&args);
    let show_filename = paths.len() > 1 || (!paths.is_empty() && args.files.len() > 1);

    // No matches yet → status 1.
    let mut status = 1i32;

    if paths.is_empty() {
        // Read from stdin
        let stdin = io::stdin();
        let reader = BufReader::new(stdin.lock());
        let mut searcher = Searcher {
            re: &re,
            args: &args,
            colors,
            out: &mut out,
            show_filename: false,
            status: 1,
        };
        searcher.search_file(None, reader);
        status = searcher.status;
    } else {
        for path in &paths {
            let file = match File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("aigumo: {}: {e}", path.display());
                    status = 2;
                    continue;
                }
            };
            let reader = BufReader::new(file);
            let mut searcher = Searcher {
                re: &re,
                args: &args,
                colors,
                out: &mut out,
                show_filename,
                status: 1,
            };
            searcher.search_file(Some(path), reader);
            if searcher.status < status {
                status = searcher.status;
            }
        }
    }

    let _ = out.flush();
    process::exit(status);
}
