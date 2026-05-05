//! Parser + translator for PA expression strings (the `@...` and `@{...}`
//! syntax that lives inside string values in a PA flow's JSON).
//!
//! Used by the decoder (slice 44c+) to recover pax source from PA value
//! slots that contain expressions. The split is deliberate: the parser
//! produces a faithful PA-shaped AST, and the translator maps that AST to
//! pax source — only when every node has a pax-renderable form. Anything
//! the translator can't render forces the caller to fall back to `pa <Name>`.
//!
//! Translatable today:
//!   - JSON-style literals (numbers, single-quoted strings, true/false/null)
//!   - `variables('name')` — when `name` is a declared pax binding
//!   - `outputs('Compose_<id>')` — when `<id>` is a declared pax binding
//!   - `?['<key>']` member access — when `<key>` is a valid pax identifier
//!   - PA arithmetic: `add/sub/mul/div` (2-arg) → `+ - * /`
//!   - PA concat: `concat(a, b, ...)` (≥2-arg) → `&` chain
//!   - PA comparisons: `equals/less/lessOrEquals/greater/greaterOrEquals`
//!   - PA logical: `not`, `and`, `or` (variadic for and/or)
//!   - Synthesized inverses: `not(equals(a, b))` → `a != b`, `sub(0, x)` → `-x`
//!   - Generic `f(args)` call where every arg renders and `f` is a pax-known
//!     identifier (lets through anything in pa::names::is_known_function and
//!     valid identifier shape; rejects PA accessors that need scope context)
//!
//! Forces fallback (returns None):
//!   - `triggerBody`, `triggerOutputs`, `parameters(...)`, `body(...)`,
//!     `actions(...)`, `trigger(...)`, `items(...)`, `iterationIndexes(...)` —
//!     PA accessors that pax has no native form for at slice 44c
//!   - Member keys that aren't valid pax identifiers (slashes, spaces, dots
//!     inside the key, leading digit, etc.)
//!   - `@@` escape sequences in template strings (rare, deferred)
//!
//! The grammar implemented here is the subset of PA expressions paxc emits
//! plus the variants observed in the corpus. PA's full expression language
//! (date/time literals, complex operators, etc.) is not exhaustively covered;
//! anything outside this subset returns None and the caller falls back.

use std::collections::{HashMap, HashSet};

/// A JSON-like value used as a PA literal inside expressions or value slots.
/// Mirrors the subset of `serde_json::Value` we can render in pax source.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PaLit {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    EmptyArray,
    EmptyObject,
}

/// PA expression AST. Faithful representation of the parsed source — no
/// translation decisions made at parse time.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PaExpr {
    Lit(PaLit),
    /// `?['key']` member access on a target expression. Slash-bearing or
    /// otherwise-non-identifier keys are still parsed faithfully here; the
    /// translator decides whether they're representable in pax.
    Member {
        target: Box<PaExpr>,
        key: String,
    },
    /// `name(args...)` function or accessor invocation.
    Call {
        name: String,
        args: Vec<PaExpr>,
    },
}

/// Top-level parse result for a string value found in a PA flow's JSON.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum PaValue {
    /// The string had no `@` magic — it's a literal.
    Literal(PaLit),
    /// The whole string is a single PA expression (`@expr` or `@{expr}`).
    Expression(PaExpr),
    /// Mixed text + `@{expr}` placeholders. The vector alternates Text and
    /// Expr parts in source order. A vector with a single Expr part and no
    /// Text neighbors is normalized into the `Expression` variant by the
    /// caller, never produced here.
    Template(Vec<TemplatePart>),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TemplatePart {
    Text(String),
    Expr(PaExpr),
}

/// Parse a string from a PA action's value slot into a `PaValue`. Returns
/// None when the string contains `@` patterns we don't understand (e.g. a
/// `@@` escape outside of a clear context, malformed `@{...}` braces, or a
/// PA expression body that doesn't parse).
pub(super) fn parse_pa_string(s: &str) -> Option<PaValue> {
    if !s.contains('@') {
        return Some(PaValue::Literal(PaLit::String(s.to_string())));
    }

    // Whole-string single-expression form: `@expr` (no braces, no leading
    // text, must consume the whole string).
    if let Some(rest) = s.strip_prefix('@')
        && !rest.starts_with('{')
        && !rest.starts_with('@')
        && let Some(expr) = parse_expression(rest)
    {
        return Some(PaValue::Expression(expr));
    }

    // Template form: scan for `@{...}` placeholders and `@@` escapes,
    // collecting alternating Text and Expr parts.
    let parts = parse_template(s)?;
    // Normalize a single-Expr template into Expression. A template like
    // `@{variables('x')}` (no surrounding text) reads more naturally as a
    // single expression to downstream code.
    if parts.len() == 1
        && let TemplatePart::Expr(e) = &parts[0]
    {
        return Some(PaValue::Expression(e.clone()));
    }
    Some(PaValue::Template(parts))
}

/// Parse the inside of a `@{...}` placeholder or after a leading `@`. The
/// whole input must be one expression — trailing characters fail.
fn parse_expression(src: &str) -> Option<PaExpr> {
    let mut p = Parser::new(src);
    let expr = p.parse_expr()?;
    p.skip_ws();
    if !p.at_end() {
        return None;
    }
    Some(expr)
}

/// Walk the source and split into Text / `@{expr}` / Text parts. Handles
/// `@@` as a literal `@` in text. A bare `@` not followed by `{` or `@`
/// causes the whole template to fail (callers should already have tried
/// the single-expression form).
fn parse_template(src: &str) -> Option<Vec<TemplatePart>> {
    let bytes = src.as_bytes();
    let mut parts: Vec<TemplatePart> = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'@' {
            let next = bytes.get(i + 1).copied();
            match next {
                Some(b'@') => {
                    text.push('@');
                    i += 2;
                }
                Some(b'{') => {
                    if !text.is_empty() {
                        parts.push(TemplatePart::Text(std::mem::take(&mut text)));
                    }
                    // Find the matching `}` honoring `'...'` strings and
                    // nested braces inside expression args.
                    let body_start = i + 2;
                    let body_end = find_matching_brace(&src[body_start..])?;
                    let body = &src[body_start..body_start + body_end];
                    let expr = parse_expression(body)?;
                    parts.push(TemplatePart::Expr(expr));
                    i = body_start + body_end + 1; // past the `}`
                }
                _ => {
                    // Bare `@` — only valid as the very first char of the
                    // string and only when the rest is one expression.
                    // Inside a template, treat it as a parse failure so the
                    // caller falls back.
                    return None;
                }
            }
        } else {
            // Append the next UTF-8 char (cluster of bytes) to text. We only
            // matched on ASCII, so we can read one char at a time via the
            // chars iterator from this point — but to keep i in sync, find
            // the boundary of the next char.
            let ch_end = next_char_boundary(src, i);
            text.push_str(&src[i..ch_end]);
            i = ch_end;
        }
    }
    if !text.is_empty() {
        parts.push(TemplatePart::Text(text));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// Returns the byte index of the matching `}` for an expression body, given
/// the slice that starts AFTER the opening `@{`. Honors `'...'` string
/// literals (with `''` doubled-quote escape) and nested `{...}` pairs.
fn find_matching_brace(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut depth: usize = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                // Skip past the matching quote, accounting for `''` escapes.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
                i += 1; // past closing quote
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

// --- Recursive-descent parser for the inside of `@{...}` ---

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek_byte(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek_byte() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek_byte() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_expr(&mut self) -> Option<PaExpr> {
        self.skip_ws();
        let atom = self.parse_atom()?;
        self.parse_postfix(atom)
    }

    fn parse_atom(&mut self) -> Option<PaExpr> {
        self.skip_ws();
        match self.peek_byte()? {
            b'\'' => self
                .parse_string_literal()
                .map(|s| PaExpr::Lit(PaLit::String(s))),
            b'-' | b'0'..=b'9' => self.parse_number_literal(),
            b if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => self.parse_ident_or_call(),
            _ => None,
        }
    }

    fn parse_postfix(&mut self, mut expr: PaExpr) -> Option<PaExpr> {
        loop {
            self.skip_ws();
            if !self.eat(b'?') {
                break;
            }
            self.skip_ws();
            if !self.eat(b'[') {
                return None;
            }
            self.skip_ws();
            // Bracket key: PA almost always uses a single-quoted string here.
            // Other shapes (numeric index, expression index) we don't
            // translate; bail.
            if self.peek_byte() != Some(b'\'') {
                return None;
            }
            let key = self.parse_string_literal()?;
            self.skip_ws();
            if !self.eat(b']') {
                return None;
            }
            expr = PaExpr::Member {
                target: Box::new(expr),
                key,
            };
        }
        Some(expr)
    }

    /// Parse a single-quoted PA string literal with `''` escape for an
    /// embedded single quote. Position must currently point at the opening
    /// `'`. Advances past the closing `'`.
    fn parse_string_literal(&mut self) -> Option<String> {
        if !self.eat(b'\'') {
            return None;
        }
        let bytes = self.src.as_bytes();
        let mut out = String::new();
        while self.pos < bytes.len() {
            let b = bytes[self.pos];
            if b == b'\'' {
                // Check for `''` doubled-quote escape.
                if bytes.get(self.pos + 1) == Some(&b'\'') {
                    out.push('\'');
                    self.pos += 2;
                    continue;
                }
                self.pos += 1; // past closing quote
                return Some(out);
            }
            // Append next UTF-8 char.
            let end = next_char_boundary(self.src, self.pos);
            out.push_str(&self.src[self.pos..end]);
            self.pos = end;
        }
        // Unterminated string.
        None
    }

    fn parse_number_literal(&mut self) -> Option<PaExpr> {
        let start = self.pos;
        if self.peek_byte() == Some(b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == digits_start {
            return None;
        }
        let mut is_float = false;
        if self.peek_byte() == Some(b'.') {
            // Need at least one digit after the dot.
            let dot_pos = self.pos;
            self.pos += 1;
            let frac_start = self.pos;
            while let Some(b) = self.peek_byte() {
                if b.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.pos == frac_start {
                // Not a float; back up the dot. PA wouldn't write `5.` so
                // this is more defensive than necessary.
                self.pos = dot_pos;
            } else {
                is_float = true;
            }
        }
        let lex = &self.src[start..self.pos];
        if is_float {
            let n: f64 = lex.parse().ok()?;
            Some(PaExpr::Lit(PaLit::Float(n)))
        } else {
            let n: i64 = lex.parse().ok()?;
            Some(PaExpr::Lit(PaLit::Int(n)))
        }
    }

    fn parse_ident_or_call(&mut self) -> Option<PaExpr> {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return None;
        }
        let name = self.src[start..self.pos].to_string();

        // Reserved words that aren't function/accessor names.
        match name.as_str() {
            "true" => return Some(PaExpr::Lit(PaLit::Bool(true))),
            "false" => return Some(PaExpr::Lit(PaLit::Bool(false))),
            "null" => return Some(PaExpr::Lit(PaLit::Null)),
            _ => {}
        }

        self.skip_ws();
        if self.peek_byte() != Some(b'(') {
            // PA expressions virtually always wrap accessors in `()`. A
            // bare identifier here is an unusual shape; treat as parse
            // failure so the caller falls back.
            return None;
        }
        self.pos += 1; // past `(`

        let mut args = Vec::new();
        self.skip_ws();
        if self.peek_byte() == Some(b')') {
            self.pos += 1;
        } else {
            loop {
                let arg = self.parse_expr()?;
                args.push(arg);
                self.skip_ws();
                match self.peek_byte()? {
                    b',' => {
                        self.pos += 1;
                    }
                    b')' => {
                        self.pos += 1;
                        break;
                    }
                    _ => return None,
                }
            }
        }
        Some(PaExpr::Call { name, args })
    }
}

// --- Translation: PaValue / PaExpr → pax source string ---

/// Context passed to the translator when rendering a PA expression as pax
/// source. The decoder threads this so the translator knows which names
/// are already declared in the emitted pax (only declared names can be
/// referenced via `variables('x')` / `outputs('Compose_x')` lowering).
///
/// `iterators` maps a PA `Foreach`/`Apply_to_each` action key to the pax
/// iterator name in scope at the call site. When a `items('<key>')`
/// expression appears inside a foreach body and `<key>` is in this map, the
/// translator emits the pax iterator name. Outside any foreach scope (or
/// for an unknown key) `items(...)` forces fallback.
pub(super) struct RenderCtx<'a> {
    pub bindings: &'a HashSet<String>,
    pub iterators: &'a HashMap<String, String>,
}

impl<'a> RenderCtx<'a> {
    /// Convenience for callers that don't have an iterator scope: pass an
    /// empty map. Most slice 44a/b/c call sites use this.
    pub fn new(bindings: &'a HashSet<String>, iterators: &'a HashMap<String, String>) -> Self {
        Self {
            bindings,
            iterators,
        }
    }
}

/// Top-level: render a parsed PA value to pax source. Returns None if any
/// part of the value can't be expressed. The output is a pax expression
/// suitable for the right-hand side of `var x: T = <here>`, `let x = <here>`,
/// `x = <here>`, `x += <here>`, etc.
pub(super) fn render_pa_value(value: &PaValue, ctx: &RenderCtx<'_>) -> Option<String> {
    match value {
        PaValue::Literal(lit) => render_literal(lit),
        PaValue::Expression(expr) => render_expr(expr, ctx),
        PaValue::Template(parts) => render_template(parts, ctx),
    }
}

/// Render a `PaLit` as a pax source literal. Sibling modules call this
/// when they need to format an extracted literal (e.g. a Switch case value)
/// the same way the rest of the translator would.
pub(super) fn render_pa_lit(lit: &PaLit) -> Option<String> {
    render_literal(lit)
}

fn render_literal(lit: &PaLit) -> Option<String> {
    Some(match lit {
        PaLit::Null => "null".to_string(),
        PaLit::Bool(true) => "true".to_string(),
        PaLit::Bool(false) => "false".to_string(),
        PaLit::Int(n) => n.to_string(),
        PaLit::Float(x) => format_pax_float(*x),
        PaLit::String(s) => format!("\"{}\"", escape_pax_string(s)),
        PaLit::EmptyArray => "[]".to_string(),
        PaLit::EmptyObject => "{}".to_string(),
    })
}

fn render_expr(expr: &PaExpr, ctx: &RenderCtx<'_>) -> Option<String> {
    match expr {
        PaExpr::Lit(lit) => render_literal(lit),
        PaExpr::Member { target, key } => {
            if !is_pax_ident(key) {
                return None;
            }
            let inner = render_expr(target, ctx)?;
            Some(format!("{inner}.{key}"))
        }
        PaExpr::Call { name, args } => render_call(name, args, ctx),
    }
}

fn render_call(name: &str, args: &[PaExpr], ctx: &RenderCtx<'_>) -> Option<String> {
    // Accessors that resolve to pax identifiers.
    match name {
        "variables" => return render_var_accessor(args, ctx),
        "outputs" => return render_output_accessor(args, ctx),
        "items" => return render_items_accessor(args, ctx),
        // Accessors with no native pax form at slice 44d.
        "iterationIndexes" | "triggerBody" | "triggerOutputs" | "trigger" | "actions"
        | "parameters" | "body" => return None,
        _ => {}
    }

    // PA arithmetic, comparison, logical functions that map to pax operators.
    if let Some(s) = render_operator(name, args, ctx) {
        return Some(s);
    }

    // Generic call: f(args) — only emit if `f` is a name we recognize as a
    // pax-known function and every arg renders. This avoids producing pax
    // source that won't compile because of an unknown-function reference.
    if !crate::pa::names::is_known_function(name) || !is_pax_ident(name) {
        return None;
    }
    let rendered: Option<Vec<String>> = args.iter().map(|a| render_expr(a, ctx)).collect();
    let rendered = rendered?;
    Some(format!("{name}({})", rendered.join(", ")))
}

/// `variables('x')` → `x` if `x` is declared. Pax binds vars and lets in
/// one namespace; the caller's `bindings` set is the source of truth.
fn render_var_accessor(args: &[PaExpr], ctx: &RenderCtx<'_>) -> Option<String> {
    if args.len() != 1 {
        return None;
    }
    let key = string_arg(&args[0])?;
    if !is_pax_ident(&key) {
        return None;
    }
    if !ctx.bindings.contains(&key) {
        return None;
    }
    // Pax keywords would shadow if we emitted them as identifiers; reject
    // names that collide with reserved words (extremely defensive — pax
    // wouldn't have let the var be declared with a keyword name in the
    // first place — but cheap to verify).
    if PAX_KEYWORDS.contains(&key.as_str()) {
        return None;
    }
    Some(key)
}

/// `items('<foreach_key>')` → `<pax_iter_name>` if the key is in the
/// current foreach iterator scope. Pax body code references the iterator
/// by the local pax name, not by the underlying PA action key.
fn render_items_accessor(args: &[PaExpr], ctx: &RenderCtx<'_>) -> Option<String> {
    if args.len() != 1 {
        return None;
    }
    let key = string_arg(&args[0])?;
    let pax_name = ctx.iterators.get(&key)?;
    if PAX_KEYWORDS.contains(&pax_name.as_str()) {
        return None;
    }
    Some(pax_name.clone())
}

/// `outputs('Compose_<id>')` → `<id>` if `<id>` is declared as a let.
fn render_output_accessor(args: &[PaExpr], ctx: &RenderCtx<'_>) -> Option<String> {
    if args.len() != 1 {
        return None;
    }
    let key = string_arg(&args[0])?;
    let name = key.strip_prefix("Compose_")?;
    if !is_pax_ident(name) {
        return None;
    }
    if !ctx.bindings.contains(name) {
        return None;
    }
    if PAX_KEYWORDS.contains(&name) {
        return None;
    }
    Some(name.to_string())
}

/// Recover pax operator forms from PA function calls. Returns Some when the
/// arity matches a pax operator; None falls through to the generic-call
/// handler.
fn render_operator(name: &str, args: &[PaExpr], ctx: &RenderCtx<'_>) -> Option<String> {
    // not(equals(a, b)) → a != b. Has to be checked before the generic
    // not(...) path so the `!=` reduction wins.
    if name == "not" && args.len() == 1 {
        if let PaExpr::Call {
            name: inner_name,
            args: inner_args,
        } = &args[0]
            && inner_name == "equals"
            && inner_args.len() == 2
        {
            let lhs = render_expr(&inner_args[0], ctx)?;
            let rhs = render_expr(&inner_args[1], ctx)?;
            return Some(format!("({lhs} != {rhs})"));
        }
        let inner = render_expr(&args[0], ctx)?;
        return Some(format!("!{inner}"));
    }
    // sub(0, x) → -x. Same pattern as the emitter's UnaryOp::Neg synthesis.
    if name == "sub" && args.len() == 2 {
        if let PaExpr::Lit(PaLit::Int(0)) = &args[0] {
            let inner = render_expr(&args[1], ctx)?;
            return Some(format!("-{inner}"));
        }
        let lhs = render_expr(&args[0], ctx)?;
        let rhs = render_expr(&args[1], ctx)?;
        return Some(format!("({lhs} - {rhs})"));
    }
    let binary_op = match (name, args.len()) {
        ("add", 2) => Some("+"),
        ("mul", 2) => Some("*"),
        ("div", 2) => Some("/"),
        ("equals", 2) => Some("=="),
        ("less", 2) => Some("<"),
        ("lessOrEquals", 2) => Some("<="),
        ("greater", 2) => Some(">"),
        ("greaterOrEquals", 2) => Some(">="),
        _ => None,
    };
    if let Some(op) = binary_op {
        let lhs = render_expr(&args[0], ctx)?;
        let rhs = render_expr(&args[1], ctx)?;
        return Some(format!("({lhs} {op} {rhs})"));
    }

    // concat(a, b, c) → (a & b & c). PA permits any number of args ≥ 1.
    if name == "concat" && args.len() >= 2 {
        let parts: Option<Vec<String>> = args.iter().map(|a| render_expr(a, ctx)).collect();
        let parts = parts?;
        return Some(format!("({})", parts.join(" & ")));
    }

    // and(a, b, ...) → (a && b && ...) — variadic.
    // or(a, b, ...) → (a || b || ...).
    let logical_op = match name {
        "and" => Some("&&"),
        "or" => Some("||"),
        _ => None,
    };
    if let Some(op) = logical_op
        && args.len() >= 2
    {
        let parts: Option<Vec<String>> = args.iter().map(|a| render_expr(a, ctx)).collect();
        let parts = parts?;
        return Some(format!("({})", parts.join(&format!(" {op} "))));
    }

    None
}

fn render_template(parts: &[TemplatePart], ctx: &RenderCtx<'_>) -> Option<String> {
    if parts.is_empty() {
        return None;
    }
    let pieces: Option<Vec<String>> = parts
        .iter()
        .map(|p| match p {
            TemplatePart::Text(s) => Some(format!("\"{}\"", escape_pax_string(s))),
            TemplatePart::Expr(e) => {
                // Wrap expressions in parens so the precedence inside the
                // chain is unambiguous (pax `&` is left-associative; an
                // operand like `a + b` shouldn't bind weirdly).
                let s = render_expr(e, ctx)?;
                Some(s)
            }
        })
        .collect();
    let pieces = pieces?;
    if pieces.len() == 1 {
        // Caller normalized single-Expr templates to PaValue::Expression,
        // so this path is single-Text — render as a plain string literal.
        return Some(pieces.into_iter().next().unwrap());
    }
    Some(format!("({})", pieces.join(" & ")))
}

/// Pull the string content from an arg that's a `PaLit::String`. Returns
/// None for anything else — accessors like `variables(...)` must be called
/// with a literal name, not a computed expression.
fn string_arg(expr: &PaExpr) -> Option<String> {
    if let PaExpr::Lit(PaLit::String(s)) = expr {
        Some(s.clone())
    } else {
        None
    }
}

fn is_pax_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

const PAX_KEYWORDS: &[&str] = &[
    "var",
    "let",
    "if",
    "else",
    "foreach",
    "in",
    "until",
    "pa",
    "debug",
    "terminate",
    "switch",
    "case",
    "default",
    "scope",
    "on",
    "null",
    "true",
    "false",
    "int",
    "float",
    "string",
    "bool",
    "array",
    "object",
];

fn format_pax_float(x: f64) -> String {
    if x.is_finite() && x == x.trunc() {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

fn escape_pax_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// Lift a `serde_json::Value` to a `PaLit` when it's a renderable pax
/// literal (no `@`-prefixed strings, only empty arrays/objects). String
/// values that contain `@` patterns return None — the caller should route
/// them through `parse_pa_string` instead.
pub(super) fn json_to_pa_lit(v: &serde_json::Value) -> Option<PaLit> {
    use serde_json::Value;
    match v {
        Value::Null => Some(PaLit::Null),
        Value::Bool(b) => Some(PaLit::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(PaLit::Int(i))
            } else {
                n.as_f64().map(PaLit::Float)
            }
        }
        Value::String(s) => {
            if s.contains('@') {
                None
            } else {
                Some(PaLit::String(s.clone()))
            }
        }
        Value::Array(items) => {
            if items.is_empty() {
                Some(PaLit::EmptyArray)
            } else {
                None
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                Some(PaLit::EmptyObject)
            } else {
                None
            }
        }
    }
}

/// Top-level entry the decoder calls for a `serde_json::Value` in an action's
/// value slot. Tries the literal path first (fastest, covers 44a), then the
/// PA-expression-string path for `@`-flavored strings.
pub(super) fn json_to_pax(v: &serde_json::Value, ctx: &RenderCtx<'_>) -> Option<String> {
    if let Some(lit) = json_to_pa_lit(v) {
        return render_literal(&lit);
    }
    if let serde_json::Value::String(s) = v {
        let value = parse_pa_string(s)?;
        return render_pa_value(&value, ctx);
    }
    None
}

/// Lift a `serde_json::Value` to a `PaExpr` so it can be embedded as an
/// argument inside another `PaExpr::Call`. Used by the object-form
/// condition decoder when an `equals: [a, b]` arm has a literal arg
/// alongside an `@`-prefixed PA expression arg.
fn json_to_pa_expr(v: &serde_json::Value) -> Option<PaExpr> {
    if let Some(lit) = json_to_pa_lit(v) {
        return Some(PaExpr::Lit(lit));
    }
    if let serde_json::Value::String(s) = v {
        let value = parse_pa_string(s)?;
        return match value {
            PaValue::Literal(lit) => Some(PaExpr::Lit(lit)),
            PaValue::Expression(expr) => Some(expr),
            // A templated string can't appear as a sub-expression of a
            // PA condition object — PA wouldn't write that. Bail.
            PaValue::Template(_) => None,
        };
    }
    None
}

/// Decode PA's *condition object* form (used by `If` / `Until`-with-object)
/// into a pax expression source string. PA's designer exports If's
/// `expression` field as a structured JSON object like:
///
/// ```json
/// {"and": [{"equals": ["@variables('approved')", true]}]}
/// ```
///
/// rather than the `@`-prefixed string form paxc itself emits. This entry
/// point handles both shapes: a string flows through `parse_pa_string` and
/// renders as a normal PA expression; an object is interpreted recursively
/// as a `PaExpr::Call` with the JSON key as the function name and its array
/// of children as arguments. Unary forms (`{"not": <inner>}` with a single
/// non-array child) are accepted; everything else expects an array.
///
/// Returns None if any node can't be rendered or the structure is unfamiliar.
pub(super) fn condition_value_to_pax(v: &serde_json::Value, ctx: &RenderCtx<'_>) -> Option<String> {
    let expr = condition_value_to_pa_expr(v)?;
    render_expr(&expr, ctx)
}

fn condition_value_to_pa_expr(v: &serde_json::Value) -> Option<PaExpr> {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            let value = parse_pa_string(s)?;
            match value {
                PaValue::Literal(lit) => Some(PaExpr::Lit(lit)),
                PaValue::Expression(expr) => Some(expr),
                PaValue::Template(_) => None,
            }
        }
        Value::Object(map) => {
            // PA's condition operators each appear as a single-key object
            // whose value is an array of operand expressions. Multi-key
            // shapes (or unknown ops) fall back.
            if map.len() != 1 {
                return None;
            }
            let (op, operand) = map.iter().next().unwrap();
            // Recognized operators. The list mirrors PA's condition vocabulary
            // observed in real exports; anything outside this set forces fallback
            // so paxc never invents a pax operator that PA didn't write.
            const KNOWN_OPS: &[&str] = &[
                "and",
                "or",
                "not",
                "equals",
                "less",
                "lessOrEquals",
                "greater",
                "greaterOrEquals",
                "contains",
                "startsWith",
                "endsWith",
                "empty",
            ];
            if !KNOWN_OPS.contains(&op.as_str()) {
                return None;
            }
            let args: Vec<PaExpr> = match operand {
                Value::Array(items) => items
                    .iter()
                    .map(condition_value_to_pa_expr)
                    .collect::<Option<Vec<_>>>()?,
                // Not-operator can take a single non-array child in some
                // exports; coerce to a one-arg vec for uniform Call shape.
                _ if op == "not" => vec![condition_value_to_pa_expr(operand)?],
                _ => return None,
            };
            Some(PaExpr::Call {
                name: op.clone(),
                args,
            })
        }
        // Bare bool/number/string-without-`@` literals are valid condition
        // operands (they appear as the right-hand side of `equals`) but the
        // top-level `expression` field is always a string or object in PA.
        // Still accept them defensively for compositional reuse.
        _ => json_to_pa_expr(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(names: &[&str]) -> (HashSet<String>, HashMap<String, String>) {
        let set: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        (set, HashMap::new())
    }

    fn render(s: &str, names: &[&str]) -> Option<String> {
        let (bindings, iterators) = ctx_with(names);
        let ctx = RenderCtx::new(&bindings, &iterators);
        let value = parse_pa_string(s)?;
        render_pa_value(&value, &ctx)
    }

    fn render_with_iterators(s: &str, names: &[&str], iters: &[(&str, &str)]) -> Option<String> {
        let bindings: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        let iterators: HashMap<String, String> = iters
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let ctx = RenderCtx::new(&bindings, &iterators);
        let value = parse_pa_string(s)?;
        render_pa_value(&value, &ctx)
    }

    // ---------- parser ----------

    #[test]
    fn parse_plain_string_is_literal() {
        let v = parse_pa_string("hello").unwrap();
        assert_eq!(v, PaValue::Literal(PaLit::String("hello".to_string())));
    }

    #[test]
    fn parse_at_variables_is_expression() {
        let v = parse_pa_string("@variables('x')").unwrap();
        assert_eq!(
            v,
            PaValue::Expression(PaExpr::Call {
                name: "variables".to_string(),
                args: vec![PaExpr::Lit(PaLit::String("x".to_string()))]
            })
        );
    }

    #[test]
    fn parse_at_braces_is_expression() {
        let v = parse_pa_string("@{variables('x')}").unwrap();
        assert!(matches!(v, PaValue::Expression(_)));
    }

    #[test]
    fn parse_template_with_text_and_expr() {
        let v = parse_pa_string("hello @{variables('name')}!").unwrap();
        match v {
            PaValue::Template(parts) => {
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0], TemplatePart::Text("hello ".to_string()));
                match &parts[1] {
                    TemplatePart::Expr(_) => {}
                    _ => panic!("expected Expr in middle: {parts:?}"),
                }
                assert_eq!(parts[2], TemplatePart::Text("!".to_string()));
            }
            other => panic!("expected Template, got {other:?}"),
        }
    }

    #[test]
    fn parse_member_access_with_quoted_key() {
        let v = parse_pa_string("@outputs('A')?['body']").unwrap();
        match v {
            PaValue::Expression(PaExpr::Member { target, key }) => {
                assert_eq!(key, "body");
                assert!(matches!(*target, PaExpr::Call { .. }));
            }
            other => panic!("expected Member, got {other:?}"),
        }
    }

    #[test]
    fn parse_chained_member_access() {
        let v = parse_pa_string("@outputs('A')?['body']?['name']").unwrap();
        match v {
            PaValue::Expression(PaExpr::Member { target, key }) => {
                assert_eq!(key, "name");
                match *target {
                    PaExpr::Member { key: inner_key, .. } => assert_eq!(inner_key, "body"),
                    other => panic!("expected outer Member to wrap Member, got {other:?}"),
                }
            }
            other => panic!("expected outer Member, got {other:?}"),
        }
    }

    #[test]
    fn parse_call_with_nested_args() {
        let v = parse_pa_string("@add(variables('a'), 5)").unwrap();
        match v {
            PaValue::Expression(PaExpr::Call { name, args }) => {
                assert_eq!(name, "add");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn parse_literal_int_inside_expression() {
        let v = parse_pa_string("@add(1, 2)").unwrap();
        match v {
            PaValue::Expression(PaExpr::Call { args, .. }) => {
                assert_eq!(args[0], PaExpr::Lit(PaLit::Int(1)));
                assert_eq!(args[1], PaExpr::Lit(PaLit::Int(2)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_literal_negative_int() {
        let v = parse_pa_string("@sub(0, 5)").unwrap();
        match v {
            PaValue::Expression(PaExpr::Call { args, .. }) => {
                assert_eq!(args[0], PaExpr::Lit(PaLit::Int(0)));
                assert_eq!(args[1], PaExpr::Lit(PaLit::Int(5)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_string_literal_with_doubled_quote() {
        let v = parse_pa_string("@equals(variables('x'), 'it''s')").unwrap();
        match v {
            PaValue::Expression(PaExpr::Call { args, .. }) => {
                assert_eq!(args[1], PaExpr::Lit(PaLit::String("it's".to_string())));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_bool_and_null_literals() {
        match parse_pa_string("@equals(variables('x'), true)").unwrap() {
            PaValue::Expression(PaExpr::Call { args, .. }) => {
                assert_eq!(args[1], PaExpr::Lit(PaLit::Bool(true)))
            }
            _ => panic!(),
        }
        match parse_pa_string("@equals(variables('x'), null)").unwrap() {
            PaValue::Expression(PaExpr::Call { args, .. }) => {
                assert_eq!(args[1], PaExpr::Lit(PaLit::Null))
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parse_rejects_unterminated_string() {
        // PA exporter never emits this, but defensive check on the parser.
        assert!(parse_pa_string("@equals(variables('x'), 'unfinished").is_none());
    }

    #[test]
    fn parse_at_at_in_template_is_literal_at() {
        let v = parse_pa_string("price@@example.com").unwrap();
        match v {
            PaValue::Template(parts) => {
                assert_eq!(
                    parts,
                    vec![TemplatePart::Text("price@example.com".to_string())]
                );
            }
            _ => panic!(),
        }
    }

    // ---------- translation: accessors ----------

    #[test]
    fn translate_variables_when_declared() {
        assert_eq!(render("@variables('x')", &["x"]), Some("x".to_string()));
    }

    #[test]
    fn translate_variables_falls_back_when_undeclared() {
        // Conservative: if the var hasn't been declared in pax yet, the
        // emitted source wouldn't compile. Force fallback.
        assert_eq!(render("@variables('x')", &[]), None);
    }

    #[test]
    fn translate_outputs_strips_compose_prefix() {
        assert_eq!(
            render("@outputs('Compose_total')", &["total"]),
            Some("total".to_string())
        );
    }

    #[test]
    fn translate_outputs_falls_back_for_non_compose_action() {
        // The action key needs the `Compose_` prefix to recover a pax let
        // name. Anything else is a connector / compose-with-no-name etc.
        assert_eq!(render("@outputs('Send_an_email_V2')", &[]), None);
    }

    #[test]
    fn translate_falls_back_for_trigger_accessors() {
        // No native pax form for triggerBody/triggerOutputs/parameters/body
        // at slice 44c. They force fallback.
        assert!(render("@triggerBody()", &[]).is_none());
        assert!(render("@triggerOutputs()", &[]).is_none());
        assert!(render("@parameters('$authentication')", &[]).is_none());
        assert!(render("@body('GetX')", &[]).is_none());
        assert!(render("@items('For_each')", &[]).is_none());
    }

    // ---------- translation: operators ----------

    #[test]
    fn translate_add_to_plus() {
        assert_eq!(
            render("@add(variables('x'), 1)", &["x"]),
            Some("(x + 1)".to_string())
        );
    }

    #[test]
    fn translate_arithmetic_family() {
        assert_eq!(
            render("@sub(variables('x'), 2)", &["x"]).as_deref(),
            Some("(x - 2)")
        );
        assert_eq!(
            render("@mul(variables('x'), 2)", &["x"]).as_deref(),
            Some("(x * 2)")
        );
        assert_eq!(
            render("@div(variables('x'), 2)", &["x"]).as_deref(),
            Some("(x / 2)")
        );
    }

    #[test]
    fn translate_concat_to_amp() {
        assert_eq!(
            render("@concat('hi ', variables('name'))", &["name"]).as_deref(),
            Some("(\"hi \" & name)")
        );
    }

    #[test]
    fn translate_concat_variadic() {
        assert_eq!(
            render("@concat('a', 'b', 'c')", &[]).as_deref(),
            Some("(\"a\" & \"b\" & \"c\")")
        );
    }

    #[test]
    fn translate_comparisons() {
        assert_eq!(
            render("@equals(variables('x'), 1)", &["x"]).as_deref(),
            Some("(x == 1)")
        );
        assert_eq!(
            render("@less(variables('x'), 1)", &["x"]).as_deref(),
            Some("(x < 1)")
        );
        assert_eq!(
            render("@lessOrEquals(variables('x'), 1)", &["x"]).as_deref(),
            Some("(x <= 1)")
        );
        assert_eq!(
            render("@greater(variables('x'), 1)", &["x"]).as_deref(),
            Some("(x > 1)")
        );
        assert_eq!(
            render("@greaterOrEquals(variables('x'), 1)", &["x"]).as_deref(),
            Some("(x >= 1)")
        );
    }

    #[test]
    fn translate_not_equals_synthesizes_bang_eq() {
        assert_eq!(
            render("@not(equals(variables('x'), 1))", &["x"]).as_deref(),
            Some("(x != 1)")
        );
    }

    #[test]
    fn translate_not_general_form() {
        assert_eq!(
            render("@not(variables('flag'))", &["flag"]).as_deref(),
            Some("!flag")
        );
    }

    #[test]
    fn translate_neg_synthesizes_minus() {
        assert_eq!(
            render("@sub(0, variables('x'))", &["x"]).as_deref(),
            Some("-x")
        );
    }

    #[test]
    fn translate_logical_and_or() {
        assert_eq!(
            render("@and(variables('a'), variables('b'))", &["a", "b"]).as_deref(),
            Some("(a && b)")
        );
        assert_eq!(
            render(
                "@or(variables('a'), variables('b'), variables('c'))",
                &["a", "b", "c"]
            )
            .as_deref(),
            Some("(a || b || c)")
        );
    }

    // ---------- translation: members ----------

    #[test]
    fn translate_member_with_identifier_key() {
        assert_eq!(
            render("@outputs('Compose_obj')?['name']", &["obj"]).as_deref(),
            Some("obj.name")
        );
    }

    #[test]
    fn translate_member_with_slash_key_falls_back() {
        // The classic Forms pattern: `body/raf2bb...`. Pax can't represent
        // a slash in a field name.
        assert!(render("@outputs('Compose_obj')?['body/foo']", &["obj"]).is_none());
    }

    #[test]
    fn translate_chained_member() {
        assert_eq!(
            render("@outputs('Compose_obj')?['body']?['name']", &["obj"]).as_deref(),
            Some("obj.body.name")
        );
    }

    // ---------- translation: templates ----------

    #[test]
    fn translate_template_to_pax_concat() {
        assert_eq!(
            render("hello @{variables('name')}!", &["name"]).as_deref(),
            Some("(\"hello \" & name & \"!\")")
        );
    }

    #[test]
    fn translate_template_with_multiple_exprs() {
        assert_eq!(
            render("@{variables('a')} and @{variables('b')}", &["a", "b"]).as_deref(),
            Some("(a & \" and \" & b)")
        );
    }

    #[test]
    fn translate_template_falls_back_when_part_doesnt_render() {
        // Slash in the ?['..'] key forces a single-part fallback, which
        // poisons the whole template.
        assert!(render("hello @{outputs('Compose_x')?['body/foo']}", &["x"]).is_none());
    }

    // ---------- translation: generic call passthrough ----------

    #[test]
    fn translate_generic_call_for_known_function() {
        assert_eq!(
            render("@length(variables('s'))", &["s"]).as_deref(),
            Some("length(s)")
        );
    }

    #[test]
    fn translate_generic_call_falls_back_for_unknown_function() {
        // PA might allow weird custom names; pax only emits what the
        // function registry knows about.
        assert!(render("@somethingExoticUnknown(variables('s'))", &["s"]).is_none());
    }

    // ---------- json_to_pax entry ----------

    #[test]
    fn json_to_pax_int_passthrough() {
        let bindings = HashSet::new();
        let iters = HashMap::new();
        let ctx = RenderCtx::new(&bindings, &iters);
        assert_eq!(
            json_to_pax(&serde_json::json!(42), &ctx).as_deref(),
            Some("42")
        );
    }

    #[test]
    fn json_to_pax_string_with_at_routes_to_expression_path() {
        let bindings: HashSet<String> = ["x"].iter().map(|s| s.to_string()).collect();
        let iters = HashMap::new();
        let ctx = RenderCtx::new(&bindings, &iters);
        assert_eq!(
            json_to_pax(&serde_json::json!("@variables('x')"), &ctx).as_deref(),
            Some("x")
        );
    }

    #[test]
    fn json_to_pax_plain_string_quotes_it() {
        let bindings = HashSet::new();
        let iters = HashMap::new();
        let ctx = RenderCtx::new(&bindings, &iters);
        assert_eq!(
            json_to_pax(&serde_json::json!("plain"), &ctx).as_deref(),
            Some("\"plain\"")
        );
    }

    // ---------- 44d: items() iterator accessor ----------

    #[test]
    fn items_accessor_resolves_to_pax_iter_name() {
        // Inside a foreach body, `items('For_each')` is a reference to the
        // current iteration value. The decoder maps the PA action key
        // (`For_each`) to the pax iterator name (`item`) via RenderCtx.
        let out = render_with_iterators("@items('For_each')", &[], &[("For_each", "item")]);
        assert_eq!(out.as_deref(), Some("item"));
    }

    #[test]
    fn items_accessor_with_member_access() {
        let out = render_with_iterators(
            "@{items('For_each')?['name']}",
            &[],
            &[("For_each", "item")],
        );
        assert_eq!(out.as_deref(), Some("item.name"));
    }

    #[test]
    fn items_accessor_unknown_key_falls_back() {
        // `items('SomeOtherForeach')` not in the iterators map → None.
        let out = render_with_iterators("@items('SomeOtherForeach')", &[], &[("For_each", "item")]);
        assert!(out.is_none());
    }

    #[test]
    fn items_accessor_with_no_iter_scope_falls_back() {
        // Outside any foreach body, `items(...)` has no native form.
        assert!(render("@items('For_each')", &[]).is_none());
    }

    // ---------- 44d: condition object form ----------

    fn cond(v: serde_json::Value, names: &[&str]) -> Option<String> {
        let bindings: HashSet<String> = names.iter().map(|s| s.to_string()).collect();
        let iters = HashMap::new();
        let ctx = RenderCtx::new(&bindings, &iters);
        condition_value_to_pax(&v, &ctx)
    }

    #[test]
    fn condition_string_form_renders_via_existing_path() {
        // Plain `@`-prefixed expression strings work the same as elsewhere.
        let out = cond(serde_json::json!("@variables('approved')"), &["approved"]);
        assert_eq!(out.as_deref(), Some("approved"));
    }

    #[test]
    fn condition_object_and_equals_to_pax_chain() {
        // PA designer's `if approved == true`:
        //   {"and":[{"equals":["@variables('approved')", true]}]}
        // The `and([equals(...)])` round-trips back to paxc's emitter as
        // `and(equals(...))` which the optimizer treats correctly. We render
        // the literal object form to its semantic equivalent.
        let v = serde_json::json!({
            "and": [{ "equals": ["@variables('approved')", true] }]
        });
        let out = cond(v, &["approved"]);
        // Single-arg `and` doesn't get the && operator (we require ≥2);
        // the generic-call path emits `and((approved == true))`.
        assert_eq!(out.as_deref(), Some("and((approved == true))"));
    }

    #[test]
    fn condition_object_or_chain_to_pax() {
        // `or` with two contains() arms (real corpus shape).
        let v = serde_json::json!({
            "or": [
                { "contains": ["@variables('s')", "Foo"] },
                { "contains": ["@variables('s')", "Bar"] }
            ]
        });
        let out = cond(v, &["s"]);
        assert_eq!(
            out.as_deref(),
            Some("(contains(s, \"Foo\") || contains(s, \"Bar\"))")
        );
    }

    #[test]
    fn condition_object_unknown_op_falls_back() {
        let v = serde_json::json!({"weirdOp": [1, 2]});
        assert!(cond(v, &[]).is_none());
    }

    #[test]
    fn condition_object_undeclared_var_falls_back() {
        let v = serde_json::json!({"equals": ["@variables('missing')", true]});
        assert!(cond(v, &[]).is_none());
    }

    #[test]
    fn condition_object_two_keys_falls_back() {
        // PA's condition objects are always single-key. A two-key map is
        // either malformed or something we don't recognize — bail.
        let v = serde_json::json!({"and": [], "or": []});
        assert!(cond(v, &[]).is_none());
    }
}
