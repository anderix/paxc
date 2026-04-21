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

use crate::ast::{BinOp, DebugArg, Expr, Literal, UnaryOp};
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
    }
}

pub fn interpret(src: &str, program: &ResolvedProgram) -> Result<(), InterpretError> {
    let mut interp = Interpreter::new(src);
    interp.run_actions(&program.actions)?;
    Ok(())
}

struct Interpreter<'src> {
    src: &'src str,
    vars: HashMap<String, Value>,
    /// Keyed by Compose action name (e.g. `Compose_remaining`).
    compose_outputs: HashMap<String, Value>,
    /// Keyed by Apply_to_each action name -> current iterator element.
    iterators: HashMap<String, Value>,
}

impl<'src> Interpreter<'src> {
    fn new(src: &'src str) -> Self {
        Self {
            src,
            vars: HashMap::new(),
            compose_outputs: HashMap::new(),
            iterators: HashMap::new(),
        }
    }

    fn run_actions(&mut self, actions: &[ResolvedAction]) -> Result<(), InterpretError> {
        for action in actions {
            self.run_action(action)?;
        }
        Ok(())
    }

    fn run_action(&mut self, action: &ResolvedAction) -> Result<(), InterpretError> {
        match &action.kind {
            ActionKind::InitializeVariable { var, value, .. } => {
                let v = self.eval(value)?;
                self.vars.insert(var.clone(), v);
            }
            ActionKind::SetVariable { var, value } => {
                let v = self.eval(value)?;
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
                self.vars.insert(var.clone(), Value::Int(current + delta));
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
                self.vars.insert(var.clone(), Value::Int(current - delta));
            }
            ActionKind::AppendToStringVariable { var, value } => {
                let suffix = self.eval(value)?.coerce_str();
                let current = match self.vars.get(var) {
                    Some(Value::Str(s)) => s.clone(),
                    _ => return Err(err(format!("variable {var} is not a string"))),
                };
                self.vars.insert(var.clone(), Value::Str(current + &suffix));
            }
            ActionKind::AppendToArrayVariable { var, value } => {
                let item = self.eval(value)?;
                let mut arr = match self.vars.get(var) {
                    Some(Value::Array(items)) => items.clone(),
                    _ => return Err(err(format!("variable {var} is not an array"))),
                };
                arr.push(item);
                self.vars.insert(var.clone(), Value::Array(arr));
            }
            ActionKind::Compose { value } => {
                let v = self.eval(value)?;
                self.compose_outputs.insert(action.name.clone(), v);
            }
            ActionKind::Raw { .. } => {
                // paxr does not execute raw blocks. Surface the skip so the
                // developer knows their state may diverge from what the
                // compiled flow would produce.
                println!("<skipping raw \"{}\">", action.name);
            }
            ActionKind::Condition {
                condition,
                true_branch,
                false_branch,
            } => {
                let taken = self.eval(condition)?.as_bool().unwrap_or(false);
                let branch = if taken { true_branch } else { false_branch };
                self.run_actions(branch)?;
            }
            ActionKind::Foreach { collection, body } => {
                let items = match self.eval(collection)? {
                    Value::Array(items) => items,
                    _ => return Err(err("foreach requires an array")),
                };
                for item in items {
                    self.iterators.insert(action.name.clone(), item);
                    self.run_actions(body)?;
                }
                self.iterators.remove(&action.name);
            }
            ActionKind::Debug { args, span } => {
                self.emit_debug(args, *span)?;
            }
        }
        Ok(())
    }

    fn emit_debug(&mut self, args: &[DebugArg], stmt_span: Span) -> Result<(), InterpretError> {
        let line = span_to_line(self.src, stmt_span.start);
        if args.is_empty() {
            println!("debug: at line {line}");
            return Ok(());
        }
        let mut parts: Vec<String> = Vec::with_capacity(args.len());
        for arg in args {
            let label = source_slice(self.src, arg.span);
            let value = self.eval(&arg.expr)?;
            parts.push(format!("{label}={}", value.display_compact()));
        }
        println!("debug: {} at line {line}", parts.join(", "));
        Ok(())
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, InterpretError> {
        match expr {
            Expr::Literal(lit) => Ok(literal_to_value(lit)),
            Expr::Ref(name) => Err(err(format!(
                "internal error: unresolved ref {name} reached interpreter"
            ))),
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
                Ok(eval_call(name, vals))
            }
        }
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

fn eval_call(name: &str, args: Vec<Value>) -> Value {
    match name {
        "add" => binary_int(&args, i64::wrapping_add),
        "sub" => binary_int(&args, i64::wrapping_sub),
        "mul" => binary_int(&args, i64::wrapping_mul),
        "div" => {
            if args.len() == 2 {
                if let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int()) {
                    if b != 0 {
                        return Value::Int(a / b);
                    }
                }
            }
            Value::Null
        }
        "concat" => {
            let mut s = String::new();
            for a in &args {
                s.push_str(&a.coerce_str());
            }
            Value::Str(s)
        }
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
        _ => {
            println!("<skipping unknown \"{name}\">");
            Value::Null
        }
    }
}

fn binary_int<F: Fn(i64, i64) -> i64>(args: &[Value], f: F) -> Value {
    if args.len() == 2 {
        if let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int()) {
            return Value::Int(f(a, b));
        }
    }
    Value::Null
}

fn binary_cmp<F: Fn(i64, i64) -> bool>(args: &[Value], f: F) -> Value {
    if args.len() == 2 {
        if let (Some(a), Some(b)) = (args[0].as_int(), args[1].as_int()) {
            return Value::Bool(f(a, b));
        }
    }
    Value::Null
}

fn binary_bool<F: Fn(bool, bool) -> bool>(args: &[Value], f: F) -> Value {
    if args.len() == 2 {
        if let (Some(a), Some(b)) = (args[0].as_bool(), args[1].as_bool()) {
            return Value::Bool(f(a, b));
        }
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

    fn run(src: &str) -> Result<(), InterpretError> {
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
}
