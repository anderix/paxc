//! Pretty diagnostic rendering via the `ariadne` crate.
//!
//! Errors from the lexer, parser, and resolver are funneled through
//! `Diagnostic` for consistent presentation: colored header, filename and
//! line:col, source line with the offending span underlined, short label.

use std::ops::Range;

use ariadne::{Color, Label, Report, ReportKind, Source};
use chumsky::error::{Rich, RichPattern, RichReason};

use crate::lexer::{Span, Token};

/// A single diagnostic to render. `primary` carries the source span to
/// underline with the main message; `notes` become footer lines.
pub struct Diagnostic {
    pub message: String,
    pub primary: Option<(Range<usize>, String)>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn spanned(message: impl Into<String>, span: Span, label: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            primary: Some((span.start..span.end, label.into())),
            notes: Vec::new(),
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Render to stderr using ariadne. `filename` is used as the source id
    /// in the rendered header; `src` is the full source text.
    pub fn report(&self, filename: &str, src: &str) {
        let offset = self
            .primary
            .as_ref()
            .map(|(r, _)| r.start)
            .unwrap_or(0);

        let mut builder = Report::build(ReportKind::Error, (filename, offset..offset))
            .with_message(&self.message);

        if let Some((range, label)) = &self.primary {
            builder = builder.with_label(
                Label::new((filename, range.clone()))
                    .with_message(label)
                    .with_color(Color::Red),
            );
        }

        for note in &self.notes {
            builder = builder.with_note(note);
        }

        let _ = builder
            .finish()
            .eprint((filename, Source::from(src)));
    }
}

/// Convert a chumsky lex error into a diagnostic.
pub fn from_lex_error(err: &Rich<'_, char, Span>) -> Diagnostic {
    let label = render_rich(err, |c| format!("`{c}`"), "character");
    Diagnostic::spanned("lex error", *err.span(), label)
}

/// Convert a chumsky parse error into a diagnostic.
pub fn from_parse_error<'src>(err: &Rich<'_, Token<'src>, Span>) -> Diagnostic {
    let label = render_rich(err, |t| format!("{t}"), "token");
    Diagnostic::spanned("parse error", *err.span(), label)
}

/// Convert a runtime error from paxr into a diagnostic. The error's span
/// is used if present; otherwise the report renders with just the header.
pub fn from_interpret_error(err: &crate::interpreter::InterpretError) -> Diagnostic {
    match err.span {
        Some(span) => Diagnostic::spanned(
            format!("runtime error: {}", err.message),
            span,
            "here",
        ),
        None => Diagnostic {
            message: format!("runtime error: {}", err.message),
            primary: None,
            notes: Vec::new(),
        },
    }
}

/// Convert a resolver error into a diagnostic. Uses the error's own span
/// (the offending identifier) as the primary label. Adds a "did you mean
/// to call it?" hint when an undefined name matches a known function.
pub fn from_resolve_error(err: &crate::resolver::ResolveError) -> Diagnostic {
    let diag = Diagnostic::spanned(format!("{err}"), err.span(), err.label());
    if let crate::resolver::ResolveError::UndefinedVariable { name, .. } = err {
        if is_known_function(name) {
            return diag.with_note(format!(
                "`{name}` is a function -- did you mean to call it? try `{name}(...)`"
            ));
        }
    }
    diag
}

/// Whether a name matches a function paxr implements or a common PA
/// expression function. Conservative list: add only names that are clearly
/// function-shaped in PA land, to avoid false "did you mean" prompts on
/// plain identifier typos.
fn is_known_function(name: &str) -> bool {
    matches!(
        name,
        // paxr's compiler-synthesized library
        "add" | "sub" | "mul" | "div"
        | "concat" | "equals"
        | "less" | "lessOrEquals" | "greater" | "greaterOrEquals"
        | "not" | "and" | "or"
        // common PA expression functions users reach for without (...)
        | "length" | "empty" | "first" | "last"
        | "body" | "items" | "outputs" | "variables" | "parameters"
        | "triggerBody" | "triggerOutputs"
        | "coalesce" | "createArray" | "join"
        | "contains" | "startsWith" | "endsWith"
        | "indexOf" | "lastIndexOf" | "substring" | "replace"
        | "toLower" | "toUpper" | "trim" | "split"
        | "utcNow" | "formatDateTime"
        | "int" | "string" | "bool" | "float" | "array"
    )
}

/// Humanize a chumsky Rich error as a single label string. `render_token`
/// formats a single token of the error's value type (`char` for the lexer,
/// `Token` for the parser); `input_kind` is the word used when the error's
/// expected set collapses to just `SomethingElse` (e.g. "token", "character").
fn render_rich<'a, T: 'a, S>(
    err: &Rich<'a, T, S>,
    render_token: impl Fn(&T) -> String,
    input_kind: &str,
) -> String {
    match err.reason() {
        RichReason::Custom(msg) => msg.clone(),
        RichReason::ExpectedFound { .. } => {
            let found = match err.found() {
                Some(t) => render_token(t),
                None => "end of input".to_string(),
            };

            let mut items: Vec<String> = err
                .expected()
                .filter_map(|p| render_pattern(p, &render_token))
                .collect();
            items.sort();
            items.dedup();

            let expected = if items.is_empty() {
                format!(", expected something other than this {input_kind}")
            } else {
                format!(", expected {}", join_alternatives(&items))
            };
            format!("found {found}{expected}")
        }
    }
}

fn render_pattern<T>(
    p: &RichPattern<'_, T>,
    render_token: &impl Fn(&T) -> String,
) -> Option<String> {
    match p {
        RichPattern::Token(t) => Some(render_token(&**t)),
        RichPattern::Label(l) => Some(l.to_string()),
        RichPattern::Identifier(s) => Some(format!("`{s}`")),
        RichPattern::Any => Some("any input".to_string()),
        RichPattern::EndOfInput => Some("end of input".to_string()),
        // `SomethingElse` is chumsky's catch-all when an alternative can't
        // be coalesced into a specific pattern. Dropping it on the floor
        // lets the more specific alternatives carry the message; when every
        // alternative collapses to `SomethingElse` we fall back to the
        // "something other than this token" phrasing above.
        RichPattern::SomethingElse => None,
        _ => None,
    }
}

fn join_alternatives(items: &[String]) -> String {
    match items.len() {
        1 => items[0].clone(),
        2 => format!("{} or {}", items[0], items[1]),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("one of {}, or {}", rest.join(", "), last)
        }
    }
}
