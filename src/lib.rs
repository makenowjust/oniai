//! Oniai — Onigmo-compatible regular expression engine.
//!
//! # Example
//! ```
//! use oniai::Regex;
//!
//! let re = Regex::new(r"(\w+)\s+(\w+)").unwrap();
//! let caps = re.captures("hello world").unwrap();
//! assert_eq!(caps.get(1).unwrap().as_str(), "hello");
//! assert_eq!(caps.get(2).unwrap().as_str(), "world");
//! ```

mod ast;
mod bytetrie;
mod casefold;
mod casefold_trie;
mod charset;
mod compile;
mod data;
mod error;
mod general_category;
#[cfg(feature = "jit")]
mod jit;
mod parser;
mod vm;

pub use error::Error;

use std::ops::Range;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A compiled regular expression.
pub struct Regex {
    inner: vm::CompiledRegex,
}

impl Regex {
    /// Compile a pattern with default (Ruby) syntax.
    pub fn new(pattern: &str) -> Result<Self, Error> {
        let inner = vm::CompiledRegex::new(pattern, Default::default())?;
        Ok(Regex { inner })
    }

    /// Returns true if the pattern matches anywhere in `text`.
    pub fn is_match(&self, text: &str) -> bool {
        self.inner.find(text, 0).is_some()
    }

    /// Returns the leftmost match in `text`, or `None`.
    pub fn find<'t>(&self, text: &'t str) -> Option<Match<'t>> {
        self.inner
            .find(text, 0)
            .map(|(start, end, _)| Match { text, start, end })
    }

    /// Like [`find`], but forces the interpreter path even when JIT is compiled in.
    /// Available in test, fuzzing, and JIT-enabled builds; used for differential testing.
    #[cfg(any(test, fuzzing, feature = "jit"))]
    pub fn find_interp<'t>(&self, text: &'t str) -> Option<Match<'t>> {
        self.inner
            .find_interp(text, 0)
            .map(|(start, end, _)| Match { text, start, end })
    }

    /// Returns an iterator over all non-overlapping matches.
    pub fn find_iter<'r, 't>(&'r self, text: &'t str) -> FindIter<'r, 't> {
        FindIter {
            re: self,
            text,
            pos: 0,
            #[cfg(feature = "jit")]
            scratch: vm::ExecScratch::new(),
            #[cfg(not(feature = "jit"))]
            scratch: vm::ExecScratch,
        }
    }

    /// Returns the leftmost match with capture groups, or `None`.
    pub fn captures<'t>(&self, text: &'t str) -> Option<Captures<'t>> {
        let (start, end, caps) = self.inner.find(text, 0)?;
        Some(Captures {
            text,
            match_start: start,
            match_end: end,
            slots: caps,
            named: self.inner.named_groups.clone(),
        })
    }

    /// Returns an iterator over all non-overlapping capture matches.
    pub fn captures_iter<'r, 't>(&'r self, text: &'t str) -> CapturesIter<'r, 't> {
        CapturesIter {
            re: self,
            text,
            pos: 0,
            #[cfg(feature = "jit")]
            scratch: vm::ExecScratch::new(),
            #[cfg(not(feature = "jit"))]
            scratch: vm::ExecScratch,
        }
    }
    /// Returns an iterator over interpreter-path matches.
    /// Not part of the public API; exposed only for JIT vs interpreter benchmarking.
    #[cfg(feature = "jit")]
    #[doc(hidden)]
    pub fn find_iter_interp<'r, 't>(&'r self, text: &'t str) -> FindIterInterp<'r, 't> {
        FindIterInterp {
            re: self,
            text,
            pos: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Match
// ---------------------------------------------------------------------------

/// A match in a string, identified by byte offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match<'t> {
    text: &'t str,
    start: usize,
    end: usize,
}

impl<'t> Match<'t> {
    /// The start byte offset of the match.
    pub fn start(&self) -> usize {
        self.start
    }
    /// The end byte offset (exclusive) of the match.
    pub fn end(&self) -> usize {
        self.end
    }
    /// The byte range of the match.
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }
    /// The matched string slice.
    pub fn as_str(&self) -> &'t str {
        &self.text[self.start..self.end]
    }
}

// ---------------------------------------------------------------------------
// Captures
// ---------------------------------------------------------------------------

/// Capture groups from a match.
pub struct Captures<'t> {
    text: &'t str,
    match_start: usize,
    match_end: usize,
    /// Flat list: slots[2*n] = start of group n+1, slots[2*n+1] = end
    /// Group 0 (whole match) uses match_start/match_end.
    slots: Vec<Option<usize>>,
    named: Vec<(String, usize)>,
}

impl<'t> Captures<'t> {
    /// Returns capture group `i` (0 = whole match, 1.. = groups).
    pub fn get(&self, i: usize) -> Option<Match<'t>> {
        if i == 0 {
            return Some(Match {
                text: self.text,
                start: self.match_start,
                end: self.match_end,
            });
        }
        let slot = i - 1;
        let start = *self.slots.get(slot * 2)?.as_ref()?;
        let end = *self.slots.get(slot * 2 + 1)?.as_ref()?;
        Some(Match {
            text: self.text,
            start,
            end,
        })
    }

    /// Returns a named capture group.
    pub fn name(&self, name: &str) -> Option<Match<'t>> {
        let &(_, idx) = self.named.iter().rev().find(|(n, _)| n == name)?;
        self.get(idx)
    }

    /// Number of capture groups (not counting the whole match).
    pub fn len(&self) -> usize {
        self.slots.len() / 2
    }

    /// Returns true if there are no capture groups.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// Iterators
// ---------------------------------------------------------------------------

pub struct FindIter<'r, 't> {
    re: &'r Regex,
    text: &'t str,
    pos: usize,
    scratch: vm::ExecScratch,
}

impl<'r, 't> Iterator for FindIter<'r, 't> {
    type Item = Match<'t>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos > self.text.len() {
            return None;
        }
        let (start, end, _) =
            self.re
                .inner
                .find_with_scratch(self.text, self.pos, &mut self.scratch)?;
        // advance past zero-length matches
        self.pos = if end > start {
            end
        } else {
            end + next_char_len(self.text, end)
        };
        Some(Match {
            text: self.text,
            start,
            end,
        })
    }
}

pub struct CapturesIter<'r, 't> {
    re: &'r Regex,
    text: &'t str,
    pos: usize,
    scratch: vm::ExecScratch,
}

impl<'r, 't> Iterator for CapturesIter<'r, 't> {
    type Item = Captures<'t>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos > self.text.len() {
            return None;
        }
        let (start, end, caps) =
            self.re
                .inner
                .find_with_scratch(self.text, self.pos, &mut self.scratch)?;
        self.pos = if end > start {
            end
        } else {
            end + next_char_len(self.text, end)
        };
        Some(Captures {
            text: self.text,
            match_start: start,
            match_end: end,
            slots: caps,
            named: self.re.inner.named_groups.clone(),
        })
    }
}

fn next_char_len(s: &str, pos: usize) -> usize {
    s[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Iterator over interpreter-path matches; mirrors [`FindIter`] but bypasses JIT.
/// Not part of the public API; exposed only for JIT vs interpreter benchmarking.
#[cfg(feature = "jit")]
#[doc(hidden)]
pub struct FindIterInterp<'r, 't> {
    re: &'r Regex,
    text: &'t str,
    pos: usize,
}

#[cfg(feature = "jit")]
impl<'r, 't> Iterator for FindIterInterp<'r, 't> {
    type Item = Match<'t>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.pos > self.text.len() {
            return None;
        }
        let (start, end, _) = self.re.inner.find_interp(self.text, self.pos)?;
        self.pos = if end > start {
            end
        } else {
            end + next_char_len(self.text, end)
        };
        Some(Match {
            text: self.text,
            start,
            end,
        })
    }
}
