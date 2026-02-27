/// Parser for Onigmo-compatible regular expressions.
///
/// Produces an AST (`Node`) from a pattern string.
use crate::ast::*;
use crate::error::Error;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn parse(pattern: &str) -> Result<(Node, Vec<(String, u32)>), Error> {
    let mut p = Parser::new(pattern, Flags::default());
    let node = p.parse_top()?;
    if !p.at_end() {
        return Err(Error::Parse(format!(
            "unexpected char {:?} at pos {}",
            p.peek(),
            p.pos
        )));
    }
    Ok((node, p.named))
}

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

struct Parser {
    chars: Vec<char>,
    pos: usize,
    group_count: u32,
    named: Vec<(String, u32)>, // (name, 1-based index)
}

impl Parser {
    fn new(pattern: &str, _flags: Flags) -> Self {
        Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            group_count: 0,
            named: Vec::new(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn eat(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn expect(&mut self, c: char) -> Result<(), Error> {
        match self.eat() {
            Some(x) if x == c => Ok(()),
            Some(x) => Err(Error::Parse(format!("expected {:?}, got {:?}", c, x))),
            None => Err(Error::Parse(format!(
                "expected {:?}, got end-of-pattern",
                c
            ))),
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn new_capture(&mut self) -> u32 {
        self.group_count += 1;
        self.group_count
    }

    // Skip whitespace and comments in extended mode
    fn skip_extended(&mut self, flags: &Flags) {
        if !flags.extended {
            return;
        }
        loop {
            match self.peek() {
                Some('#') => {
                    while let Some(c) = self.eat() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some(c) if c == ' ' || c == '\t' || c == '\n' || c == '\r' => {
                    self.eat();
                }
                _ => break,
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Top-level parse
    // ---------------------------------------------------------------------------

    fn parse_top(&mut self) -> Result<Node, Error> {
        self.parse_alternation(Flags::default())
    }

    fn parse_alternation(&mut self, flags: Flags) -> Result<Node, Error> {
        let mut alts = vec![self.parse_concat(flags)?];
        while self.peek() == Some('|') {
            self.pos += 1;
            alts.push(self.parse_concat(flags)?);
        }
        if alts.len() == 1 {
            Ok(alts.remove(0))
        } else {
            Ok(Node::Alternation(alts))
        }
    }

    fn parse_concat(&mut self, flags: Flags) -> Result<Node, Error> {
        let mut nodes = Vec::new();
        loop {
            self.skip_extended(&flags);
            match self.peek() {
                None | Some(')') | Some('|') => break,
                _ => {
                    let node = self.parse_quantified(flags)?;
                    // Check if this is an isolated inline flag (?i) that affects the rest
                    if let Node::InlineFlags {
                        flags: ref flag_mod,
                        node: ref inner,
                    } = node
                        && matches!(**inner, Node::Empty)
                    {
                        // Re-parse remaining atoms with updated flags
                        let new_flags = flags.apply_on(flag_mod);
                        let rest = self.parse_concat(new_flags)?;
                        // Wrap rest in InlineFlags so the compiler applies the new flags
                        if !matches!(rest, Node::Empty) {
                            nodes.push(Node::InlineFlags {
                                flags: flag_mod.clone(),
                                node: Box::new(rest),
                            });
                        }
                        break;
                    }
                    nodes.push(node);
                }
            }
        }
        match nodes.len() {
            0 => Ok(Node::Empty),
            1 => Ok(nodes.remove(0)),
            _ => Ok(Node::Concat(nodes)),
        }
    }

    fn parse_quantified(&mut self, flags: Flags) -> Result<Node, Error> {
        let atom = self.parse_atom(flags)?;
        // Skip whitespace before quantifier in extended mode
        // (not done in Onigmo, quantifier must follow immediately)
        self.parse_quant_suffix(atom)
    }

    fn parse_quant_suffix(&mut self, node: Node) -> Result<Node, Error> {
        let (range, kind) = match self.peek() {
            Some('?') => {
                self.pos += 1;
                (
                    QuantRange {
                        min: 0,
                        max: Some(1),
                    },
                    self.quant_kind(),
                )
            }
            Some('*') => {
                self.pos += 1;
                (QuantRange { min: 0, max: None }, self.quant_kind())
            }
            Some('+') => {
                self.pos += 1;
                (QuantRange { min: 1, max: None }, self.quant_kind())
            }
            Some('{') => {
                if let Some((range, consumed)) = self.try_parse_braces() {
                    self.pos += consumed;
                    let kind = self.quant_kind();
                    (range, kind)
                } else {
                    return Ok(node);
                }
            }
            _ => return Ok(node),
        };
        Ok(Node::Quantifier {
            node: Box::new(node),
            range,
            kind,
        })
    }

    fn quant_kind(&mut self) -> QuantKind {
        match self.peek() {
            Some('?') => {
                self.pos += 1;
                QuantKind::Reluctant
            }
            Some('+') => {
                self.pos += 1;
                QuantKind::Possessive
            }
            _ => QuantKind::Greedy,
        }
    }

    /// Returns `Some((range, chars_consumed))` for `{n}`, `{n,m}`, `{n,}`, `{,m}`.
    fn try_parse_braces(&self) -> Option<(QuantRange, usize)> {
        let slice = &self.chars[self.pos..];
        if slice.first() != Some(&'{') {
            return None;
        }
        let mut i = 1;

        let mut min_s = String::new();
        while i < slice.len() && slice[i].is_ascii_digit() {
            min_s.push(slice[i]);
            i += 1;
        }

        if i >= slice.len() {
            return None;
        }

        if slice[i] == '}' {
            if min_s.is_empty() {
                return None;
            } // `{}` is invalid
            let n: u32 = min_s.parse().ok()?;
            i += 1;
            return Some((
                QuantRange {
                    min: n,
                    max: Some(n),
                },
                i,
            ));
        }

        if slice[i] != ',' {
            return None;
        }
        i += 1;

        let mut max_s = String::new();
        while i < slice.len() && slice[i].is_ascii_digit() {
            max_s.push(slice[i]);
            i += 1;
        }

        if i >= slice.len() || slice[i] != '}' {
            return None;
        }
        i += 1;

        // {,} is invalid per doc
        if min_s.is_empty() && max_s.is_empty() {
            return None;
        }

        let min: u32 = if min_s.is_empty() {
            0
        } else {
            min_s.parse().ok()?
        };
        let max: Option<u32> = if max_s.is_empty() {
            None
        } else {
            Some(max_s.parse().ok()?)
        };
        Some((QuantRange { min, max }, i))
    }

    // ---------------------------------------------------------------------------
    // Atom parsing
    // ---------------------------------------------------------------------------

    fn parse_atom(&mut self, flags: Flags) -> Result<Node, Error> {
        self.skip_extended(&flags);
        match self.peek() {
            Some('.') => {
                self.pos += 1;
                Ok(Node::AnyChar)
            }
            Some('^') => {
                self.pos += 1;
                Ok(Node::Anchor(AnchorKind::Start))
            }
            Some('$') => {
                self.pos += 1;
                Ok(Node::Anchor(AnchorKind::End))
            }
            Some('[') => {
                self.pos += 1;
                self.parse_char_class()
            }
            Some('(') => self.parse_group(flags),
            Some('\\') => self.parse_escape(flags),
            Some(c) => {
                self.pos += 1;
                Ok(Node::Literal(c))
            }
            None => Err(Error::Parse("unexpected end of pattern".into())),
        }
    }

    // ---------------------------------------------------------------------------
    // Escape sequences
    // ---------------------------------------------------------------------------

    fn parse_escape(&mut self, _flags: Flags) -> Result<Node, Error> {
        self.pos += 1; // skip '\'
        match self.eat() {
            // Character escapes
            Some('t') => Ok(Node::Literal('\t')),
            Some('v') => Ok(Node::Literal('\x0B')),
            Some('n') => Ok(Node::Literal('\n')),
            Some('r') => Ok(Node::Literal('\r')),
            Some('b') => Ok(Node::Anchor(AnchorKind::WordBoundary)), // outside char class
            Some('f') => Ok(Node::Literal('\x0C')),
            Some('a') => Ok(Node::Literal('\x07')),
            Some('e') => Ok(Node::Literal('\x1B')),
            Some('K') => Ok(Node::Keep),

            // Anchors
            Some('A') => Ok(Node::Anchor(AnchorKind::StringStart)),
            Some('Z') => Ok(Node::Anchor(AnchorKind::StringEndOrNl)),
            Some('z') => Ok(Node::Anchor(AnchorKind::StringEnd)),
            Some('B') => Ok(Node::Anchor(AnchorKind::NonWordBoundary)),
            Some('G') => Ok(Node::Anchor(AnchorKind::SearchStart)),

            // Shorthands
            Some('w') => Ok(Node::Shorthand(Shorthand::Word)),
            Some('W') => Ok(Node::Shorthand(Shorthand::NonWord)),
            Some('d') => Ok(Node::Shorthand(Shorthand::Digit)),
            Some('D') => Ok(Node::Shorthand(Shorthand::NonDigit)),
            Some('s') => Ok(Node::Shorthand(Shorthand::Space)),
            Some('S') => Ok(Node::Shorthand(Shorthand::NonSpace)),
            Some('h') => Ok(Node::Shorthand(Shorthand::HexDigit)),
            Some('H') => Ok(Node::Shorthand(Shorthand::NonHexDigit)),

            // Hex
            Some('x') => {
                if self.peek() == Some('{') {
                    self.pos += 1;
                    let n = self.parse_hex_digits_max(8)?;
                    self.expect('}')?;
                    Ok(Node::Literal(char_from_u32(n)?))
                } else {
                    let n = self.parse_hex_digits_exact(2)?;
                    Ok(Node::Literal(char_from_u32(n)?))
                }
            }

            // Unicode \uHHHH
            Some('u') => {
                let n = self.parse_hex_digits_exact(4)?;
                Ok(Node::Literal(char_from_u32(n)?))
            }

            // Control chars \cX or \C-X
            Some('c') => {
                let c = self
                    .eat()
                    .ok_or_else(|| Error::Parse("incomplete \\c".into()))?;
                Ok(Node::Literal(ctrl_char(c)))
            }
            Some('C') => {
                self.expect('-')?;
                let c = self
                    .eat()
                    .ok_or_else(|| Error::Parse("incomplete \\C-".into()))?;
                Ok(Node::Literal(ctrl_char(c)))
            }
            Some('M') => {
                self.expect('-')?;
                let next = self
                    .eat()
                    .ok_or_else(|| Error::Parse("incomplete \\M-".into()))?;
                if next == '\\' {
                    self.expect('C')?;
                    self.expect('-')?;
                    let c = self
                        .eat()
                        .ok_or_else(|| Error::Parse("incomplete \\M-\\C-".into()))?;
                    Ok(Node::Literal(
                        char::from_u32(ctrl_char(c) as u32 | 0x80).unwrap_or('\u{FFFD}'),
                    ))
                } else {
                    Ok(Node::Literal(
                        char::from_u32(next as u32 | 0x80).unwrap_or('\u{FFFD}'),
                    ))
                }
            }

            // Octal (1-3 digits starting with a digit)
            Some(c) if c.is_ascii_digit() => {
                let mut s = String::new();
                s.push(c);
                // Check if it could be a backreference (single non-zero digit)
                // Convention: if it's one digit (1-9) and looks like a backreference,
                // treat it as backreference. But octal has precedence for 0xx.
                // For simplicity: 1-3 octal digits for \0xx; single digit 1-9 = backref
                if c == '0' {
                    while s.len() < 3 {
                        match self.peek() {
                            Some(d) if d.is_ascii_digit() => {
                                s.push(d);
                                self.pos += 1;
                            }
                            _ => break,
                        }
                    }
                    let n = u32::from_str_radix(&s, 8)
                        .map_err(|_| Error::Parse(format!("invalid octal {:?}", s)))?;
                    Ok(Node::Literal(char_from_u32(n)?))
                } else {
                    // Could be \1-\9 (backref) or \123 (octal)
                    // We try to read more octal digits
                    while s.len() < 3 {
                        match self.peek() {
                            Some(d) if d.is_ascii_digit() && d < '8' => {
                                s.push(d);
                                self.pos += 1;
                            }
                            _ => break,
                        }
                    }
                    if s.len() == 1 {
                        // single digit 1-9: backreference
                        let n: u32 = s.parse().unwrap();
                        Ok(Node::BackRef {
                            target: GroupRef::Index(n),
                            level: None,
                        })
                    } else {
                        // multi-digit octal
                        let n = u32::from_str_radix(&s, 8)
                            .map_err(|_| Error::Parse(format!("invalid octal {:?}", s)))?;
                        Ok(Node::Literal(char_from_u32(n)?))
                    }
                }
            }

            // Subexpression call \g<...> or \g'...'
            Some('g') => self.parse_subexpr_call(),

            // Backreference \k<...> or \k'...'
            Some('k') => self.parse_backref_named(),

            // Unicode property \p{...} or \P{...}
            Some('p') => self.parse_unicode_prop(false),
            Some('P') => self.parse_unicode_prop(true),

            // \R linebreak (simplified)
            Some('R') => {
                // (?>\x0D\x0A|[\x0A-\x0D]) simplified as any line break
                Ok(Node::Shorthand(Shorthand::Space)) // approximate; improve if needed
            }

            // Escaped literals
            Some(c) => Ok(Node::Literal(c)),
            None => Err(Error::Parse("trailing backslash".into())),
        }
    }

    fn parse_hex_digits_exact(&mut self, count: usize) -> Result<u32, Error> {
        let mut s = String::new();
        for _ in 0..count {
            match self.eat() {
                Some(c) if c.is_ascii_hexdigit() => s.push(c),
                Some(c) => return Err(Error::Parse(format!("expected hex digit, got {:?}", c))),
                None => return Err(Error::Parse("unexpected end in hex escape".into())),
            }
        }
        u32::from_str_radix(&s, 16).map_err(|_| Error::Parse(format!("invalid hex {:?}", s)))
    }

    fn parse_hex_digits_max(&mut self, max: usize) -> Result<u32, Error> {
        let mut s = String::new();
        for _ in 0..max {
            match self.peek() {
                Some(c) if c.is_ascii_hexdigit() => {
                    s.push(c);
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if s.is_empty() {
            return Err(Error::Parse("expected hex digits".into()));
        }
        u32::from_str_radix(&s, 16).map_err(|_| Error::Parse(format!("invalid hex {:?}", s)))
    }

    fn parse_subexpr_call(&mut self) -> Result<Node, Error> {
        let (open, close) = match self.eat() {
            Some('<') => ('<', '>'),
            Some('\'') => ('\'', '\''),
            Some(c) => {
                return Err(Error::Parse(format!(
                    "expected < or ' after \\g, got {:?}",
                    c
                )));
            }
            None => return Err(Error::Parse("unexpected end after \\g".into())),
        };
        let _ = open;
        let s = self.read_until(close)?;
        let target = parse_group_ref(&s)?;
        Ok(Node::SubexpCall(target))
    }

    fn parse_backref_named(&mut self) -> Result<Node, Error> {
        let (_, close) = match self.peek() {
            Some('<') => {
                self.pos += 1;
                ('<', '>')
            }
            Some('\'') => {
                self.pos += 1;
                ('\'', '\'')
            }
            Some(c) => {
                return Err(Error::Parse(format!(
                    "expected < or ' after \\k, got {:?}",
                    c
                )));
            }
            None => return Err(Error::Parse("unexpected end after \\k".into())),
        };
        // Read name, possibly with level like name+0 or name-1
        let content = self.read_until(close)?;
        let (target, level) = parse_backref_target(&content)?;
        Ok(Node::BackRef { target, level })
    }

    fn parse_unicode_prop(&mut self, negate: bool) -> Result<Node, Error> {
        self.expect('{')?;
        let name = self.read_until('}')?;
        // Handle \p{^name} (negative inside braces)
        let (name, negate) = if let Some(stripped) = name.strip_prefix('^') {
            (stripped.to_string(), !negate)
        } else {
            (name, negate)
        };
        Ok(Node::UnicodeProp { name, negate })
    }

    fn read_until(&mut self, close: char) -> Result<String, Error> {
        let mut s = String::new();
        loop {
            match self.eat() {
                Some(c) if c == close => return Ok(s),
                Some(c) => s.push(c),
                None => {
                    return Err(Error::Parse(format!(
                        "expected {:?}, got end-of-pattern",
                        close
                    )));
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Groups
    // ---------------------------------------------------------------------------

    fn parse_group(&mut self, flags: Flags) -> Result<Node, Error> {
        self.pos += 1; // consume '('
        if self.peek() == Some('?') {
            self.pos += 1;
            self.parse_group_ext(flags)
        } else {
            // Plain capturing group
            let idx = self.new_capture();
            let inner = self.parse_alternation(flags)?;
            self.expect(')')?;
            Ok(Node::Capture {
                index: idx,
                node: Box::new(inner),
                flags,
            })
        }
    }

    fn parse_group_ext(&mut self, flags: Flags) -> Result<Node, Error> {
        match self.peek() {
            // (?#...) comment
            Some('#') => {
                self.pos += 1;
                while let Some(c) = self.eat() {
                    if c == ')' {
                        return Ok(Node::Empty);
                    }
                }
                Err(Error::Parse("unclosed comment group".into()))
            }

            // (?:...) non-capturing
            Some(':') => {
                self.pos += 1;
                let inner = self.parse_alternation(flags)?;
                self.expect(')')?;
                Ok(Node::Group {
                    node: Box::new(inner),
                    flags,
                })
            }

            // (?>...) atomic
            Some('>') => {
                self.pos += 1;
                let inner = self.parse_alternation(flags)?;
                self.expect(')')?;
                Ok(Node::Atomic(Box::new(inner)))
            }

            // (?~...) absence operator
            Some('~') => {
                self.pos += 1;
                let inner = self.parse_alternation(flags)?;
                self.expect(')')?;
                Ok(Node::Absence(Box::new(inner)))
            }

            // (?=...) positive lookahead
            Some('=') => {
                self.pos += 1;
                let inner = self.parse_alternation(flags)?;
                self.expect(')')?;
                Ok(Node::LookAround {
                    dir: LookDir::Ahead,
                    pol: LookPol::Positive,
                    node: Box::new(inner),
                })
            }

            // (?!...) negative lookahead
            Some('!') => {
                self.pos += 1;
                let inner = self.parse_alternation(flags)?;
                self.expect(')')?;
                Ok(Node::LookAround {
                    dir: LookDir::Ahead,
                    pol: LookPol::Negative,
                    node: Box::new(inner),
                })
            }

            // (?<...) — named group or lookbehind
            Some('<') => {
                self.pos += 1;
                match self.peek() {
                    Some('=') => {
                        self.pos += 1;
                        let inner = self.parse_alternation(flags)?;
                        self.expect(')')?;
                        Ok(Node::LookAround {
                            dir: LookDir::Behind,
                            pol: LookPol::Positive,
                            node: Box::new(inner),
                        })
                    }
                    Some('!') => {
                        self.pos += 1;
                        let inner = self.parse_alternation(flags)?;
                        self.expect(')')?;
                        Ok(Node::LookAround {
                            dir: LookDir::Behind,
                            pol: LookPol::Negative,
                            node: Box::new(inner),
                        })
                    }
                    _ => {
                        // Named group (?<name>...)
                        let name = self.read_until('>')?;
                        let idx = self.new_capture();
                        self.named.push((name.clone(), idx));
                        let inner = self.parse_alternation(flags)?;
                        self.expect(')')?;
                        Ok(Node::NamedCapture {
                            name,
                            index: idx,
                            node: Box::new(inner),
                            flags,
                        })
                    }
                }
            }

            // (?'name'...) named group alt syntax
            Some('\'') => {
                self.pos += 1;
                let name = self.read_until('\'')?;
                let idx = self.new_capture();
                self.named.push((name.clone(), idx));
                let inner = self.parse_alternation(flags)?;
                self.expect(')')?;
                Ok(Node::NamedCapture {
                    name,
                    index: idx,
                    node: Box::new(inner),
                    flags,
                })
            }

            // (?(cond)yes|no) conditional
            Some('(') => {
                self.pos += 1;
                self.parse_conditional(flags)
            }

            // (?imxdau-imx) or (?imxdau-imx:subexp) flag groups
            Some(c) if is_flag_char(c) || c == '-' => self.parse_flag_group(flags),

            Some(c) => Err(Error::Parse(format!("unknown group type (?{:?}", c))),
            None => Err(Error::Parse("unexpected end in group".into())),
        }
    }

    fn parse_flag_group(&mut self, outer_flags: Flags) -> Result<Node, Error> {
        let flag_mod = self.parse_flag_mod()?;
        let new_flags = outer_flags.apply_on(&flag_mod);

        match self.peek() {
            Some(')') => {
                // Inline flag to end of current group
                self.pos += 1;
                // The effect: the rest of the enclosing group uses new_flags
                // Represent as InlineFlags wrapping the remainder
                // Actually in Onigmo, (?imx) without subexp spans to end of current group context.
                // We represent this by wrapping subsequent atoms.
                // But we've already parsed the group opening. The simplest representation:
                // Emit an InlineFlags node with an empty body — the compiler handles the
                // "rest of current group" by having the parser re-enter with new flags.
                // This is tricky. Instead, we wrap an inner parse with new flags.
                // To handle this correctly, we need to parse the remaining atoms at the
                // caller level. We'll signal this by returning a special node.
                Ok(Node::InlineFlags {
                    flags: flag_mod,
                    node: Box::new(Node::Empty),
                })
            }
            Some(':') => {
                self.pos += 1;
                let inner = self.parse_alternation(new_flags)?;
                self.expect(')')?;
                Ok(Node::InlineFlags {
                    flags: flag_mod,
                    node: Box::new(inner),
                })
            }
            Some(c) => Err(Error::Parse(format!("unexpected {:?} after flags", c))),
            None => Err(Error::Parse("unexpected end after flags".into())),
        }
    }

    fn parse_flag_mod(&mut self) -> Result<FlagMod, Error> {
        let mut on = Flags::default();
        let mut off = Flags::default();
        let mut negating = false;
        loop {
            match self.peek() {
                Some('i') => {
                    self.pos += 1;
                    if negating {
                        off.ignore_case = true;
                    } else {
                        on.ignore_case = true;
                    }
                }
                Some('m') => {
                    self.pos += 1;
                    if negating {
                        off.multiline = true;
                    } else {
                        on.multiline = true;
                    }
                }
                Some('x') => {
                    self.pos += 1;
                    if negating {
                        off.extended = true;
                    } else {
                        on.extended = true;
                    }
                }
                Some('d') | Some('u') => {
                    self.pos += 1;
                } // charset options (ignore for now)
                Some('a') => {
                    self.pos += 1;
                    if negating {
                        off.ascii_range = true;
                    } else {
                        on.ascii_range = true;
                    }
                }
                Some('-') => {
                    self.pos += 1;
                    negating = true;
                }
                _ => break,
            }
        }
        Ok(FlagMod { on, off })
    }

    fn parse_conditional(&mut self, flags: Flags) -> Result<Node, Error> {
        // We already consumed '(' after '(?('
        // Content is either a number or a name
        let cond = self.parse_condition()?;
        self.expect(')')?;

        let yes = self.parse_concat(flags)?;
        let no = if self.peek() == Some('|') {
            self.pos += 1;
            self.parse_concat(flags)?
        } else {
            Node::Empty
        };
        self.expect(')')?;
        Ok(Node::Conditional {
            cond,
            yes: Box::new(yes),
            no: Box::new(no),
        })
    }

    fn parse_condition(&mut self) -> Result<Condition, Error> {
        // number or <name> or 'name'
        match self.peek() {
            Some('<') => {
                self.pos += 1;
                let name = self.read_until('>')?;
                Ok(Condition::GroupName(name))
            }
            Some('\'') => {
                self.pos += 1;
                let name = self.read_until('\'')?;
                Ok(Condition::GroupName(name))
            }
            Some(c) if c.is_ascii_digit() => {
                let mut s = String::new();
                while let Some(d) = self.peek() {
                    if d.is_ascii_digit() {
                        s.push(d);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let n: u32 = s
                    .parse()
                    .map_err(|_| Error::Parse("invalid condition number".into()))?;
                Ok(Condition::GroupNum(n))
            }
            Some(_c) => {
                // Named condition without angle brackets (e.g., `(?(name)`)
                let mut s = String::new();
                while let Some(c) = self.peek() {
                    if c == ')' {
                        break;
                    }
                    s.push(c);
                    self.pos += 1;
                }
                Ok(Condition::GroupName(s))
            }
            None => Err(Error::Parse("unexpected end in condition".into())),
        }
    }

    // ---------------------------------------------------------------------------
    // Character class [...]
    // ---------------------------------------------------------------------------

    fn parse_char_class(&mut self) -> Result<Node, Error> {
        let negate = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut items: Vec<ClassItem> = Vec::new();
        let mut intersections: Vec<CharClass> = Vec::new();

        // First char can be ']' (literal)
        let mut first = true;
        loop {
            match self.peek() {
                Some(']') if !first => {
                    self.pos += 1;
                    break;
                }
                None => return Err(Error::Parse("unclosed character class".into())),
                Some('&') if self.peek_at(1) == Some('&') => {
                    // Intersection
                    self.pos += 2;
                    // Parse RHS as a nested char class expression (until the outer `]`)
                    // The RHS can be a [...] or a sequence of items
                    if self.peek() == Some('[') {
                        self.pos += 1;
                        let inner = self.parse_char_class_inner()?;
                        intersections.push(inner);
                    } else {
                        // Parse remaining items as an intersection set
                        let inner = self.parse_char_class_inner_until_end()?;
                        intersections.push(inner);
                        // The ']' was consumed by inner
                        let cc = CharClass {
                            negate,
                            items: std::mem::take(&mut items),
                            intersections: Vec::new(),
                        };
                        // Build intersection result
                        let base_cc = cc;
                        let result = build_intersection(base_cc, intersections);
                        return Ok(Node::CharClass(result));
                    }
                }
                Some('[') => {
                    if self.peek_at(1) == Some(':') {
                        // POSIX bracket
                        let item = self.parse_class_item(first)?;
                        items.push(item);
                    } else {
                        self.pos += 1;
                        let inner = self.parse_char_class_inner()?;
                        items.push(ClassItem::Nested(inner));
                    }
                    first = false;
                }
                _ => {
                    let item = self.parse_class_item(first)?;
                    items.push(item);
                    first = false;
                }
            }
        }

        let base = CharClass {
            negate,
            items,
            intersections: Vec::new(),
        };
        let result = build_intersection(base, intersections);
        Ok(Node::CharClass(result))
    }

    fn parse_char_class_inner(&mut self) -> Result<CharClass, Error> {
        let negate = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        let mut first = true;
        loop {
            match self.peek() {
                Some(']') if !first => {
                    self.pos += 1;
                    break;
                }
                None => return Err(Error::Parse("unclosed nested class".into())),
                Some('[') => {
                    if self.peek_at(1) == Some(':') {
                        // POSIX bracket
                        let item = self.parse_class_item(first)?;
                        items.push(item);
                    } else {
                        self.pos += 1;
                        let inner = self.parse_char_class_inner()?;
                        items.push(ClassItem::Nested(inner));
                    }
                    first = false;
                }
                _ => {
                    items.push(self.parse_class_item(first)?);
                    first = false;
                }
            }
        }
        Ok(CharClass {
            negate,
            items,
            intersections: Vec::new(),
        })
    }

    fn parse_char_class_inner_until_end(&mut self) -> Result<CharClass, Error> {
        // Parse items until the final ']' of the outer class
        let mut items = Vec::new();
        loop {
            match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(Error::Parse("unclosed class intersection".into())),
                _ => items.push(self.parse_class_item(false)?),
            }
        }
        Ok(CharClass {
            negate: false,
            items,
            intersections: Vec::new(),
        })
    }

    fn parse_class_item(&mut self, _first: bool) -> Result<ClassItem, Error> {
        let start = self.parse_class_char()?;

        // Check for range `x-y`
        if self.peek() == Some('-') && self.peek_at(1) != Some(']') && self.peek_at(1).is_some() {
            // Peek ahead: is it a range?
            let saved = self.pos;
            self.pos += 1; // consume '-'
            match self.peek() {
                // Could be the end '-' before ']' — not a range
                Some(']') => {
                    self.pos -= 1;
                    return Ok(start);
                }
                Some(_) => {
                    let end = self.parse_class_char()?;
                    if let (ClassItem::Char(lo), ClassItem::Char(hi)) = (&start, &end) {
                        return Ok(ClassItem::Range(*lo, *hi));
                    } else {
                        // Not chars (e.g., shorthands) — not a valid range; rewind
                        self.pos = saved;
                        return Ok(start);
                    }
                }
                None => {
                    self.pos -= 1;
                    return Ok(start);
                }
            }
        }
        Ok(start)
    }

    fn parse_class_char(&mut self) -> Result<ClassItem, Error> {
        match self.peek() {
            Some('\\') => {
                self.pos += 1;
                match self.eat() {
                    Some('d') => Ok(ClassItem::Shorthand(Shorthand::Digit)),
                    Some('D') => Ok(ClassItem::Shorthand(Shorthand::NonDigit)),
                    Some('w') => Ok(ClassItem::Shorthand(Shorthand::Word)),
                    Some('W') => Ok(ClassItem::Shorthand(Shorthand::NonWord)),
                    Some('s') => Ok(ClassItem::Shorthand(Shorthand::Space)),
                    Some('S') => Ok(ClassItem::Shorthand(Shorthand::NonSpace)),
                    Some('h') => Ok(ClassItem::Shorthand(Shorthand::HexDigit)),
                    Some('H') => Ok(ClassItem::Shorthand(Shorthand::NonHexDigit)),
                    Some('t') => Ok(ClassItem::Char('\t')),
                    Some('v') => Ok(ClassItem::Char('\x0B')),
                    Some('n') => Ok(ClassItem::Char('\n')),
                    Some('r') => Ok(ClassItem::Char('\r')),
                    Some('b') => Ok(ClassItem::Char('\x08')), // backspace in class
                    Some('f') => Ok(ClassItem::Char('\x0C')),
                    Some('a') => Ok(ClassItem::Char('\x07')),
                    Some('e') => Ok(ClassItem::Char('\x1B')),
                    Some('x') => {
                        if self.peek() == Some('{') {
                            self.pos += 1;
                            let n = self.parse_hex_digits_max(8)?;
                            self.expect('}')?;
                            Ok(ClassItem::Char(char_from_u32(n)?))
                        } else {
                            let n = self.parse_hex_digits_exact(2)?;
                            Ok(ClassItem::Char(char_from_u32(n)?))
                        }
                    }
                    Some('u') => {
                        let n = self.parse_hex_digits_exact(4)?;
                        Ok(ClassItem::Char(char_from_u32(n)?))
                    }
                    Some('p') => {
                        self.expect('{')?;
                        let name = self.read_until('}')?;
                        let (name, neg) = if let Some(stripped) = name.strip_prefix('^') {
                            (stripped.to_string(), true)
                        } else {
                            (name, false)
                        };
                        Ok(ClassItem::Unicode(name, neg))
                    }
                    Some('P') => {
                        self.expect('{')?;
                        let name = self.read_until('}')?;
                        Ok(ClassItem::Unicode(name, true))
                    }
                    Some(c) if c.is_ascii_digit() => {
                        // Octal in class
                        let mut s = String::new();
                        s.push(c);
                        while s.len() < 3 {
                            match self.peek() {
                                Some(d) if d.is_ascii_digit() => {
                                    s.push(d);
                                    self.pos += 1;
                                }
                                _ => break,
                            }
                        }
                        let n = u32::from_str_radix(&s, 8)
                            .map_err(|_| Error::Parse(format!("invalid octal {:?}", s)))?;
                        Ok(ClassItem::Char(char_from_u32(n)?))
                    }
                    Some('c') => {
                        let c = self
                            .eat()
                            .ok_or_else(|| Error::Parse("incomplete \\c in class".into()))?;
                        Ok(ClassItem::Char(ctrl_char(c)))
                    }
                    Some('C') => {
                        self.expect('-')?;
                        let c = self
                            .eat()
                            .ok_or_else(|| Error::Parse("incomplete \\C- in class".into()))?;
                        Ok(ClassItem::Char(ctrl_char(c)))
                    }
                    Some(c) => Ok(ClassItem::Char(c)),
                    None => Err(Error::Parse("trailing \\ in character class".into())),
                }
            }
            // POSIX bracket [:name:] or [:^name:]
            Some('[') if self.peek_at(1) == Some(':') => {
                self.pos += 2; // consume '[' ':'
                let negate = if self.peek() == Some('^') {
                    self.pos += 1;
                    true
                } else {
                    false
                };
                let mut name = String::new();
                loop {
                    match self.eat() {
                        Some(':') => {
                            self.expect(']')?;
                            break;
                        }
                        Some(c) => name.push(c),
                        None => return Err(Error::Parse("unclosed POSIX bracket".into())),
                    }
                }
                let cls = parse_posix_class(&name)?;
                Ok(ClassItem::Posix(cls, negate))
            }
            Some(c) => {
                self.pos += 1;
                Ok(ClassItem::Char(c))
            }
            None => Err(Error::Parse("unexpected end in character class".into())),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_intersection(base: CharClass, intersections: Vec<CharClass>) -> CharClass {
    CharClass {
        negate: base.negate,
        items: base.items,
        intersections,
    }
}

fn parse_posix_class(name: &str) -> Result<PosixClass, Error> {
    match name {
        "alnum" => Ok(PosixClass::Alnum),
        "alpha" => Ok(PosixClass::Alpha),
        "ascii" => Ok(PosixClass::Ascii),
        "blank" => Ok(PosixClass::Blank),
        "cntrl" => Ok(PosixClass::Cntrl),
        "digit" => Ok(PosixClass::Digit),
        "graph" => Ok(PosixClass::Graph),
        "lower" => Ok(PosixClass::Lower),
        "print" => Ok(PosixClass::Print),
        "punct" => Ok(PosixClass::Punct),
        "space" => Ok(PosixClass::Space),
        "upper" => Ok(PosixClass::Upper),
        "xdigit" => Ok(PosixClass::XDigit),
        "word" => Ok(PosixClass::Word),
        _ => Err(Error::Parse(format!("unknown POSIX class {:?}", name))),
    }
}

fn parse_group_ref(s: &str) -> Result<GroupRef, Error> {
    if s == "0" {
        return Ok(GroupRef::Whole);
    }
    // Check explicit relative forward (`+n`) BEFORE trying integer parse, because
    // Rust's `str::parse::<i32>` accepts a leading '+' and would turn "+1" into
    // GroupRef::Index(1) rather than GroupRef::RelativeFwd(1).
    if let Some(rest) = s.strip_prefix('+')
        && let Ok(n) = rest.parse::<u32>()
    {
        return Ok(GroupRef::RelativeFwd(n));
    }
    if let Ok(n) = s.parse::<i32>() {
        if n > 0 {
            return Ok(GroupRef::Index(n as u32));
        } else if n < 0 {
            return Ok(GroupRef::RelativeBack((-n) as u32));
        }
    }
    // Name
    Ok(GroupRef::Name(s.to_string()))
}

fn parse_backref_target(content: &str) -> Result<(GroupRef, Option<i32>), Error> {
    // Check for level suffix: name+N or name-N
    // Only treat as level when there's a non-empty prefix (i.e. the sign is not the very
    // first character — a leading sign means a relative group number, not a level).
    if let Some(plus) = content.rfind('+')
        && plus > 0
        && let Ok(level) = content[plus + 1..].parse::<i32>()
    {
        let target = parse_group_ref(&content[..plus])?;
        return Ok((target, Some(level)));
    }
    if let Some(minus) = content.rfind('-')
        && minus > 0
        && let Ok(level) = content[minus + 1..].parse::<i32>()
    {
        let target = parse_group_ref(&content[..minus])?;
        return Ok((target, Some(-level)));
    }
    let target = parse_group_ref(content)?;
    Ok((target, None))
}

fn is_flag_char(c: char) -> bool {
    matches!(c, 'i' | 'm' | 'x' | 'd' | 'a' | 'u')
}

fn char_from_u32(n: u32) -> Result<char, Error> {
    char::from_u32(n).ok_or_else(|| Error::Parse(format!("invalid codepoint U+{:04X}", n)))
}

fn ctrl_char(c: char) -> char {
    char::from_u32((c as u32) & 0x1f).unwrap_or(c)
}
