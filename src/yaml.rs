//! The subset of YAML the configuration file uses (DESIGN §7.2), parsed and
//! written back by hand: a YAML crate would cost more than the rest of the
//! client's parsing put together (DESIGN §9).
//!
//! Supported: block mappings and sequences (including `- key: value` items),
//! plain, single- and double-quoted scalars, one-line flow collections
//! (`[a, b]`, `{k: v}`), comments and blank lines. Anchors, aliases, tags,
//! block scalars (`|`, `>`), multi-line flow collections and several
//! documents are refused with the line they are on.
//!
//! The tree keeps each node's line, the comment and blank lines before it and
//! a comment at the end of its line, so [`emit`] writes a file back with its
//! comments and order intact (indentation is normalised to two spaces).

use std::fmt;

/// A parsed file: the root node and the comment lines after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub root: Node,
    /// Comment and blank lines at the end of the file.
    pub tail: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub value: Value,
    /// 1-based line of the node (of its key, for a mapping value); 0 for a
    /// node built in code.
    pub line: usize,
    /// Comment (`# …`) and blank (`""`) lines just before the node.
    pub before: Vec<String>,
    /// A `# comment` at the end of the node's line.
    pub comment: Option<String>,
    /// Written as a one-line flow collection (`[a, b]`, `{k: v}`).
    pub flow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Scalar(Scalar),
    Seq(Vec<Node>),
    Map(Vec<(String, Node)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Single,
    Double,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scalar {
    /// The value with quotes and escapes resolved.
    pub text: String,
    pub style: Style,
}

/// A parse error and the 1-based line it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// How deeply a configuration may nest before it is refused (acs-q4f).
///
/// Both parsers recurse — the block one through maps and sequences, the
/// flow one through `[` and `{` — and neither had a limit, so a few
/// thousand brackets, which fit on one line, overflowed the stack. With
/// `panic = "abort"` in the release profile that is not an error a caller
/// can report but an immediate kill. Real configurations nest three or
/// four deep.
const MAX_DEPTH: usize = 64;

/// The message a configuration too deep to parse gets.
fn too_deep<T>(line: usize) -> Result<T> {
    err(
        line,
        format!("nested more than {MAX_DEPTH} deep; this is not a configuration acs can use"),
    )
}

fn err<T>(line: usize, msg: impl Into<String>) -> Result<T> {
    Err(Error {
        line,
        msg: msg.into(),
    })
}

impl Node {
    pub fn new(value: Value) -> Node {
        Node {
            value,
            line: 0,
            before: Vec::new(),
            comment: None,
            flow: false,
        }
    }

    /// A string scalar, quoted when plain would read as something else.
    pub fn string(s: &str) -> Node {
        let style = if plain_ok(s) && !looks_typed(s) {
            Style::Plain
        } else {
            Style::Double
        };
        Node::new(Value::Scalar(Scalar {
            text: s.to_string(),
            style,
        }))
    }

    pub fn bool(b: bool) -> Node {
        Node::new(Value::Scalar(Scalar {
            text: if b { "true" } else { "false" }.into(),
            style: Style::Plain,
        }))
    }
}

impl Value {
    /// The entries of a mapping, if this is one.
    pub fn map(&self) -> Option<&Vec<(String, Node)>> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn map_mut(&mut self) -> Option<&mut Vec<(String, Node)>> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    /// What kind of value this is, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "nothing",
            Value::Scalar(_) => "a single value",
            Value::Seq(_) => "a list",
            Value::Map(_) => "a mapping",
        }
    }
}

// ---- parsing ---------------------------------------------------------------

struct Line {
    no: usize,
    indent: usize,
    /// Content after the indentation, trailing whitespace removed. Empty for
    /// a blank line; starts with `#` for a comment line.
    text: String,
}

impl Line {
    fn trivia(&self) -> bool {
        self.text.is_empty() || self.text.starts_with('#')
    }
}

struct Parser {
    lines: Vec<Line>,
    pos: usize,
    /// How deep the block parser is (acs-q4f).
    depth: usize,
}

/// Parse a configuration file.
pub fn parse(src: &str) -> Result<Document> {
    // A BOM is not part of the first key: editors that write one produce a
    // file every other YAML reader accepts (acs-qjx).
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let mut lines = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let no = i + 1;
        let raw = raw.trim_end();
        let text = raw.trim_start_matches(' ');
        if text.starts_with('\t') {
            return err(no, "tabs are not allowed for indentation; use spaces");
        }
        let indent = raw.len() - text.len();
        if indent == 0 && (text == "---" || text.starts_with("--- ")) {
            if lines.iter().any(|l: &Line| !l.trivia()) {
                return err(no, "only one YAML document is supported");
            }
            continue;
        }
        if indent == 0 && text == "..." {
            continue;
        }
        lines.push(Line {
            no,
            indent,
            text: text.to_string(),
        });
    }
    let mut p = Parser {
        lines,
        pos: 0,
        depth: 0,
    };
    let root = match p.peek() {
        None => Node::new(Value::Null),
        Some(i) => {
            let indent = p.lines[i].indent;
            let before = p.take_before(i);
            let mut node = p.block_node(indent)?;
            node.before.splice(0..0, before);
            node
        }
    };
    if let Some(i) = p.peek() {
        return err(p.lines[i].no, "unexpected content (check the indentation)");
    }
    let tail = p.take_before(p.lines.len());
    Ok(Document { root, tail })
}

fn is_seq_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl Parser {
    /// Index of the next line that is not a comment or blank.
    fn peek(&self) -> Option<usize> {
        (self.pos..self.lines.len()).find(|&i| !self.lines[i].trivia())
    }

    /// Consume the comment and blank lines up to `upto`.
    fn take_before(&mut self, upto: usize) -> Vec<String> {
        let v = self.lines[self.pos..upto]
            .iter()
            .map(|l| l.text.clone())
            .collect();
        self.pos = upto;
        v
    }

    /// The block node starting at the current significant line, which has
    /// indentation `indent`.
    fn block_node(&mut self, indent: usize) -> Result<Node> {
        let i = self.peek().expect("a significant line");
        let line = self.lines[i].no;
        if self.depth >= MAX_DEPTH {
            return too_deep(line);
        }
        self.depth += 1;
        let out = self.block_node_inner(i, line, indent);
        self.depth -= 1;
        out
    }

    fn block_node_inner(&mut self, i: usize, line: usize, indent: usize) -> Result<Node> {
        let text = self.lines[i].text.clone();
        if is_seq_item(&text) {
            return Ok(Node {
                line,
                ..Node::new(self.seq(indent)?)
            });
        }
        if split_key(&text, line)?.is_some() {
            return Ok(Node {
                line,
                ..Node::new(self.map(indent)?)
            });
        }
        // A scalar on a line of its own.
        let before = self.take_before(i);
        self.pos = i + 1;
        let (value, comment) = inline(&text, line)?;
        if let Some(j) = self.peek() {
            if self.lines[j].indent > indent {
                return err(
                    self.lines[j].no,
                    "multi-line values are not supported; put the value on one line",
                );
            }
        }
        let (value, flow) = value.unwrap_or((Value::Null, false));
        Ok(Node {
            value,
            line,
            before,
            comment,
            flow,
        })
    }

    fn seq(&mut self, indent: usize) -> Result<Value> {
        let mut items = Vec::new();
        while let Some(i) = self.peek() {
            let (no, ind, text) = {
                let l = &self.lines[i];
                (l.no, l.indent, l.text.clone())
            };
            if ind < indent || (ind == indent && !is_seq_item(&text)) {
                break;
            }
            if ind > indent {
                return err(no, "unexpected indentation");
            }
            let before = self.take_before(i);
            let rest = text[1..].trim_start_matches(' ');
            let mut node = if rest.is_empty() || rest.starts_with('#') {
                // `-` alone: the item is the block below it, or nothing.
                self.pos = i + 1;
                let comment = (!rest.is_empty()).then(|| rest.to_string());
                let mut node = match self.peek() {
                    Some(j) if self.lines[j].indent > indent => {
                        let n = self.lines[j].indent;
                        self.block_node(n)?
                    }
                    _ => Node::new(Value::Null),
                };
                node.line = no;
                node.comment = comment;
                node
            } else {
                // `- rest`: parse `rest` as if it started its own line at
                // its column, so a `- key: value` item continues with the
                // keys aligned under `key`.
                let col = ind + (text.len() - rest.len());
                self.lines[i].indent = col;
                self.lines[i].text = rest.to_string();
                self.block_node(col)?
            };
            node.before.splice(0..0, before);
            items.push(node);
        }
        Ok(Value::Seq(items))
    }

    fn map(&mut self, indent: usize) -> Result<Value> {
        let mut entries: Vec<(String, Node)> = Vec::new();
        while let Some(i) = self.peek() {
            let (no, ind, text) = {
                let l = &self.lines[i];
                (l.no, l.indent, l.text.clone())
            };
            if ind < indent {
                break;
            }
            if ind > indent {
                return err(no, "unexpected indentation");
            }
            if is_seq_item(&text) {
                return err(no, "a list item where a 'key: value' line was expected");
            }
            let Some((key, rest)) = split_key(&text, no)? else {
                return err(no, "expected 'key: value'");
            };
            if entries.iter().any(|(k, _)| *k == key) {
                return err(no, format!("duplicate key '{key}'"));
            }
            let before = self.take_before(i);
            self.pos = i + 1;
            let (value, comment) = inline(rest, no)?;
            let (value, flow) = match value {
                Some(v) => v,
                None => match self.peek() {
                    Some(j) if self.lines[j].indent > indent => {
                        let n = self.lines[j].indent;
                        let child = self.block_node(n)?;
                        (child.value, false)
                    }
                    // `key:` followed by a list at the same indentation.
                    Some(j)
                        if self.lines[j].indent == indent && is_seq_item(&self.lines[j].text) =>
                    {
                        (self.seq(indent)?, false)
                    }
                    _ => (Value::Null, false),
                },
            };
            entries.push((
                key,
                Node {
                    value,
                    line: no,
                    before,
                    comment,
                    flow,
                },
            ));
        }
        Ok(Value::Map(entries))
    }
}

/// Split `key: rest` (or `key:`); `None` if the line is not a mapping entry.
fn split_key(text: &str, line: usize) -> Result<Option<(String, &str)>> {
    let (key, after) = if text.starts_with('"') || text.starts_with('\'') {
        let (s, used) = quoted(text, line)?;
        (s, &text[used..])
    } else {
        if text.starts_with(['#', '[', '{', '&', '*', '!', '|', '>']) || is_seq_item(text) {
            return Ok(None);
        }
        let b = text.as_bytes();
        // YAML 1.2 separates the key with a space or a tab (acs-qjx).
        let Some(colon) = (0..b.len())
            .find(|&i| b[i] == b':' && (i + 1 == b.len() || b[i + 1] == b' ' || b[i + 1] == b'\t'))
        else {
            return Ok(None);
        };
        let key = text[..colon].trim_end();
        if key.is_empty() || key.contains(" #") {
            return Ok(None);
        }
        (key.to_string(), &text[colon..])
    };
    let Some(rest) = after.strip_prefix(':') else {
        return Ok(None);
    };
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return Ok(None);
    }
    Ok(Some((key, rest)))
}

/// A value written after `key:` or `- `: the value (and whether it was a
/// flow collection) and a trailing comment.
#[allow(clippy::type_complexity)]
fn inline(text: &str, line: usize) -> Result<(Option<(Value, bool)>, Option<String>)> {
    let t = text.trim_start_matches([' ', '\t']);
    if t.is_empty() {
        return Ok((None, None));
    }
    if t.starts_with('#') {
        return Ok((None, Some(t.to_string())));
    }
    let (value, used, flow) = match t.as_bytes()[0] {
        b'"' | b'\'' => {
            let (s, used) = quoted(t, line)?;
            let style = if t.starts_with('"') {
                Style::Double
            } else {
                Style::Single
            };
            (Value::Scalar(Scalar { text: s, style }), used, false)
        }
        b'[' | b'{' => {
            let mut f = Flow {
                s: t.as_bytes(),
                i: 0,
                text: t,
                line,
                depth: 0,
            };
            let v = f.value()?;
            (v, f.i, true)
        }
        b'|' | b'>' => {
            return err(
                line,
                "block scalars (| and >) are not supported; put the value on one line",
            )
        }
        b'&' | b'*' | b'!' => return err(line, "anchors, aliases and tags are not supported"),
        b'@' | b'`' => return err(line, "a value starting with @ or ` must be quoted"),
        _ => {
            let end = t.find(" #").unwrap_or(t.len());
            let s = t[..end].trim_end();
            if s.contains(": ") || s.ends_with(':') {
                return err(line, format!("a value containing ': ' must be quoted: {s}"));
            }
            let v = Value::Scalar(Scalar {
                text: s.to_string(),
                style: Style::Plain,
            });
            (v, end, false)
        }
    };
    let rest = t[used..].trim_start_matches(' ');
    let comment = if rest.is_empty() {
        None
    } else if rest.starts_with('#') && (used == t.len() || t.as_bytes()[used] == b' ') {
        Some(rest.to_string())
    } else {
        return err(line, format!("unexpected text after the value: {rest}"));
    };
    Ok((Some((value, flow)), comment))
}

/// A quoted scalar at the start of `t`: its value and the bytes it used.
fn quoted(t: &str, line: usize) -> Result<(String, usize)> {
    let q = t.as_bytes()[0];
    let mut out = String::new();
    let mut chars = t.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' if q == b'\'' => {
                if chars.peek().map(|&(_, c)| c) == Some('\'') {
                    chars.next();
                    out.push('\'');
                } else {
                    return Ok((out, i + 1));
                }
            }
            '"' if q == b'"' => return Ok((out, i + 1)),
            '\\' if q == b'"' => {
                let Some((_, e)) = chars.next() else { break };
                match e {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '/' => out.push('/'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    'x' | 'u' | 'U' => {
                        let n = match e {
                            'x' => 2,
                            'u' => 4,
                            _ => 8,
                        };
                        let hex: String =
                            (0..n).filter_map(|_| chars.next().map(|x| x.1)).collect();
                        let c = u32::from_str_radix(&hex, 16)
                            .ok()
                            .filter(|_| hex.len() == n)
                            .and_then(char::from_u32);
                        match c {
                            Some(c) => out.push(c),
                            None => return err(line, format!("bad escape \\{e}{hex}")),
                        }
                    }
                    other => return err(line, format!("unknown escape \\{other}")),
                }
            }
            c => out.push(c),
        }
    }
    err(line, "unterminated quoted string")
}

/// A one-line flow collection.
struct Flow<'a> {
    s: &'a [u8],
    i: usize,
    text: &'a str,
    line: usize,
    /// How deep the flow parser is (acs-q4f).
    depth: usize,
}

impl Flow<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i] == b' ' {
            self.i += 1;
        }
    }

    fn node(&mut self) -> Result<Node> {
        if self.depth >= MAX_DEPTH {
            return too_deep(self.line);
        }
        self.depth += 1;
        let v = self.value();
        self.depth -= 1;
        let v = v?;
        let flow = matches!(v, Value::Seq(_) | Value::Map(_));
        Ok(Node {
            line: self.line,
            flow,
            ..Node::new(v)
        })
    }

    fn value(&mut self) -> Result<Value> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Ok(Value::Seq(items));
                    }
                    items.push(self.node()?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => {
                            return err(
                                self.line,
                                "expected ',' or ']' (lists must fit on one line)",
                            )
                        }
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut entries: Vec<(String, Node)> = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Ok(Value::Map(entries));
                    }
                    let key = match self.value()? {
                        Value::Scalar(s) => s.text,
                        _ => return err(self.line, "a mapping key must be a single value"),
                    };
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return err(self.line, format!("expected ':' after '{key}'"));
                    }
                    self.i += 1;
                    self.ws();
                    let node = match self.s.get(self.i) {
                        Some(b',') | Some(b'}') => Node {
                            line: self.line,
                            ..Node::new(Value::Null)
                        },
                        _ => self.node()?,
                    };
                    if entries.iter().any(|(k, _)| *k == key) {
                        return err(self.line, format!("duplicate key '{key}'"));
                    }
                    entries.push((key, node));
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => {
                            return err(
                                self.line,
                                "expected ',' or '}' (mappings in braces must fit on one line)",
                            )
                        }
                    }
                }
            }
            Some(b'"') | Some(b'\'') => {
                let style = if self.s[self.i] == b'"' {
                    Style::Double
                } else {
                    Style::Single
                };
                let (text, used) = quoted(&self.text[self.i..], self.line)?;
                self.i += used;
                Ok(Value::Scalar(Scalar { text, style }))
            }
            Some(_) => {
                let start = self.i;
                while let Some(&c) = self.s.get(self.i) {
                    let colon_end = c == b':'
                        && matches!(
                            self.s.get(self.i + 1),
                            None | Some(b' ' | b',' | b']' | b'}')
                        );
                    if matches!(c, b',' | b']' | b'}' | b'#') || colon_end {
                        break;
                    }
                    self.i += 1;
                }
                let text = self.text[start..self.i].trim_end().to_string();
                if text.is_empty() {
                    return err(self.line, "expected a value");
                }
                Ok(Value::Scalar(Scalar {
                    text,
                    style: Style::Plain,
                }))
            }
            None => err(self.line, "unterminated list or mapping"),
        }
    }
}

// ---- writing ---------------------------------------------------------------

/// Whether `s` can be written without quotes and read back as the same text.
fn plain_ok(s: &str) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    !(s.starts_with(' ')
        || s.ends_with(' ')
        || s.ends_with(':')
        || "?:,[]{}#&*!|>'\"%@`".contains(first)
        || s == "-"
        || s.starts_with("- ")
        || s.contains(": ")
        || s.contains(" #")
        || s.chars().any(|c| c.is_control()))
}

/// Whether a plain `s` would read as a boolean, null or number.
fn looks_typed(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~"
    ) || s.parse::<f64>().is_ok()
}

fn quote_double(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn scalar_text(s: &Scalar) -> String {
    match s.style {
        Style::Plain if plain_ok(&s.text) => s.text.clone(),
        Style::Single if !s.text.chars().any(|c| c.is_control()) => {
            format!("'{}'", s.text.replace('\'', "''"))
        }
        _ => quote_double(&s.text),
    }
}

fn key_text(k: &str) -> String {
    if plain_ok(k) {
        k.to_string()
    } else {
        quote_double(k)
    }
}

/// A value written on one line (scalars, empty and flow collections).
fn flow_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Scalar(s) => scalar_text(s),
        Value::Seq(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|n| flow_text(&n.value))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Map(entries) => format!(
            "{{{}}}",
            entries
                .iter()
                .map(|(k, n)| match &n.value {
                    Value::Null => format!("{}:", key_text(k)),
                    v => format!("{}: {}", key_text(k), flow_text(v)),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Whether `n` is written on the line of its key or `-`.
fn one_line(n: &Node) -> bool {
    n.flow
        || match &n.value {
            Value::Null | Value::Scalar(_) => true,
            Value::Seq(v) => v.is_empty(),
            Value::Map(m) => m.is_empty(),
        }
}

fn push_before(out: &mut String, before: &[String], indent: usize) {
    for b in before {
        if b.is_empty() {
            out.push('\n');
        } else {
            out.push_str(&" ".repeat(indent));
            out.push_str(b);
            out.push('\n');
        }
    }
}

/// The end of a line holding `value` (after `key:` or `-`), with its comment.
fn line_end(out: &mut String, n: &Node) {
    if one_line(n) {
        let t = flow_text(&n.value);
        if !t.is_empty() {
            out.push(' ');
            out.push_str(&t);
        }
    }
    if let Some(c) = &n.comment {
        out.push(' ');
        out.push_str(c);
    }
    out.push('\n');
}

fn emit_block(out: &mut String, v: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match v {
        Value::Map(entries) => {
            for (k, n) in entries {
                push_before(out, &n.before, indent);
                out.push_str(&pad);
                out.push_str(&key_text(k));
                out.push(':');
                line_end(out, n);
                if !one_line(n) {
                    emit_block(out, &n.value, indent + 2);
                }
            }
        }
        Value::Seq(items) => {
            for n in items {
                match &n.value {
                    // `- key: value` with the other keys aligned under it.
                    Value::Map(entries) if !one_line(n) && n.comment.is_none() => {
                        let (k0, n0) = &entries[0];
                        push_before(out, &n.before, indent);
                        push_before(out, &n0.before, indent);
                        out.push_str(&pad);
                        out.push_str("- ");
                        out.push_str(&key_text(k0));
                        out.push(':');
                        line_end(out, n0);
                        if !one_line(n0) {
                            emit_block(out, &n0.value, indent + 4);
                        }
                        emit_block(out, &Value::Map(entries[1..].to_vec()), indent + 2);
                    }
                    _ => {
                        push_before(out, &n.before, indent);
                        out.push_str(&pad);
                        out.push('-');
                        line_end(out, n);
                        if !one_line(n) {
                            emit_block(out, &n.value, indent + 2);
                        }
                    }
                }
            }
        }
        other => {
            out.push_str(&pad);
            out.push_str(&flow_text(other));
            out.push('\n');
        }
    }
}

/// Write a document back as text.
pub fn emit(doc: &Document) -> String {
    let mut out = String::new();
    let root = &doc.root;
    match &root.value {
        Value::Null => push_before(&mut out, &root.before, 0),
        Value::Map(_) | Value::Seq(_) if !root.flow => {
            push_before(&mut out, &root.before, 0);
            emit_block(&mut out, &root.value, 0);
        }
        _ => {
            push_before(&mut out, &root.before, 0);
            out.push_str(&flow_text(&root.value));
            if let Some(c) = &root.comment {
                out.push(' ');
                out.push_str(c);
            }
            out.push('\n');
        }
    }
    push_before(&mut out, &doc.tail, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// acs-q4f: both parsers recurse and neither was bounded, so a few
    /// thousand brackets — which fit on one line — overflowed the stack.
    /// With `panic = "abort"` in the release profile that is not an error
    /// a caller can report but an immediate kill.
    #[test]
    fn a_configuration_nested_past_all_reason_is_an_error_not_a_crash() {
        // Flow style: one line, thousands deep.
        let deep = format!("k: {}{}", "[".repeat(5000), "]".repeat(5000));
        let e = parse(&deep).expect_err("accepted 5000 levels of nesting");
        assert!(e.to_string().contains("nested more than"), "{e}");

        // Block style: a map inside a map inside a map…
        let mut block = String::new();
        for i in 0..5000 {
            block.push_str(&" ".repeat(i * 2));
            block.push_str(&format!("k{i}:\n"));
        }
        let e = parse(&block).expect_err("accepted 5000 levels of block nesting");
        assert!(e.to_string().contains("nested more than"), "{e}");

        // What a person actually writes still parses.
        assert!(parse("a:\n  b:\n    c: [1, {d: 2}]\n").is_ok());
        let ok = format!("k: {}1{}", "[".repeat(60), "]".repeat(60));
        assert!(parse(&ok).is_ok(), "60 deep should be fine");
    }

    fn text(n: &Node) -> &str {
        match &n.value {
            Value::Scalar(s) => &s.text,
            other => panic!("not a scalar: {other:?}"),
        }
    }

    fn get<'a>(n: &'a Node, key: &str) -> &'a Node {
        let m = n.value.map().expect("a mapping");
        &m.iter().find(|(k, _)| k == key).expect(key).1
    }

    fn items(n: &Node) -> &Vec<Node> {
        match &n.value {
            Value::Seq(v) => v,
            other => panic!("not a list: {other:?}"),
        }
    }

    const SAMPLE: &str = "\
# acs configuration
install_on_remote: false   # not on shared boxes

hosts:
  devbox:
    - host: devbox.lan
      reachability_check: true
    # the way in from outside
    - host: devbox.example.com
      user: 'michel'
  nas:
  - {host: nas.lan, reachability_check: false}
list: [a, \"b c\", 3]
";

    /// Regression (acs-qjx): YAML 1.2 separates a key from its value with a
    /// space or a tab, and a leading BOM belongs to no key.
    #[test]
    fn a_tab_after_the_colon_and_a_leading_bom_are_accepted() {
        let d =
            parse("install_on_remote:\tfalse\naliases:\n  nas:\n    - host:\tnas.lan\n").unwrap();
        assert_eq!(text(get(&d.root, "install_on_remote")), "false");
        let nas = &items(get(get(&d.root, "aliases"), "nas"))[0];
        assert_eq!(text(get(nas, "host")), "nas.lan");
        // Written back, the separator is the usual single space.
        assert!(
            emit(&d).starts_with("install_on_remote: false\n"),
            "{:?}",
            emit(&d)
        );

        let d = parse("\u{feff}install_on_remote: true\n").unwrap();
        assert_eq!(text(get(&d.root, "install_on_remote")), "true");
        assert_eq!(emit(&d), "install_on_remote: true\n");

        // A colon with no space at all is still not a key (a bare scalar).
        assert!(parse("install_on_remote:false\n")
            .unwrap()
            .root
            .value
            .map()
            .is_none());
    }

    #[test]
    fn parses_the_config_shapes() {
        let d = parse(SAMPLE).unwrap();
        let r = &d.root;
        let i = get(r, "install_on_remote");
        assert_eq!(text(i), "false");
        assert_eq!(i.line, 2);
        // The file's opening comments belong to the file, not its first key,
        // so they survive that key's removal.
        assert_eq!(r.before, ["# acs configuration"]);
        assert!(i.before.is_empty());
        assert_eq!(i.comment.as_deref(), Some("# not on shared boxes"));

        let hosts = get(r, "hosts");
        assert_eq!(hosts.before, [""]);
        let devbox = items(get(hosts, "devbox"));
        assert_eq!(devbox.len(), 2);
        assert_eq!(text(get(&devbox[0], "host")), "devbox.lan");
        assert_eq!(get(&devbox[0], "host").line, 6);
        assert_eq!(text(get(&devbox[0], "reachability_check")), "true");
        assert_eq!(devbox[1].before, ["# the way in from outside"]);
        assert_eq!(text(get(&devbox[1], "user")), "michel");
        assert_eq!(get(&devbox[1], "user").line, 10);

        // A list at the key's own indentation, and a flow mapping item.
        let nas = items(get(hosts, "nas"));
        assert_eq!(text(get(&nas[0], "host")), "nas.lan");
        assert_eq!(text(get(&nas[0], "reachability_check")), "false");

        let list: Vec<&str> = items(get(r, "list")).iter().map(text).collect();
        assert_eq!(list, ["a", "b c", "3"]);
    }

    #[test]
    fn writes_back_unchanged_up_to_indentation() {
        let d = parse(SAMPLE).unwrap();
        let out = emit(&d);
        // Parsing what we wrote gives the same tree (lines aside).
        let again = emit(&parse(&out).unwrap());
        assert_eq!(out, again);
        assert_eq!(
            out,
            "\
# acs configuration
install_on_remote: false # not on shared boxes

hosts:
  devbox:
    - host: devbox.lan
      reachability_check: true
    # the way in from outside
    - host: devbox.example.com
      user: 'michel'
  nas:
    - {host: nas.lan, reachability_check: false}
list: [a, \"b c\", 3]
"
        );
    }

    #[test]
    fn quoting_and_escapes() {
        let d = parse(
            "a: \"x\\ty \\\"q\\\" \\u00e9\"\nb: 'it''s # not a comment'\nc: plain # comment\nd: http://x:8/y\ne:\n",
        )
        .unwrap();
        assert_eq!(text(get(&d.root, "a")), "x\ty \"q\" é");
        assert_eq!(text(get(&d.root, "b")), "it's # not a comment");
        assert_eq!(text(get(&d.root, "c")), "plain");
        assert_eq!(text(get(&d.root, "d")), "http://x:8/y");
        assert_eq!(get(&d.root, "e").value, Value::Null);
        assert_eq!(parse(&emit(&d)).unwrap().root.value, d.root.value);
    }

    #[test]
    fn new_strings_are_quoted_when_plain_would_change_them() {
        for (s, want) in [
            ("devbox.lan", "devbox.lan"),
            ("true", "\"true\""),
            ("123", "\"123\""),
            ("a: b", "\"a: b\""),
            ("#x", "\"#x\""),
            ("", "\"\""),
            ("-x", "-x"),
        ] {
            let mut d = parse("k: v\n").unwrap();
            d.root.value.map_mut().unwrap()[0].1 = Node::string(s);
            let out = emit(&d);
            assert_eq!(out, format!("k: {want}\n"), "{s:?}");
            assert_eq!(text(get(&parse(&out).unwrap().root, "k")), s);
        }
    }

    #[test]
    fn empty_documents() {
        for src in ["", "\n", "# only a comment\n", "---\n"] {
            let d = parse(src).unwrap();
            assert_eq!(d.root.value, Value::Null, "{src:?}");
        }
        assert_eq!(emit(&parse("# c\n\n").unwrap()), "# c\n\n");
    }

    #[test]
    fn errors_name_the_line() {
        let cases: &[(&str, usize, &str)] = &[
            ("a: 1\n\tb: 2\n", 2, "tabs"),
            ("a: 1\na: 2\n", 2, "duplicate key 'a'"),
            ("a:\n  b: 1\n c: 2\n", 3, "indentation"),
            ("a: 1\n  b: 2\n", 2, "indentation"),
            ("a: |\n  x\n", 1, "block scalars"),
            ("a: &x 1\n", 1, "anchors"),
            ("a: [1, 2\n", 1, "one line"),
            ("a: \"open\n", 1, "unterminated"),
            ("a: b: c\n", 1, "must be quoted"),
            ("a: 'x' y\n", 1, "unexpected text"),
            ("a:\n  - 1\n  b: 2\n", 3, "indentation"),
            ("a: 1\n- 2\n", 2, "list item"),
            ("a: 1\n---\nb: 2\n", 2, "one YAML document"),
            ("a: \"\\q\"\n", 1, "unknown escape"),
        ];
        for (src, line, want) in cases {
            let e = parse(src).unwrap_err();
            assert_eq!(e.line, *line, "{src:?}: {e}");
            assert!(e.msg.contains(want), "{src:?}: {e}");
        }
    }

    #[test]
    fn nested_lists_and_dash_alone() {
        let d = parse("a:\n  -\n    x: 1\n  - - p\n    - q\n  -\n").unwrap();
        let a = items(get(&d.root, "a"));
        assert_eq!(text(get(&a[0], "x")), "1");
        let inner: Vec<&str> = items(&a[1]).iter().map(text).collect();
        assert_eq!(inner, ["p", "q"]);
        assert_eq!(a[2].value, Value::Null);
    }
}
