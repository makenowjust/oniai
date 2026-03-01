# Oniai Pattern Specification

This document describes the full syntax and semantics of regular expression
patterns accepted by Oniai, which targets compatibility with
[Onigmo](https://github.com/k-takata/Onigmo) (Ruby's regex engine).

---

## Table of Contents

1. [Literals](#1-literals)
2. [Escape Sequences](#2-escape-sequences)
3. [Character Types](#3-character-types)
4. [Character Classes](#4-character-classes)
5. [Quantifiers](#5-quantifiers)
6. [Anchors](#6-anchors)
7. [Groups](#7-groups)
8. [Lookaround](#8-lookaround)
9. [Backreferences](#9-backreferences)
10. [Subexpression Calls](#10-subexpression-calls)
11. [Inline Flags](#11-inline-flags)
12. [Absence Operator](#12-absence-operator)
13. [Conditional Groups](#13-conditional-groups)
14. [Unicode Case Folding](#14-unicode-case-folding)

---

## 1. Literals

Any character that is not a metacharacter (`\ . | ( ) [ ] { } ? * + ^ $`) is
matched literally.  Metacharacters can be escaped with a backslash.

```
foo      # matches the string "foo"
foo\.bar # matches "foo.bar" (dot escaped)
```

---

## 2. Escape Sequences

### 2.1 Single-character escapes

| Escape | Character | Codepoint |
|--------|-----------|-----------|
| `\t`   | Tab       | U+0009    |
| `\n`   | Newline (LF) | U+000A |
| `\r`   | Carriage Return | U+000D |
| `\v`   | Vertical Tab | U+000B |
| `\f`   | Form Feed | U+000C    |
| `\a`   | Bell      | U+0007    |
| `\e`   | Escape    | U+001B    |

### 2.2 Hex and Unicode escapes

| Escape        | Meaning                                    |
|---------------|--------------------------------------------|
| `\xHH`        | Character with hex code `HH` (exactly 2 digits) |
| `\x{H…}` | Character with hex codepoint (1–8 hex digits)    |
| `\uHHHH`      | Character with hex code `HHHH` (exactly 4 digits) |

### 2.3 Octal escapes

| Escape      | Meaning                                   |
|-------------|-------------------------------------------|
| `\0oo`      | Character with octal code (leading `0`, up to 3 digits) |
| `\nnn`      | **Multi-digit octal** when a 3-digit octal sequence starts with `1`–`7` and reads as octal |

> **Disambiguation rule** — `\N` where N is a single digit `1`–`9` is a
> **backreference** (see §9), not an octal escape.  A sequence of 2–3 octal
> digits (all `< 8`) is always an octal literal.  Use `\0NN` for zero-prefixed
> octal.

### 2.4 Control-character escapes

| Escape      | Meaning                              |
|-------------|--------------------------------------|
| `\cX`       | Control character for `X` (`X & 0x1F`) |
| `\C-X`      | Same as `\cX`                        |
| `\M-X`      | Meta character (`X | 0x80`)          |
| `\M-\C-X`   | Meta + control (`(X & 0x1F) | 0x80`) |

### 2.5 Special escapes (outside character classes)

| Escape | Meaning |
|--------|---------|
| `\b`   | Word boundary anchor (see §6); **not** backspace |
| `\K`   | Keep — resets the start of the match (see §7.10) |

> **Inside character classes**, `\b` means **backspace** (U+0008), not word
> boundary.

---

## 3. Character Types

### 3.1 Wildcard `.`

`.` matches any single character **except newline** (`\n`, U+000A).

Enable the `(?m)` flag (dotall mode) to make `.` also match `\n`.

### 3.2 Shorthand classes

| Shorthand | Meaning | Notes |
|-----------|---------|-------|
| `\w` | Word character: `[A-Za-z0-9_]` (Unicode by default) | Alphanumeric per Unicode + `_` |
| `\W` | Non-word character | Complement of `\w` |
| `\d` | Digit character | Unicode numeric digit by default |
| `\D` | Non-digit character | Complement of `\d` |
| `\s` | Whitespace: `[\t\n\v\f\r ]` | Always exactly these 6 characters |
| `\S` | Non-whitespace | Complement of `\s` |
| `\h` | Hex digit: `[0-9A-Fa-f]` | Always ASCII only |
| `\H` | Non-hex-digit | Complement of `\h` |

> **ASCII-range flag (`a`)** — When `(?a)` is active, `\w` and `\d` are
> restricted to ASCII characters only: `\w` = `[A-Za-z0-9_]`, `\d` =
> `[0-9]`.  `\s` and `\h` are unaffected (they are already ASCII-only).

> **`\R`** — Approximately matched as `\s` (simplified; not a full Unicode
> line-break cluster).

### 3.3 Unicode properties `\p{…}` and `\P{…}`

Match characters by Unicode General Category or named property.

Syntax:
- `\p{Name}` — matches characters with property *Name*
- `\P{Name}` — negates: matches characters **not** having *Name*
- `\p{^Name}` — alternative negation syntax inside `\p{}`

Property names are **case-insensitive** and **underscore/hyphen/space are
ignored** in the name (e.g. `\p{Lu}` ≡ `\p{Uppercase_Letter}` ≡
`\p{uppercaseletter}`).

#### Supported General Categories

| Code | Long name | Matches |
|------|-----------|---------|
| `L`  | Letter    | All letters (Lu + Ll + Lt + Lm + Lo) |
| `Lu` | Uppercase_Letter | Uppercase letters |
| `Ll` | Lowercase_Letter | Lowercase letters |
| `Lt` | Titlecase_Letter | Titlecase letters |
| `Lm` | Modifier_Letter | Modifier letters |
| `Lo` | Other_Letter | Other letters (e.g. CJK ideographs) |
| `M`  | Mark / Combining_Mark | All marks (Mn + Mc + Me) |
| `Mn` | Nonspacing_Mark | Non-spacing marks |
| `Mc` | Spacing_Mark | Spacing combining marks |
| `Me` | Enclosing_Mark | Enclosing marks |
| `N`  | Number | All numbers (Nd + Nl + No) |
| `Nd` | Decimal_Number / Decimal_Digit_Number | Decimal digit numbers |
| `Nl` | Letter_Number | Letter numbers (e.g. Roman numerals) |
| `No` | Other_Number | Other numbers (e.g. superscripts) |
| `P`  | Punctuation | All punctuation |
| `Pc` | Connector_Punctuation | Connector punctuation (e.g. `_`) |
| `Pd` | Dash_Punctuation | Dash punctuation |
| `Ps` | Open_Punctuation | Opening brackets |
| `Pe` | Close_Punctuation | Closing brackets |
| `Pi` | Initial_Punctuation | Initial quotes |
| `Pf` | Final_Punctuation | Final quotes |
| `Po` | Other_Punctuation | Other punctuation |
| `S`  | Symbol | All symbols (Sm + Sc + Sk + So) |
| `Sm` | Math_Symbol | Math symbols |
| `Sc` | Currency_Symbol | Currency symbols |
| `Sk` | Modifier_Symbol | Modifier symbols |
| `So` | Other_Symbol | Other symbols |
| `Z`  | Separator | All separators (Zs + Zl + Zp) |
| `Zs` | Space_Separator | Space separators |
| `Zl` | Line_Separator | Line separators |
| `Zp` | Paragraph_Separator | Paragraph separators |
| `C`  | Other | All other characters |
| `Cc` | Control | Control characters |
| `Cf` | Format | Format characters |
| `Cs` | Surrogate | Surrogates |
| `Co` | Private_Use | Private-use characters |
| `Cn` | Unassigned / Not_Assigned | Unassigned codepoints |

#### Special properties

| Name | Matches |
|------|---------|
| `Any` | Any character |
| `Assigned` | Any assigned character (complement of `Cn`) |

#### POSIX-like properties (accessible via `\p{}`)

| Name | Equivalent |
|------|-----------|
| `Alpha` | Alphabetic characters |
| `Alnum` | Alphanumeric characters |
| `ASCII` | ASCII characters (U+0000–U+007F) |
| `Blank` | Space and tab |
| `Cntrl` | Control characters (codepoint < 32 or == 127) |
| `Digit` | Decimal digit characters |
| `Graph` | Visible characters (not space, not control) |
| `Lower` | Lowercase letters |
| `Print` | Printable characters |
| `Punct` | ASCII punctuation characters |
| `Space` | Whitespace (`\t\n\v\f\r `) |
| `Upper` | Uppercase letters |
| `Word`  | Word characters (alphanumeric + `_`) |
| `XDigit` | Hex digits `[0-9A-Fa-f]` |

#### Binary properties

| Name | Matches |
|------|---------|
| `Alphabetic` | Characters with Alphabetic property |
| `Uppercase` | Characters with Uppercase property |
| `Lowercase` | Characters with Lowercase property |
| `Whitespace` | Characters with White_Space property |
| `HexDigit` | Hex digits |
| `Numeric` | Numeric characters |
| `Math` | Math symbols (same as `Sm`) |

An **unknown property name** is a compile-time error.

---

## 4. Character Classes

Character classes are written `[…]`.

### 4.1 Basic syntax

```
[abc]     # matches 'a', 'b', or 'c'
[a-z]     # matches any lowercase ASCII letter
[^abc]    # negated: matches anything except 'a', 'b', 'c'
[^a-z]    # negated range
```

The `^` must appear immediately after `[` to negate.  `]` is treated as a
literal character only at the very first position (before any other char,
after optional `^`).

### 4.2 Escape sequences inside classes

All escape sequences from §2 work inside character classes, with one
exception:

- `\b` inside a character class means **backspace** (U+0008), **not** the
  word-boundary anchor.

### 4.3 Shorthand classes inside `[…]`

`\w`, `\W`, `\d`, `\D`, `\s`, `\S`, `\h`, `\H` can appear inside character
classes with their usual meanings.

### 4.4 POSIX bracket expressions

POSIX classes are written `[:name:]` inside a character class:

```
[[:alpha:]]   # alphabetic characters
[[:digit:]]   # decimal digits
[[:alnum:]]   # alphanumeric
[[:upper:]]   # uppercase letters
[[:lower:]]   # lowercase letters
[[:space:]]   # whitespace
[[:blank:]]   # space and tab
[[:punct:]]   # punctuation
[[:graph:]]   # visible characters
[[:print:]]   # printable characters
[[:cntrl:]]   # control characters
[[:ascii:]]   # ASCII characters
[[:xdigit:]]  # hex digits
[[:word:]]    # word characters (\w equivalent)
```

Negated POSIX: `[[:^alpha:]]` matches non-alphabetic characters.

### 4.5 Unicode properties inside `[…]`

`\p{Name}` and `\P{Name}` work inside character classes:

```
[\p{Lu}\p{Ll}]   # all letters (uppercase or lowercase)
[\p{Lu}a-z]      # uppercase letters or ASCII lowercase
```

### 4.6 Nested classes

`[…]` can be nested inside another `[…]`:

```
[[abc][def]]     # union: matches 'a', 'b', 'c', 'd', 'e', or 'f'
```

### 4.7 Intersection `[A&&B]`

```
[a-w&&[^c-g]]    # intersection: [a-w] minus [c-g] = [abh-w]
[a-z&&[^aeiou]]  # consonants
```

The `&&` operator computes the set intersection between the left-hand set and
the right-hand set.  Multiple `&&` can be chained.

### 4.8 Case-insensitive character classes

Under `(?i)`, character matching inside classes works as follows:

- **Single characters** (e.g. `[a]`): uses full Unicode case-fold equality
  (`chars_eq_ci`), which compares the complete fold sequences.  A character
  `c` matches a class item `p` if and only if
  `c.case_fold() == p.case_fold()`.  This means a single-char class entry
  does **not** match characters with a *longer* fold — e.g. `(?i)[ß]` matches
  only `ß` itself (and `SS` etc. only if their fold equals `['s','s']`), but
  does **not** match a single `s` because `['s'] ≠ ['s','s']`.

- **Negated classes under `(?i)`** (e.g. `[^ß]`): a position is excluded if
  the **positive** version `[ß]` would match there (including multi-char fold
  sequences).  When the negated class does match, it consumes exactly one
  character.  Example: `(?i)[^ß]` does **not** match at position 0 of `ss`
  (because `(?i)[ß]` matches the two-char sequence `ss` there), but **does**
  match the second `s` at position 1.

- **Ranges** (e.g. `[a-z]`): each character `c` is compared against the range
  bounds using its **simple (single-codepoint) case fold**.  A character `c`
  has a simple fold if and only if `c.case_fold()` yields exactly one codepoint
  `fc`; in that case the comparison is `lo_fold ≤ fc ≤ hi_fold`, where
  `lo_fold` and `hi_fold` are the single-codepoint folds of the bounds.  If `c`
  has a **multi-codepoint full fold** (e.g. `ß → ss`), then `c` itself is used,
  meaning it only matches a range that literally contains `c`.  As a consequence:
  - `(?i)[a-z]` matches all ASCII uppercase letters.
  - `(?i)[a-z]` matches `ſ` (U+017F, Latin Small Letter Long S), because its
    simple fold is `s`.
  - `(?i)[a-z]` also matches the Kelvin sign `K` (U+212A), because its fold
    is `k`.
  - `(?i)[a-z]` does **not** match `ß` (U+00DF), because `ß` folds to the
    two-character sequence `ss` and `ß` itself is not in `[a-z]`.

---

## 5. Quantifiers

### 5.1 Basic quantifiers

| Syntax   | Meaning                           |
|----------|-----------------------------------|
| `?`      | Zero or one (greedy)              |
| `*`      | Zero or more (greedy)             |
| `+`      | One or more (greedy)              |
| `{n}`    | Exactly *n* times                 |
| `{n,m}`  | Between *n* and *m* times (inclusive) |
| `{n,}`   | At least *n* times                |
| `{,m}`   | Zero to *m* times                 |

`{}` and `{,}` are **not** valid quantifiers and are treated as literal `{}`
characters.

### 5.2 Quantifier modes

Each quantifier can be followed by a mode modifier:

| Suffix | Mode | Behaviour |
|--------|------|-----------|
| (none) | Greedy | Match as many as possible; backtrack if needed |
| `?`    | Lazy / Reluctant | Match as few as possible; expand if needed |
| `+`    | Possessive | Match as many as possible; **never** backtrack |

Examples:

```
a*       # greedy
a*?      # lazy
a*+      # possessive (no backtracking)
```

Possessive quantifiers are implemented via the atomic group instruction;
they are equivalent to `(?>a*)`.

---

## 6. Anchors

| Anchor | Matches |
|--------|---------|
| `^`    | Start of line (default: start of string; after `\n` in multiline mode) |
| `$`    | End of line (default: end of string or before final `\n`; before any `\n` in multiline mode) |
| `\A`   | Absolute start of string (not affected by multiline mode) |
| `\z`   | Absolute end of string |
| `\Z`   | End of string or just before a final newline |
| `\b`   | Word boundary (transition between `\w` and `\W`) |
| `\B`   | Non-word boundary |
| `\G`   | Start of search (the position where the last match ended, or the search start passed to `find`) |

### Multiline mode (`(?m)`)

In Onigmo/Ruby, `(?m)` enables **dotall** mode (dot matches `\n`), **not**
the PCRE-style "multi-line `^`/`$`" mode.  `^` and `$` always match line
boundaries (before/after `\n`).

---

## 7. Groups

### 7.1 Capturing group `(…)`

```
(foo)    # group 1 captures "foo"
```

Groups are numbered 1-based in left-to-right opening-parenthesis order.
Group 0 refers to the whole match.

### 7.2 Non-capturing group `(?:…)`

```
(?:foo)  # matches "foo" but does not create a capture
```

### 7.3 Named capturing group

```
(?<name>…)    # Onigmo / Ruby / .NET syntax
(?'name'…)    # Perl alternative syntax
```

Named groups can be referenced with `\k<name>` or `\k'name'` (backreference)
and `\g<name>` or `\g'name'` (subexpression call).

### 7.4 Atomic group `(?>…)`

```
(?>a*)   # possessive: never backtracks into the group once it has matched
```

Equivalent to a possessive quantifier applied to a group.

### 7.5 Comment group `(?#…)`

```
foo(?#this is a comment)bar  # matches "foobar"
```

The comment runs until the first `)`.  Nesting is not supported.

### 7.6 Inline flags `(?imxa-imxa:…)` and `(?imxa-imxa)`

See [§11 Inline Flags](#11-inline-flags).

### 7.7 Lookahead `(?=…)` and `(?!…)`

See [§8 Lookaround](#8-lookaround).

### 7.8 Lookbehind `(?<=…)` and `(?<!…)`

See [§8 Lookaround](#8-lookaround).

### 7.9 Absence operator `(?~…)`

See [§12 Absence Operator](#12-absence-operator).

### 7.10 Keep `\K`

`\K` resets the effective start of the current match to the current position.
Text consumed before `\K` is not included in the returned match.

```
foo\Kbar  # on "foobar": reports match "bar"
```

---

## 8. Lookaround

Lookaround assertions are zero-width: they test a condition at the current
position without consuming any characters.

| Syntax     | Name | Meaning |
|------------|------|---------|
| `(?=…)`    | Positive lookahead  | Succeeds if `…` matches at current position (forward) |
| `(?!…)`    | Negative lookahead  | Succeeds if `…` does NOT match at current position (forward) |
| `(?<=…)`   | Positive lookbehind | Succeeds if `…` matches ending at current position (backward) |
| `(?<!…)`   | Negative lookbehind | Succeeds if `…` does NOT match ending at current position (backward) |

### Variable-length lookbehind

Oniai supports **unbounded** lookbehind — there is no fixed-width restriction.
Quantifiers such as `*`, `+`, and `{n,}` are permitted inside `(?<=…)` and
`(?<!…)`.

```
(?<=a+)b     # matches 'b' preceded by one or more 'a'
(?<=foo|fo)bar  # variable-length alternative
```

### Captures inside lookaround

Capturing groups inside lookahead bodies **do** set capture slots on success.
The captured values are merged into the outer match state (only the changed
slots are recorded as a delta for the lookaround result cache).

Lookbehind capture groups are also supported; the start/end offsets are
always stored in the correct `(start < end)` order despite backward execution.

---

## 9. Backreferences

A backreference re-matches the same text that was captured by an earlier group.

### 9.1 Numeric backreference

```
\1  …  \9   # refer to capture group by number
```

A single digit `1`–`9` is always a backreference.  The group must have been
defined earlier in the pattern.

### 9.2 Named backreference

```
\k<name>    # angle-bracket syntax
\k'name'    # single-quote syntax
```

### 9.3 Relative backreference `\k<-n>`

Relative backreferences count backwards from the group that contains the
`\k` escape.  `\k<-1>` refers to the immediately preceding capture group.

```
(a)(\k<-1>)   # group 2 references group 1
```

### 9.4 Case-insensitive backreferences

When the `(?i)` flag is active, backreferences use **Unicode case folding**
for comparison.  The matched text and the captured text are both fully
case-folded (via `unicode_casefold`) before comparison.  Multi-codepoint folds
are handled correctly:

```
(?i)(ss)\1    # matches "ssß" (ß folds to "ss")
(?i)(ß)\1     # matches "ßss" (captured ß's fold "ss" matches text "ss")
```

See [§14 Unicode Case Folding](#14-unicode-case-folding) for details.

---

## 10. Subexpression Calls

A subexpression call re-executes the body of a named or numbered capture group
as if it were inlined at the call site (similar to a subroutine call).

```
\g<name>    # call named group
\g'name'    # alternative syntax
\g<n>       # call group n
\g<0>       # call the whole pattern (recursive)
\g<-n>      # call group n positions before the current group
\g<+n>      # call group n positions after the current group
```

Subexpression calls enable recursive patterns:

```
\A(?<a>|.|\g<a>)\z   # matches any-length palindrome
a\g<0>?b             # matches "ab", "aabb", "aaabbb", …
```

The call stack is limited to **200** levels; deeper recursion returns a match
failure (not a panic or stack overflow).

---

## 11. Inline Flags

Flags can be set or unset at any point inside a pattern.

### 11.1 Scoped flags `(?flags:…)`

```
(?i:foo)    # case-insensitive for this subpattern only
(?-i:foo)   # case-sensitive inside this scope
(?im:a.b)   # case-insensitive and dotall
```

### 11.2 Rest-of-group flags `(?flags)`

```
(?i)foo     # case-insensitive from here to end of enclosing group
```

The flag change takes effect on all atoms following the `(?flags)` within the
same group level; it does not affect atoms already parsed before the flag group.

### 11.3 Available flags

| Flag | Name | Effect |
|------|------|--------|
| `i`  | Ignore case | Case-insensitive matching using Unicode case folding |
| `m`  | Multiline / Dotall | `.` matches `\n` (Ruby `m` semantics) |
| `x`  | Extended | Whitespace and `#`-comments are ignored |
| `a`  | ASCII range | `\w`, `\d` restricted to ASCII characters |

Flags can be combined and negated: `(?im-x)`, `(?-i)`.

### 11.4 Extended mode (`(?x)`)

In extended mode all literal whitespace (space, tab, newline, carriage return)
is ignored, and a `#` introduces a comment that runs to the end of the line.
This allows patterns to be formatted for readability:

```
(?x)
  foo   # match "foo"
  bar   # match "bar"
# The above matches "foobar"
```

---

## 12. Absence Operator

```
(?~X)
```

`(?~X)` matches the **longest** string starting at the current position that
does **not contain** `X` as a substring anywhere within it.

```
/\*(?~\*/)\*/      # C-style block comment /* … */
(?~abc)            # string not containing "abc"
```

- `(?~X)` can match the empty string (when `X` cannot appear in an empty
  string).
- `(?~X)*` is valid; the body of the loop can match empty, and the null-check
  guard prevents infinite loops.

---

## 13. Conditional Groups

```
(?(cond)yes)
(?(cond)yes|no)
```

Conditional groups match `yes` if condition `cond` is true at this point in
the match, or `no` otherwise (`no` defaults to the empty pattern if omitted).

### Condition syntax

| Condition | Meaning |
|-----------|---------|
| `(?(n)…)` | True if capture group *n* has matched (n ≥ 1) |
| `(?(<name>)…)` | True if named group *name* has matched |
| `(?('name')…)` | Alternative syntax for named condition |

Group number 0 is **not valid** as a condition and produces a parse error.

---

## 14. Unicode Case Folding

Case-insensitive matching (`(?i)`) uses **full Unicode case folding** via the
`unicode-casefold` crate.  The specification below covers several subtle edge
cases.

### 14.1 Single-character literals outside character classes

When a literal character `c` appears outside a character class under `(?i)`,
it is compiled into a **`FoldSeq`** instruction containing the complete Unicode
case fold of `c` as a sequence of characters.

The `FoldSeq` instruction advances through the input by consuming characters
whose successive case folds concatenate to exactly the stored fold sequence.
This means a single source character can match **multiple** input characters
and vice versa.

#### Key examples

| Pattern | Matches | Explanation |
|---------|---------|-------------|
| `(?i)ß`  | `ß`, `SS`, `Ss`, `sS`, `ss` | `ß` folds to `['s','s']`; two `s`/`S` chars each fold to `['s']` |
| `(?i)ss` | `ss`, `SS`, `Ss`, `sS`, `ß` | Consecutive literals merged into `FoldSeq(['s','s'])`; `ß` produces same fold |
| `(?i)k`  | `k`, `K`, `K` (U+212A Kelvin sign) | `K` and Kelvin sign both fold to `['k']` |
| `(?i)K`  | `k`, `K`, `K` (U+212A) | Same |
| `(?i)ﬁ` (U+FB01) | `ﬁ`, `fi`, `fI`, `Fi`, `FI` | `ﬁ` folds to `['f','i']` |

### 14.2 Single-character literals **inside** character classes

Inside a character class `[…]`, each literal entry uses `chars_eq_ci(a, b)`,
which compares `a.case_fold().collect::<Vec<_>>()` with
`b.case_fold().collect::<Vec<_>>()`.  Equality requires the fold sequences to
have **exactly the same length**.

Consequently:
- `(?i)[ß]` matches `ß` (self-fold `['s','s']`), and also `SS`, `Ss`, etc.
  (any char whose complete fold equals `['s','s']`).
- `(?i)[ß]` does **not** match a single `s`, because `'s'.case_fold()` =
  `['s']` ≠ `['s','s']`.
- `(?i)[k]` matches `k`, `K`, and the Kelvin sign `K` (U+212A) — all fold to
  `['k']`.

### 14.3 Ranges inside character classes

For a range `[lo-hi]` under `(?i)`, the engine uses the **simple
(single-codepoint) case fold** of `ch` for comparison:

```
simple_fold(c) = c.case_fold().next()   if c.case_fold() yields exactly one codepoint
               = c                      if c.case_fold() yields multiple codepoints (no simple fold)

match if  simple_fold(lo) ≤ simple_fold(ch) ≤ simple_fold(hi)
```

Characters with a **multi-codepoint full fold** (e.g. `ß → ss`) have no
simple fold and are compared as themselves.

Practical consequences:

- `(?i)[a-z]` matches all 26 ASCII uppercase letters.
- `(?i)[a-z]` matches `ſ` (U+017F, Latin Small Letter Long S), because its
  simple fold is `s`.
- `(?i)[a-z]` also matches the Kelvin sign `K` (U+212A), because its simple
  fold is `k`.
- `(?i)[a-z]` does **not** match `ß` (U+00DF), because `ß` has no simple fold
  (`ß.case_fold()` yields `['s','s']`) and `ß` itself is outside `[a-z]`.
- `(?i)[A-Z]` and `(?i)[a-z]` behave identically (both bounds fold to the
  same single-char range).

### 14.4 Backreferences under `(?i)`

Case-insensitive backreferences use `caseless_advance`, which folds both the
captured text and the candidate input text into their full Unicode case fold
sequences and compares them element-by-element.  Multi-codepoint folds are
fully supported:

```
(?i)(ß)\1    matches "ßss"  (fold of "ß" = ['s','s'] matches "ss")
(?i)(ss)\1   matches "ssß"  (fold of "ss" = ['s','s'] matches "ß")
(?i)(ﬁ)\1   matches "ﬁfi"  (fold of "ﬁ" = ['f','i'] matches "fi")
```

### 14.5 Negated character classes under `(?i)`

A negated class `(?i)[^X]` is the complement of `(?i)[X]`, **including
multi-codepoint fold equivalences**.  Specifically:

- At a given position, if `(?i)[X]` would match (possibly consuming more than
  one byte — e.g. matching `ss` for `[ß]`), then `(?i)[^X]` does **not** match
  at that position.
- When `(?i)[^X]` does match, it always consumes exactly **one character**
  (the character at that position).

Practical consequences for `(?i)[^ß]` on the text `ss`:

| Position | `(?i)[ß]` result | `(?i)[^ß]` result | Reason |
|----------|-----------------|-------------------|--------|
| 0 | matches `ss` (2 bytes) | **no match** | positive class matches here |
| 1 | no match | matches `s` (1 byte) | `s` alone ≠ fold of `ß` |

More examples:

```
(?i)[^ß]  on "ssX"  → matches "s" at pos 1, then "X" at pos 2
(?i)[^ß]  on "ßX"   → does not match at pos 0 (ß itself is in [ß]),
                       matches "X" at pos 2
(?i)[^ß]  on "ẞX"   → does not match at pos 0 (ẞ folds to "ss" = fold of ß),
                       matches "X" at pos 3
```

### 14.6 Notable Unicode characters

| Character | Codepoint | Folds to | Notes |
|-----------|-----------|----------|-------|
| `ß` (Sharp S) | U+00DF | `ss` | Common in German |
| `K` (Kelvin sign) | U+212A | `k` | Matches `k` and `K` |
| `Å` (Angstrom) | U+212B | `å` | Matches `å` and `Å` (U+00C5) |
| `ﬁ` (fi ligature) | U+FB01 | `fi` | |
| `ﬀ` (ff ligature) | U+FB00 | `ff` | |
| `ﬃ` (ffi ligature) | U+FB03 | `ffi` | |
| `ﬄ` (ffl ligature) | U+FB04 | `ffl` | |

### 14.7 Memoization and case-insensitive patterns

Case-insensitive matching does not affect whether memoization is enabled.
Memoization is disabled only when the pattern contains `BackRef` or
`CheckGroup` instructions (see the VM design notes in `doc/DESIGN.md`).

---

## Limitations and Known Gaps

- **`\R`** — The general linebreak escape `\R` is currently approximated as
  `\s`; it does not implement the full Unicode linebreak cluster
  (`\r\n | [\r\n\x0B\x0C\x85\u{2028}\u{2029}]`).

- **Recursive patterns** — `\g<0>` and mutual recursion via named groups are
  supported with a 200-level call stack limit.  Very deep recursive patterns
  may fail to match rather than stack-overflow.

- **No NFA/DFA engine** — The engine uses iterative backtracking.  The
  memoization framework (Algorithms 5–7 of Fujinami & Hasuo 2024,
  arXiv:2401.12639) bounds many common pathological cases, but adversarial
  inputs can still cause super-linear behaviour for patterns not covered by
  the memo.

- **Lookaround depth** — Sub-executions for lookaround and absence operators
  are limited to **100** levels of nesting; deeper nesting returns a match
  failure.
