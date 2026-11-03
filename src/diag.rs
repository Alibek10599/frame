use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub node_id: Option<String>,
    pub file: String,
    pub span: Span,
    pub line: usize,
    pub column: usize,
    pub details: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Diagnostic {
    pub fn error(code: &str, message: impl Into<String>, file: &str, span: Span) -> Self {
        Diagnostic {
            code: code.into(),
            severity: Severity::Error,
            message: message.into(),
            node_id: None,
            file: file.into(),
            span,
            line: 0,
            column: 0,
            details: None,
        }
    }

    pub fn warning(code: &str, message: impl Into<String>, file: &str, span: Span) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            ..Diagnostic::error(code, message, file, span)
        }
    }

    pub fn node(mut self, id: impl Into<String>) -> Self {
        self.node_id = Some(id.into());
        self
    }

    pub fn details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn locate(mut self, src: &str) -> Self {
        let at = self.span.start.min(src.len());
        let before = &src[..floor_boundary(src, at)];
        self.line = before.matches('\n').count() + 1;
        self.column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    pub fn is_unsupported(&self) -> bool {
        self.code.starts_with("E_UNSUPPORTED")
    }

    pub fn render(&self) -> String {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let node = self
            .node_id
            .as_deref()
            .map(|n| format!(" [{n}]"))
            .unwrap_or_default();
        let place = if self.file.is_empty() {
            String::new()
        } else if self.line == 0 {
            format!("{}: ", self.file)
        } else {
            format!("{}:{}:{}: ", self.file, self.line, self.column)
        };
        let mut s = format!("{place}{sev} {}: {}{node}", self.code, self.message);
        if let Some(d) = &self.details {
            s.push_str("\n  ");
            s.push_str(&d.replace('\n', "\n  "));
        }
        s
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn exit_code(diags: &[Diagnostic]) -> u8 {
    let errors: Vec<_> = diags.iter().filter(|d| d.is_error()).collect();
    if errors.is_empty() {
        0
    } else if errors.iter().any(|d| d.is_unsupported()) {
        2
    } else {
        1
    }
}
