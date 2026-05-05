//! paxr interpreter: executes a resolved pax program in-process.
//!
//! The interpreter walks `ResolvedProgram.actions` in source order, treating
//! each `ResolvedAction` as a statement. runAfter is ignored here -- paxr
//! runs single-threaded top-to-bottom, so the source-order sequence already
//! captures dependency order. PA's graph model is only relevant for the
//! compiled flow.
//!
//! Expression evaluation mirrors what Power Automate does at runtime with
//! the compiler-synthesized functions (`add`, `sub`, `concat`, `equals`,
//! etc.). Any other function name (anything the user wrote directly, like
//! `length("x")`) is printed as `<skipping unknown "name">` and evaluates
//! to `Null`. This keeps the interpreter focused and avoids reimplementing
//! PA's 200+ expression functions.

use crate::ast::{BinOp, DebugArg, Expr, HandlerStatus, Literal, Type, UnaryOp};
use crate::lexer::Span;
use crate::resolver::{ActionKind, ResolvedAction, ResolvedProgram};
use serde_json::{Map, Value as JsonValue, json};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    fn to_json(&self) -> JsonValue {
        match self {
            Value::Null => JsonValue::Null,
            Value::Int(n) => json!(n),
            Value::Float(x) => json!(x),
            Value::Str(s) => JsonValue::String(s.clone()),
            Value::Bool(b) => json!(b),
            Value::Array(items) => JsonValue::Array(items.iter().map(Value::to_json).collect()),
            Value::Object(entries) => {
                let mut map = Map::new();
                for (k, v) in entries {
                    map.insert(k.clone(), v.to_json());
                }
                JsonValue::Object(map)
            }
        }
    }

    /// Compact JSON rendering for inline display (debug output).
    fn display_compact(&self) -> String {
        serde_json::to_string(&self.to_json()).unwrap_or_default()
    }

    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Numeric view: Int and Float both lift to f64. Returns None for
    /// non-numeric values.
    pub(crate) fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(x) => Some(*x),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub(crate) fn coerce_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(x) => format_float_display(*x),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            other => serde_json::to_string(&other.to_json()).unwrap_or_default(),
        }
    }

    /// Happy-path numeric equality: `5 == 5.0` → true. Non-numeric pairs
    /// fall back to JSON structural equality (the pre-float behavior).
    /// Documented divergence from PA's strict JToken comparison -- paxr is
    /// a simulator, not a spec replica.
    pub(crate) fn equals(&self, other: &Value) -> bool {
        if let (Some(a), Some(b)) = (self.as_number(), other.as_number()) {
            return a == b;
        }
        self.to_json() == other.to_json()
    }
}

/// Display form for a float in debug output. Mirrors emitter::format_float:
/// always show at least one fractional digit so a reader can tell at a
/// glance that the value is a float, not an int.
fn format_float_display(x: f64) -> String {
    let s = format!("{x}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

#[derive(Debug, Clone)]
pub struct InterpretError {
    pub message: String,
    /// Source span the error should be attributed to, when available.
    /// Defensive-only errors (internal "should never happen" cases) start
    /// spanless; the `run_action` wrapper decorates any spanless error with
    /// the current action's span on its way out.
    pub span: Option<Span>,
}

impl fmt::Display for InterpretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for InterpretError {}

fn err(msg: impl Into<String>) -> InterpretError {
    InterpretError {
        message: msg.into(),
        span: None,
    }
}

/// paxr iteration cap for Until loops. Matches PA's default `limit.count`
/// of 60; guards against infinite loops when the exit condition never fires.
const UNTIL_ITERATION_CAP: u32 = 60;

fn err_at(msg: impl Into<String>, span: Span) -> InterpretError {
    InterpretError {
        message: msg.into(),
        span: Some(span),
    }
}

/// Runtime configuration for a paxr invocation. The boolean fields are
/// pairwise mutually exclusive; paxr's CLI enforces that before
/// constructing the struct.
#[derive(Debug, Clone, Copy, Default)]
pub struct Config {
    /// When true, emit an action-by-action trace to stdout as the interpreter
    /// walks the resolved program.
    pub verbose: bool,
    /// When true, suppress all stdout output -- including debug() calls,
    /// raw / unknown-call skip notices, and the end-of-run state dump. Used
    /// for scripted / CI runs that only care about exit code.
    pub quiet: bool,
    /// When true, emit ONLY debug() output. Suppresses raw / unknown skip
    /// notices and the end-of-run state dump so the user sees nothing but
    /// the breadcrumbs they placed.
    pub debug_only: bool,
}

pub fn interpret(src: &str, program: &ResolvedProgram) -> Result<FinalState, InterpretError> {
    interpret_with(src, program, Config::default())
}

pub fn interpret_with(
    src: &str,
    program: &ResolvedProgram,
    config: Config,
) -> Result<FinalState, InterpretError> {
    let mut interp = Interpreter::new(src, config);
    interp.run_actions(&program.actions, true)?;
    Ok(FinalState {
        bindings: interp.bindings,
        vars: interp.vars,
        compose_outputs: interp.compose_outputs,
    })
}

/// Snapshot of the interpreter at end-of-run. `bindings` preserves source
/// declaration order so paxr can print the state dump top-to-bottom.
#[derive(Debug, Clone)]
pub struct FinalState {
    pub bindings: Vec<Binding>,
    pub vars: HashMap<String, Value>,
    pub compose_outputs: HashMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Binding {
    /// User-facing name used in the state dump.
    pub name: String,
    pub kind: BindingKind,
    /// Where to look the current value up in `FinalState`.
    pub lookup: BindingLookup,
}

#[derive(Debug, Clone)]
pub enum BindingKind {
    Var(Type),
    Let,
}

#[derive(Debug, Clone)]
pub enum BindingLookup {
    /// Look up in `FinalState.vars` by var name.
    Var(String),
    /// Look up in `FinalState.compose_outputs` by action name.
    Compose(String),
}

struct Interpreter<'src> {
    src: &'src str,
    config: Config,
    /// Current verbose-trace indent depth (in nesting levels, 2 spaces each).
    indent: usize,
    vars: HashMap<String, Value>,
    /// Keyed by Compose action name (e.g. `Compose_remaining`).
    compose_outputs: HashMap<String, Value>,
    /// Keyed by Apply_to_each action name -> current iterator element.
    iterators: HashMap<String, Value>,
    /// Top-level var + let bindings in source order. Bindings declared
    /// inside `if` or `foreach` bodies are scoped to those branches and
    /// do not land in the end-of-run state dump.
    bindings: Vec<Binding>,
    /// Set when a `terminate` statement fires. `run_actions` checks this at
    /// the top of each iteration and stops walking; the Foreach loop also
    /// checks after each iteration so nested termination really halts the
    /// loop. The state dump still runs, reflecting what was set up to the
    /// point of termination.
    halted: bool,
}

impl<'src> Interpreter<'src> {
    fn new(src: &'src str, config: Config) -> Self {
        Self {
            src,
            config,
            indent: 0,
            vars: HashMap::new(),
            compose_outputs: HashMap::new(),
            iterators: HashMap::new(),
            bindings: Vec::new(),
            halted: false,
        }
    }

    /// Verbose-only trace line. Only fires under `--verbose`.
    fn trace(&self, msg: &str) {
        if self.config.verbose {
            self.write_line(msg);
        }
    }

    /// User-placed `debug()` breadcrumb. Prints under default, --verbose,
    /// and --debug; silenced only by --quiet.
    fn print_debug_line(&self, msg: &str) {
        if self.config.quiet {
            return;
        }
        self.write_line(msg);
    }

    /// Interpreter-generated notice (raw skip, unknown-call skip). Prints
    /// under default and --verbose; silenced by --quiet or --debug (the
    /// latter because --debug is meant to surface only the user's own
    /// breadcrumbs).
    fn print_notice(&self, msg: &str) {
        if self.config.quiet || self.config.debug_only {
            return;
        }
        self.write_line(msg);
    }

    /// Shared formatter: indent only under --verbose, flush-left otherwise.
    /// Indent is context-free in non-verbose modes, so leading whitespace
    /// there would be confusing.
    fn write_line(&self, msg: &str) {
        if self.config.verbose {
            println!("{}{}", "  ".repeat(self.indent), msg);
        } else {
            println!("{msg}");
        }
    }

    fn run_actions(
        &mut self,
        actions: &[ResolvedAction],
        top_level: bool,
    ) -> Result<(), InterpretError> {
        for action in actions {
            if self.halted {
                break;
            }
            self.run_action(action, top_level)?;
        }
        Ok(())
    }

    fn run_action(
        &mut self,
        action: &ResolvedAction,
        top_level: bool,
    ) -> Result<(), InterpretError> {
        // Attribute any spanless runtime error to this action's span.
        // Nested actions (inside if/foreach bodies) resolve first and the
        // innermost error-originating action wins -- outer frames see the
        // span already set and don't overwrite it.
        self.run_action_inner(action, top_level).map_err(|mut e| {
            if e.span.is_none() {
                e.span = Some(action.span);
            }
            e
        })
    }

    fn run_action_inner(
        &mut self,
        action: &ResolvedAction,
        top_level: bool,
    ) -> Result<(), InterpretError> {
        match &action.kind {
            ActionKind::InitializeVariable { var, ty, value } => {
                let v = self.eval(value)?;
                // Coerce numeric init values to match the declared type so
                // floatness survives later increments. `var x: float = 5`
                // stores Float(5.0), not Int(5) -- otherwise `x += 1` would
                // stay in int-land and silently lose the declared type.
                let v = match (ty, &v) {
                    (Type::Float, Value::Int(n)) => Value::Float(*n as f64),
                    _ => v,
                };
                self.trace(&format!("init {var} = {}", v.display_compact()));
                self.vars.insert(var.clone(), v);
                // Resolver already enforces var decls are top-level, but
                // guard anyway -- the interpreter doesn't need to know why.
                if top_level {
                    self.bindings.push(Binding {
                        name: var.clone(),
                        kind: BindingKind::Var(ty.clone()),
                        lookup: BindingLookup::Var(var.clone()),
                    });
                }
            }
            ActionKind::SetVariable { var, value } => {
                let v = self.eval(value)?;
                self.trace(&format!("set {var} = {}", v.display_compact()));
                self.vars.insert(var.clone(), v);
            }
            ActionKind::IncrementVariable { var, value } => {
                let delta = self.eval(value)?;
                let current = self
                    .vars
                    .get(var)
                    .cloned()
                    .ok_or_else(|| err(format!("unknown variable {var}")))?;
                let new = numeric_combine(&current, &delta, "+", |a, b| a + b, |a, b| a + b)
                    .ok_or_else(|| err(format!("increment on {var} requires numeric values")))?;
                self.trace(&format!("increment {var} = {}", new.display_compact()));
                self.vars.insert(var.clone(), new);
            }
            ActionKind::DecrementVariable { var, value } => {
                let delta = self.eval(value)?;
                let current = self
                    .vars
                    .get(var)
                    .cloned()
                    .ok_or_else(|| err(format!("unknown variable {var}")))?;
                let new = numeric_combine(&current, &delta, "-", |a, b| a - b, |a, b| a - b)
                    .ok_or_else(|| err(format!("decrement on {var} requires numeric values")))?;
                self.trace(&format!("decrement {var} = {}", new.display_compact()));
                self.vars.insert(var.clone(), new);
            }
            ActionKind::AppendToStringVariable { var, value } => {
                let suffix = self.eval(value)?.coerce_str();
                let current = match self.vars.get(var) {
                    Some(Value::Str(s)) => s.clone(),
                    _ => return Err(err(format!("variable {var} is not a string"))),
                };
                let new = Value::Str(current + &suffix);
                self.trace(&format!("append_string {var} = {}", new.display_compact()));
                self.vars.insert(var.clone(), new);
            }
            ActionKind::AppendToArrayVariable { var, value } => {
                let item = self.eval(value)?;
                let mut arr = match self.vars.get(var) {
                    Some(Value::Array(items)) => items.clone(),
                    _ => return Err(err(format!("variable {var} is not an array"))),
                };
                arr.push(item);
                let new = Value::Array(arr);
                self.trace(&format!("append_array {var} = {}", new.display_compact()));
                self.vars.insert(var.clone(), new);
            }
            ActionKind::Compose { name, value } => {
                let v = self.eval(value)?;
                self.trace(&format!("compose {name} = {}", v.display_compact()));
                self.compose_outputs.insert(action.name.clone(), v);
                if top_level {
                    self.bindings.push(Binding {
                        name: name.clone(),
                        kind: BindingKind::Let,
                        lookup: BindingLookup::Compose(action.name.clone()),
                    });
                }
            }
            ActionKind::Raw { .. } => {
                // Surface raw skips so the developer knows their state may
                // diverge from the compiled flow. --debug / --quiet silence.
                self.print_notice(&format!("<skipping raw \"{}\">", action.name));
            }
            ActionKind::Condition {
                condition,
                condition_span,
                true_branch,
                false_branch,
            } => {
                let taken = self.eval(condition)?.as_bool().unwrap_or(false);
                let source = source_slice(self.src, *condition_span);
                self.trace(&format!("condition? ({source}) = {taken}"));
                let branch = if taken { true_branch } else { false_branch };
                self.indent += 1;
                self.run_actions(branch, false)?;
                self.indent -= 1;
            }
            ActionKind::Foreach {
                collection,
                iter_name,
                body,
            } => {
                let items = match self.eval(collection)? {
                    Value::Array(items) => items,
                    _ => return Err(err("foreach requires an array")),
                };
                self.trace(&format!("foreach {} ({} items)", action.name, items.len()));
                for (idx, item) in items.into_iter().enumerate() {
                    self.iterators.insert(action.name.clone(), item.clone());
                    self.indent += 1;
                    self.trace(&format!(
                        "iter[{idx}] {iter_name} = {}",
                        item.display_compact()
                    ));
                    self.run_actions(body, false)?;
                    self.indent -= 1;
                    if self.halted {
                        break;
                    }
                }
                self.iterators.remove(&action.name);
            }
            ActionKind::Debug { args, span } => {
                self.emit_debug(args, *span)?;
            }
            ActionKind::Scope { body } => {
                self.trace(&format!("scope {}", action.name));
                self.indent += 1;
                self.run_actions(body, false)?;
                self.indent -= 1;
            }
            ActionKind::OnHandler { statuses, body, .. } => {
                // paxr walks the happy path: every scope's body succeeds.
                // Under that assumption, a handler whose status list includes
                // `succeeded` fires and its body runs in the local simulation.
                // Handlers without `succeeded` would only fire on a real PA
                // failure -- paxr has no way to force one, so it surfaces a
                // notice listing every status and skips.
                let label = statuses
                    .iter()
                    .map(|s| s.as_label())
                    .collect::<Vec<_>>()
                    .join("-or-");
                if statuses.contains(&HandlerStatus::Succeeded) {
                    self.trace(&format!("on {} {}", label, action.name));
                    self.indent += 1;
                    self.run_actions(body, false)?;
                    self.indent -= 1;
                } else {
                    self.print_notice(&format!(
                        "<skipping on-{} handler \"{}\" (paxr cannot simulate non-success)>",
                        label, action.name
                    ));
                }
            }
            ActionKind::Until {
                condition,
                condition_span,
                limit_count,
                body,
                ..
            } => {
                let source = source_slice(self.src, *condition_span);
                let cap = limit_count.unwrap_or(UNTIL_ITERATION_CAP);
                self.trace(&format!("until {} (exit: {source})", action.name));
                let mut iters = 0u32;
                loop {
                    if self.halted {
                        break;
                    }
                    self.indent += 1;
                    self.trace(&format!("iter[{iters}]"));
                    self.run_actions(body, false)?;
                    self.indent -= 1;
                    if self.halted {
                        break;
                    }
                    let exit = self.eval(condition)?.as_bool().unwrap_or(false);
                    self.trace(&format!("until? ({source}) = {exit}"));
                    if exit {
                        break;
                    }
                    iters += 1;
                    if iters >= cap {
                        // Match PA semantics: at the limit, exit the loop.
                        // Surface a notice so the user sees why execution
                        // stopped short of the exit condition.
                        self.print_notice(&format!(
                            "<until \"{}\" hit iteration cap of {}>",
                            action.name, cap
                        ));
                        break;
                    }
                }
            }
            ActionKind::Switch {
                subject,
                subject_span,
                cases,
                default,
            } => {
                let subject_val = self.eval(subject)?;
                let source = source_slice(self.src, *subject_span);
                let matched = cases.iter().find(|c| {
                    let case_val = literal_to_value(&c.value);
                    subject_val.equals(&case_val)
                });
                let (body, label): (&[ResolvedAction], String) = match matched {
                    Some(c) => (
                        c.body.as_slice(),
                        format!("case {}", literal_to_value(&c.value).display_compact()),
                    ),
                    None => match default {
                        Some(body) => (body.as_slice(), "default".to_string()),
                        None => {
                            self.trace(&format!(
                                "switch ({source} = {}) -> no match",
                                subject_val.display_compact()
                            ));
                            return Ok(());
                        }
                    },
                };
                self.trace(&format!(
                    "switch ({source} = {}) -> {label}",
                    subject_val.display_compact()
                ));
                self.indent += 1;
                self.run_actions(body, false)?;
                self.indent -= 1;
            }
            ActionKind::Terminate { status, message } => {
                let rendered = match message {
                    Some(m) => Some(self.eval(m)?.coerce_str()),
                    None => None,
                };
                let line = match &rendered {
                    Some(msg) => format!("terminate: {}: {msg}", status.as_label()),
                    None => format!("terminate: {}", status.as_label()),
                };
                // Same visibility rules as debug() breadcrumbs: prints under
                // default, --verbose, and --debug; silenced by --quiet.
                self.print_debug_line(&line);
                self.halted = true;
            }
        }
        Ok(())
    }

    fn emit_debug(&mut self, args: &[DebugArg], stmt_span: Span) -> Result<(), InterpretError> {
        let line = span_to_line(self.src, stmt_span.start);
        if args.is_empty() {
            self.print_debug_line(&format!("debug: at line {line}"));
            return Ok(());
        }
        let mut parts: Vec<String> = Vec::with_capacity(args.len());
        for arg in args {
            let label = source_slice(self.src, arg.span);
            let value = self.eval(&arg.expr)?;
            parts.push(format!("{label}={}", value.display_compact()));
        }
        self.print_debug_line(&format!("debug: {} at line {line}", parts.join(", ")));
        Ok(())
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, InterpretError> {
        match expr {
            Expr::Literal(lit) => Ok(literal_to_value(lit)),
            Expr::Ref { name, span } => Err(err_at(
                format!("internal error: unresolved ref {name} reached interpreter"),
                *span,
            )),
            Expr::VarRef { name, span } => self
                .vars
                .get(name)
                .cloned()
                .ok_or_else(|| err_at(format!("undefined variable {name}"), *span)),
            Expr::ComposeRef { action_name, span } => self
                .compose_outputs
                .get(action_name)
                .cloned()
                .ok_or_else(|| {
                    err_at(
                        format!("compose output {action_name} not yet computed"),
                        *span,
                    )
                }),
            Expr::IteratorRef { action_name, span } => {
                self.iterators.get(action_name).cloned().ok_or_else(|| {
                    err_at(
                        format!("iterator {action_name} has no current value"),
                        *span,
                    )
                })
            }
            Expr::Member { target, field } => {
                let target_val = self.eval(target)?;
                match target_val {
                    Value::Object(entries) => entries
                        .into_iter()
                        .find(|(k, _)| k == field)
                        .map(|(_, v)| v)
                        .ok_or_else(|| err(format!("object has no field '{field}'"))),
                    _ => Err(err(format!("cannot access field '{field}' on non-object"))),
                }
            }
            Expr::BinaryOp { op, lhs, rhs } => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                eval_binop(*op, l, r)
            }
            Expr::UnaryOp { op, operand } => {
                let v = self.eval(operand)?;
                eval_unop(*op, v)
            }
            Expr::Call { name, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                let (value, unknown) = eval_call(name, vals);
                if unknown {
                    self.print_notice(&format!("<skipping unknown \"{name}\">"));
                }
                Ok(value)
            }
        }
    }
}

/// Renders `FinalState.bindings` in source declaration order, one line per
/// scalar binding and multi-line pretty JSON for non-empty composites.
/// Empty arrays / objects are rendered inline as `[]` / `{}`.
pub fn format_state_dump(state: &FinalState) -> String {
    let mut out = String::new();
    for binding in &state.bindings {
        let value = match &binding.lookup {
            BindingLookup::Var(name) => state.vars.get(name),
            BindingLookup::Compose(key) => state.compose_outputs.get(key),
        };
        let Some(value) = value else {
            continue;
        };
        let kind_s = match &binding.kind {
            BindingKind::Var(ty) => format!("var {}", type_name(ty)),
            BindingKind::Let => "let".to_string(),
        };
        let rendered = render_for_dump(value);
        out.push_str(&format!("{} ({}) = {}\n", binding.name, kind_s, rendered));
    }
    out
}

fn type_name(ty: &Type) -> &'static str {
    match ty {
        Type::Int => "int",
        Type::Float => "float",
        Type::String => "string",
        Type::Bool => "bool",
        Type::Array => "array",
        Type::Object => "object",
    }
}

fn render_for_dump(v: &Value) -> String {
    match v {
        Value::Array(items) if items.is_empty() => "[]".to_string(),
        Value::Object(entries) if entries.is_empty() => "{}".to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(&v.to_json()).unwrap_or_default()
        }
        _ => v.display_compact(),
    }
}

fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(x) => Value::Float(*x),
        Literal::String(s) => Value::Str(s.clone()),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Array(items) => Value::Array(items.iter().map(literal_to_value).collect()),
        Literal::Object(entries) => Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), literal_to_value(v)))
                .collect(),
        ),
    }
}

fn eval_binop(op: BinOp, l: Value, r: Value) -> Result<Value, InterpretError> {
    match op {
        BinOp::Add => numeric_combine(&l, &r, "+", |a, b| a + b, |a, b| a + b)
            .ok_or_else(|| err("+ requires numeric operands")),
        BinOp::Sub => numeric_combine(&l, &r, "-", |a, b| a - b, |a, b| a - b)
            .ok_or_else(|| err("- requires numeric operands")),
        BinOp::Mul => numeric_combine(&l, &r, "*", |a, b| a * b, |a, b| a * b)
            .ok_or_else(|| err("* requires numeric operands")),
        BinOp::Div => numeric_div(&l, &r),
        BinOp::Concat => Ok(Value::Str(l.coerce_str() + &r.coerce_str())),
        BinOp::Less => numeric_cmp(&l, &r, "<", |a, b| a < b, |a, b| a < b),
        BinOp::LessEq => numeric_cmp(&l, &r, "<=", |a, b| a <= b, |a, b| a <= b),
        BinOp::Greater => numeric_cmp(&l, &r, ">", |a, b| a > b, |a, b| a > b),
        BinOp::GreaterEq => numeric_cmp(&l, &r, ">=", |a, b| a >= b, |a, b| a >= b),
        BinOp::Equals => Ok(Value::Bool(l.equals(&r))),
        BinOp::NotEquals => Ok(Value::Bool(!l.equals(&r))),
        BinOp::And => bool_pair(l, r, "&&", |a, b| a && b),
        BinOp::Or => bool_pair(l, r, "||", |a, b| a || b),
    }
}

/// Numeric combinator: Int+Int stays Int, any Float involvement promotes to
/// Float. Returns None if either side isn't numeric.
fn numeric_combine<Fi, Ff>(l: &Value, r: &Value, _op: &str, fi: Fi, ff: Ff) -> Option<Value>
where
    Fi: FnOnce(i64, i64) -> i64,
    Ff: FnOnce(f64, f64) -> f64,
{
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Some(Value::Int(fi(*a, *b))),
        _ => {
            let a = l.as_number()?;
            let b = r.as_number()?;
            Some(Value::Float(ff(a, b)))
        }
    }
}

fn numeric_div(l: &Value, r: &Value) -> Result<Value, InterpretError> {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => {
            if *b == 0 {
                Err(err("division by zero"))
            } else {
                Ok(Value::Int(a / b))
            }
        }
        _ => {
            let a = l
                .as_number()
                .ok_or_else(|| err("left side of / is not numeric"))?;
            let b = r
                .as_number()
                .ok_or_else(|| err("right side of / is not numeric"))?;
            if b == 0.0 {
                Err(err("division by zero"))
            } else {
                Ok(Value::Float(a / b))
            }
        }
    }
}

fn numeric_cmp<Fi, Ff>(
    l: &Value,
    r: &Value,
    op: &str,
    fi: Fi,
    ff: Ff,
) -> Result<Value, InterpretError>
where
    Fi: FnOnce(i64, i64) -> bool,
    Ff: FnOnce(f64, f64) -> bool,
{
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Bool(fi(*a, *b))),
        _ => {
            let a = l
                .as_number()
                .ok_or_else(|| err(format!("left side of {op} is not numeric")))?;
            let b = r
                .as_number()
                .ok_or_else(|| err(format!("right side of {op} is not numeric")))?;
            Ok(Value::Bool(ff(a, b)))
        }
    }
}

fn bool_pair<F: FnOnce(bool, bool) -> bool>(
    l: Value,
    r: Value,
    op: &str,
    f: F,
) -> Result<Value, InterpretError> {
    let a = l
        .as_bool()
        .ok_or_else(|| err(format!("left side of {op} is not a bool")))?;
    let b = r
        .as_bool()
        .ok_or_else(|| err(format!("right side of {op} is not a bool")))?;
    Ok(Value::Bool(f(a, b)))
}

fn eval_unop(op: UnaryOp, v: Value) -> Result<Value, InterpretError> {
    match op {
        UnaryOp::Not => {
            let b = v.as_bool().ok_or_else(|| err("! requires bool"))?;
            Ok(Value::Bool(!b))
        }
        UnaryOp::Neg => {
            let n = v.as_int().ok_or_else(|| err("unary - requires int"))?;
            Ok(Value::Int(-n))
        }
    }
}

/// Dispatches a PA function call through the central registry in
/// `pa::functions`. Returns the value and an `unknown` flag the caller
/// uses to decide whether to print a `<skipping unknown "name">` notice
/// at the current indent.
///
/// The flag is true in three cases: the name is not in the registry,
/// the arity is not satisfied, or the registry entry exists but has no
/// paxr evaluator (e.g. `utcNow` / `formatDateTime` -- recognized by
/// paxc, not simulated locally). All three look the same to the caller,
/// matching the pre-registry behavior where any failed match arm fell
/// through to the same unknown branch.
fn eval_call(name: &str, args: Vec<Value>) -> (Value, bool) {
    let Some(def) = crate::pa::functions::lookup(name) else {
        return (Value::Null, true);
    };
    if !def.arity.check(args.len()) {
        return (Value::Null, true);
    }
    match def.paxr_eval {
        Some(eval_fn) => (eval_fn(&args), false),
        None => (Value::Null, true),
    }
}

/// PA function names paxr evaluates locally, derived from the central
/// registry in `pa::functions`. Stable handle for tests that pin
/// function-library coverage; the shape returned is allocation-cheap
/// (a Vec built on demand from a 45-entry static slice).
pub fn evaluated_function_names() -> Vec<&'static str> {
    crate::pa::functions::FUNCTIONS
        .iter()
        .filter(|f| f.paxr_eval.is_some())
        .map(|f| f.name)
        .collect()
}

fn span_to_line(src: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(src.len());
    1 + src[..clamped].bytes().filter(|b| *b == b'\n').count()
}

fn source_slice(src: &str, span: Span) -> String {
    let start = span.start.min(src.len());
    let end = span.end.min(src.len()).max(start);
    src[start..end].trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser, resolver};
    use chumsky::prelude::*;

    fn run(src: &str) -> Result<FinalState, InterpretError> {
        let full = format!("trigger manual\n{src}");
        let tokens = lexer::lexer().parse(full.as_str()).into_result().unwrap();
        let program = parser::parser()
            .parse(
                tokens
                    .as_slice()
                    .map((full.len()..full.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .unwrap();
        let resolved = resolver::resolve(&program).unwrap();
        interpret(&full, &resolved)
    }

    #[test]
    fn executes_arithmetic_and_comparisons() {
        run("var x: int = 3\nx += 4\nlet ok = x == 7\n").unwrap();
    }

    #[test]
    fn division_by_zero_errors() {
        let e = run("var x: int = 1\nlet y = x / 0").unwrap_err();
        assert!(e.message.contains("division by zero"));
        // Span-decoration invariant: spanless runtime errors inherit the
        // current action's span on their way out through `run_action`, so
        // the error always points at some source location for diagnostics.
        assert!(
            e.span.is_some(),
            "runtime error should carry a span for diagnostic attribution"
        );
    }

    #[test]
    fn foreach_type_mismatch_carries_span() {
        let e = run("var n: int = 5\nforeach x in n { }").unwrap_err();
        assert!(e.message.contains("foreach requires an array"));
        assert!(e.span.is_some());
    }

    #[test]
    fn foreach_iteration_works() {
        run("var total: int = 0\nvar items: array = [1, 2, 3]\nforeach item in items { total += 1 }")
            .unwrap();
    }

    #[test]
    fn unknown_call_returns_null_without_aborting() {
        // length() is not in the compiler-synthesized set.
        run(r#"let x = length("hi")"#).unwrap();
    }

    #[test]
    fn slice22_state_dump_preserves_source_order() {
        let state = run(
            "var total: int = 0\nvar label: string = \"start\"\ntotal = 5\nlet doubled = total + total",
        )
        .unwrap();
        let dump = format_state_dump(&state);
        // One line per binding, in declaration order.
        let lines: Vec<&str> = dump.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "total (var int) = 5");
        assert_eq!(lines[1], "label (var string) = \"start\"");
        assert_eq!(lines[2], "doubled (let) = 10");
    }

    #[test]
    fn slice22_state_dump_excludes_nested_lets() {
        // `let inner = ...` inside the if-branch must not appear in the
        // top-level state dump.
        let state = run("var x: int = 1\nif x == 1 {\n  let inner = 99\n}\nlet outer = x").unwrap();
        let dump = format_state_dump(&state);
        assert!(
            !dump.contains("inner"),
            "nested let leaked into dump:\n{dump}"
        );
        assert!(
            dump.contains("outer (let) = 1"),
            "missing outer binding:\n{dump}"
        );
    }

    #[test]
    fn slice22_state_dump_renders_composites_as_pretty_json() {
        let state = run("var empty_arr: array = []\nvar obj: object = { \"a\": 1 }").unwrap();
        let dump = format_state_dump(&state);
        // Empty composites inline.
        assert!(dump.contains("empty_arr (var array) = []"));
        // Non-empty composites pretty-printed on multiple lines.
        assert!(dump.contains("obj (var object) = {\n  \"a\": 1\n}"));
    }

    #[test]
    fn terminate_halts_subsequent_statements() {
        // After `terminate succeeded`, the second assignment to `x` must not
        // execute. The state dump still runs so `x` reflects the value at
        // the point of termination.
        let state = run("var x: int = 1\nterminate succeeded\nx = 99").unwrap();
        assert_eq!(state.vars.get("x").and_then(Value::as_int), Some(1));
    }

    #[test]
    fn terminate_inside_foreach_breaks_loop() {
        // `terminate` in the foreach body must stop the loop, not just the
        // current iteration. `processed` reaches 3 (the iteration that tripped
        // the guard) but no further.
        let state = run(
            "var processed: int = 0\nvar items: array = [1, 2, 3, 4, 5]\nforeach item in items { processed += 1\n  if processed == 3 { terminate failed \"stop\" } }",
        )
        .unwrap();
        assert_eq!(state.vars.get("processed").and_then(Value::as_int), Some(3));
    }

    #[test]
    fn terminate_inside_if_branch_halts_top_level() {
        // Halting inside a nested `if` body propagates up: statements after
        // the enclosing `if` do not execute.
        let state = run("var x: int = 0\nif x == 0 { terminate succeeded }\nx = 42").unwrap();
        assert_eq!(state.vars.get("x").and_then(Value::as_int), Some(0));
    }

    #[test]
    fn slice31_float_init_and_increment() {
        let state = run("var rate: float = 1.5\nrate += 0.25").unwrap();
        match state.vars.get("rate") {
            Some(Value::Float(x)) => assert!((x - 1.75).abs() < 1e-12),
            other => panic!("expected Value::Float, got {other:?}"),
        }
    }

    #[test]
    fn slice31_float_var_coerces_int_literal_at_init() {
        // `var x: float = 5` stores Float(5.0), not Int(5), so later
        // increments stay in float-land.
        let state = run("var x: float = 5\nx += 0.5").unwrap();
        match state.vars.get("x") {
            Some(Value::Float(v)) => assert!((v - 5.5).abs() < 1e-12),
            other => panic!("expected Value::Float, got {other:?}"),
        }
    }

    #[test]
    fn slice31_mixed_int_float_promotes() {
        // Int * Float -> Float(12.5)
        let v = eval_let("var qty: int = 10\nlet total = qty * 1.25", "total");
        match v {
            Value::Float(x) => assert!((x - 12.5).abs() < 1e-12),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn slice31_int_int_division_stays_int() {
        // Consistent with PA: int/int is integer division.
        let v = eval_let("let q = 7 / 2", "q");
        assert!(matches!(v, Value::Int(3)));
        // Any float side promotes.
        let v = eval_let("let q = 7.0 / 2", "q");
        match v {
            Value::Float(x) => assert!((x - 3.5).abs() < 1e-12),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn slice31_equals_is_numeric_across_int_and_float() {
        // 5 == 5.0 -> true (paxr's documented happy-path divergence from PA).
        assert!(matches!(
            eval_let("let eq = 5 == 5.0", "eq"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let("let eq = 5.0 == 5", "eq"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let("let eq = 5.5 == 5", "eq"),
            Value::Bool(false)
        ));
    }

    #[test]
    fn slice31_float_division_by_zero_errors() {
        let e = run("let q = 1.0 / 0.0").unwrap_err();
        assert!(e.message.contains("division by zero"));
    }

    #[test]
    fn terminate_message_evaluates_expression() {
        // The message is a full expression, not just a literal. Reference a
        // variable to confirm eval happens and halts cleanly.
        let state = run(
            "var reason: string = \"queue empty\"\nterminate failed reason\nvar after: int = 99",
        )
        .unwrap();
        // `after` should not have been declared because terminate halted first.
        assert!(!state.vars.contains_key("after"));
    }

    /// Convenience helper: run a pax program and return the value of a
    /// specific `let` binding as a Value. Compact way to assert on a function
    /// call result without repeating the parser/resolver/interpreter dance.
    fn eval_let(src: &str, binding_name: &str) -> Value {
        let state = run(src).expect("program errored");
        let key = state
            .bindings
            .iter()
            .find(|b| b.name == binding_name)
            .and_then(|b| match &b.lookup {
                BindingLookup::Compose(k) => Some(k.clone()),
                _ => None,
            })
            .expect("binding not found");
        state
            .compose_outputs
            .get(&key)
            .cloned()
            .expect("compose output missing")
    }

    #[test]
    fn fn_string_case_and_trim() {
        assert!(
            matches!(eval_let(r#"let x = toUpper("hello")"#, "x"), Value::Str(s) if s == "HELLO")
        );
        assert!(
            matches!(eval_let(r#"let x = toLower("HeLLo")"#, "x"), Value::Str(s) if s == "hello")
        );
        assert!(matches!(eval_let(r#"let x = trim("  hi  ")"#, "x"), Value::Str(s) if s == "hi"));
    }

    #[test]
    fn fn_length_polymorphic() {
        assert!(matches!(
            eval_let(r#"let x = length("hello")"#, "x"),
            Value::Int(5)
        ));
        assert!(matches!(
            eval_let("var a: array = [1, 2, 3]\nlet x = length(a)", "x"),
            Value::Int(3)
        ));
        assert!(matches!(
            eval_let(
                "var o: object = { \"a\": 1, \"b\": 2 }\nlet x = length(o)",
                "x"
            ),
            Value::Int(2)
        ));
    }

    #[test]
    fn fn_empty_polymorphic() {
        assert!(matches!(
            eval_let(r#"let x = empty("")"#, "x"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let(r#"let x = empty("hi")"#, "x"),
            Value::Bool(false)
        ));
        assert!(matches!(
            eval_let("var a: array = []\nlet x = empty(a)", "x"),
            Value::Bool(true)
        ));
        // Object case: polymorphism extends to objects too.
        assert!(matches!(
            eval_let("var o: object = {}\nlet x = empty(o)", "x"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let("var o: object = { \"a\": 1 }\nlet x = empty(o)", "x"),
            Value::Bool(false)
        ));
    }

    #[test]
    fn fn_contains_string_case_sensitive() {
        // PA's string contains is case-sensitive.
        assert!(matches!(
            eval_let(r#"let x = contains("hello world", "WORLD")"#, "x"),
            Value::Bool(false)
        ));
        assert!(matches!(
            eval_let(r#"let x = contains("hello world", "world")"#, "x"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn fn_contains_array_membership() {
        assert!(matches!(
            eval_let(
                "var a: array = [\"x\", \"y\", \"z\"]\nlet r = contains(a, \"y\")",
                "r"
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let(
                "var a: array = [\"x\", \"y\", \"z\"]\nlet r = contains(a, \"q\")",
                "r"
            ),
            Value::Bool(false)
        ));
    }

    #[test]
    fn fn_starts_and_ends_with_case_insensitive() {
        // PA quirk: these are case-insensitive.
        assert!(matches!(
            eval_let(r#"let x = startsWith("Hello", "HE")"#, "x"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let(r#"let x = endsWith("WORLD", "rld")"#, "x"),
            Value::Bool(true)
        ));
        // Symmetric direction: uppercase haystack and lowercase needle also match.
        assert!(matches!(
            eval_let(r#"let x = startsWith("HELLO", "hello")"#, "x"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let(r#"let x = endsWith("hello", "LO")"#, "x"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn fn_index_of_case_insensitive_char_index() {
        assert!(matches!(
            eval_let(r#"let x = indexOf("Hello World", "WORLD")"#, "x"),
            Value::Int(6)
        ));
        assert!(matches!(
            eval_let(r#"let x = indexOf("abc", "z")"#, "x"),
            Value::Int(-1)
        ));
        assert!(matches!(
            eval_let(r#"let x = lastIndexOf("a-b-c", "-")"#, "x"),
            Value::Int(3)
        ));
        // lastIndexOf with a mixed-case haystack confirms the case-insensitive
        // behavior (the "-" delimiter case above is case-neutral on its own).
        assert!(matches!(
            eval_let(r#"let x = lastIndexOf("Banana", "ANA")"#, "x"),
            Value::Int(3)
        ));
    }

    #[test]
    fn fn_substring_two_and_three_arg() {
        assert!(
            matches!(eval_let(r#"let x = substring("hello world", 6)"#, "x"), Value::Str(s) if s == "world")
        );
        assert!(
            matches!(eval_let(r#"let x = substring("hello world", 0, 5)"#, "x"), Value::Str(s) if s == "hello")
        );
        // Out-of-range start returns empty, not error.
        assert!(
            matches!(eval_let(r#"let x = substring("hi", 10)"#, "x"), Value::Str(s) if s.is_empty())
        );
    }

    #[test]
    fn fn_replace_and_split() {
        assert!(
            matches!(eval_let(r#"let x = replace("a-b-c", "-", "_")"#, "x"), Value::Str(s) if s == "a_b_c")
        );
        let v = eval_let(r#"let x = split("a,b,c", ",")"#, "x");
        match v {
            Value::Array(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(&items[0], Value::Str(s) if s == "a"));
                assert!(matches!(&items[2], Value::Str(s) if s == "c"));
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn fn_join() {
        assert!(matches!(
            eval_let("var a: array = [\"a\", \"b\", \"c\"]\nlet x = join(a, \", \")", "x"),
            Value::Str(s) if s == "a, b, c"
        ));
    }

    #[test]
    fn fn_first_last_skip_take() {
        assert!(matches!(
            eval_let("var a: array = [10, 20, 30]\nlet x = first(a)", "x"),
            Value::Int(10)
        ));
        assert!(matches!(
            eval_let("var a: array = [10, 20, 30]\nlet x = last(a)", "x"),
            Value::Int(30)
        ));
        // first on an empty array is null, not an error.
        assert!(matches!(
            eval_let("var a: array = []\nlet x = first(a)", "x"),
            Value::Null
        ));
        // skip/take produce sliced arrays.
        match eval_let("var a: array = [1, 2, 3, 4, 5]\nlet x = skip(a, 2)", "x") {
            Value::Array(items) => assert_eq!(items.len(), 3),
            _ => panic!("expected array"),
        }
        match eval_let("var a: array = [1, 2, 3, 4, 5]\nlet x = take(a, 2)", "x") {
            Value::Array(items) => assert_eq!(items.len(), 2),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn fn_mod_min_max_range() {
        assert!(matches!(eval_let("let x = mod(10, 3)", "x"), Value::Int(1)));
        assert!(matches!(
            eval_let("let x = min(3, 1, 2)", "x"),
            Value::Int(1)
        ));
        assert!(matches!(
            eval_let("let x = max(3, 1, 2)", "x"),
            Value::Int(3)
        ));
        // min/max also accept a single array argument (PA behavior).
        assert!(matches!(
            eval_let("var a: array = [5, 3, 9, 1]\nlet x = min(a)", "x"),
            Value::Int(1)
        ));
        // range(start, count) produces a sequence.
        match eval_let("let x = range(10, 3)", "x") {
            Value::Array(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(&items[0], Value::Int(10)));
                assert!(matches!(&items[2], Value::Int(12)));
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn fn_unknown_still_skips_with_notice() {
        // Regression: not all PA functions are implemented. Unknown names
        // must continue to return Null and flag the `unknown` branch.
        run(r#"let x = formatDateTime(utcNow(), "yyyy-MM-dd")"#).unwrap();
    }

    #[test]
    fn fn_coalesce_returns_first_non_null() {
        assert!(
            matches!(eval_let(r#"let x = coalesce(null, null, "hi")"#, "x"), Value::Str(s) if s == "hi")
        );
        assert!(matches!(
            eval_let("let x = coalesce(null, 42, null)", "x"),
            Value::Int(42)
        ));
        // All-null → null.
        assert!(matches!(
            eval_let("let x = coalesce(null, null)", "x"),
            Value::Null
        ));
    }

    #[test]
    fn fn_create_array_wraps_args() {
        match eval_let(r#"let x = createArray("a", "b", 3)"#, "x") {
            Value::Array(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(&items[0], Value::Str(s) if s == "a"));
                assert!(matches!(&items[2], Value::Int(3)));
            }
            _ => panic!("expected array"),
        }
        // Zero-arg form returns an empty array (variadic edge).
        match eval_let("let x = createArray()", "x") {
            Value::Array(items) => assert!(items.is_empty()),
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn fn_string_coerces_any_value() {
        assert!(matches!(eval_let("let x = string(42)", "x"), Value::Str(s) if s == "42"));
        assert!(matches!(eval_let("let x = string(true)", "x"), Value::Str(s) if s == "true"));
        // Idempotent on strings.
        assert!(matches!(eval_let(r#"let x = string("hi")"#, "x"), Value::Str(s) if s == "hi"));
    }

    #[test]
    fn fn_int_parses_or_passes_through() {
        assert!(matches!(
            eval_let(r#"let x = int("123")"#, "x"),
            Value::Int(123)
        ));
        // Whitespace trimmed.
        assert!(matches!(
            eval_let(r#"let x = int("  42  ")"#, "x"),
            Value::Int(42)
        ));
        // Unparseable → Null (not error).
        assert!(matches!(
            eval_let(r#"let x = int("nope")"#, "x"),
            Value::Null
        ));
        // Int passthrough.
        assert!(matches!(eval_let("let x = int(7)", "x"), Value::Int(7)));
    }

    #[test]
    fn fn_bool_parses_or_passes_through() {
        assert!(matches!(
            eval_let(r#"let x = bool("true")"#, "x"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let(r#"let x = bool("FALSE")"#, "x"),
            Value::Bool(false)
        ));
        assert!(matches!(
            eval_let(r#"let x = bool("1")"#, "x"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let("let x = bool(0)", "x"),
            Value::Bool(false)
        ));
        // Bogus → Null.
        assert!(matches!(
            eval_let(r#"let x = bool("maybe")"#, "x"),
            Value::Null
        ));
    }

    #[test]
    fn fn_guid_produces_rfc4122_string() {
        // Non-deterministic by design. Assert only the shape (36 chars with
        // hyphens in the right places) rather than a specific value.
        match eval_let("let x = guid()", "x") {
            Value::Str(s) => {
                assert_eq!(s.len(), 36, "guid string should be 36 chars, got {s:?}");
                let parts: Vec<&str> = s.split('-').collect();
                assert_eq!(parts.len(), 5);
                assert_eq!(parts[0].len(), 8);
                assert_eq!(parts[4].len(), 12);
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn slice30_on_succeeded_handler_runs_in_paxr_happy_path() {
        // paxr assumes every scope succeeds, so an `on succeeded` handler
        // fires locally and its body side-effects are visible.
        let state = run(r#"var trail: string = ""
scope work {
  trail &= "w"
}
on succeeded work {
  trail &= "-ok"
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("trail"),
            Some(Value::Str(s)) if s == "w-ok"
        ));
    }

    #[test]
    fn slice32_multi_status_handler_with_succeeded_runs_in_paxr() {
        // A multi-status handler whose list contains `succeeded` fires on
        // paxr's happy path, same as a plain `on succeeded` handler would.
        let state = run(r#"var trail: string = ""
scope work {
  trail &= "w"
}
on succeeded or failed work {
  trail &= "-ok"
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("trail"),
            Some(Value::Str(s)) if s == "w-ok"
        ));
    }

    #[test]
    fn slice32_multi_status_handler_without_succeeded_is_skipped() {
        // A multi-status handler whose list does NOT contain `succeeded`
        // only fires on real PA failures; paxr skips it like a single
        // `on failed` handler.
        let state = run(r#"var trail: string = ""
scope work {
  trail &= "w"
}
on failed or timedout work {
  trail &= "-boom"
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("trail"),
            Some(Value::Str(s)) if s == "w"
        ));
    }

    #[test]
    fn slice30_on_failed_handler_skipped_in_paxr() {
        // paxr can't simulate failure; `on failed` handlers are announced
        // and skipped so the state dump matches the happy path.
        let state = run(r#"var trail: string = ""
scope work {
  trail &= "w"
}
on failed work {
  trail &= "-fail"
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("trail"),
            Some(Value::Str(s)) if s == "w"
        ));
    }

    #[test]
    fn slice29_until_runs_body_at_least_once() {
        // Condition already true on entry -- body still runs once, then
        // the loop exits.
        let state = run(r#"var n: int = 10
until n > 5 {
  n += 1
}"#)
        .unwrap();
        assert!(matches!(state.vars.get("n"), Some(Value::Int(11))));
    }

    #[test]
    fn slice29_until_exits_when_condition_becomes_true() {
        let state = run(r#"var n: int = 0
until n >= 3 {
  n += 1
}"#)
        .unwrap();
        assert!(matches!(state.vars.get("n"), Some(Value::Int(3))));
    }

    #[test]
    fn slice29_until_iteration_cap_stops_runaway() {
        // Exit condition never becomes true; loop must stop at the cap and
        // not error. After 60 iterations, n has been incremented 60 times.
        let state = run(r#"var n: int = 0
until false {
  n += 1
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("n"),
            Some(Value::Int(n)) if *n == UNTIL_ITERATION_CAP as i64
        ));
    }

    #[test]
    fn slice34_until_max_caps_paxr_at_user_value() {
        // A user-set `max 3` caps paxr's local simulation at 3 iterations,
        // overriding the default of 60. n increments 3 times and the loop
        // surfaces the iteration-cap notice.
        let state = run(r#"var n: int = 0
until false max 3 {
  n += 1
}"#)
        .unwrap();
        assert!(
            matches!(state.vars.get("n"), Some(Value::Int(3))),
            "n = 3 after the user-set cap fires, not 60"
        );
    }

    #[test]
    fn slice34_until_timeout_does_not_affect_paxr_cap() {
        // paxr cannot simulate wall-clock time, so a user `timeout "..."`
        // is ignored locally. The default iteration cap still applies when
        // `max` isn't set -- here n goes up to 60.
        let state = run(r#"var n: int = 0
until false timeout "PT1M" {
  n += 1
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("n"),
            Some(Value::Int(n)) if *n == UNTIL_ITERATION_CAP as i64
        ));
    }

    #[test]
    fn slice29_terminate_in_until_body_halts_loop() {
        let state = run(r#"var n: int = 0
until n >= 100 {
  n += 1
  if n == 4 { terminate failed "stop" }
}"#)
        .unwrap();
        assert!(matches!(state.vars.get("n"), Some(Value::Int(4))));
    }

    #[test]
    fn slice28_scope_body_executes_in_order() {
        let state = run(r#"var n: int = 0
scope {
  n = 1
  n += 4
}"#)
        .unwrap();
        assert!(matches!(state.vars.get("n"), Some(Value::Int(5))));
    }

    #[test]
    fn slice28_nested_scope_works() {
        let state = run(r#"var n: int = 0
scope outer {
  scope inner {
    n = 42
  }
}"#)
        .unwrap();
        assert!(matches!(state.vars.get("n"), Some(Value::Int(42))));
    }

    #[test]
    fn slice28_scope_let_scopes_to_body() {
        // A `let` inside a scope should not appear in the end-of-run dump
        // alongside top-level bindings.
        let state = run(r#"var x: int = 1
scope {
  let inner = x + 10
}
let outer = x"#)
        .unwrap();
        let dump = format_state_dump(&state);
        assert!(!dump.contains("inner"), "scope let leaked:\n{dump}");
        assert!(dump.contains("outer"));
    }

    #[test]
    fn slice27_switch_runs_matching_case() {
        let state = run(r#"var status: string = "active"
var tag: string = ""
switch status {
  case "active" {
    tag = "ACTIVE"
  }
  case "pending" {
    tag = "PENDING"
  }
  default {
    tag = "OTHER"
  }
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("tag"),
            Some(Value::Str(s)) if s == "ACTIVE"
        ));
    }

    #[test]
    fn slice27_switch_falls_through_to_default() {
        let state = run(r#"var status: string = "archived"
var tag: string = ""
switch status {
  case "active" {
    tag = "A"
  }
  default {
    tag = "D"
  }
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("tag"),
            Some(Value::Str(s)) if s == "D"
        ));
    }

    #[test]
    fn slice27_switch_no_match_no_default_is_noop() {
        let state = run(r#"var n: int = 99
var tag: string = "untouched"
switch n {
  case 1 {
    tag = "changed"
  }
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("tag"),
            Some(Value::Str(s)) if s == "untouched"
        ));
    }

    #[test]
    fn slice27_switch_int_cases_match_by_value() {
        let state = run(r#"var code: int = 2
var tag: string = ""
switch code {
  case 1 { tag = "one" }
  case 2 { tag = "two" }
  case 3 { tag = "three" }
}"#)
        .unwrap();
        assert!(matches!(
            state.vars.get("tag"),
            Some(Value::Str(s)) if s == "two"
        ));
    }

    #[test]
    fn fn_uri_component_encode_and_decode() {
        // Spaces and slashes get percent-encoded; unreserved chars pass through.
        assert!(matches!(
            eval_let(r#"let x = uriComponent("hello world / pax")"#, "x"),
            Value::Str(s) if s == "hello%20world%20%2F%20pax"
        ));
        // Round-trip.
        assert!(matches!(
            eval_let(
                r#"let enc = uriComponent("a b&c=d")
        let dec = uriComponentToString(enc)"#,
                "dec"
            ),
            Value::Str(s) if s == "a b&c=d"
        ));
    }

    /// Defensive `err_at` paths in eval() shouldn't fire from normal source
    /// (the resolver catches undefined refs first), but if they ever do,
    /// the span on the resulting error should point at the originating
    /// identifier rather than being None. Drive eval directly with a
    /// hand-built ref to exercise each path.
    #[test]
    fn varref_lookup_miss_carries_originating_span() {
        let mut interp = Interpreter::new("", Config::default());
        let span: Span = (10..15).into();
        let err = interp
            .eval(&Expr::VarRef {
                name: "ghost".to_string(),
                span,
            })
            .unwrap_err();
        assert_eq!(err.span, Some(span));
        assert!(err.message.contains("ghost"));
    }

    #[test]
    fn composeref_lookup_miss_carries_originating_span() {
        let mut interp = Interpreter::new("", Config::default());
        let span: Span = (3..9).into();
        let err = interp
            .eval(&Expr::ComposeRef {
                action_name: "Compose_phantom".to_string(),
                span,
            })
            .unwrap_err();
        assert_eq!(err.span, Some(span));
    }

    #[test]
    fn iteratorref_lookup_miss_carries_originating_span() {
        let mut interp = Interpreter::new("", Config::default());
        let span: Span = (0..4).into();
        let err = interp
            .eval(&Expr::IteratorRef {
                action_name: "Apply_to_each_missing".to_string(),
                span,
            })
            .unwrap_err();
        assert_eq!(err.span, Some(span));
    }

    /// Behavior pinning for the paxr function library, pre-registry-refactor.
    /// These tests lock the exact set of evaluated function names and the
    /// quirks documented in REFERENCE.md. The next slice moves the dispatch
    /// from `eval_call`'s match arms into a `FunctionDef` table; these
    /// pinning tests must continue to pass unchanged across that refactor,
    /// proving behavior preservation.
    mod function_pinning {
        use super::*;

        /// Registry surface lock: the exact set of function names paxr
        /// evaluates today. After the registry refactor, this expected list
        /// stays the same and `evaluated_function_names()` derives from the
        /// FunctionDef table; equality must continue to hold.
        #[test]
        fn evaluated_function_names_returns_exact_expected_set() {
            let expected: &[&str] = &[
                "add",
                "sub",
                "mul",
                "div",
                "mod",
                "min",
                "max",
                "range",
                "coalesce",
                "equals",
                "less",
                "lessOrEquals",
                "greater",
                "greaterOrEquals",
                "not",
                "and",
                "or",
                "concat",
                "toUpper",
                "toLower",
                "trim",
                "substring",
                "indexOf",
                "lastIndexOf",
                "startsWith",
                "endsWith",
                "replace",
                "split",
                "join",
                "uriComponent",
                "uriComponentToString",
                "string",
                "int",
                "bool",
                "guid",
                "createArray",
                "length",
                "empty",
                "contains",
                "first",
                "last",
                "skip",
                "take",
            ];
            let actual = evaluated_function_names();
            let actual_set: std::collections::BTreeSet<&str> = actual.iter().copied().collect();
            let expected_set: std::collections::BTreeSet<&str> = expected.iter().copied().collect();
            assert_eq!(
                actual_set, expected_set,
                "evaluated_function_names() drifted from expected list"
            );
            assert_eq!(actual.len(), expected.len(), "duplicate names in registry");
        }

        /// Dispatch sweep: every name in the registry is actually recognized
        /// by `eval_call`. Catches the failure mode where a name is in the
        /// registry table but the dispatch is broken or missing (the unknown
        /// branch fires).
        #[test]
        fn every_evaluated_function_dispatches() {
            // Per-function minimum-passing arguments. Each entry must satisfy
            // the function's arity guard so the dispatch hits its match arm
            // rather than falling through to the unknown branch.
            let cases: &[(&str, Vec<Value>)] = &[
                ("add", vec![Value::Int(1), Value::Int(2)]),
                ("sub", vec![Value::Int(2), Value::Int(1)]),
                ("mul", vec![Value::Int(2), Value::Int(3)]),
                ("div", vec![Value::Int(6), Value::Int(2)]),
                ("mod", vec![Value::Int(7), Value::Int(3)]),
                ("min", vec![Value::Int(1)]),
                ("max", vec![Value::Int(1)]),
                ("range", vec![Value::Int(0), Value::Int(3)]),
                ("coalesce", vec![Value::Null, Value::Int(1)]),
                ("equals", vec![Value::Int(1), Value::Int(1)]),
                ("less", vec![Value::Int(1), Value::Int(2)]),
                ("lessOrEquals", vec![Value::Int(1), Value::Int(2)]),
                ("greater", vec![Value::Int(2), Value::Int(1)]),
                ("greaterOrEquals", vec![Value::Int(2), Value::Int(1)]),
                ("not", vec![Value::Bool(true)]),
                ("and", vec![Value::Bool(true), Value::Bool(true)]),
                ("or", vec![Value::Bool(false), Value::Bool(true)]),
                (
                    "concat",
                    vec![Value::Str("a".to_string()), Value::Str("b".to_string())],
                ),
                ("toUpper", vec![Value::Str("hi".to_string())]),
                ("toLower", vec![Value::Str("HI".to_string())]),
                ("trim", vec![Value::Str(" hi ".to_string())]),
                (
                    "substring",
                    vec![
                        Value::Str("hello".to_string()),
                        Value::Int(0),
                        Value::Int(2),
                    ],
                ),
                (
                    "indexOf",
                    vec![Value::Str("hello".to_string()), Value::Str("l".to_string())],
                ),
                (
                    "lastIndexOf",
                    vec![Value::Str("hello".to_string()), Value::Str("l".to_string())],
                ),
                (
                    "startsWith",
                    vec![
                        Value::Str("hello".to_string()),
                        Value::Str("he".to_string()),
                    ],
                ),
                (
                    "endsWith",
                    vec![
                        Value::Str("hello".to_string()),
                        Value::Str("lo".to_string()),
                    ],
                ),
                (
                    "replace",
                    vec![
                        Value::Str("a-b".to_string()),
                        Value::Str("-".to_string()),
                        Value::Str("_".to_string()),
                    ],
                ),
                (
                    "split",
                    vec![Value::Str("a,b".to_string()), Value::Str(",".to_string())],
                ),
                (
                    "join",
                    vec![
                        Value::Array(vec![Value::Str("a".to_string())]),
                        Value::Str(",".to_string()),
                    ],
                ),
                ("uriComponent", vec![Value::Str("a b".to_string())]),
                (
                    "uriComponentToString",
                    vec![Value::Str("a%20b".to_string())],
                ),
                ("string", vec![Value::Int(1)]),
                ("int", vec![Value::Str("1".to_string())]),
                ("bool", vec![Value::Str("true".to_string())]),
                ("guid", vec![]),
                ("createArray", vec![Value::Int(1)]),
                ("length", vec![Value::Str("hi".to_string())]),
                ("empty", vec![Value::Str("".to_string())]),
                (
                    "contains",
                    vec![
                        Value::Str("hello".to_string()),
                        Value::Str("ll".to_string()),
                    ],
                ),
                ("first", vec![Value::Array(vec![Value::Int(1)])]),
                ("last", vec![Value::Array(vec![Value::Int(1)])]),
                (
                    "skip",
                    vec![
                        Value::Array(vec![Value::Int(1), Value::Int(2)]),
                        Value::Int(1),
                    ],
                ),
                (
                    "take",
                    vec![
                        Value::Array(vec![Value::Int(1), Value::Int(2)]),
                        Value::Int(1),
                    ],
                ),
            ];
            // Cases must cover the full registry (and only the registry).
            let case_names: std::collections::BTreeSet<&str> =
                cases.iter().map(|(n, _)| *n).collect();
            let registry: std::collections::BTreeSet<&str> =
                evaluated_function_names().iter().copied().collect();
            assert_eq!(
                case_names, registry,
                "dispatch sweep cases drifted from registry"
            );

            for (name, args) in cases {
                let (_, unknown) = eval_call(name, args.clone());
                assert!(
                    !unknown,
                    "function `{name}` should dispatch but hit the unknown branch"
                );
            }
        }

        // --- Boundary pins for the implicit-only comparison operators. ---

        #[test]
        fn fn_less_or_equals_boundary() {
            assert!(matches!(
                eval_let("let x = lessOrEquals(3, 3)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = lessOrEquals(3, 4)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = lessOrEquals(4, 3)", "x"),
                Value::Bool(false)
            ));
        }

        #[test]
        fn fn_greater_or_equals_boundary() {
            assert!(matches!(
                eval_let("let x = greaterOrEquals(3, 3)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = greaterOrEquals(4, 3)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = greaterOrEquals(3, 4)", "x"),
                Value::Bool(false)
            ));
        }

        // --- Quirk pins not currently covered by named tests. ---

        /// String indexing is character-based, not byte-based. UTF-8
        /// multi-byte characters count as one each. Documented in
        /// REFERENCE.md; was previously untested.
        #[test]
        fn fn_string_indexing_is_character_based_utf8() {
            // "café" is 4 chars / 5 bytes (é is 2 bytes in UTF-8).
            assert!(matches!(
                eval_let(r#"let x = length("café")"#, "x"),
                Value::Int(4)
            ));
            assert!(matches!(
                eval_let(r#"let x = substring("café", 0, 4)"#, "x"),
                Value::Str(s) if s == "café"
            ));
            assert!(matches!(
                eval_let(r#"let x = indexOf("café", "é")"#, "x"),
                Value::Int(3)
            ));
        }

        /// Empty string is a valid value, distinct from null. `coalesce`
        /// only skips nulls, not empties.
        #[test]
        fn fn_coalesce_empty_string_is_not_null() {
            assert!(matches!(
                eval_let(r#"let x = coalesce("", "fallback")"#, "x"),
                Value::Str(s) if s.is_empty()
            ));
        }

        /// `bool` parses with whitespace trimming (matches `int`'s behavior).
        #[test]
        fn fn_bool_trims_whitespace() {
            assert!(matches!(
                eval_let(r#"let x = bool("  true  ")"#, "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let(r#"let x = bool("  FALSE  ")"#, "x"),
                Value::Bool(false)
            ));
        }

        /// `concat` is fully variadic: zero args → empty string, one arg →
        /// itself, many args → concatenation.
        #[test]
        fn fn_concat_variadic_edges() {
            assert!(matches!(eval_let("let x = concat()", "x"), Value::Str(s) if s.is_empty()));
            assert!(matches!(eval_let(r#"let x = concat("a")"#, "x"), Value::Str(s) if s == "a"));
            assert!(matches!(
                eval_let(r#"let x = concat("a", "b", "c")"#, "x"),
                Value::Str(s) if s == "abc"
            ));
        }

        /// `and` and `or` are variadic, matching PA's semantics. Zero args
        /// returns null (no identity baked in); one arg returns the arg
        /// as-is; two-or-more folds left.
        #[test]
        fn fn_and_or_variadic() {
            // Two-arg base case.
            assert!(matches!(
                eval_let("let x = and(true, true)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = and(true, false)", "x"),
                Value::Bool(false)
            ));
            assert!(matches!(
                eval_let("let x = or(false, true)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = or(false, false)", "x"),
                Value::Bool(false)
            ));
            // Three-arg fold.
            assert!(matches!(
                eval_let("let x = and(true, true, true)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = and(true, false, true)", "x"),
                Value::Bool(false)
            ));
            assert!(matches!(
                eval_let("let x = or(false, false, true)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = or(false, false, false)", "x"),
                Value::Bool(false)
            ));
            // One-arg form returns the argument as-is.
            assert!(matches!(
                eval_let("let x = and(true)", "x"),
                Value::Bool(true)
            ));
            assert!(matches!(
                eval_let("let x = or(false)", "x"),
                Value::Bool(false)
            ));
            // Zero-arg form returns null (PA has no identity element here).
            let (v, unknown) = eval_call("and", vec![]);
            assert!(matches!(v, Value::Null));
            assert!(!unknown, "and() should dispatch even with zero args");
            let (v, unknown) = eval_call("or", vec![]);
            assert!(matches!(v, Value::Null));
            assert!(!unknown);
        }

        /// `not` is strictly unary. Wrong arity returns Null (does not panic
        /// or error), matching paxr's permissive function-call model.
        #[test]
        fn fn_not_arity_guard() {
            assert!(matches!(
                eval_let("let x = not(true)", "x"),
                Value::Bool(false)
            ));
            assert!(matches!(
                eval_let("let x = not(false)", "x"),
                Value::Bool(true)
            ));
            // Two-arg form falls through the guard; eval_call returns Null.
            let (v, unknown) = eval_call("not", vec![Value::Bool(true), Value::Bool(false)]);
            assert!(matches!(v, Value::Null));
            assert!(unknown, "two-arg not() should fall to the unknown branch");
        }
    }
}
