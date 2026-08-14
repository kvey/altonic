//! A compact regex engine, built from first principles.
//!
//! Dialect (SPEC §10.3): literals, `.`, character classes `[a-z0-9_]` with
//! negation and ranges, escapes `\d \w \s \D \W \S` plus escaped metachars,
//! anchors `^ $`, quantifiers `* + ? {m} {m,} {m,n}` with lazy `?` variants,
//! alternation `|`, capturing groups `(...)` and non-capturing `(?:...)`.
//! No backreferences or lookaround.
//!
//! Implementation: the pattern is parsed to an AST, compiled to a small
//! bytecode program, and matched with a backtracking VM. Failed (pc, pos)
//! states are memoized (packrat-style), which makes the worst case
//! O(program × input) instead of exponential and makes zero-width loops
//! (e.g. `(a?)*`) terminate: re-entering an in-flight state is a failure.

use crate::{verr, Error};

// ---------- AST ----------

#[derive(Debug, Clone)]
enum Node {
    Char(char),
    AnyChar,
    Class { negated: bool, items: Vec<ClassItem> },
    Start,
    End,
    Group { index: Option<usize>, node: Box<Node> },
    Concat(Vec<Node>),
    Alternate(Vec<Node>),
    Repeat { node: Box<Node>, min: usize, max: Option<usize>, greedy: bool },
}

#[derive(Debug, Clone, PartialEq)]
enum ClassItem {
    Single(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

// ---------- Bytecode ----------

#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class { negated: bool, items: Vec<ClassItem> },
    Start,
    End,
    /// Save current position into capture slot n (2k = group k start, 2k+1 = end).
    Save(usize),
    /// Try `a` first; on failure try `b`.
    Split(usize, usize),
    Jmp(usize),
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
    pub group_count: usize,
}

/// A successful match: overall span plus capture group spans (char indices).
#[derive(Debug, Clone)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    /// groups[0] is the whole match; groups[i] the i-th capture (None if unmatched).
    pub groups: Vec<Option<(usize, usize)>>,
}

impl Match {
    pub fn group_text(&self, chars: &[char], i: usize) -> Option<String> {
        self.groups.get(i).copied().flatten().map(|(s, e)| chars[s..e].iter().collect())
    }
}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex, Error> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut p = RParser { chars, pos: 0, group_index: 0 };
        let root = p.alternation(0)?;
        if p.pos != p.chars.len() {
            return Err(verr(format!("regex: unexpected ')' at position {}", p.pos)));
        }
        let mut prog = Vec::new();
        compile(&root, &mut prog);
        prog.push(Inst::Match);
        Ok(Regex { prog, group_count: p.group_index })
    }

    pub fn is_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        self.find_chars(&chars, 0).is_some()
    }

    /// Whether the pattern matches the ENTIRE text (implicit anchoring).
    pub fn is_full_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        match self.match_at(&chars, 0) {
            Some(m) => m.end == chars.len(),
            None => false,
        }
    }

    pub fn find(&self, text: &str) -> Option<Match> {
        let chars: Vec<char> = text.chars().collect();
        self.find_chars(&chars, 0)
    }

    /// All non-overlapping matches, left to right (leftmost, then greedy).
    pub fn find_iter(&self, text: &str) -> Vec<Match> {
        let chars: Vec<char> = text.chars().collect();
        self.find_iter_chars(&chars)
    }

    pub fn find_iter_chars(&self, chars: &[char]) -> Vec<Match> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= chars.len() {
            match self.find_chars(chars, at) {
                Some(m) => {
                    let next = if m.end == m.start { m.end + 1 } else { m.end };
                    out.push(m);
                    at = next;
                }
                None => break,
            }
        }
        out
    }

    pub fn find_chars(&self, chars: &[char], from: usize) -> Option<Match> {
        for start in from..=chars.len() {
            if let Some(m) = self.match_at(chars, start) {
                return Some(m);
            }
        }
        None
    }

    /// Attempt an anchored match beginning exactly at `start`.
    pub fn match_at(&self, chars: &[char], start: usize) -> Option<Match> {
        let mut vm = Vm {
            prog: &self.prog,
            chars,
            saves: vec![None; (self.group_count + 1) * 2],
            // dead[pc * (len+1) + pos] — states proven unable to match.
            dead: vec![false; self.prog.len() * (chars.len() + 2)],
            width: chars.len() + 2,
        };
        vm.saves[0] = Some(start);
        if vm.run(0, start) {
            let end = vm.saves[1].expect("Match sets end slot");
            let mut groups = Vec::with_capacity(self.group_count + 1);
            for g in 0..=self.group_count {
                groups.push(match (vm.saves[g * 2], vm.saves[g * 2 + 1]) {
                    (Some(s), Some(e)) => Some((s, e)),
                    _ => None,
                });
            }
            Some(Match { start, end, groups })
        } else {
            None
        }
    }
}

struct Vm<'a> {
    prog: &'a [Inst],
    chars: &'a [char],
    saves: Vec<Option<usize>>,
    dead: Vec<bool>,
    width: usize,
}

impl Vm<'_> {
    fn run(&mut self, pc: usize, pos: usize) -> bool {
        let key = pc * self.width + pos;
        if self.dead[key] {
            return false;
        }
        // Mark in-progress: a zero-width cycle back to this state cannot
        // produce a match this frame couldn't produce itself.
        self.dead[key] = true;
        let ok = match &self.prog[pc] {
            Inst::Char(c) => pos < self.chars.len() && self.chars[pos] == *c && self.run(pc + 1, pos + 1),
            Inst::Any => pos < self.chars.len() && self.chars[pos] != '\n' && self.run(pc + 1, pos + 1),
            Inst::Class { negated, items } => {
                pos < self.chars.len()
                    && (class_matches(items, self.chars[pos]) != *negated)
                    && self.run(pc + 1, pos + 1)
            }
            Inst::Start => pos == 0 && self.run(pc + 1, pos),
            Inst::End => pos == self.chars.len() && self.run(pc + 1, pos),
            Inst::Save(slot) => {
                let slot = *slot;
                let old = self.saves[slot];
                self.saves[slot] = Some(pos);
                if self.run(pc + 1, pos) {
                    true
                } else {
                    self.saves[slot] = old;
                    false
                }
            }
            Inst::Split(a, b) => {
                let (a, b) = (*a, *b);
                self.run(a, pos) || self.run(b, pos)
            }
            Inst::Jmp(a) => self.run(*a, pos),
            Inst::Match => {
                self.saves[1] = Some(pos);
                true
            }
        };
        if ok {
            // Un-mark: successful states must not be treated as dead for
            // subsequent independent searches (find_iter reuses nothing, so
            // this only matters for correctness of sibling branches wanting
            // the same state — success short-circuits anyway).
            self.dead[key] = false;
        }
        ok
    }
}

fn compile(node: &Node, prog: &mut Vec<Inst>) {
    match node {
        Node::Char(c) => prog.push(Inst::Char(*c)),
        Node::AnyChar => prog.push(Inst::Any),
        Node::Class { negated, items } => {
            prog.push(Inst::Class { negated: *negated, items: items.clone() })
        }
        Node::Start => prog.push(Inst::Start),
        Node::End => prog.push(Inst::End),
        Node::Group { index, node } => {
            if let Some(i) = index {
                prog.push(Inst::Save(i * 2));
                compile(node, prog);
                prog.push(Inst::Save(i * 2 + 1));
            } else {
                compile(node, prog);
            }
        }
        Node::Concat(nodes) => {
            for n in nodes {
                compile(n, prog);
            }
        }
        Node::Alternate(branches) => {
            // split L1, L2; L1: b0; jmp end; L2: split ...
            let mut jmp_ends = Vec::new();
            let n = branches.len();
            for (i, b) in branches.iter().enumerate() {
                if i + 1 < n {
                    let split_at = prog.len();
                    prog.push(Inst::Split(0, 0)); // patched below
                    let l1 = prog.len();
                    compile(b, prog);
                    jmp_ends.push(prog.len());
                    prog.push(Inst::Jmp(0)); // patched below
                    let l2 = prog.len();
                    prog[split_at] = Inst::Split(l1, l2);
                } else {
                    compile(b, prog);
                }
            }
            let end = prog.len();
            for j in jmp_ends {
                prog[j] = Inst::Jmp(end);
            }
        }
        Node::Repeat { node, min, max, greedy } => {
            // Mandatory copies.
            for _ in 0..*min {
                compile(node, prog);
            }
            match max {
                Some(m) => {
                    // (m - min) optional copies: split(body, end) each.
                    let mut splits = Vec::new();
                    for _ in *min..*m {
                        let s = prog.len();
                        prog.push(Inst::Split(0, 0));
                        let body = prog.len();
                        splits.push((s, body));
                        compile(node, prog);
                    }
                    let end = prog.len();
                    for (s, body) in splits {
                        prog[s] = if *greedy {
                            Inst::Split(body, end)
                        } else {
                            Inst::Split(end, body)
                        };
                    }
                }
                None => {
                    // Unbounded: L: split(body, end); body; jmp L; end
                    let l = prog.len();
                    prog.push(Inst::Split(0, 0));
                    let body = prog.len();
                    compile(node, prog);
                    prog.push(Inst::Jmp(l));
                    let end = prog.len();
                    prog[l] = if *greedy { Inst::Split(body, end) } else { Inst::Split(end, body) };
                }
            }
        }
    }
}

fn class_matches(items: &[ClassItem], c: char) -> bool {
    items.iter().any(|item| match item {
        ClassItem::Single(x) => *x == c,
        ClassItem::Range(lo, hi) => (*lo..=*hi).contains(&c),
        ClassItem::Digit => c.is_ascii_digit(),
        ClassItem::NotDigit => !c.is_ascii_digit(),
        ClassItem::Word => c.is_alphanumeric() || c == '_',
        ClassItem::NotWord => !(c.is_alphanumeric() || c == '_'),
        ClassItem::Space => c.is_whitespace(),
        ClassItem::NotSpace => !c.is_whitespace(),
    })
}

// ---------- Parser ----------

const MAX_REGEX_DEPTH: usize = 64;
const MAX_COUNTED_REPEAT: usize = 1000;

struct RParser {
    chars: Vec<char>,
    pos: usize,
    group_index: usize,
}

impl RParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn alternation(&mut self, depth: usize) -> Result<Node, Error> {
        if depth > MAX_REGEX_DEPTH {
            return Err(verr("regex: nesting too deep"));
        }
        let mut branches = vec![self.concat(depth)?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.concat(depth)?);
        }
        Ok(if branches.len() == 1 { branches.pop().unwrap() } else { Node::Alternate(branches) })
    }

    fn concat(&mut self, depth: usize) -> Result<Node, Error> {
        let mut nodes = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            nodes.push(self.repeat(depth)?);
        }
        Ok(if nodes.len() == 1 { nodes.pop().unwrap() } else { Node::Concat(nodes) })
    }

    fn repeat(&mut self, depth: usize) -> Result<Node, Error> {
        let atom = self.atom(depth)?;
        let (min, max) = match self.peek() {
            Some('*') => {
                self.pos += 1;
                (0, None)
            }
            Some('+') => {
                self.pos += 1;
                (1, None)
            }
            Some('?') => {
                self.pos += 1;
                (0, Some(1))
            }
            Some('{') => {
                let save = self.pos;
                self.pos += 1;
                match self.bounds() {
                    Some(b) => b,
                    None => {
                        // Not a valid bound — treat '{' as a literal.
                        self.pos = save;
                        return Ok(atom);
                    }
                }
            }
            _ => return Ok(atom),
        };
        if let Some(m) = max {
            if m < min {
                return Err(verr("regex: quantifier max < min"));
            }
        }
        if min > MAX_COUNTED_REPEAT || max.unwrap_or(0) > MAX_COUNTED_REPEAT {
            return Err(verr(format!("regex: counted repeat exceeds {MAX_COUNTED_REPEAT}")));
        }
        let greedy = if self.peek() == Some('?') {
            self.pos += 1;
            false
        } else {
            true
        };
        if matches!(atom, Node::Start | Node::End) {
            return Err(verr("regex: cannot quantify an anchor"));
        }
        Ok(Node::Repeat { node: Box::new(atom), min, max, greedy })
    }

    fn bounds(&mut self) -> Option<(usize, Option<usize>)> {
        let mut min_s = String::new();
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            min_s.push(self.bump().unwrap());
        }
        if min_s.is_empty() {
            return None;
        }
        let min: usize = min_s.parse().ok()?;
        match self.bump() {
            Some('}') => Some((min, Some(min))),
            Some(',') => {
                if self.peek() == Some('}') {
                    self.pos += 1;
                    return Some((min, None));
                }
                let mut max_s = String::new();
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    max_s.push(self.bump().unwrap());
                }
                if self.bump() != Some('}') {
                    return None;
                }
                Some((min, Some(max_s.parse().ok()?)))
            }
            _ => None,
        }
    }

    fn atom(&mut self, depth: usize) -> Result<Node, Error> {
        match self.bump() {
            None => Err(verr("regex: unexpected end of pattern")),
            Some('(') => {
                let index = if self.peek() == Some('?') {
                    self.pos += 1;
                    if self.bump() != Some(':') {
                        return Err(verr("regex: only (?:...) group modifier is supported"));
                    }
                    None
                } else {
                    self.group_index += 1;
                    Some(self.group_index)
                };
                let inner = self.alternation(depth + 1)?;
                if self.bump() != Some(')') {
                    return Err(verr("regex: missing ')'"));
                }
                Ok(Node::Group { index, node: Box::new(inner) })
            }
            Some('[') => self.class(),
            Some('.') => Ok(Node::AnyChar),
            Some('^') => Ok(Node::Start),
            Some('$') => Ok(Node::End),
            Some('*') | Some('+') | Some('?') => Err(verr("regex: quantifier with nothing to repeat")),
            Some('\\') => {
                let c = self.bump().ok_or_else(|| verr("regex: trailing backslash"))?;
                Ok(match c {
                    'd' => Node::Class { negated: false, items: vec![ClassItem::Digit] },
                    'D' => Node::Class { negated: false, items: vec![ClassItem::NotDigit] },
                    'w' => Node::Class { negated: false, items: vec![ClassItem::Word] },
                    'W' => Node::Class { negated: false, items: vec![ClassItem::NotWord] },
                    's' => Node::Class { negated: false, items: vec![ClassItem::Space] },
                    'S' => Node::Class { negated: false, items: vec![ClassItem::NotSpace] },
                    'n' => Node::Char('\n'),
                    'r' => Node::Char('\r'),
                    't' => Node::Char('\t'),
                    c => Node::Char(c),
                })
            }
            Some(c) => Ok(Node::Char(c)),
        }
    }

    fn class(&mut self) -> Result<Node, Error> {
        let negated = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };
        let mut items = Vec::new();
        // ']' as the first item is a literal.
        if self.peek() == Some(']') {
            self.pos += 1;
            items.push(ClassItem::Single(']'));
        }
        loop {
            match self.bump() {
                None => return Err(verr("regex: unterminated character class")),
                Some(']') => break,
                Some('\\') => {
                    let c = self.bump().ok_or_else(|| verr("regex: trailing backslash in class"))?;
                    items.push(match c {
                        'd' => ClassItem::Digit,
                        'D' => ClassItem::NotDigit,
                        'w' => ClassItem::Word,
                        'W' => ClassItem::NotWord,
                        's' => ClassItem::Space,
                        'S' => ClassItem::NotSpace,
                        'n' => ClassItem::Single('\n'),
                        'r' => ClassItem::Single('\r'),
                        't' => ClassItem::Single('\t'),
                        c => ClassItem::Single(c),
                    });
                }
                Some(lo) => {
                    if self.peek() == Some('-')
                        && self.chars.get(self.pos + 1).is_some_and(|&c| c != ']')
                    {
                        self.pos += 1; // consume '-'
                        let hi = self.bump().unwrap();
                        let hi = if hi == '\\' {
                            self.bump().ok_or_else(|| verr("regex: trailing backslash in class"))?
                        } else {
                            hi
                        };
                        if hi < lo {
                            return Err(verr("regex: invalid class range"));
                        }
                        items.push(ClassItem::Range(lo, hi));
                    } else {
                        items.push(ClassItem::Single(lo));
                    }
                }
            }
        }
        if items.is_empty() {
            return Err(verr("regex: empty character class"));
        }
        Ok(Node::Class { negated, items })
    }
}

/// Replace all matches using a template where `$0`..`$9` refer to capture
/// groups and `$$` is a literal `$`.
pub fn replace_all(re: &Regex, text: &str, template: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let matches = re.find_iter_chars(&chars);
    let mut out = String::new();
    let mut last = 0;
    for m in &matches {
        out.extend(chars[last..m.start].iter());
        expand_template(template, m, &chars, &mut out);
        last = m.end;
    }
    out.extend(chars[last..].iter());
    out
}

fn expand_template(template: &str, m: &Match, chars: &[char], out: &mut String) {
    let t: Vec<char> = template.chars().collect();
    let mut i = 0;
    while i < t.len() {
        if t[i] == '$' && i + 1 < t.len() {
            let c = t[i + 1];
            if c == '$' {
                out.push('$');
                i += 2;
                continue;
            }
            if let Some(d) = c.to_digit(10) {
                if let Some(Some((s, e))) = m.groups.get(d as usize) {
                    out.extend(chars[*s..*e].iter());
                }
                i += 2;
                continue;
            }
        }
        out.push(t[i]);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, text: &str) -> Option<(usize, usize)> {
        Regex::new(pat).unwrap().find(text).map(|mm| (mm.start, mm.end))
    }

    #[test]
    fn literals_and_classes() {
        assert_eq!(m("abc", "xxabcxx"), Some((2, 5)));
        assert_eq!(m("[a-c]+", "zzabcabz"), Some((2, 7)));
        assert_eq!(m("[^a-z]+", "abc123"), Some((3, 6)));
        assert_eq!(m(r"\d{3}-\d{2}", "x 123-45 y"), Some((2, 8)));
        assert!(Regex::new(r"^\w+$").unwrap().is_match("hello_world1"));
        assert!(!Regex::new(r"^\d+$").unwrap().is_match("12a"));
    }

    #[test]
    fn quantifiers() {
        assert_eq!(m("a*", "aaa"), Some((0, 3)));
        assert_eq!(m("a*?", "aaa"), Some((0, 0))); // lazy
        assert_eq!(m("a{2,3}", "aaaa"), Some((0, 3)));
        assert_eq!(m("a{2}", "a"), None);
        assert_eq!(m("ab?c", "ac"), Some((0, 2)));
        assert_eq!(m("(ab)+", "ababab"), Some((0, 6)));
        assert_eq!(m("a{2,}", "aaaa"), Some((0, 4)));
    }

    #[test]
    fn alternation_and_groups() {
        let re = Regex::new(r"(\d+)-(\d+)").unwrap();
        let mm = re.find("call 555-1234 now").unwrap();
        assert_eq!(mm.groups[1], Some((5, 8)));
        assert_eq!(mm.groups[2], Some((9, 13)));
        assert_eq!(m("cat|dog", "hotdog"), Some((3, 6)));
        assert_eq!(m("(?:ab)+c", "ababc"), Some((0, 5)));
    }

    #[test]
    fn group_capture_last_iteration() {
        let re = Regex::new(r"(a|b)+").unwrap();
        let mm = re.find("abab").unwrap();
        assert_eq!(mm.end, 4);
        assert_eq!(mm.groups[1], Some((3, 4))); // last iteration wins
    }

    #[test]
    fn unmatched_group_is_none() {
        let re = Regex::new(r"(x)?y").unwrap();
        let mm = re.find("y").unwrap();
        assert_eq!(mm.groups[1], None);
    }

    #[test]
    fn anchors() {
        assert_eq!(m("^abc", "abcdef"), Some((0, 3)));
        assert_eq!(m("^abc", "xabc"), None);
        assert_eq!(m("def$", "abcdef"), Some((3, 6)));
        assert!(Regex::new("^$").unwrap().is_match(""));
    }

    #[test]
    fn email_like() {
        let re = Regex::new(r"^[\w.+-]+@[\w-]+(\.[\w-]+)+$").unwrap();
        assert!(re.is_match("a.b+tag@example.co.uk"));
        assert!(!re.is_match("not-an-email"));
        assert!(!re.is_match("a@b"));
    }

    #[test]
    fn find_iter_and_replace() {
        let re = Regex::new(r"\d+").unwrap();
        let ms = re.find_iter("a1b22c333");
        assert_eq!(ms.len(), 3);
        assert_eq!(replace_all(&re, "a1b22c333", "<$0>"), "a<1>b<22>c<333>");
        let re2 = Regex::new(r"(\w+)@(\w+)").unwrap();
        assert_eq!(replace_all(&re2, "bob@corp", "$2/$1"), "corp/bob");
    }

    #[test]
    fn full_match() {
        let re = Regex::new(r"\d{5}").unwrap();
        assert!(re.is_full_match("12345"));
        assert!(!re.is_full_match("123456"));
        assert!(!re.is_full_match("z12345"));
    }

    #[test]
    fn zero_width_repeat_terminates() {
        assert_eq!(m("(a?)*", "b"), Some((0, 0)));
        assert_eq!(m("(a|)+x", "aax"), Some((0, 3)));
        assert_eq!(m("()*x", "x"), Some((0, 1)));
    }

    #[test]
    fn pathological_is_fast() {
        // Classic exponential case for naive backtrackers.
        let re = Regex::new("(a+)+$").unwrap();
        let text = "a".repeat(40) + "b";
        assert!(!re.is_match(&text));
        let re2 = Regex::new("(a|a)+$").unwrap();
        assert!(!re2.is_match(&text));
    }

    #[test]
    fn class_edge_cases() {
        assert_eq!(m(r"[]a]+", "]a]"), Some((0, 3))); // ] first is literal
        assert_eq!(m(r"[a-]", "-"), Some((0, 1))); // trailing - is literal
        assert_eq!(m(r"[\]]", "]"), Some((0, 1)));
        assert_eq!(m(r"[.+*]", "+"), Some((0, 1))); // metachars literal in class
    }

    #[test]
    fn unicode() {
        assert_eq!(m("é+", "café"), Some((3, 4)));
        assert!(Regex::new(r"^\w+$").unwrap().is_match("naïve"));
    }

    #[test]
    fn parse_errors() {
        for bad in ["(", ")", "a{2,1}", "[z-a]", "*a", "(?<x>a)", "[", "a\\", "a{2000}"] {
            assert!(Regex::new(bad).is_err(), "should reject: {bad}");
        }
        // Literal '{' when not a bound
        assert_eq!(m("a{x", "a{x"), Some((0, 3)));
    }
}
