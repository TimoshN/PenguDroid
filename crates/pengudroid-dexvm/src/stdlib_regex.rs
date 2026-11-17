//! Minimal Java `Pattern` / `Matcher` regex engine for `pengudroid-dexvm`.

use crate::stdlib::BuiltinResult;
use crate::value::{Heap, HeapObject, Value};

#[derive(Clone, Debug)]
enum Node {
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    Group { idx: Option<usize>, inner: Box<Node> },
    Repeat { inner: Box<Node>, min: usize, max: Option<usize>, lazy: bool },
    Atom(Atom),
    AnchorStart,
    AnchorEnd,
}

#[derive(Clone, Debug)]
enum Atom {
    Any,
    Char(char),
    Digit(bool),
    Word(bool),
    Space(bool),
    Class { negated: bool, ranges: Vec<(char, char)>, chars: Vec<char>, digits: bool, words: bool, spaces: bool },
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    next_group: usize,
}

impl Parser {
    fn parse(pattern: &str) -> Result<(Node, usize), String> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut p = Self { chars, pos: 0, next_group: 1 };
        let node = p.parse_alt()?;
        Ok((node, p.next_group - 1))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    fn parse_alt(&mut self) -> Result<Node, String> {
        let mut branches = vec![self.parse_seq()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_seq()?);
        }
        Ok(if branches.len() == 1 { branches.pop().unwrap() } else { Node::Alt(branches) })
    }

    fn parse_seq(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == ')' || c == '|' {
                break;
            }
            items.push(self.parse_quantified()?);
        }
        Ok(if items.len() == 1 { items.pop().unwrap() } else { Node::Seq(items) })
    }

    fn parse_quantified(&mut self) -> Result<Node, String> {
        let atom = self.parse_atom()?;
        let (min, max) = match self.peek() {
            Some('*') => { self.pos += 1; (0, None) }
            Some('+') => { self.pos += 1; (1, None) }
            Some('?') => { self.pos += 1; (0, Some(1)) }
            Some('{') => self.parse_brace_quant().unwrap_or((1, Some(1))),
            _ => return Ok(atom),
        };
        let lazy = if self.peek() == Some('?') { self.pos += 1; true } else { false };
        Ok(Node::Repeat { inner: Box::new(atom), min, max, lazy })
    }

    fn parse_brace_quant(&mut self) -> Option<(usize, Option<usize>)> {
        let save = self.pos;
        self.pos += 1;
        let min = self.read_uint()?;
        let max = if self.peek() == Some(',') {
            self.pos += 1;
            if self.peek() == Some('}') { None } else { Some(self.read_uint()?) }
        } else {
            Some(min)
        };
        if self.next() == Some('}') {
            Some((min, max))
        } else {
            self.pos = save;
            None
        }
    }

    fn read_uint(&mut self) -> Option<usize> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.pos == start { return None; }
        self.chars[start..self.pos].iter().collect::<String>().parse().ok()
    }

    fn parse_atom(&mut self) -> Result<Node, String> {
        match self.next() {
            Some('^') => Ok(Node::AnchorStart),
            Some('$') => Ok(Node::AnchorEnd),
            Some('.') => Ok(Node::Atom(Atom::Any)),
            Some('(') => {
                let idx = if self.chars.get(self.pos..self.pos + 2) == Some(&['?', ':']) {
                    self.pos += 2;
                    None
                } else {
                    let g = self.next_group;
                    self.next_group += 1;
                    Some(g)
                };
                let inner = self.parse_alt()?;
                if self.peek() == Some(')') {
                    self.pos += 1;
                }
                Ok(Node::Group { idx, inner: Box::new(inner) })
            }
            Some('[') => self.parse_class(),
            Some('\\') => Ok(Node::Atom(self.parse_escape())),
            Some(c) => Ok(Node::Atom(Atom::Char(c))),
            None => Ok(Node::Seq(Vec::new())),
        }
    }

    fn parse_escape(&mut self) -> Atom {
        match self.next() {
            Some('d') => Atom::Digit(false),
            Some('D') => Atom::Digit(true),
            Some('w') => Atom::Word(false),
            Some('W') => Atom::Word(true),
            Some('s') => Atom::Space(false),
            Some('S') => Atom::Space(true),
            Some('n') => Atom::Char('\n'),
            Some('r') => Atom::Char('\r'),
            Some('t') => Atom::Char('\t'),
            Some(c) => Atom::Char(c),
            None => Atom::Char('\\'),
        }
    }

    fn parse_class(&mut self) -> Result<Node, String> {
        let negated = if self.peek() == Some('^') { self.pos += 1; true } else { false };
        let mut ranges = Vec::new();
        let mut chars = Vec::new();
        let (mut digits, mut words, mut spaces) = (false, false, false);
        let mut first = true;
        while let Some(c) = self.next() {
            if c == ']' && !first {
                break;
            }
            first = false;
            if c == '\\' {
                match self.parse_escape() {
                    Atom::Digit(false) => digits = true,
                    Atom::Word(false) => words = true,
                    Atom::Space(false) => spaces = true,
                    Atom::Char(ch) => {
                        if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&n| n != ']') {
                            self.pos += 1;
                            if let Some(end) = self.next() {
                                ranges.push((ch, end));
                                continue;
                            }
                        }
                        chars.push(ch);
                    }
                    _ => {}
                }
            } else if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&n| n != ']') {
                self.pos += 1;
                if let Some(end) = self.next() {
                    ranges.push((c, end));
                } else {
                    chars.push(c);
                }
            } else {
                chars.push(c);
            }
        }
        Ok(Node::Atom(Atom::Class { negated, ranges, chars, digits, words, spaces }))
    }
}

fn eq_char(a: char, b: char, ci: bool) -> bool {
    a == b || (ci && a.eq_ignore_ascii_case(&b))
}

fn match_atom(atom: &Atom, ch: char, ci: bool, dotall: bool) -> bool {
    match atom {
        Atom::Any => dotall || (ch != '\n' && ch != '\r'),
        Atom::Char(c) => eq_char(*c, ch, ci),
        Atom::Digit(neg) => ch.is_ascii_digit() ^ neg,
        Atom::Word(neg) => (ch.is_ascii_alphanumeric() || ch == '_') ^ neg,
        Atom::Space(neg) => ch.is_whitespace() ^ neg,
        Atom::Class { negated, ranges, chars, digits, words, spaces } => {
            let hit = (*digits && ch.is_ascii_digit())
                || (*words && (ch.is_ascii_alphanumeric() || ch == '_'))
                || (*spaces && ch.is_whitespace())
                || chars.iter().any(|&c| eq_char(c, ch, ci))
                || ranges.iter().any(|&(lo, hi)| {
                    (lo..=hi).contains(&ch)
                        || (ci && ((lo..=hi).contains(&ch.to_ascii_lowercase()) || (lo..=hi).contains(&ch.to_ascii_uppercase())))
                });
            hit ^ negated
        }
    }
}

fn eval_node(
    node: &Node,
    input: &[char],
    pos: usize,
    ci: bool,
    dotall: bool,
    groups: &mut Vec<Option<(usize, usize)>>,
    cont: &mut dyn FnMut(usize, &mut Vec<Option<(usize, usize)>>) -> bool,
) -> bool {
    match node {
        Node::AnchorStart => pos == 0 && cont(pos, groups),
        Node::AnchorEnd => pos == input.len() && cont(pos, groups),
        Node::Atom(atom) => input.get(pos).is_some_and(|&ch| match_atom(atom, ch, ci, dotall)) && cont(pos + 1, groups),
        Node::Seq(nodes) => eval_seq(nodes, input, pos, ci, dotall, groups, cont),
        Node::Alt(branches) => {
            for b in branches {
                let saved = groups.clone();
                if eval_node(b, input, pos, ci, dotall, groups, cont) {
                    return true;
                }
                *groups = saved;
            }
            false
        }
        Node::Group { idx, inner } => {
            let start = pos;
            eval_node(inner, input, pos, ci, dotall, groups, &mut |end, g| {
                let prev = idx.and_then(|i| g.get(i).copied().flatten());
                if let Some(i) = *idx {
                    if i < g.len() {
                        g[i] = Some((start, end));
                    }
                }
                if cont(end, g) {
                    return true;
                }
                if let Some(i) = *idx {
                    if i < g.len() {
                        g[i] = prev;
                    }
                }
                false
            })
        }
        Node::Repeat { inner, min, max, lazy } => {
            eval_repeat(inner, *min, *max, *lazy, 0, input, pos, ci, dotall, groups, cont)
        }
    }
}

fn eval_seq(
    nodes: &[Node],
    input: &[char],
    pos: usize,
    ci: bool,
    dotall: bool,
    groups: &mut Vec<Option<(usize, usize)>>,
    cont: &mut dyn FnMut(usize, &mut Vec<Option<(usize, usize)>>) -> bool,
) -> bool {
    let Some((first, rest)) = nodes.split_first() else {
        return cont(pos, groups);
    };
    eval_node(first, input, pos, ci, dotall, groups, &mut |next_pos, g| {
        eval_seq(rest, input, next_pos, ci, dotall, g, cont)
    })
}

fn eval_repeat(
    inner: &Node,
    min: usize,
    max: Option<usize>,
    lazy: bool,
    count: usize,
    input: &[char],
    pos: usize,
    ci: bool,
    dotall: bool,
    groups: &mut Vec<Option<(usize, usize)>>,
    cont: &mut dyn FnMut(usize, &mut Vec<Option<(usize, usize)>>) -> bool,
) -> bool {
    if count < min {
        return eval_node(inner, input, pos, ci, dotall, groups, &mut |next_pos, g| {
            if next_pos == pos && min > count + 1 {
                return false;
            }
            eval_repeat(inner, min, max, lazy, count + 1, input, next_pos, ci, dotall, g, cont)
        });
    }
    if lazy && cont(pos, groups) {
        return true;
    }
    if max.is_none_or(|m| count < m) {
        let saved = groups.clone();
        let matched = eval_node(inner, input, pos, ci, dotall, groups, &mut |next_pos, g| {
            if next_pos == pos {
                return false;
            }
            eval_repeat(inner, min, max, lazy, count + 1, input, next_pos, ci, dotall, g, cont)
        });
        if matched {
            return true;
        }
        *groups = saved;
    }
    !lazy && cont(pos, groups)
}

pub fn run_match(pattern: &str, flags: i32, input: &str, start: usize, anchored_start: bool, full_match: bool) -> Option<Vec<Option<(usize, usize)>>> {
    let (node, n_groups) = Parser::parse(pattern).ok()?;
    let chars: Vec<char> = input.chars().collect();
    if start > chars.len() {
        return None;
    }
    let ci = (flags & 2) != 0;
    let dotall = (flags & 32) != 0;
    let end_limit = if anchored_start { start } else { chars.len() };
    for s in start..=end_limit {
        let mut groups = vec![None; n_groups + 1];
        let ok = eval_node(&node, &chars, s, ci, dotall, &mut groups, &mut |end_pos, g| {
            if full_match && end_pos != chars.len() {
                return false;
            }
            g[0] = Some((s, end_pos));
            true
        });
        if ok {
            return Some(groups);
        }
    }
    None
}

pub fn try_invoke(
    heap: &mut Heap,
    class_name: &str,
    method_name: &str,
    args: &[Value],
) -> Result<Option<BuiltinResult>, String> {
    match (class_name, method_name) {
        ("Ljava/util/regex/Pattern;", "compile") => {
            let pat = args.first().and_then(Value::as_ref).and_then(|id| heap.as_str(id)).unwrap_or("").to_string();
            let flags = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            let p_sid = heap.new_string(pat);
            let id = heap.new_instance("Ljava/util/regex/Pattern;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(id) {
                fields.insert("pattern".into(), Value::Ref(Some(p_sid)));
                fields.insert("flags".into(), Value::Int(flags));
            }
            Ok(Some(BuiltinResult::Handled(Value::Ref(Some(id)))))
        }
        ("Ljava/util/regex/Pattern;", "matches") => {
            let pat = args.first().and_then(Value::as_ref).and_then(|id| heap.as_str(id)).unwrap_or("");
            let input = args.get(1).and_then(Value::as_ref).and_then(|id| heap.as_str(id)).unwrap_or("");
            let ok = run_match(pat, 0, input, 0, true, true).is_some();
            Ok(Some(BuiltinResult::Handled(Value::Int(i32::from(ok)))))
        }
        ("Ljava/util/regex/Pattern;", "matcher") => {
            let pat_id = args.first().and_then(Value::as_ref);
            let (pat_val, flags) = pat_id
                .and_then(|id| match heap.get(id) {
                    Some(HeapObject::Instance { fields, .. }) => Some((
                        fields.get("pattern").cloned().unwrap_or(Value::Ref(None)),
                        fields.get("flags").and_then(|v| v.as_int().ok()).unwrap_or(0),
                    )),
                    _ => None,
                })
                .unwrap_or((Value::Ref(None), 0));
            let input_val = args.get(1).cloned().unwrap_or(Value::Ref(None));
            let m = heap.new_instance("Ljava/util/regex/Matcher;");
            if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(m) {
                fields.insert("pattern".into(), pat_val);
                fields.insert("flags".into(), Value::Int(flags));
                fields.insert("input".into(), input_val);
                fields.insert("pos".into(), Value::Int(0));
                fields.insert("matched".into(), Value::Int(0));
            }
            Ok(Some(BuiltinResult::Handled(Value::Ref(Some(m)))))
        }
        ("Ljava/util/regex/Matcher;", "matches" | "lookingAt" | "find") => {
            let Some(mid) = args.first().and_then(Value::as_ref) else {
                return Ok(Some(BuiltinResult::Handled(Value::Int(0))));
            };
            let (pat, flags, input, pos) = match heap.get(mid) {
                Some(HeapObject::Instance { fields, .. }) => (
                    fields.get("pattern").and_then(Value::as_ref).and_then(|i| heap.as_str(i)).unwrap_or("").to_string(),
                    fields.get("flags").and_then(|v| v.as_int().ok()).unwrap_or(0),
                    fields.get("input").and_then(Value::as_ref).and_then(|i| heap.as_str(i)).unwrap_or("").to_string(),
                    fields.get("pos").and_then(|v| v.as_int().ok()).unwrap_or(0).max(0) as usize,
                ),
                _ => return Ok(Some(BuiltinResult::Handled(Value::Int(0)))),
            };
            let start = if method_name == "find" {
                args.get(1).and_then(|v| v.as_int().ok()).map(|s| s.max(0) as usize).unwrap_or(pos)
            } else {
                pos
            };
            let anchored = method_name != "find";
            let full = method_name == "matches";
            if let Some(groups) = run_match(&pat, flags, &input, start, anchored, full) {
                let chars: Vec<char> = input.chars().collect();
                let group_strs: Vec<Option<usize>> = groups
                    .iter()
                    .map(|g| g.map(|(s, e)| heap.new_string(chars[s..e].iter().collect())))
                    .collect();
                let next_pos = groups[0].map(|(_, e)| e).unwrap_or(start);
                let g_count = groups.len().saturating_sub(1) as i32;
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(mid) {
                    fields.insert("matched".into(), Value::Int(1));
                    fields.insert("groupCount".into(), Value::Int(g_count));
                    fields.insert("pos".into(), Value::Int(next_pos as i32));
                    for (idx, span) in groups.iter().enumerate() {
                        let (s, e) = span.map(|(a, b)| (a as i32, b as i32)).unwrap_or((-1, -1));
                        fields.insert(format!("gs:{idx}"), Value::Int(s));
                        fields.insert(format!("ge:{idx}"), Value::Int(e));
                        fields.insert(format!("gv:{idx}"), Value::Ref(group_strs[idx]));
                    }
                }
                Ok(Some(BuiltinResult::Handled(Value::Int(1))))
            } else {
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(mid) {
                    fields.insert("matched".into(), Value::Int(0));
                }
                Ok(Some(BuiltinResult::Handled(Value::Int(0))))
            }
        }
        ("Ljava/util/regex/Matcher;", "replaceAll") => {
            let Some(mid) = args.first().and_then(Value::as_ref) else {
                return Ok(Some(BuiltinResult::Handled(Value::Ref(None))));
            };
            let repl = args
                .get(1)
                .and_then(Value::as_ref)
                .and_then(|i| heap.as_str(i))
                .unwrap_or("")
                .to_string();
            let (pat, flags, input) = match heap.get(mid) {
                Some(HeapObject::Instance { fields, .. }) => (
                    fields.get("pattern").and_then(Value::as_ref).and_then(|i| heap.as_str(i)).unwrap_or("").to_string(),
                    fields.get("flags").and_then(|v| v.as_int().ok()).unwrap_or(0),
                    fields.get("input").and_then(Value::as_ref).and_then(|i| heap.as_str(i)).unwrap_or("").to_string(),
                ),
                _ => return Ok(Some(BuiltinResult::Handled(Value::Ref(None)))),
            };
            let chars: Vec<char> = input.chars().collect();
            let mut out = String::new();
            let mut pos = 0usize;
            while pos <= chars.len() {
                if let Some(groups) = run_match(&pat, flags, &input, pos, false, false) {
                    if let Some((s, e)) = groups[0] {
                        for &c in &chars[pos..s] {
                            out.push(c);
                        }
                        out.push_str(&repl);
                        pos = if e > pos { e } else {
                            if e < chars.len() {
                                out.push(chars[e]);
                            }
                            e + 1
                        };
                        continue;
                    }
                }
                if pos < chars.len() {
                    for &c in &chars[pos..] {
                        out.push(c);
                    }
                }
                break;
            }
            Ok(Some(BuiltinResult::Handled(Value::Ref(Some(heap.new_string(out))))))
        }
        ("Ljava/util/regex/Matcher;", "region") => {
            if let Some(mid) = args.first().and_then(Value::as_ref) {
                let start = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0).max(0);
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(mid) {
                    fields.insert("pos".into(), Value::Int(start));
                }
            }
            Ok(Some(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None)))))
        }
        ("Ljava/util/regex/Matcher;", "usePattern") => {
            if let (Some(mid), Some(pid)) = (args.first().and_then(Value::as_ref), args.get(1).and_then(Value::as_ref)) {
                let (pv, fl) = match heap.get(pid) {
                    Some(HeapObject::Instance { fields, .. }) => (
                        fields.get("pattern").cloned().unwrap_or(Value::Ref(None)),
                        fields.get("flags").and_then(|v| v.as_int().ok()).unwrap_or(0),
                    ),
                    _ => (Value::Ref(None), 0),
                };
                if let Some(HeapObject::Instance { fields, .. }) = heap.get_mut(mid) {
                    fields.insert("pattern".into(), pv);
                    fields.insert("flags".into(), Value::Int(fl));
                }
            }
            Ok(Some(BuiltinResult::Handled(args.first().cloned().unwrap_or(Value::Ref(None)))))
        }
        ("Ljava/util/regex/Matcher;", "groupCount") => {
            let c = args.first().and_then(Value::as_ref).and_then(|mid| match heap.get(mid) {
                Some(HeapObject::Instance { fields, .. }) => fields.get("groupCount").and_then(|v| v.as_int().ok()),
                _ => None,
            }).unwrap_or(0);
            Ok(Some(BuiltinResult::Handled(Value::Int(c))))
        }
        ("Ljava/util/regex/Matcher;", "group") => {
            let idx = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            let val = args.first().and_then(Value::as_ref).and_then(|mid| match heap.get(mid) {
                Some(HeapObject::Instance { fields, .. }) => fields.get(&format!("gv:{idx}")).cloned(),
                _ => None,
            }).unwrap_or(Value::Ref(None));
            Ok(Some(BuiltinResult::Handled(val)))
        }
        ("Ljava/util/regex/Matcher;", "start" | "end") => {
            let idx = args.get(1).and_then(|v| v.as_int().ok()).unwrap_or(0);
            let key = if method_name == "start" { format!("gs:{idx}") } else { format!("ge:{idx}") };
            let val = args.first().and_then(Value::as_ref).and_then(|mid| match heap.get(mid) {
                Some(HeapObject::Instance { fields, .. }) => fields.get(&key).and_then(|v| v.as_int().ok()),
                _ => None,
            }).unwrap_or(-1);
            Ok(Some(BuiltinResult::Handled(Value::Int(val))))
        }
        _ => Ok(None),
    }
}
