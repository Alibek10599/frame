use crate::syntax::{Comment, Document, Entry, Value, ValueKind};

pub fn format(src: &str, doc: &Document) -> String {
    let mut f = Formatter {
        src,
        comments: &doc.comments,
        next: 0,
        prev_end: 0,
        out: String::new(),
        line_open: false,
        fresh_scope: true,
    };
    f.comments_before(doc.span.start, 0);
    f.line(0, &format!("imanari {};", quote(&doc.version)));
    f.prev_end = doc.header_end;
    f.comments_before(doc.app_keyword.start, 0);
    f.gap_blank(doc.app_keyword.start);
    f.line(0, &format!("app {} {{", doc.app.name));
    f.open_scope(doc.open + 1);
    for e in &doc.entries {
        f.entry(e, 1);
    }
    f.comments_before(doc.span.end - 1, 1);
    f.line(0, "}");
    f.prev_end = doc.span.end;
    f.fresh_scope = false;
    f.comments_before(usize::MAX, 0);
    f.close();
    f.out
}

struct Formatter<'a> {
    src: &'a str,
    comments: &'a [Comment],
    next: usize,
    prev_end: usize,
    out: String,
    line_open: bool,
    fresh_scope: bool,
}

impl Formatter<'_> {
    fn close(&mut self) {
        if self.line_open {
            self.out.push('\n');
            self.line_open = false;
        }
    }

    fn line(&mut self, indent: usize, text: &str) {
        self.close();
        for _ in 0..indent {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.line_open = true;
        self.fresh_scope = false;
    }

    fn open_scope(&mut self, after: usize) {
        self.prev_end = after;
        self.fresh_scope = true;
    }

    fn newlines_before(&self, pos: usize) -> usize {
        let end = pos.min(self.src.len());
        if self.prev_end >= end {
            return 0;
        }
        self.src[self.prev_end..end].matches('\n').count()
    }

    fn gap_blank(&mut self, pos: usize) {
        if self.newlines_before(pos) >= 2 && !self.fresh_scope {
            self.close();
            self.out.push('\n');
        }
    }

    fn comments_before(&mut self, pos: usize, indent: usize) {
        while let Some(c) = self.comments.get(self.next) {
            if c.span.start >= pos {
                break;
            }
            if self.line_open && self.newlines_before(c.span.start) == 0 {
                self.out.push(' ');
                self.out.push_str(&c.text);
                self.close();
            } else {
                self.gap_blank(c.span.start);
                self.line(indent, &c.text);
                self.close();
            }
            self.prev_end = c.span.end;
            self.next += 1;
        }
    }

    fn has_comment_within(&self, start: usize, end: usize) -> bool {
        self.comments[self.next..]
            .iter()
            .any(|c| c.span.start > start && c.span.start < end)
    }

    fn entry(&mut self, e: &Entry, indent: usize) {
        let span = e.span();
        self.comments_before(span.start, indent);
        self.gap_blank(span.start);
        match e {
            Entry::Property(p) => {
                self.line(indent, &format!("{} = ", p.key.name));
                self.value(&p.value, indent);
                self.out.push(';');
            }
            Entry::Block(b) => {
                self.line(indent, &format!("{} {} {{", b.kind.name, b.name.name));
                self.open_scope(b.open + 1);
                for child in &b.entries {
                    self.entry(child, indent + 1);
                }
                self.comments_before(span.end - 1, indent + 1);
                self.line(indent, "}");
            }
        }
        self.prev_end = span.end;
    }

    fn multiline(&self, v: &Value) -> bool {
        match &v.kind {
            ValueKind::Object(m) => {
                m.len() > 1
                    || m.iter().any(|(_, v)| self.multiline(v))
                    || self.has_comment_within(v.span.start, v.span.end)
            }
            ValueKind::Array(items) => {
                items.iter().any(|v| self.multiline(v))
                    || self.has_comment_within(v.span.start, v.span.end)
            }
            _ => false,
        }
    }

    fn value(&mut self, v: &Value, indent: usize) {
        match &v.kind {
            ValueKind::Object(m) if m.is_empty() => self.out.push_str("{}"),
            ValueKind::Array(items) if items.is_empty() => self.out.push_str("[]"),
            ValueKind::Object(m) if self.multiline(v) => {
                self.out.push('{');
                self.open_scope(v.span.start + 1);
                for (k, val) in m {
                    self.comments_before(k.span.start, indent + 1);
                    self.gap_blank(k.span.start);
                    self.line(indent + 1, &format!("{}: ", k.name));
                    self.value(val, indent + 1);
                    self.out.push(',');
                    self.prev_end = val.span.end;
                }
                self.comments_before(v.span.end - 1, indent + 1);
                self.line(indent, "}");
            }
            ValueKind::Array(items) if self.multiline(v) => {
                self.out.push('[');
                self.open_scope(v.span.start + 1);
                for item in items {
                    self.comments_before(item.span.start, indent + 1);
                    self.gap_blank(item.span.start);
                    self.line(indent + 1, "");
                    self.value(item, indent + 1);
                    self.out.push(',');
                    self.prev_end = item.span.end;
                }
                self.comments_before(v.span.end - 1, indent + 1);
                self.line(indent, "]");
            }
            _ => self.out.push_str(&inline(v)),
        }
        self.prev_end = v.span.end;
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("string serialization cannot fail")
}

pub fn inline(v: &Value) -> String {
    match &v.kind {
        ValueKind::String(s) => quote(s),
        ValueKind::Integer(n) => n.to_string(),
        ValueKind::Bool(b) => b.to_string(),
        ValueKind::Symbol(s) => s.clone(),
        ValueKind::Reference(path) => {
            let parts: Vec<&str> = path.iter().map(|p| p.name.as_str()).collect();
            format!("@{}", parts.join("."))
        }
        ValueKind::Array(items) => {
            let parts: Vec<String> = items.iter().map(inline).collect();
            format!("[{}]", parts.join(", "))
        }
        ValueKind::Object(m) if m.is_empty() => "{}".into(),
        ValueKind::Object(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{}: {}", k.name, inline(v)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}
