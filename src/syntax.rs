use crate::diag::{Diagnostic, Span};

pub const MAX_SOURCE: usize = 1_048_576;
pub const MAX_DEPTH: usize = 64;
pub const MAX_DECLARATIONS: usize = 10_000;
pub const MAX_SAFE_INT: i64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Property(Property),
    Block(Block),
}

impl Entry {
    pub fn span(&self) -> Span {
        match self {
            Entry::Property(p) => p.span,
            Entry::Block(b) => b.span,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    pub key: Ident,
    pub value: Value,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub kind: Ident,
    pub name: Ident,
    pub entries: Vec<Entry>,
    pub open: usize,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    pub kind: ValueKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ValueKind {
    String(String),
    Integer(i64),
    Bool(bool),
    Symbol(String),
    Reference(Vec<Ident>),
    Array(Vec<Value>),
    Object(Vec<(Ident, Value)>),
}

impl ValueKind {
    pub fn describe(&self) -> &'static str {
        match self {
            ValueKind::String(_) => "string",
            ValueKind::Integer(_) => "integer",
            ValueKind::Bool(_) => "boolean",
            ValueKind::Symbol(_) => "symbol",
            ValueKind::Reference(_) => "reference",
            ValueKind::Array(_) => "array",
            ValueKind::Object(_) => "object",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub span: Span,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Document {
    pub file: String,
    pub version: String,
    pub version_span: Span,
    pub header_end: usize,
    pub app: Ident,
    pub app_keyword: Span,
    pub open: usize,
    pub entries: Vec<Entry>,
    pub comments: Vec<Comment>,
    pub span: Span,
}

struct Parser<'a> {
    src: &'a str,
    file: &'a str,
    pos: usize,
    declarations: usize,
    comments: Vec<Comment>,
}

type PResult<T> = Result<T, Diagnostic>;

pub fn parse(src: &str, file: &str) -> PResult<Document> {
    if src.len() > MAX_SOURCE {
        return Err(Diagnostic::error(
            "E_LIMIT_SOURCE",
            format!(
                "source is {} bytes; the limit is {MAX_SOURCE} bytes",
                src.len()
            ),
            file,
            Span::new(MAX_SOURCE, src.len()),
        ));
    }
    let mut p = Parser {
        src,
        file,
        pos: 0,
        declarations: 0,
        comments: Vec::new(),
    };
    p.skip_trivia()?;
    let start = p.pos;
    p.keyword("imanari")?;
    p.skip_trivia()?;
    let version_start = p.pos;
    let version = p.string()?;
    let version_span = Span::new(version_start, p.pos);
    p.expect(';')?;
    let header_end = p.pos;
    p.skip_trivia()?;
    let app_keyword = Span::new(p.pos, p.pos + 3);
    p.keyword("app")?;
    let app = p.name()?;
    p.skip_trivia()?;
    let open = p.pos;
    p.expect('{')?;
    let entries = p.entries(1)?;
    let end = p.pos;
    p.skip_trivia()?;
    if p.pos != src.len() {
        return Err(p.err("E_SYNTAX", "unexpected input after the app block", p.pos));
    }
    Ok(Document {
        file: file.into(),
        version,
        version_span,
        header_end,
        app,
        app_keyword,
        open,
        entries,
        comments: p.comments,
        span: Span::new(start, end),
    })
}

impl Parser<'_> {
    fn err(&self, code: &str, msg: impl Into<String>, at: usize) -> Diagnostic {
        let end = self.next_char_end(at);
        Diagnostic::error(code, msg, self.file, Span::new(at, end))
    }

    fn next_char_end(&self, at: usize) -> usize {
        self.src[at.min(self.src.len())..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_trivia(&mut self) -> PResult<()> {
        loop {
            while let Some(c) = self.peek() {
                if matches!(c, ' ' | '\t' | '\n' | '\r') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.src[self.pos..].starts_with("//") {
                let start = self.pos;
                let len = self.src[start..]
                    .find('\n')
                    .unwrap_or(self.src.len() - start);
                self.pos = start + len;
                let text = self.src[start..self.pos].trim_end_matches('\r').to_string();
                self.comments.push(Comment {
                    span: Span::new(start, start + text.len()),
                    text,
                });
            } else if let Some(c) = self.peek().filter(|c| c.is_whitespace()) {
                return Err(self.err(
                    "E_SYNTAX",
                    format!("unsupported whitespace character U+{:04X}", c as u32),
                    self.pos,
                ));
            } else {
                return Ok(());
            }
        }
    }

    fn eat(&mut self, c: char) -> PResult<bool> {
        self.skip_trivia()?;
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn expect(&mut self, c: char) -> PResult<()> {
        if self.eat(c)? {
            Ok(())
        } else {
            let found = self.describe_here();
            Err(self.err(
                "E_SYNTAX",
                format!("expected '{c}', found {found}"),
                self.pos,
            ))
        }
    }

    fn describe_here(&self) -> String {
        match self.peek() {
            None => "end of input".into(),
            Some(c) => format!("'{c}'"),
        }
    }

    fn keyword(&mut self, kw: &str) -> PResult<()> {
        let at = self.pos;
        let id = self.ident()?;
        if id.name == kw {
            Ok(())
        } else {
            Err(Diagnostic::error(
                "E_SYNTAX",
                format!("expected '{kw}', found '{}'", id.name),
                self.file,
                Span::new(at, self.pos),
            ))
        }
    }

    fn ident(&mut self) -> PResult<Ident> {
        self.skip_trivia()?;
        self.ident_here()
    }

    fn ident_here(&mut self) -> PResult<Ident> {
        let start = self.pos;
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => {
                let found = self.describe_here();
                return Err(self.err(
                    "E_SYNTAX",
                    format!("expected identifier, found {found}"),
                    start,
                ));
            }
        }
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.pos += 1;
        }
        Ok(Ident {
            name: self.src[start..self.pos].to_string(),
            span: Span::new(start, self.pos),
        })
    }

    fn name(&mut self) -> PResult<Ident> {
        self.skip_trivia()?;
        self.name_here()
    }

    fn name_here(&mut self) -> PResult<Ident> {
        let id = self.ident_here()?;
        if id.name == "true" || id.name == "false" {
            return Err(Diagnostic::error(
                "E_RESERVED_WORD",
                format!("'{}' is reserved and cannot be used as a name", id.name),
                self.file,
                id.span,
            ));
        }
        Ok(id)
    }

    fn string(&mut self) -> PResult<String> {
        self.skip_trivia()?;
        let start = self.pos;
        if self.peek() != Some('"') {
            let found = self.describe_here();
            return Err(self.err("E_SYNTAX", format!("expected string, found {found}"), start));
        }
        self.pos += 1;
        let mut escaped = false;
        while let Some(c) = self.bump() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                let raw = &self.src[start..self.pos];
                return serde_json::from_str::<String>(raw).map_err(|e| {
                    Diagnostic::error(
                        "E_STRING",
                        format!("invalid string literal: {e}"),
                        self.file,
                        Span::new(start, self.pos),
                    )
                });
            } else if c == '\n' {
                break;
            }
        }
        Err(Diagnostic::error(
            "E_STRING",
            "unterminated string literal",
            self.file,
            Span::new(start, self.pos),
        ))
    }

    fn integer(&mut self) -> PResult<i64> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        let digits = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        let span = Span::new(start, self.pos);
        let bad = |msg: &str| Diagnostic::error("E_INTEGER", msg, self.file, span);
        if self.pos == digits {
            return Err(bad("expected digits"));
        }
        if self.src[digits..self.pos].len() > 1 && self.src[digits..].starts_with('0') {
            return Err(bad("leading zeros are not allowed"));
        }
        if matches!(self.peek(), Some('.' | 'e' | 'E')) {
            return Err(self.err(
                "E_INTEGER",
                "fractions and exponents are not supported; integers only",
                self.pos,
            ));
        }
        if self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(self.err("E_SYNTAX", "unexpected character after integer", self.pos));
        }
        match text.parse::<i64>() {
            Ok(n) if (-MAX_SAFE_INT..=MAX_SAFE_INT).contains(&n) => Ok(n),
            _ => Err(bad("integer is outside the safe range ±9007199254740991")),
        }
    }

    fn value(&mut self, depth: usize) -> PResult<Value> {
        self.skip_trivia()?;
        let start = self.pos;
        if depth > MAX_DEPTH {
            return Err(self.err(
                "E_LIMIT_DEPTH",
                format!("nesting depth exceeds {MAX_DEPTH}"),
                start,
            ));
        }
        let kind = match self.peek() {
            Some('"') => ValueKind::String(self.string()?),
            Some('[') => {
                self.pos += 1;
                let mut items = Vec::new();
                while !self.eat(']')? {
                    items.push(self.value(depth + 1)?);
                    if !self.eat(',')? {
                        self.expect(']')?;
                        break;
                    }
                }
                ValueKind::Array(items)
            }
            Some('{') => {
                self.pos += 1;
                let mut members: Vec<(Ident, Value)> = Vec::new();
                while !self.eat('}')? {
                    let key = self.name()?;
                    if let Some((prev, _)) = members.iter().find(|(k, _)| k.name == key.name) {
                        return Err(Diagnostic::error(
                            "E_DUPLICATE",
                            format!("duplicate object key '{}'", key.name),
                            self.file,
                            key.span,
                        )
                        .details(format!("first defined at byte {}", prev.span.start)));
                    }
                    self.expect(':')?;
                    let v = self.value(depth + 1)?;
                    members.push((key, v));
                    if !self.eat(',')? {
                        self.expect('}')?;
                        break;
                    }
                }
                ValueKind::Object(members)
            }
            Some('@') => {
                self.pos += 1;
                let mut path = vec![self.name_here()?];
                while self.peek() == Some('.') {
                    self.pos += 1;
                    path.push(self.name_here()?);
                }
                ValueKind::Reference(path)
            }
            Some(c) if c == '-' || c.is_ascii_digit() => ValueKind::Integer(self.integer()?),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let id = self.ident()?;
                match id.name.as_str() {
                    "true" => ValueKind::Bool(true),
                    "false" => ValueKind::Bool(false),
                    _ => ValueKind::Symbol(id.name),
                }
            }
            _ => {
                let found = self.describe_here();
                return Err(self.err(
                    "E_SYNTAX",
                    format!("expected a value, found {found}"),
                    start,
                ));
            }
        };
        Ok(Value {
            kind,
            span: Span::new(start, self.pos),
        })
    }

    fn entries(&mut self, depth: usize) -> PResult<Vec<Entry>> {
        let mut out = Vec::new();
        loop {
            if self.eat('}')? {
                return Ok(out);
            }
            if self.pos >= self.src.len() {
                return Err(self.err("E_SYNTAX", "expected '}' before end of input", self.pos));
            }
            if depth > MAX_DEPTH {
                return Err(self.err(
                    "E_LIMIT_DEPTH",
                    format!("nesting depth exceeds {MAX_DEPTH}"),
                    self.pos,
                ));
            }
            self.declarations += 1;
            if self.declarations > MAX_DECLARATIONS {
                return Err(self.err(
                    "E_LIMIT_DECLARATIONS",
                    format!("more than {MAX_DECLARATIONS} declarations"),
                    self.pos,
                ));
            }
            let key = self.name()?;
            let start = key.span.start;
            if self.eat('=')? {
                let value = self.value(depth + 1)?;
                self.expect(';')?;
                out.push(Entry::Property(Property {
                    key,
                    value,
                    span: Span::new(start, self.pos),
                }));
            } else {
                let name = self.name()?;
                self.skip_trivia()?;
                let open = self.pos;
                self.expect('{')?;
                let entries = self.entries(depth + 1)?;
                out.push(Entry::Block(Block {
                    kind: key,
                    name,
                    entries,
                    open,
                    span: Span::new(start, self.pos),
                }));
            }
        }
    }
}
