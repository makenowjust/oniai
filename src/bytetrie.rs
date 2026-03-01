/// Immutable trie over raw UTF-8 byte sequences.
///
/// Used to pre-expand case-fold equivalences at compile time so that matching
/// reduces to a plain byte-walk at match time with no `case_fold()` calls and
/// no UTF-8 decoding.
///
/// Nodes are stored in a flat `Vec<TrieNode>` (index 0 is the root).  Each
/// node holds a sorted list of `(byte_value, child_node_index)` transitions for
/// binary-search dispatch, and an `is_accept` flag marking positions where a
/// complete byte sequence ends.
///
/// Matching is greedy: `advance` / `advance_back` consume as many bytes as
/// possible and return the **longest** accepted end-position.

// ---------------------------------------------------------------------------
// Data structure
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct TrieNode {
    /// Sorted by byte value for binary-search dispatch.
    pub transitions: Vec<(u8, u32)>,
    /// True when a complete byte sequence terminates at this node.
    pub is_accept: bool,
}

impl TrieNode {
    fn child(&self, byte: u8) -> Option<u32> {
        self.transitions
            .binary_search_by_key(&byte, |&(b, _)| b)
            .ok()
            .map(|i| self.transitions[i].1)
    }
}

#[derive(Debug, Clone)]
pub struct ByteTrie {
    pub nodes: Vec<TrieNode>,
}

impl ByteTrie {
    /// Create an empty trie (just the root node).
    pub fn new() -> Self {
        ByteTrie {
            nodes: vec![TrieNode::default()],
        }
    }

    /// Insert a byte sequence into the trie.
    pub fn insert(&mut self, bytes: &[u8]) {
        let mut node = 0u32;
        for &b in bytes {
            let child = self.nodes[node as usize].child(b);
            node = match child {
                Some(c) => c,
                None => {
                    let new_id = self.nodes.len() as u32;
                    self.nodes.push(TrieNode::default());
                    let pos = self.nodes[node as usize]
                        .transitions
                        .partition_point(|&(existing, _)| existing < b);
                    self.nodes[node as usize]
                        .transitions
                        .insert(pos, (b, new_id));
                    new_id
                }
            };
        }
        self.nodes[node as usize].is_accept = true;
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        !self.nodes[0].is_accept && self.nodes[0].transitions.is_empty()
    }

    /// Returns the set of bytes that can appear as the **first** byte of any
    /// accepted sequence (i.e., the non-empty transition bytes from the root).
    #[allow(dead_code)]
    pub fn first_bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.nodes[0].transitions.iter().map(|&(b, _)| b)
    }

    // -----------------------------------------------------------------------
    // Forward matching
    // -----------------------------------------------------------------------

    /// Walk the trie forward over `text[pos..]`.
    ///
    /// Returns the byte position **after** the longest accepted prefix, or
    /// `None` if no accepted sequence starts at `pos`.
    pub fn advance(&self, text: &[u8], pos: usize) -> Option<usize> {
        let mut node = 0u32;
        let mut last_accept: Option<usize> = None;
        // Root may itself be an accept (empty sequence) — not expected in
        // practice for regex matching, but handle it for correctness.
        if self.nodes[0].is_accept {
            last_accept = Some(pos);
        }
        let mut cur = pos;
        while cur < text.len() {
            match self.nodes[node as usize].child(text[cur]) {
                None => break,
                Some(child) => {
                    cur += 1;
                    node = child;
                    if self.nodes[node as usize].is_accept {
                        last_accept = Some(cur);
                    }
                }
            }
        }
        last_accept
    }

    // -----------------------------------------------------------------------
    // Backward matching (for FoldSeqBack / look-behind)
    // -----------------------------------------------------------------------

    /// Walk the trie **backward** over `text[..pos]`.
    ///
    /// Builds a reversed-bytes trie internally: inserts the reverse of every
    /// accepted sequence.  This method takes a pre-built *reversed* trie and
    /// walks it right-to-left over the text.
    ///
    /// Returns the byte position **before** the longest accepted suffix ending
    /// at `pos`, or `None` if no accepted sequence ends there.
    pub fn advance_back(&self, text: &[u8], pos: usize) -> Option<usize> {
        let mut node = 0u32;
        let mut last_accept: Option<usize> = None;
        if self.nodes[0].is_accept {
            last_accept = Some(pos);
        }
        let mut cur = pos;
        while cur > 0 {
            let b = text[cur - 1];
            match self.nodes[node as usize].child(b) {
                None => break,
                Some(child) => {
                    cur -= 1;
                    node = child;
                    if self.nodes[node as usize].is_accept {
                        last_accept = Some(cur);
                    }
                }
            }
        }
        last_accept
    }

    /// Build a reversed copy of this trie (for use with `advance_back`).
    pub fn reversed(&self) -> ByteTrie {
        // Collect all accepted sequences, reverse them, insert into new trie.
        let mut rev = ByteTrie::new();
        let mut buf = Vec::new();
        self.collect_sequences_into(0, &mut buf, &mut rev);
        rev
    }

    /// Collect all accepted byte sequences as UTF-8 strings.
    pub fn all_strings(&self) -> Vec<String> {
        let mut result = Vec::new();
        let mut buf = Vec::new();
        self.collect_strings(0, &mut buf, &mut result);
        result
    }

    fn collect_strings(&self, node: u32, buf: &mut Vec<u8>, out: &mut Vec<String>) {
        if self.nodes[node as usize].is_accept
            && let Ok(s) = std::str::from_utf8(buf)
        {
            out.push(s.to_owned());
        }
        for &(b, child) in &self.nodes[node as usize].transitions {
            buf.push(b);
            self.collect_strings(child, buf, out);
            buf.pop();
        }
    }

    /// Returns `true` if the string set is prefix-free: no accepted string is
    /// a proper prefix of another accepted string.  When prefix-free, the trie
    /// can be used for deterministic alternation (no backtracking needed).
    pub fn is_prefix_free(&self) -> bool {
        // A trie is prefix-free iff no accept node has any outgoing transitions.
        // If an accept node (string S) has a transition to child C, then the
        // string S is a proper prefix of the string reaching C, so S's accepted
        // path would be prefix of a longer accepted path.
        self.nodes
            .iter()
            .all(|n| !n.is_accept || n.transitions.is_empty())
    }

    fn collect_sequences_into(&self, node: u32, buf: &mut Vec<u8>, out: &mut ByteTrie) {
        if self.nodes[node as usize].is_accept {
            let rev: Vec<u8> = buf.iter().rev().copied().collect();
            out.insert(&rev);
        }
        for &(b, child) in &self.nodes[node as usize].transitions {
            buf.push(b);
            self.collect_sequences_into(child, buf, out);
            buf.pop();
        }
    }
}

impl Default for ByteTrie {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trie_empty() {
        let t = ByteTrie::new();
        assert!(t.is_empty());
        assert_eq!(t.advance(b"hello", 0), None);
    }

    #[test]
    fn trie_single_byte() {
        let mut t = ByteTrie::new();
        t.insert(b"a");
        assert_eq!(t.advance(b"abc", 0), Some(1));
        assert_eq!(t.advance(b"bc", 0), None);
    }

    #[test]
    fn trie_multi_byte() {
        let mut t = ByteTrie::new();
        t.insert(b"ab");
        t.insert(b"abc");
        // Greedy: returns longest match.
        assert_eq!(t.advance(b"abcd", 0), Some(3));
        assert_eq!(t.advance(b"ab", 0), Some(2));
        assert_eq!(t.advance(b"a", 0), None);
    }

    #[test]
    fn trie_multiple_sequences() {
        let mut t = ByteTrie::new();
        t.insert(b"s");
        t.insert(b"S");
        // U+017F ſ encodes as 0xC5 0xBF
        t.insert("\u{017F}".as_bytes());
        // U+212A K (Kelvin sign) is not in this trie; use 'k' instead.
        t.insert(b"k");

        assert_eq!(t.advance(b"s", 0), Some(1));
        assert_eq!(t.advance(b"S", 0), Some(1));
        // ſ is a 2-byte sequence
        let long_s = "\u{017F}".as_bytes();
        assert_eq!(t.advance(long_s, 0), Some(long_s.len()));
        assert_eq!(t.advance(b"k", 0), Some(1));
        assert_eq!(t.advance(b"x", 0), None);
    }

    #[test]
    fn trie_offset() {
        let mut t = ByteTrie::new();
        t.insert(b"s");
        t.insert(b"S");
        let text = b"xSy";
        assert_eq!(t.advance(text, 0), None);
        assert_eq!(t.advance(text, 1), Some(2));
    }

    #[test]
    fn trie_first_bytes() {
        let mut t = ByteTrie::new();
        t.insert(b"a");
        t.insert(b"A");
        t.insert("\u{017F}".as_bytes()); // starts with 0xC5
        let fb: Vec<u8> = t.first_bytes().collect();
        assert!(fb.contains(&b'a'));
        assert!(fb.contains(&b'A'));
        assert!(fb.contains(&0xC5));
    }

    #[test]
    fn trie_advance_back() {
        let mut fwd = ByteTrie::new();
        fwd.insert(b"ss");
        fwd.insert(b"SS");
        // ß = 0xC3 0x9F
        fwd.insert("\u{00DF}".as_bytes());

        let rev = fwd.reversed();

        // "xss" — match backward from pos 3 should give pos 1
        assert_eq!(rev.advance_back(b"xss", 3), Some(1));
        // "xSS"
        assert_eq!(rev.advance_back(b"xSS", 3), Some(1));
        // "xß" (x + 2 bytes)
        let text = "x\u{00DF}".to_string();
        assert_eq!(rev.advance_back(text.as_bytes(), text.len()), Some(1));
        // no match
        assert_eq!(rev.advance_back(b"xab", 3), None);
    }

    #[test]
    fn trie_reversed_single_byte() {
        let mut fwd = ByteTrie::new();
        fwd.insert(b"a");
        fwd.insert(b"b");
        let rev = fwd.reversed();
        assert_eq!(rev.advance_back(b"xa", 2), Some(1));
        assert_eq!(rev.advance_back(b"xb", 2), Some(1));
        assert_eq!(rev.advance_back(b"xc", 2), None);
    }
}
