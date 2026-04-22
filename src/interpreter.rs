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

use crate::ast::{BinOp, DebugArg, Expr, Literal, Type, UnaryOp};
use crate::lexer::Span;
use crate::resolver::{ActionKind, ResolvedAction, ResolvedProgram};
use serde_json::{Map, Value as JsonValue, json};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Int(i64),
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

    fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn coerce_str(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => String::new(),
            other => serde_json::to_string(&other.to_json()).unwrap_or_default(),
        }
    }

    fn equals(&self, other: &Value) -> bool {
        self.to_json() == other.to_json()
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
                let delta = self.eval(value)?.as_int().ok_or_else(|| {
                    err(format!("increment on {var} requires int value"))
                })?;
                let current = self
                    .vars
                    .get(var)
                    .and_then(Value::as_int)
                    .ok_or_else(|| err(format!("variable {var} is not an int")))?;
                let new = current + delta;
                self.trace(&format!("increment {var} = {new}"));
                self.vars.insert(var.clone(), Value::Int(new));
            }
            ActionKind::DecrementVariable { var, value } => {
                let delta = self.eval(value)?.as_int().ok_or_else(|| {
                    err(format!("decrement on {var} requires int value"))
                })?;
                let current = self
                    .vars
                    .get(var)
                    .and_then(Value::as_int)
                    .ok_or_else(|| err(format!("variable {var} is not an int")))?;
                let new = current - delta;
                self.trace(&format!("decrement {var} = {new}"));
                self.vars.insert(var.clone(), Value::Int(new));
            }
            ActionKind::AppendToStringVariable { var, value } => {
                let suffix = self.eval(value)?.coerce_str();
                let current = match self.vars.get(var) {
                    Some(Value::Str(s)) => s.clone(),
                    _ => return Err(err(format!("variable {var} is not a string"))),
                };
                let new = Value::Str(current + &suffix);
                self.trace(&format!(
                    "append_string {var} = {}",
                    new.display_compact()
                ));
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
                self.trace(&format!(
                    "append_array {var} = {}",
                    new.display_compact()
                ));
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
                self.trace(&format!(
                    "foreach {} ({} items)",
                    action.name,
                    items.len()
                ));
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
            Expr::VarRef(name) => self
                .vars
                .get(name)
                .cloned()
                .ok_or_else(|| err(format!("undefined variable {name}"))),
            Expr::ComposeRef(action_name) => self
                .compose_outputs
                .get(action_name)
                .cloned()
                .ok_or_else(|| err(format!("compose output {action_name} not yet computed"))),
            Expr::IteratorRef(action_name) => self
                .iterators
                .get(action_name)
                .cloned()
                .ok_or_else(|| err(format!("iterator {action_name} has no current value"))),
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
        BinOp::Add => int_pair(l, r, "+", |a, b| a + b),
        BinOp::Sub => int_pair(l, r, "-", |a, b| a - b),
        BinOp::Mul => int_pair(l, r, "*", |a, b| a * b),
        BinOp::Div => {
            let (a, b) = int_extract(l, r, "/")?;
            if b == 0 {
                return Err(err("division by zero"));
            }
            Ok(Value::Int(a / b))
        }
        BinOp::Concat => Ok(Value::Str(l.coerce_str() + &r.coerce_str())),
        BinOp::Less => int_cmp(l, r, "<", |a, b| a < b),
        BinOp::LessEq => int_cmp(l, r, "<=", |a, b| a <= b),
        BinOp::Greater => int_cmp(l, r, ">", |a, b| a > b),
        BinOp::GreaterEq => int_cmp(l, r, ">=", |a, b| a >= b),
        BinOp::Equals => Ok(Value::Bool(l.equals(&r))),
        BinOp::NotEquals => Ok(Value::Bool(!l.equals(&r))),
        BinOp::And => bool_pair(l, r, "&&", |a, b| a && b),
        BinOp::Or => bool_pair(l, r, "||", |a, b| a || b),
    }
}

fn int_pair<F: FnOnce(i64, i64) -> i64>(
    l: Value,
    r: Value,
    op: &str,
    f: F,
) -> Result<Value, InterpretError> {
    let (a, b) = int_extract(l, r, op)?;
    Ok(Value::Int(f(a, b)))
}

fn int_cmp<F: FnOnce(i64, i64) -> bool>(
    l: Value,
    r: Value,
    op: &str,
    f: F,
) -> Result<Value, InterpretError> {
    let (a, b) = int_extract(l, r, op)?;
    Ok(Value::Bool(f(a, b)))
}

fn int_extract(l: Value, r: Value, op: &str) -> Result<(i64, i64), InterpretError> {
    let a = l
        .as_int()
        .ok_or_else(|| err(format!("left side of {op} is not an int")))?;
    let b = r
        .as_int()
        .ok_or_else(|| err(format!("right side of {op} is not an int")))?;
    Ok((a, b))
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

/// Evaluates a compiler-synthesized PA function call. Returns the value and
/// an `unknown` flag the caller uses to decide whether to print a
/// `<skipping unknown "name">` notice at the current indent.
///
/// Functions implemented here should match PA expression-language semantics
/// closely enough that paxr is a useful local sanity check. A few PA quirks
/// worth noting:
///
/// - String `startsWith`, `endsWith`, `indexOf`, and `lastIndexOf` are
///   case-insensitive. `contains` on a string is case-sensitive.
/// - `length` / `empty` / `contains` are polymorphic: they accept strings,
///   arrays, and objects.
/// - String indices are character-based (not byte-based), so Unicode code
///   points count as one each, matching PA.
fn eval_call(name: &str, args: Vec<Value>) -> (Value, bool) {
    let v = match name {
        // --- arithmetic / logic (compiler-synthesized from pax operators) ---
        "add" => binary_int(&args, i64::wrapping_add),
        "sub" => binary_int(&args, i64::wrapping_sub),
        "mul" => binary_int(&args, i64::wrapping_mul),
        "div" => {
            if args.len() == 2
                && let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int())
                && b != 0
            {
                return (Value::Int(a / b), false);
            }
            return (Value::Null, false);
        }
        "mod" => {
            if args.len() == 2
                && let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int())
                && b != 0
            {
                return (Value::Int(a.rem_euclid(b)), false);
            }
            return (Value::Null, false);
        }
        "min" => min_or_max_int(&args, true),
        "max" => min_or_max_int(&args, false),
        "range" => range_int(&args),

        // --- boolean / comparison ---
        "equals" if args.len() == 2 => Value::Bool(args[0].equals(&args[1])),
        "less" => binary_cmp(&args, |a, b| a < b),
        "lessOrEquals" => binary_cmp(&args, |a, b| a <= b),
        "greater" => binary_cmp(&args, |a, b| a > b),
        "greaterOrEquals" => binary_cmp(&args, |a, b| a >= b),
        "not" if args.len() == 1 => match args[0].as_bool() {
            Some(b) => Value::Bool(!b),
            None => Value::Null,
        },
        "and" => binary_bool(&args, |a, b| a && b),
        "or" => binary_bool(&args, |a, b| a || b),

        // --- string ---
        "concat" => {
            let mut s = String::new();
            for a in &args {
                s.push_str(&a.coerce_str());
            }
            Value::Str(s)
        }
        "toUpper" if args.len() == 1 => match &args[0] {
            Value::Str(s) => Value::Str(s.to_uppercase()),
            _ => Value::Null,
        },
        "toLower" if args.len() == 1 => match &args[0] {
            Value::Str(s) => Value::Str(s.to_lowercase()),
            _ => Value::Null,
        },
        "trim" if args.len() == 1 => match &args[0] {
            Value::Str(s) => Value::Str(s.trim().to_string()),
            _ => Value::Null,
        },
        "substring" => substring_fn(&args),
        "indexOf" if args.len() == 2 => index_of_ci(&args[0], &args[1], false),
        "lastIndexOf" if args.len() == 2 => index_of_ci(&args[0], &args[1], true),
        "startsWith" if args.len() == 2 => string_boundary_ci(&args[0], &args[1], true),
        "endsWith" if args.len() == 2 => string_boundary_ci(&args[0], &args[1], false),
        "replace" if args.len() == 3 => match (&args[0], &args[1], &args[2]) {
            (Value::Str(s), Value::Str(old), Value::Str(new)) if !old.is_empty() => {
                Value::Str(s.replace(old.as_str(), new))
            }
            _ => Value::Null,
        },
        "split" if args.len() == 2 => match (&args[0], &args[1]) {
            (Value::Str(s), Value::Str(delim)) if !delim.is_empty() => Value::Array(
                s.split(delim.as_str())
                    .map(|p| Value::Str(p.to_string()))
                    .collect(),
            ),
            _ => Value::Null,
        },
        "join" if args.len() == 2 => match (&args[0], &args[1]) {
            (Value::Array(items), Value::Str(delim)) => {
                let parts: Vec<String> = items.iter().map(Value::coerce_str).collect();
                Value::Str(parts.join(delim))
            }
            _ => Value::Null,
        },

        // --- polymorphic (string + array + object) ---
        "length" if args.len() == 1 => length_of(&args[0]),
        "empty" if args.len() == 1 => is_empty(&args[0]),
        "contains" if args.len() == 2 => contains_of(&args[0], &args[1]),

        // --- array ---
        "first" if args.len() == 1 => match &args[0] {
            Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
            Value::Str(s) => s
                .chars()
                .next()
                .map(|c| Value::Str(c.to_string()))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "last" if args.len() == 1 => match &args[0] {
            Value::Array(items) => items.last().cloned().unwrap_or(Value::Null),
            Value::Str(s) => s
                .chars()
                .next_back()
                .map(|c| Value::Str(c.to_string()))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "skip" if args.len() == 2 => match (&args[0], args[1].as_int()) {
            (Value::Array(items), Some(n)) => {
                let n = n.max(0) as usize;
                Value::Array(items.iter().skip(n).cloned().collect())
            }
            _ => Value::Null,
        },
        "take" if args.len() == 2 => match (&args[0], args[1].as_int()) {
            (Value::Array(items), Some(n)) => {
                let n = n.max(0) as usize;
                Value::Array(items.iter().take(n).cloned().collect())
            }
            _ => Value::Null,
        },

        _ => return (Value::Null, true),
    };
    (v, false)
}

fn substring_fn(args: &[Value]) -> Value {
    if !(args.len() == 2 || args.len() == 3) {
        return Value::Null;
    }
    let Value::Str(s) = &args[0] else {
        return Value::Null;
    };
    let Some(start) = args[1].as_int() else {
        return Value::Null;
    };
    let chars: Vec<char> = s.chars().collect();
    let start = start.max(0) as usize;
    if start >= chars.len() {
        return Value::Str(String::new());
    }
    let end = if args.len() == 3 {
        match args[2].as_int() {
            Some(n) => (start + n.max(0) as usize).min(chars.len()),
            None => return Value::Null,
        }
    } else {
        chars.len()
    };
    Value::Str(chars[start..end].iter().collect())
}

/// Case-insensitive search. `from_end` picks lastIndexOf semantics.
/// Returns char index or -1. An empty needle returns 0 (matches PA).
fn index_of_ci(haystack: &Value, needle: &Value, from_end: bool) -> Value {
    let (Value::Str(h), Value::Str(n)) = (haystack, needle) else {
        return Value::Null;
    };
    if n.is_empty() {
        return Value::Int(0);
    }
    let hl = h.to_lowercase();
    let nl = n.to_lowercase();
    let byte_idx = if from_end { hl.rfind(&nl) } else { hl.find(&nl) };
    match byte_idx {
        Some(b) => Value::Int(hl[..b].chars().count() as i64),
        None => Value::Int(-1),
    }
}

/// Case-insensitive startsWith / endsWith (PA behavior).
fn string_boundary_ci(haystack: &Value, needle: &Value, is_start: bool) -> Value {
    let (Value::Str(h), Value::Str(n)) = (haystack, needle) else {
        return Value::Null;
    };
    let hl = h.to_lowercase();
    let nl = n.to_lowercase();
    let result = if is_start { hl.starts_with(&nl) } else { hl.ends_with(&nl) };
    Value::Bool(result)
}

fn length_of(v: &Value) -> Value {
    match v {
        Value::Str(s) => Value::Int(s.chars().count() as i64),
        Value::Array(items) => Value::Int(items.len() as i64),
        Value::Object(entries) => Value::Int(entries.len() as i64),
        _ => Value::Null,
    }
}

fn is_empty(v: &Value) -> Value {
    match v {
        Value::Null => Value::Bool(true),
        Value::Str(s) => Value::Bool(s.is_empty()),
        Value::Array(items) => Value::Bool(items.is_empty()),
        Value::Object(entries) => Value::Bool(entries.is_empty()),
        _ => Value::Null,
    }
}

fn contains_of(haystack: &Value, needle: &Value) -> Value {
    match haystack {
        // PA: string contains is case-sensitive.
        Value::Str(s) => {
            let n = needle.coerce_str();
            Value::Bool(s.contains(&n))
        }
        Value::Array(items) => Value::Bool(items.iter().any(|i| i.equals(needle))),
        Value::Object(entries) => {
            let n = needle.coerce_str();
            Value::Bool(entries.iter().any(|(k, _)| k == &n))
        }
        _ => Value::Null,
    }
}

/// `min(a, b, c, ...)` or `min([a, b, c])`. PA supports both forms.
/// `smallest = true` picks min, false picks max. Empty input → Null.
fn min_or_max_int(args: &[Value], smallest: bool) -> Value {
    let nums: Option<Vec<i64>> = if args.len() == 1 {
        match &args[0] {
            Value::Array(items) => items.iter().map(Value::as_int).collect(),
            _ => args.iter().map(Value::as_int).collect(),
        }
    } else {
        args.iter().map(Value::as_int).collect()
    };
    match nums {
        Some(ns) if !ns.is_empty() => {
            let picked = if smallest {
                *ns.iter().min().unwrap()
            } else {
                *ns.iter().max().unwrap()
            };
            Value::Int(picked)
        }
        _ => Value::Null,
    }
}

fn range_int(args: &[Value]) -> Value {
    if args.len() == 2
        && let (Some(start), Some(count)) = (args[0].as_int(), args[1].as_int())
        && count >= 0
    {
        return Value::Array((0..count).map(|i| Value::Int(start + i)).collect());
    }
    Value::Null
}

fn binary_int<F: Fn(i64, i64) -> i64>(args: &[Value], f: F) -> Value {
    if args.len() == 2
        && let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int())
    {
        return Value::Int(f(a, b));
    }
    Value::Null
}

fn binary_cmp<F: Fn(i64, i64) -> bool>(args: &[Value], f: F) -> Value {
    if args.len() == 2
        && let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int())
    {
        return Value::Bool(f(a, b));
    }
    Value::Null
}

fn binary_bool<F: Fn(bool, bool) -> bool>(args: &[Value], f: F) -> Value {
    if args.len() == 2
        && let (Some(a), Some(b)) = (args[0].as_bool(), args[1].as_bool())
    {
        return Value::Bool(f(a, b));
    }
    Value::Null
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
        let state = run(
            "var x: int = 1\nif x == 1 {\n  let inner = 99\n}\nlet outer = x",
        )
        .unwrap();
        let dump = format_state_dump(&state);
        assert!(!dump.contains("inner"), "nested let leaked into dump:\n{dump}");
        assert!(dump.contains("outer (let) = 1"), "missing outer binding:\n{dump}");
    }

    #[test]
    fn slice22_state_dump_renders_composites_as_pretty_json() {
        let state = run(
            "var empty_arr: array = []\nvar obj: object = { \"a\": 1 }",
        )
        .unwrap();
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
        let state = run(
            "var x: int = 0\nif x == 0 { terminate succeeded }\nx = 42",
        )
        .unwrap();
        assert_eq!(state.vars.get("x").and_then(Value::as_int), Some(0));
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
        assert!(matches!(eval_let(r#"let x = toUpper("hello")"#, "x"), Value::Str(s) if s == "HELLO"));
        assert!(matches!(eval_let(r#"let x = toLower("HeLLo")"#, "x"), Value::Str(s) if s == "hello"));
        assert!(matches!(eval_let(r#"let x = trim("  hi  ")"#, "x"), Value::Str(s) if s == "hi"));
    }

    #[test]
    fn fn_length_polymorphic() {
        assert!(matches!(eval_let(r#"let x = length("hello")"#, "x"), Value::Int(5)));
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
        assert!(matches!(eval_let(r#"let x = empty("")"#, "x"), Value::Bool(true)));
        assert!(matches!(eval_let(r#"let x = empty("hi")"#, "x"), Value::Bool(false)));
        assert!(matches!(
            eval_let("var a: array = []\nlet x = empty(a)", "x"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn fn_contains_string_case_sensitive() {
        // PA's string contains is case-sensitive.
        assert!(matches!(eval_let(r#"let x = contains("hello world", "WORLD")"#, "x"), Value::Bool(false)));
        assert!(matches!(eval_let(r#"let x = contains("hello world", "world")"#, "x"), Value::Bool(true)));
    }

    #[test]
    fn fn_contains_array_membership() {
        assert!(matches!(
            eval_let("var a: array = [\"x\", \"y\", \"z\"]\nlet r = contains(a, \"y\")", "r"),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval_let("var a: array = [\"x\", \"y\", \"z\"]\nlet r = contains(a, \"q\")", "r"),
            Value::Bool(false)
        ));
    }

    #[test]
    fn fn_starts_and_ends_with_case_insensitive() {
        // PA quirk: these are case-insensitive.
        assert!(matches!(eval_let(r#"let x = startsWith("Hello", "HE")"#, "x"), Value::Bool(true)));
        assert!(matches!(eval_let(r#"let x = endsWith("WORLD", "rld")"#, "x"), Value::Bool(true)));
    }

    #[test]
    fn fn_index_of_case_insensitive_char_index() {
        assert!(matches!(eval_let(r#"let x = indexOf("Hello World", "WORLD")"#, "x"), Value::Int(6)));
        assert!(matches!(eval_let(r#"let x = indexOf("abc", "z")"#, "x"), Value::Int(-1)));
        assert!(matches!(eval_let(r#"let x = lastIndexOf("a-b-c", "-")"#, "x"), Value::Int(3)));
    }

    #[test]
    fn fn_substring_two_and_three_arg() {
        assert!(matches!(eval_let(r#"let x = substring("hello world", 6)"#, "x"), Value::Str(s) if s == "world"));
        assert!(matches!(eval_let(r#"let x = substring("hello world", 0, 5)"#, "x"), Value::Str(s) if s == "hello"));
        // Out-of-range start returns empty, not error.
        assert!(matches!(eval_let(r#"let x = substring("hi", 10)"#, "x"), Value::Str(s) if s.is_empty()));
    }

    #[test]
    fn fn_replace_and_split() {
        assert!(matches!(eval_let(r#"let x = replace("a-b-c", "-", "_")"#, "x"), Value::Str(s) if s == "a_b_c"));
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
        assert!(matches!(eval_let("let x = min(3, 1, 2)", "x"), Value::Int(1)));
        assert!(matches!(eval_let("let x = max(3, 1, 2)", "x"), Value::Int(3)));
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
}
