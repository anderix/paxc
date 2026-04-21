//! Pretty diagnostic rendering via the `ariadne` crate.
//!
//! Errors from the lexer, parser, and resolver are funneled through
//! `Diagnostic` for consistent presentation: colored header, filename and
//! line:col, source line with the offending span underlined, short label.

use std::ops::Range;

use ariadne::{Color, Label, Report, ReportKind, Source};

use crate::lexer::Span;

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
pub fn from_lex_error(err: &chumsky::error::Rich<'_, char, Span>) -> Diagnostic {
    Diagnostic::spanned("lex error", *err.span(), format!("{err}"))
}

/// Convert a chumsky parse error into a diagnostic.
pub fn from_parse_error<'src>(
    err: &chumsky::error::Rich<'_, crate::lexer::Token<'src>, Span>,
) -> Diagnostic {
    Diagnostic::spanned("parse error", *err.span(), format!("{err}"))
}

/// Convert a resolver error into a diagnostic. Uses the error's own span
/// (the offending identifier) as the primary label.
pub fn from_resolve_error(err: &crate::resolver::ResolveError) -> Diagnostic {
    Diagnostic::spanned(format!("{err}"), err.span(), err.label())
}
