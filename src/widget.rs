//! Widget parser — parses a small declarative widget definition language into
//! structured [`Widget`] values.
//!
//! The parser consumes a compact, human-editable source format and produces a
//! typed [`Widget`] tree that a renderer can consume directly. It is deliberately
//! self-contained (no I/O, no network) so it can be unit-tested offline and
//! reused by any renderer that needs to materialise widgets from a declarative
//! description.
//!
//! ## Grammar
//!
//! ```text
//! document   := { widget_def }
//! widget_def := "widget" NAME KIND "{" { prop } "}"
//! prop       := NAME "=" value
//! value      := STRING | BARE
//! ```
//!
//! * `NAME` is a bare identifier (`[A-Za-z_][A-Za-z0-9_-]*`).
//! * `KIND` is one of the supported [`WidgetKind`] variants (case-insensitive).
//! * `STRING` is a double-quoted literal (with `\"` and `\\` escapes).
//! * `BARE` is any run of non-whitespace, non-brace characters.
//! * `//` starts a line comment; blank lines and surrounding whitespace are
//!   ignored.
//!
//! ## Example
//!
//! ```text
//! // A simple chat panel.
//! widget chat paragraph {
//!     title = "Chat"
//!     border = "true"
//!     width  = 40
//! }
//!
//! widget plan list {
//!     title = "Execution Plan"
//!     wrap  = "false"
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single parsed widget declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Widget {
    /// Unique widget name (the `NAME` token after `widget`).
    pub name: String,
    /// The widget's concrete kind (the `KIND` token).
    pub kind: WidgetKind,
    /// Declared properties as `key -> value` string pairs. Values are stored as
    /// their literal source text; typed interpretation is the caller's job.
    pub props: BTreeMap<String, String>,
}

impl Widget {
    /// Look up a property by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }

    /// Look up a property as a boolean (`"true"`/`"false"`, case-insensitive).
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| {
            let t = v.trim();
            t.eq_ignore_ascii_case("true")
                || t.eq_ignore_ascii_case("yes")
                || t.eq_ignore_ascii_case("1")
        })
    }

    /// Look up a property as an unsigned integer.
    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.trim().parse().ok())
    }
}

/// The concrete widget kinds the parser understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Paragraph,
    Block,
    Gauge,
    List,
    Table,
    Chart,
    Sparkline,
    Canvas,
}

impl WidgetKind {
    /// Parse a kind token case-insensitively.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "paragraph" => Some(Self::Paragraph),
            "block" => Some(Self::Block),
            "gauge" => Some(Self::Gauge),
            "list" => Some(Self::List),
            "table" => Some(Self::Table),
            "chart" => Some(Self::Chart),
            "sparkline" => Some(Self::Sparkline),
            "canvas" => Some(Self::Canvas),
            _ => None,
        }
    }

    /// The canonical string form (inverse of [`WidgetKind::parse`]).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Paragraph => "paragraph",
            Self::Block => "block",
            Self::Gauge => "gauge",
            Self::List => "list",
            Self::Table => "table",
            Self::Chart => "chart",
            Self::Sparkline => "sparkline",
            Self::Canvas => "canvas",
        }
    }
}

impl std::fmt::Display for WidgetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A parse failure with a 1-based source line number for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number where the error occurred.
    pub line: usize,
    /// Human-readable description of the problem.
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parse widget source text into an ordered list of [`Widget`]s.
///
/// Returns an error (with a line number) on the first malformed construct.
pub fn parse(source: &str) -> Result<Vec<Widget>, ParseError> {
    Parser::new(source).parse_all()
}

/// Internal line-oriented parser state.
struct Parser<'a> {
    lines: Vec<&'a str>,
    idx: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            lines: source.lines().collect(),
            idx: 0,
        }
    }

    fn parse_all(mut self) -> Result<Vec<Widget>, ParseError> {
        let mut widgets = Vec::new();
        loop {
            self.skip_blank_and_comments();
            if self.idx >= self.lines.len() {
                break;
            }
            widgets.push(self.parse_widget()?);
        }
        Ok(widgets)
    }

    /// Skip blank lines and comment-only lines.
    fn skip_blank_and_comments(&mut self) {
        while self.idx < self.lines.len() {
            let stripped = strip_comment(self.lines[self.idx]);
            if stripped.trim().is_empty() {
                self.idx += 1;
            } else {
                break;
            }
        }
    }

    /// Parse a single `widget NAME KIND { ... }` block starting at the current
    /// line.
    fn parse_widget(&mut self) -> Result<Widget, ParseError> {
        let header_line = self.idx;
        let header = strip_comment(self.lines[self.idx]);
        let tokens: Vec<&str> = header.split_whitespace().collect();

        if tokens.len() < 2 {
            return Err(self.err(header_line, "expected `widget <name> <kind>`"));
        }
        if !tokens[0].eq_ignore_ascii_case("widget") {
            return Err(self.err(header_line, "expected `widget` keyword"));
        }
        let name = tokens[1].to_string();
        if !is_name(&name) {
            return Err(self.err(header_line, format!("invalid widget name `{name}`")));
        }
        if tokens.len() < 3 {
            return Err(self.err(
                header_line,
                format!("widget `{name}` is missing a kind"),
            ));
        }
        let kind = WidgetKind::parse(tokens[2]).ok_or_else(|| {
            self.err(
                header_line,
                format!("unknown widget kind `{}`", tokens[2]),
            )
        })?;
        // Any extra tokens on the header line are an error (keeps the grammar tight).
        if tokens.len() > 3 {
            return Err(self.err(
                header_line,
                format!("unexpected token `{}` after kind", tokens[3]),
            ));
        }

        self.idx += 1;

        // Expect an opening brace on the next non-blank line.
        self.skip_blank_and_comments();
        if self.idx >= self.lines.len() {
            return Err(self.err(header_line, format!("widget `{name}` is missing `{{`")));
        }
        let open_line = self.idx;
        let open = strip_comment(self.lines[self.idx]).trim().to_string();
        if open != "{" {
            return Err(self.err(open_line, "expected `{` to open the widget body"));
        }
        self.idx += 1;

        let mut props = BTreeMap::new();
        loop {
            self.skip_blank_and_comments();
            if self.idx >= self.lines.len() {
                return Err(self.err(
                    header_line,
                    format!("widget `{name}` is missing closing `}}`"),
                ));
            }
            let line_no = self.idx;
            let line = strip_comment(self.lines[self.idx]).trim().to_string();
            if line == "}" {
                self.idx += 1;
                break;
            }
            let (key, value) = parse_prop(&line).map_err(|msg| self.err(line_no, msg))?;
            if props.insert(key.clone(), value).is_some() {
                return Err(self.err(
                    line_no,
                    format!("duplicate property `{key}` in widget `{name}`"),
                ));
            }
            self.idx += 1;
        }

        Ok(Widget {
            name,
            kind,
            props,
        })
    }

    fn err(&self, line: usize, message: impl Into<String>) -> ParseError {
        ParseError {
            line: line + 1, // 1-based
            message: message.into(),
        }
    }
}

/// Strip a trailing `//` comment (respecting quoted strings so `//` inside a
/// string literal is preserved).
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut chars = line.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '/' if !in_string => {
                if line[i + 1..].starts_with('/') {
                    return &line[..i];
                }
            }
            _ => {}
        }
    }
    line
}

/// Parse a single `key = value` property line.
fn parse_prop(line: &str) -> Result<(String, String), String> {
    let Some(eq) = line.find('=') else {
        return Err(format!("expected `key = value`, got `{line}`"));
    };
    let key = line[..eq].trim();
    if !is_name(key) {
        return Err(format!("invalid property name `{key}`"));
    }
    let raw_value = line[eq + 1..].trim();
    if raw_value.is_empty() {
        return Err(format!("property `{key}` is missing a value"));
    }
    let value = parse_value(raw_value)?;
    Ok((key.to_string(), value))
}

/// Parse a value token: a quoted string or a bare token.
fn parse_value(raw: &str) -> Result<String, String> {
    if raw.starts_with('"') {
        let rest = &raw[1..];
        let mut out = String::new();
        let mut chars = rest.char_indices().peekable();
        while let Some((_, c)) = chars.next() {
            match c {
                '"' => {
                    // Ensure nothing but whitespace follows the closing quote.
                    let tail = &rest[chars.peek().map(|(i, _)| *i).unwrap_or(rest.len())..];
                    if !tail.trim().is_empty() {
                        return Err("unexpected characters after closing quote".to_string());
                    }
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, '"')) => out.push('"'),
                    Some((_, '\\')) => out.push('\\'),
                    Some((_, other)) => {
                        return Err(format!("invalid escape `\\{other}`"));
                    }
                    None => return Err("unterminated escape sequence".to_string()),
                },
                _ => out.push(c),
            }
        }
        Err("unterminated string literal".to_string())
    } else {
        // Bare token: no whitespace, no braces, no quotes.
        if raw.contains('"') {
            return Err("unexpected quote in bare value".to_string());
        }
        if raw.contains('{') || raw.contains('}') {
            return Err("unexpected brace in bare value".to_string());
        }
        if raw.split_whitespace().count() != 1 {
            return Err("bare value must not contain whitespace".to_string());
        }
        Ok(raw.to_string())
    }
}

/// Whether `s` is a valid bare identifier.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_source() -> &'static str {
        r#"
// A simple chat panel.
widget chat paragraph {
    title = "Chat"
    border = "true"
    width  = 40
}

widget plan list {
    title = "Execution Plan"
    wrap  = "false"
}
"#
    }

    #[test]
    fn parses_multiple_widgets() {
        let widgets = parse(sample_source()).expect("sample parses");
        assert_eq!(widgets.len(), 2);

        assert_eq!(widgets[0].name, "chat");
        assert_eq!(widgets[0].kind, WidgetKind::Paragraph);
        assert_eq!(widgets[0].get("title"), Some("Chat"));
        assert_eq!(widgets[0].get_bool("border"), Some(true));
        assert_eq!(widgets[0].get_u64("width"), Some(40));

        assert_eq!(widgets[1].name, "plan");
        assert_eq!(widgets[1].kind, WidgetKind::List);
        assert_eq!(widgets[1].get_bool("wrap"), Some(false));
    }

    #[test]
    fn kind_parsing_is_case_insensitive() {
        assert_eq!(WidgetKind::parse("Paragraph"), Some(WidgetKind::Paragraph));
        assert_eq!(WidgetKind::parse("SPARKLINE"), Some(WidgetKind::Sparkline));
        assert_eq!(WidgetKind::parse("table"), Some(WidgetKind::Table));
        assert_eq!(WidgetKind::parse("bogus"), None);
        assert_eq!(WidgetKind::as_str(WidgetKind::Gauge), "gauge");
    }

    #[test]
    fn empty_source_yields_no_widgets() {
        assert_eq!(parse("").unwrap(), Vec::new());
        assert_eq!(parse("\n\n// just a comment\n\n").unwrap(), Vec::new());
    }

    #[test]
    fn bare_values_are_supported() {
        let src = "widget w gauge {\n  value = 42\n  label = ready\n}\n";
        let widgets = parse(src).unwrap();
        assert_eq!(widgets.len(), 1);
        assert_eq!(widgets[0].kind, WidgetKind::Gauge);
        assert_eq!(widgets[0].get("value"), Some("42"));
        assert_eq!(widgets[0].get("label"), Some("ready"));
    }

    #[test]
    fn string_escapes_are_decoded() {
        let src = r#"widget w block {
  title = "a \"quoted\" \\ path"
}
"#;
        let widgets = parse(src).unwrap();
        assert_eq!(widgets[0].get("title"), Some("a \"quoted\" \\ path"));
    }

    #[test]
    fn comment_inside_string_is_preserved() {
        let src = r#"widget w paragraph {
  text = "http://example.com"
}
"#;
        let widgets = parse(src).unwrap();
        assert_eq!(widgets[0].get("text"), Some("http://example.com"));
    }

    #[test]
    fn duplicate_property_is_rejected() {
        let src = "widget w paragraph {\n  a = 1\n  a = 2\n}\n";
        let err = parse(src).unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("duplicate property"));
    }

    #[test]
    fn missing_kind_is_rejected() {
        let err = parse("widget w\n").unwrap_err();
        assert!(err.message.contains("missing a kind"));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = parse("widget w frobnicate {\n}\n").unwrap_err();
        assert!(err.message.contains("unknown widget kind"));
    }

    #[test]
    fn missing_opening_brace_is_rejected() {
        let err = parse("widget w paragraph\n").unwrap_err();
        assert!(err.message.contains("missing `{`"));
    }

    #[test]
    fn missing_closing_brace_is_rejected() {
        let err = parse("widget w paragraph {\n  a = 1\n").unwrap_err();
        assert!(err.message.contains("missing closing"));
    }

    #[test]
    fn unterminated_string_is_rejected() {
        let err = parse("widget w paragraph {\n  title = \"oops\n}\n").unwrap_err();
        assert!(err.message.contains("unterminated string"));
    }

    #[test]
    fn malformed_prop_line_is_rejected() {
        let err = parse("widget w paragraph {\n  just_a_key\n}\n").unwrap_err();
        assert!(err.message.contains("expected `key = value`"));
    }

    #[test]
    fn display_of_error_includes_line() {
        let err = ParseError {
            line: 7,
            message: "boom".to_string(),
        };
        assert_eq!(err.to_string(), "line 7: boom");
    }
}
