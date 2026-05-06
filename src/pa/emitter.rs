//! Emitter: turns a resolved pax program into a Power Automate flow definition.
//!
//! The emitter walks a `ResolvedProgram` and produces the standard
//! `Microsoft.Logic/workflows` outer shape with a manual-button trigger and
//! an `actions` map. Action keys and `runAfter` predecessors come from the
//! resolver, so this layer is purely a tree-to-JSON translation.

use crate::ast::{BinOp, Expr, Literal, SubscriptKey, TerminateStatus, Type, UnaryOp};
use crate::pa::names::{action, trigger_type};
use crate::resolver::{
    ActionKind, ResolvedAction, ResolvedProgram, ResolvedSwitchCase, ResolvedTrigger, RunAfterEntry,
};
use serde_json::{Map, Value, json};

const SCHEMA_URL: &str = "https://schema.management.azure.com/providers/Microsoft.Logic/schemas/2016-06-01/workflowdefinition.json#";

pub fn emit(program: &ResolvedProgram) -> Value {
    let mut top = Map::new();
    top.insert(
        "definition".to_string(),
        json!({
            "$schema": SCHEMA_URL,
            "contentVersion": "1.0.0.0",
            "triggers": emit_trigger(&program.trigger),
            "actions": build_actions_map(&program.actions),
        }),
    );
    if let Some(refs) = &program.connection_references {
        top.insert("connectionReferences".to_string(), refs.clone());
    }
    Value::Object(top)
}

fn build_actions_map(actions: &[ResolvedAction]) -> Value {
    let mut map = Map::new();
    for action in actions {
        // Debug statements are diagnostic-only. paxc drops them from the
        // emitted PA flow; `count_debug_actions` reports them separately.
        if matches!(action.kind, ActionKind::Debug { .. }) {
            continue;
        }
        map.insert(action.name.clone(), emit_action(action));
    }
    Value::Object(map)
}

/// Recursively counts `debug()` statements in a resolved program so paxc can
/// emit the end-of-compile note telling the user how many were dropped.
pub fn count_debug_actions(actions: &[ResolvedAction]) -> usize {
    let mut n = 0;
    for action in actions {
        match &action.kind {
            ActionKind::Debug { .. } => n += 1,
            ActionKind::Condition {
                true_branch,
                false_branch,
                ..
            } => {
                n += count_debug_actions(true_branch);
                n += count_debug_actions(false_branch);
            }
            ActionKind::Foreach { body, .. } => {
                n += count_debug_actions(body);
            }
            ActionKind::Switch { cases, default, .. } => {
                for case in cases {
                    n += count_debug_actions(&case.body);
                }
                if let Some(default) = default {
                    n += count_debug_actions(default);
                }
            }
            ActionKind::Scope { body } => {
                n += count_debug_actions(body);
            }
            ActionKind::Until { body, .. } => {
                n += count_debug_actions(body);
            }
            ActionKind::OnHandler { body, .. } => {
                n += count_debug_actions(body);
            }
            _ => {}
        }
    }
    n
}

fn emit_trigger(trigger: &ResolvedTrigger) -> Value {
    match trigger {
        ResolvedTrigger::DefaultManual => json!({
            "manual": {
                "type": trigger_type::REQUEST,
                "kind": "Button",
                "inputs": {}
            }
        }),
        ResolvedTrigger::FromFile { name, body_json } => {
            let mut obj = Map::new();
            obj.insert(name.clone(), body_json.clone());
            Value::Object(obj)
        }
    }
}

fn emit_action(action: &ResolvedAction) -> Value {
    let body = match &action.kind {
        ActionKind::InitializeVariable { var, ty, value } => emit_initialize(var, ty, value),
        ActionKind::SetVariable { var, value } => emit_mutation(action::SET_VARIABLE, var, value),
        ActionKind::IncrementVariable { var, value } => {
            emit_mutation(action::INCREMENT_VARIABLE, var, value)
        }
        ActionKind::DecrementVariable { var, value } => {
            emit_mutation(action::DECREMENT_VARIABLE, var, value)
        }
        ActionKind::AppendToStringVariable { var, value } => {
            emit_mutation(action::APPEND_TO_STRING_VARIABLE, var, value)
        }
        ActionKind::AppendToArrayVariable { var, value } => {
            emit_mutation(action::APPEND_TO_ARRAY_VARIABLE, var, value)
        }
        ActionKind::Compose { value, .. } => emit_compose(value),
        ActionKind::Pa { body_json } => body_json.clone(),
        ActionKind::Condition {
            condition,
            true_branch,
            false_branch,
            ..
        } => emit_condition(condition, true_branch, false_branch),
        ActionKind::Foreach {
            collection, body, ..
        } => emit_foreach(collection, body),
        ActionKind::Debug { .. } => {
            // Filtered out in `build_actions_map` before reaching here.
            unreachable!("debug action reached emitter");
        }
        ActionKind::Terminate { status, message } => emit_terminate(*status, message.as_ref()),
        ActionKind::Switch {
            subject,
            cases,
            default,
            ..
        } => emit_switch(subject, cases, default.as_deref()),
        ActionKind::Scope { body } => emit_scope(body),
        ActionKind::Until {
            condition,
            limit_count,
            limit_timeout,
            body,
            ..
        } => emit_until(condition, *limit_count, limit_timeout.as_deref(), body),
        ActionKind::OnHandler { body, .. } => emit_scope(body),
    };
    splice_run_after(body, &action.run_after)
}

fn emit_scope(body: &[ResolvedAction]) -> Value {
    json!({
        "type": action::SCOPE,
        "actions": build_actions_map(body),
    })
}

/// PA requires `limit.count` and `limit.timeout` on every Until. paxc emits
/// PA's own defaults (60 iterations, 1 hour) so surfaces stay consistent.
/// User overrides come in via `until cond max N timeout "..."` and land
/// on the resolved ActionKind; these constants are the fallback.
const UNTIL_DEFAULT_COUNT: u32 = 60;
const UNTIL_DEFAULT_TIMEOUT: &str = "PT1H";

fn emit_until(
    condition: &Expr,
    limit_count: Option<u32>,
    limit_timeout: Option<&str>,
    body: &[ResolvedAction],
) -> Value {
    // PA's Until expression is the exit condition; same auto-wrap rule as
    // If -- if the outer expression is already boolean (comparison / logical
    // op), skip the `equals(_, true)` wrap.
    let expression = if is_boolean_expr(condition) {
        format!("@{}", pa_expr(condition))
    } else {
        format!("@equals({}, true)", pa_expr(condition))
    };
    let count = limit_count.unwrap_or(UNTIL_DEFAULT_COUNT);
    let timeout = limit_timeout.unwrap_or(UNTIL_DEFAULT_TIMEOUT);
    json!({
        "type": action::UNTIL,
        "expression": expression,
        "limit": {
            "count": count,
            "timeout": timeout,
        },
        "actions": build_actions_map(body),
    })
}

fn emit_switch(
    subject: &Expr,
    cases: &[ResolvedSwitchCase],
    default: Option<&[ResolvedAction]>,
) -> Value {
    let mut cases_map = Map::new();
    for case in cases {
        cases_map.insert(
            case.action_name.clone(),
            json!({
                "case": literal_to_json(&case.value),
                "actions": build_actions_map(&case.body),
            }),
        );
    }
    // PA accepts a Switch without a default arm -- the whole `default` key can
    // be omitted. paxc emits `default: { actions: {} }` only when the source
    // wrote a `default` block, even if the block is empty, so round-tripping
    // the source's intent is preserved.
    let mut out = Map::new();
    out.insert("type".to_string(), json!(action::SWITCH));
    out.insert("expression".to_string(), json!(expr_to_pa_field(subject)));
    out.insert("cases".to_string(), Value::Object(cases_map));
    if let Some(default_actions) = default {
        out.insert(
            "default".to_string(),
            json!({ "actions": build_actions_map(default_actions) }),
        );
    }
    Value::Object(out)
}

fn emit_terminate(status: TerminateStatus, message: Option<&Expr>) -> Value {
    let mut inputs = Map::new();
    inputs.insert(
        "runStatus".to_string(),
        Value::String(status.as_pa_str().to_string()),
    );
    if let Some(msg) = message {
        // `expr_to_json` emits literal strings as plain JSON and expressions
        // as the `@{...}` interpolation form -- both valid for PA's
        // `runError.message` field.
        inputs.insert(
            "runError".to_string(),
            json!({ "message": expr_to_json(msg) }),
        );
    }
    json!({
        "type": action::TERMINATE,
        "inputs": Value::Object(inputs),
    })
}

fn emit_foreach(collection: &Expr, body: &[ResolvedAction]) -> Value {
    json!({
        "type": action::FOREACH,
        "foreach": expr_to_pa_field(collection),
        "actions": build_actions_map(body),
    })
}

fn emit_condition(
    condition: &Expr,
    true_branch: &[ResolvedAction],
    false_branch: &[ResolvedAction],
) -> Value {
    json!({
        "type": action::IF,
        "expression": condition_to_value(condition),
        "actions": build_actions_map(true_branch),
        "else": {
            "actions": build_actions_map(false_branch),
        }
    })
}

/// Top-level entry point for an If action's `expression` field. Always
/// returns a structured object whose root key is `and` / `or` -- matching
/// PA designer's own export convention. The designer renders blank when
/// the expression is a string OR when it's a bare predicate without an
/// outer logical wrap, so paxc imitates the wrap exactly: even a single
/// `equals(...)` rule comes out as `{"and": [{"equals": [...]}]}`.
fn condition_to_value(expr: &Expr) -> Value {
    let inner = condition_node(expr);
    // If the root is already a logical combinator (and/or), keep it as-is.
    // Otherwise wrap in `and: [...]` so designer renders the rule list.
    if let Value::Object(map) = &inner
        && map.len() == 1
        && matches!(map.keys().next().unwrap().as_str(), "and" | "or")
    {
        return inner;
    }
    json!({ "and": [inner] })
}

/// Build a single PA condition node (no top-level wrap). Used both for
/// the operand position inside and/or arrays and as the body of the
/// outer wrap in [`condition_to_value`].
///
/// Shapes:
/// - and/or → `{op: [<cond>, <cond>, ...]}`
/// - not    → `{"not": <cond>}` (single child, not array — matches PA exports)
/// - comparisons / predicates → `{op: [<value>, <value>, ...]}`
/// - leaves: literals as bare JSON, expressions as `"@<pa-expr>"` strings
///
/// Anything paxc can't classify as a known boolean op gets the conservative
/// `equals(_, true)` wrap, same auto-wrap rule paxc has always applied for
/// non-boolean condition expressions.
fn condition_node(expr: &Expr) -> Value {
    if let Some(v) = try_condition_node(expr) {
        return v;
    }
    json!({ "equals": [condition_operand_value(expr), true] })
}

fn try_condition_node(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::BinaryOp { op, lhs, rhs } => match op {
            BinOp::And => Some(json!({
                "and": [condition_node(lhs), condition_node(rhs)]
            })),
            BinOp::Or => Some(json!({
                "or": [condition_node(lhs), condition_node(rhs)]
            })),
            BinOp::Equals => Some(json!({
                "equals": [condition_operand_value(lhs), condition_operand_value(rhs)]
            })),
            BinOp::NotEquals => Some(json!({
                "not": { "equals": [condition_operand_value(lhs), condition_operand_value(rhs)] }
            })),
            BinOp::Less => Some(json!({
                "less": [condition_operand_value(lhs), condition_operand_value(rhs)]
            })),
            BinOp::LessEq => Some(json!({
                "lessOrEquals": [condition_operand_value(lhs), condition_operand_value(rhs)]
            })),
            BinOp::Greater => Some(json!({
                "greater": [condition_operand_value(lhs), condition_operand_value(rhs)]
            })),
            BinOp::GreaterEq => Some(json!({
                "greaterOrEquals": [condition_operand_value(lhs), condition_operand_value(rhs)]
            })),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOp::Not,
            operand,
        } => Some(json!({ "not": condition_node(operand) })),
        Expr::Call { name, args } => match name.as_str() {
            "and" | "or" => Some(json!({
                name.as_str(): args.iter().map(condition_node).collect::<Vec<_>>()
            })),
            "not" if args.len() == 1 => Some(json!({ "not": condition_node(&args[0]) })),
            "equals" | "less" | "lessOrEquals" | "greater" | "greaterOrEquals" | "contains"
            | "startsWith" | "endsWith" | "empty" => Some(json!({
                name.as_str(): args.iter().map(condition_operand_value).collect::<Vec<_>>()
            })),
            _ => None,
        },
        _ => None,
    }
}

/// Render a leaf operand for a structured condition. Literals stay as bare
/// JSON values (PA accepts bare bool/number/string at operand slots);
/// anything else becomes an `@`-prefixed PA expression string.
fn condition_operand_value(expr: &Expr) -> Value {
    match expr {
        Expr::Literal(lit) => literal_to_json(lit),
        _ => Value::String(format!("@{}", pa_expr(expr))),
    }
}

fn is_boolean_expr(expr: &Expr) -> bool {
    match expr {
        Expr::BinaryOp { op, .. } => op.is_boolean(),
        Expr::UnaryOp {
            op: UnaryOp::Not, ..
        } => true,
        // PA functions whose return type is boolean. Recognizing them here
        // skips paxc's defensive `equals(_, true)` wrap on If conditions
        // that go through one of these calls -- which happens any time a
        // PA designer-style structured-object condition (and/or/equals)
        // round-trips through pax via `paexpr::condition_value_to_pax`.
        Expr::Call { name, .. } => matches!(
            name.as_str(),
            "and"
                | "or"
                | "not"
                | "equals"
                | "less"
                | "lessOrEquals"
                | "greater"
                | "greaterOrEquals"
                | "contains"
                | "startsWith"
                | "endsWith"
                | "empty"
                | "bool"
        ),
        _ => false,
    }
}

fn emit_compose(value: &Expr) -> Value {
    json!({
        "type": action::COMPOSE,
        "inputs": expr_to_json(value),
    })
}

fn emit_mutation(action_type: &str, var: &str, value: &Expr) -> Value {
    let value_json = expr_to_json(value);
    json!({
        "type": action_type,
        "inputs": {
            "name": var,
            "value": value_json,
        }
    })
}

fn expr_to_json(value: &Expr) -> Value {
    match value {
        Expr::Literal(lit) => literal_to_json(lit),
        _ => {
            // String-template form: a `&`-concatenation chain that mixes
            // literal text with subexpressions emits as PA's designer-style
            // template ("text @{expr} more @{expr}") rather than nested
            // `concat()` calls. Designer renders templates as inline
            // expression bubbles inside a string; concat chains render as
            // one opaque expression, which is harder to read and edit.
            if let Some(template) = try_string_template(value) {
                return Value::String(template);
            }
            // Single-expression non-literal values use the bare `@<expr>`
            // form, not `@{<expr>}`. Bare-`@` preserves the expression's
            // natural return type (so an integer-typed variable initialized
            // to `@int(...)` stores an integer); `@{...}` would coerce to
            // string. PA designer always writes bare `@` for full-expression
            // values; matching that convention also keeps round-trips
            // byte-identical.
            json!(format!("@{}", pa_expr(value)))
        }
    }
}

/// If `expr` is a `&`-concatenation chain that contains at least one
/// string literal *and* at least one non-literal subexpression, return
/// PA's template form: literal segments inlined verbatim, expression
/// segments wrapped in `@{...}`. Otherwise return None so the caller
/// falls back to the bare `@<expr>` form.
///
/// Pure-literal chains and pure-expression chains both fall through:
/// the former is unusual (resolver folds adjacent literals via concat),
/// the latter renders fine as `@concat(a, b)`. Only mixed chains gain
/// readability from the template form.
fn try_string_template(expr: &Expr) -> Option<String> {
    if !matches!(
        expr,
        Expr::BinaryOp {
            op: BinOp::Concat,
            ..
        }
    ) {
        return None;
    }
    let parts = flatten_concat(expr);
    let has_literal = parts.iter().any(|p| matches!(p, Expr::Literal(_)));
    let has_expr = parts.iter().any(|p| !matches!(p, Expr::Literal(_)));
    if !has_literal || !has_expr {
        return None;
    }
    let mut out = String::new();
    for part in parts {
        match part {
            Expr::Literal(Literal::String(s)) => {
                // PA template literals treat `@` as the start of an
                // expression; double it to escape. Empty strings contribute
                // nothing -- a `"" & x` chain just renders as `@{x}`.
                out.push_str(&s.replace('@', "@@"));
            }
            Expr::Literal(lit) => {
                // Non-string literals shouldn't appear in a `&` chain
                // (the resolver requires string operands), but render
                // defensively as raw text rather than panic.
                out.push_str(&literal_as_text(lit));
            }
            other => {
                out.push_str("@{");
                out.push_str(&pa_expr(other));
                out.push('}');
            }
        }
    }
    Some(out)
}

/// Walk a left-associative Concat tree and flatten it into the list of
/// terminal operands in source order. Non-Concat nodes return as a
/// singleton vec so this composes cleanly during recursion.
fn flatten_concat(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::BinaryOp {
            op: BinOp::Concat,
            lhs,
            rhs,
        } => {
            let mut out = flatten_concat(lhs);
            out.extend(flatten_concat(rhs));
            out
        }
        _ => vec![expr],
    }
}

/// Stringify a non-string literal for use inside a template segment.
/// Only reachable defensively -- the resolver rejects non-string `&`
/// operands -- but kept so an unexpected shape produces a sensible
/// fallback instead of a panic.
fn literal_as_text(lit: &Literal) -> String {
    match lit {
        Literal::Null => "null".to_string(),
        Literal::Int(n) => n.to_string(),
        Literal::Float(f) => format_float(*f),
        Literal::Bool(b) => b.to_string(),
        Literal::String(s) => s.clone(),
        Literal::Array(_) | Literal::Object(_) => String::new(),
    }
}

/// Builds the inner text of a PA expression (everything that goes inside `@{...}`).
fn pa_expr(expr: &Expr) -> String {
    match expr {
        Expr::VarRef { name, .. } => format!("variables('{name}')"),
        Expr::ComposeRef { action_name, .. } => format!("outputs('{action_name}')"),
        Expr::IteratorRef { action_name, .. } => format!("items('{action_name}')"),
        Expr::Member { target, field } => {
            format!("{}?['{}']", pa_expr(target), field.replace('\'', "''"))
        }
        Expr::Subscript { target, key } => match key {
            SubscriptKey::String(s) => {
                format!("{}?['{}']", pa_expr(target), s.replace('\'', "''"))
            }
            SubscriptKey::Index(i) => format!("{}?[{}]", pa_expr(target), i),
        },
        Expr::Literal(lit) => pa_literal(lit),
        Expr::BinaryOp { op, lhs, rhs } => {
            // PA has no `notEquals`; synthesize it as `not(equals(...))`.
            if let BinOp::NotEquals = op {
                return format!("not(equals({}, {}))", pa_expr(lhs), pa_expr(rhs));
            }
            let fn_name = match op {
                BinOp::Concat => "concat",
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::Mul => "mul",
                BinOp::Div => "div",
                BinOp::Less => "less",
                BinOp::LessEq => "lessOrEquals",
                BinOp::Greater => "greater",
                BinOp::GreaterEq => "greaterOrEquals",
                BinOp::Equals => "equals",
                BinOp::And => "and",
                BinOp::Or => "or",
                BinOp::NotEquals => unreachable!("handled above"),
            };
            format!("{}({}, {})", fn_name, pa_expr(lhs), pa_expr(rhs))
        }
        Expr::UnaryOp { op, operand } => match op {
            UnaryOp::Not => format!("not({})", pa_expr(operand)),
            // PA has no unary minus; synthesize via sub(0, operand).
            UnaryOp::Neg => format!("sub(0, {})", pa_expr(operand)),
        },
        Expr::Call { name, args } => {
            let args_str: Vec<String> = args.iter().map(pa_expr).collect();
            format!("{}({})", name, args_str.join(", "))
        }
        Expr::Ref { .. } => {
            unreachable!("resolver should have rewritten Expr::Ref before emit")
        }
    }
}

/// Renders a literal as it appears *inside* a PA expression (not at a JSON value slot).
/// Strings are single-quoted (with internal quotes doubled); numbers and bools are bare.
fn pa_literal(lit: &Literal) -> String {
    match lit {
        Literal::Null => "null".to_string(),
        Literal::Int(n) => n.to_string(),
        Literal::Float(x) => format_float(*x),
        Literal::Bool(b) => b.to_string(),
        Literal::String(s) => format!("'{}'", s.replace('\'', "''")),
        Literal::Array(_) | Literal::Object(_) => {
            unreachable!("array and object literals are not supported inside PA expressions yet")
        }
    }
}

/// Emits an expression as a bare PA field value (e.g. Foreach's `foreach`
/// field), which uses a single `@` prefix rather than `@{...}`.
fn expr_to_pa_field(expr: &Expr) -> String {
    format!("@{}", pa_expr(expr))
}

fn emit_initialize(name: &str, ty: &Type, value: &Option<Expr>) -> Value {
    let ty_str = match ty {
        Type::Int => "Integer",
        Type::Float => "Float",
        Type::String => "String",
        Type::Bool => "Boolean",
        Type::Array => "Array",
        Type::Object => "Object",
    };
    let mut var_entry = Map::new();
    var_entry.insert("name".to_string(), json!(name));
    var_entry.insert("type".to_string(), json!(ty_str));
    if let Some(v) = value {
        var_entry.insert("value".to_string(), expr_to_json(v));
    }
    json!({
        "type": action::INITIALIZE_VARIABLE,
        "inputs": {
            "variables": [Value::Object(var_entry)]
        }
    })
}

fn literal_to_json(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Int(n) => json!(n),
        Literal::Float(x) => json!(x),
        Literal::String(s) => json!(s),
        Literal::Bool(b) => json!(b),
        Literal::Array(items) => Value::Array(items.iter().map(literal_to_json).collect()),
        Literal::Object(entries) => {
            let mut map = Map::new();
            for (k, v) in entries {
                map.insert(k.clone(), literal_to_json(v));
            }
            Value::Object(map)
        }
    }
}

/// Renders an f64 in a form PA's expression parser accepts: always include
/// at least one fractional digit so the number is unambiguously a float.
/// `1.0` stays `1.0`, `1.5` stays `1.5`, integers-in-float stay `5.0`.
fn format_float(x: f64) -> String {
    let s = format!("{x}");
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}

fn splice_run_after(mut action_body: Value, predecessors: &[RunAfterEntry]) -> Value {
    let mut run_after = Map::new();
    for entry in predecessors {
        run_after.insert(entry.action_name.clone(), json!(entry.statuses));
    }
    if let Value::Object(ref mut map) = action_body {
        map.insert("runAfter".to_string(), Value::Object(run_after));
    }
    action_body
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lexer;
    use crate::parser::parser;
    use crate::resolver::resolve;
    use chumsky::prelude::*;

    fn compile(src: &str) -> Value {
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let program = parser()
            .parse(
                tokens
                    .as_slice()
                    .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .expect("parse failed");
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let resolved = resolve(&program, Some(&fixtures)).expect("resolve failed");
        emit(&resolved)
    }

    #[test]
    fn slice45a_subscript_emits_pa_path_form() {
        // String-key subscript emits `?['key']`; int-key emits `?[0]`.
        // Compose inputs wrap the inner expression in `@{...}`.
        let out = compile(
            r#"
            var data: object = triggerBody()
            let email = data?["body/email"]
            let first = data?[0]
            "#,
        );
        let email = &out["definition"]["actions"]["Compose_email"]["inputs"];
        assert_eq!(email.as_str().unwrap(), "@variables('data')?['body/email']");
        let first = &out["definition"]["actions"]["Compose_first"]["inputs"];
        assert_eq!(first.as_str().unwrap(), "@variables('data')?[0]");
    }

    #[test]
    fn slice45a_subscript_escapes_single_quote_in_key() {
        let out = compile(
            r#"
            var data: object = triggerBody()
            let weird = data?["it's"]
            "#,
        );
        let weird = &out["definition"]["actions"]["Compose_weird"]["inputs"];
        assert_eq!(weird.as_str().unwrap(), "@variables('data')?['it''s']");
    }

    #[test]
    fn slice31_float_var_emits_pa_float_type() {
        let out = compile("var rate: float = 1.5");
        let v = &out["definition"]["actions"]["Initialize_rate"]["inputs"]["variables"][0];
        assert_eq!(v["type"], "Float");
        assert_eq!(v["value"].as_f64().unwrap(), 1.5);
    }

    #[test]
    fn slice31_float_literal_in_pa_expression() {
        // `rate + 0.5` inside an int-style binop slot should render the float
        // literal with a fractional digit so PA's expression parser never
        // interprets it as an int.
        let out = compile("var rate: float = 1.0\nrate += 2.0");
        let v = &out["definition"]["actions"]["Increment_rate"]["inputs"]["value"];
        assert_eq!(v.as_f64().unwrap(), 2.0);
    }

    #[test]
    fn slice31_format_float_keeps_fractional() {
        // Internal check: whole-valued floats keep a `.0` so the PA
        // expression parser never sees them as integer literals.
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(1.5), "1.5");
        assert_eq!(format_float(-3.25), "-3.25");
    }

    #[test]
    fn slice1_emits_initialize_variable() {
        let out = compile("var counter: int = 1");
        let action = &out["definition"]["actions"]["Initialize_counter"];
        assert_eq!(action["type"], "InitializeVariable");
        assert_eq!(action["inputs"]["variables"][0]["name"], "counter");
        assert_eq!(action["inputs"]["variables"][0]["type"], "Integer");
        assert_eq!(action["inputs"]["variables"][0]["value"], 1);
        assert_eq!(action["runAfter"], json!({}));
    }

    #[test]
    fn slice44a_initialize_variable_omits_value_when_no_initializer() {
        let out = compile("var todo: string");
        let action = &out["definition"]["actions"]["Initialize_todo"];
        assert_eq!(action["type"], "InitializeVariable");
        let var_entry = &action["inputs"]["variables"][0];
        assert_eq!(var_entry["name"], "todo");
        assert_eq!(var_entry["type"], "String");
        // Critical: the `value` key must be ABSENT, not present-with-null. PA
        // distinguishes between "no initial value" and "null literal value";
        // emitter must mirror that.
        assert!(
            var_entry.get("value").is_none(),
            "no-initializer var must omit the `value` key entirely; got {var_entry:?}"
        );
    }

    #[test]
    fn slice15_arithmetic_emits_pa_functions() {
        let out = compile("var x: int = 2 + 3");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@add(2, 3)");

        let out = compile("var x: int = 10 - 4");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@sub(10, 4)");

        let out = compile("var x: int = 6 * 7");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@mul(6, 7)");

        let out = compile("var x: int = 20 / 4");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@div(20, 4)");
    }

    #[test]
    fn slice15_precedence_multiplicative_binds_tighter() {
        let out = compile("var x: int = 2 + 3 * 4");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@add(2, mul(3, 4))");
    }

    #[test]
    fn slice15_left_associative() {
        let out = compile("var x: int = 10 - 4 - 1");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@sub(sub(10, 4), 1)");
    }

    #[test]
    fn slice15_concat_binds_looser_than_arithmetic() {
        let out = compile(
            r#"var total: int = 5
var msg: string = "count: " & total + 1"#,
        );
        let v = &out["definition"]["actions"]["Initialize_msg"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "count: @{add(variables('total'), 1)}");
    }

    #[test]
    fn slice16_comparison_ops_emit_pa_functions() {
        let cases = [
            ("<", "less"),
            ("<=", "lessOrEquals"),
            (">", "greater"),
            (">=", "greaterOrEquals"),
            ("==", "equals"),
        ];
        for (op, fn_name) in cases {
            let src = format!("var a: int = 5\nlet check = a {op} 10");
            let out = compile(&src);
            let v = &out["definition"]["actions"]["Compose_check"]["inputs"];
            let expected = format!("@{}(variables('a'), 10)", fn_name);
            assert_eq!(v.as_str().unwrap(), expected, "op: {op}");
        }
    }

    #[test]
    fn slice16_not_equals_synthesized_via_not() {
        let out = compile("var a: int = 5\nlet check = a != 10");
        let v = &out["definition"]["actions"]["Compose_check"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@not(equals(variables('a'), 10))");
    }

    #[test]
    fn slice16_condition_with_comparison_emits_structured_form() {
        let out = compile(
            r#"var a: int = 5
if a > 0 {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond,
            &json!({ "and": [{ "greater": ["@variables('a')", 0] }] })
        );
    }

    #[test]
    fn slice16_condition_with_bare_ref_wraps_in_structured_equals() {
        let out = compile(
            r#"var ok: bool = true
if ok {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond,
            &json!({ "and": [{ "equals": ["@variables('ok')", true] }] })
        );
    }

    #[test]
    fn slice17_logical_and_or_emit_pa_functions() {
        let out = compile("var a: bool = true\nvar b: bool = false\nlet c = a && b");
        let v = &out["definition"]["actions"]["Compose_c"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@and(variables('a'), variables('b'))");

        let out = compile("var a: bool = true\nvar b: bool = false\nlet c = a || b");
        let v = &out["definition"]["actions"]["Compose_c"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@or(variables('a'), variables('b'))");
    }

    #[test]
    fn slice17_logical_precedence_and_tighter_than_or() {
        let out = compile(
            r#"var a: bool = true
var b: bool = true
var c: bool = true
let x = a || b && c"#,
        );
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(
            v.as_str().unwrap(),
            "@or(variables('a'), and(variables('b'), variables('c')))"
        );
    }

    #[test]
    fn slice17_unary_not_emits_not() {
        let out = compile("var a: bool = true\nlet x = !a");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@not(variables('a'))");
    }

    #[test]
    fn slice17_unary_neg_emits_sub_from_zero() {
        let out = compile("var a: int = 5\nlet x = -a");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@sub(0, variables('a'))");
    }

    #[test]
    fn slice17_condition_with_logical_emits_structured_and() {
        let out = compile(
            r#"var a: int = 5
var b: int = 3
if a > 0 && b > 0 {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond,
            &json!({
                "and": [
                    { "greater": ["@variables('a')", 0] },
                    { "greater": ["@variables('b')", 0] },
                ]
            })
        );
    }

    #[test]
    fn slice17_condition_with_not_emits_structured_not() {
        let out = compile(
            r#"var ok: bool = true
if !ok {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond,
            &json!({
                "and": [{ "not": { "equals": ["@variables('ok')", true] } }]
            })
        );
    }

    #[test]
    fn slice18_function_call_emits_pa_function() {
        let out = compile(
            r#"var a: int = 5
let n = length("hello")"#,
        );
        let v = &out["definition"]["actions"]["Compose_n"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@length('hello')");
    }

    #[test]
    fn slice18_call_with_multiple_args_and_exprs() {
        let out = compile(
            r#"var total: int = 10
var completed: int = 7
let msg = concat("done ", completed, " of ", total)"#,
        );
        let v = &out["definition"]["actions"]["Compose_msg"]["inputs"];
        assert_eq!(
            v.as_str().unwrap(),
            "@concat('done ', variables('completed'), ' of ', variables('total'))"
        );
    }

    #[test]
    fn slice18_nested_calls() {
        let out = compile(
            r#"var a: int = 5
let n = length(concat("x", "y"))"#,
        );
        let v = &out["definition"]["actions"]["Compose_n"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@length(concat('x', 'y'))");
    }

    #[test]
    fn slice18_call_can_be_a_condition() {
        let out = compile(
            r#"var items: array = []
if empty(items) {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond,
            &json!({ "and": [{ "empty": ["@variables('items')"] }] })
        );
    }

    #[test]
    fn slice45a_call_with_unknown_return_type_still_wraps() {
        // Calls whose name isn't on the recognized condition-op list still
        // get the conservative `equals(_, true)` wrap so PA gets a real
        // boolean shape.
        let out = compile(
            r#"var items: array = []
if length(items) {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond,
            &json!({
                "and": [{ "equals": ["@length(variables('items'))", true] }]
            })
        );
    }

    #[test]
    fn v1_1_bool_literals_inside_expressions() {
        let out = compile("var flag: bool = true\nlet x = flag == true");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@equals(variables('flag'), true)");

        let out = compile("var flag: bool = true\nlet y = flag == false");
        let v = &out["definition"]["actions"]["Compose_y"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@equals(variables('flag'), false)");
    }

    #[test]
    fn v1_1_null_literal_inside_expression() {
        let out = compile("var results: object = {}\nlet z = results == null");
        let v = &out["definition"]["actions"]["Compose_z"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@equals(variables('results'), null)");
    }

    #[test]
    fn v1_1_chained_unary_not() {
        let out = compile("var a: bool = true\nlet x = !!a");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@not(not(variables('a')))");
    }

    #[test]
    fn v1_1_chained_unary_neg() {
        let out = compile("var n: int = 5\nlet y = --n");
        let v = &out["definition"]["actions"]["Compose_y"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@sub(0, sub(0, variables('n')))");
    }

    #[test]
    fn v1_1_concat_with_literal_on_left() {
        let out = compile(
            r#"var name: string = "world"
let greeting = "hello " & name"#,
        );
        let v = &out["definition"]["actions"]["Compose_greeting"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "hello @{variables('name')}");
    }

    #[test]
    fn v1_1_not_equals_nested_in_logical_and() {
        let out = compile(
            r#"var a: int = 1
var b: int = 2
let x = a != b && a > 0"#,
        );
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(
            v.as_str().unwrap(),
            "@and(not(equals(variables('a'), variables('b'))), greater(variables('a'), 0))"
        );
    }

    #[test]
    fn v1_1_not_equals_nested_in_logical_or() {
        let out = compile(
            r#"var a: int = 1
var b: int = 2
let x = a != b || a == 0"#,
        );
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(
            v.as_str().unwrap(),
            "@or(not(equals(variables('a'), variables('b'))), equals(variables('a'), 0))"
        );
    }

    #[test]
    fn slice14_concat_with_literal_emits_template_form() {
        // Mixed literal + expression chains use PA's designer-style
        // string-template form ("text @{expr}") rather than @concat(...).
        let out = compile(
            r#"var greeting: string = "hello"
var message: string = greeting & ", world""#,
        );
        let msg_value =
            &out["definition"]["actions"]["Initialize_message"]["inputs"]["variables"][0]["value"];
        assert_eq!(
            msg_value.as_str().unwrap(),
            "@{variables('greeting')}, world"
        );
    }

    #[test]
    fn template_form_used_for_three_part_chain_with_literal_separator() {
        // The signoff_list pattern from real PA exports: multiple expression
        // segments separated by literal text. Designer emits this exact form.
        let out = compile(
            r#"var email: string = "a@b.com"
var title: string = "x"
var line: string = email & "|" & title"#,
        );
        let v = &out["definition"]["actions"]["Initialize_line"]["inputs"]["variables"][0]["value"];
        assert_eq!(
            v.as_str().unwrap(),
            "@{variables('email')}|@{variables('title')}"
        );
    }

    #[test]
    fn template_form_skipped_when_all_expressions() {
        // Pure-expression chains (no literal text) keep the bare-`@concat(...)`
        // form -- there's nothing to gain from `@{a}@{b}`, and the concat form
        // matches what designer writes when users build chains without text.
        let out = compile(
            r#"var a: string = "x"
var b: string = "y"
var c: string = a & b"#,
        );
        let v = &out["definition"]["actions"]["Initialize_c"]["inputs"]["variables"][0]["value"];
        assert_eq!(
            v.as_str().unwrap(),
            "@concat(variables('a'), variables('b'))"
        );
    }

    #[test]
    fn template_form_escapes_at_signs_in_literal_segments() {
        // Bare `@` in a literal would be misread as the start of an expression;
        // escape to `@@` so PA renders the literal `@`.
        let out = compile(
            r#"var name: string = "alice"
var line: string = "contact @ " & name"#,
        );
        let v = &out["definition"]["actions"]["Initialize_line"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "contact @@ @{variables('name')}");
    }

    #[test]
    fn slice14_concat_assign_emits_append_to_string() {
        let out = compile(
            r#"var msg: string = "hi"
msg &= "!""#,
        );
        let action = &out["definition"]["actions"]["Append_to_msg"];
        assert_eq!(action["type"], "AppendToStringVariable");
        assert_eq!(action["inputs"]["name"], "msg");
        assert_eq!(action["inputs"]["value"], "!");
    }

    #[test]
    fn slice3_chains_run_after_in_source_order() {
        let out = compile("var a: int = 1\nvar b: int = 2\nvar c: int = 3");
        let actions = &out["definition"]["actions"];
        assert_eq!(actions["Initialize_a"]["runAfter"], json!({}));
        assert_eq!(
            actions["Initialize_b"]["runAfter"],
            json!({ "Initialize_a": ["Succeeded"] })
        );
        assert_eq!(
            actions["Initialize_c"]["runAfter"],
            json!({ "Initialize_b": ["Succeeded"] })
        );
    }

    #[test]
    fn default_manual_trigger_emits_when_no_trigger_file() {
        // No source dir -> no scan -> default manual.
        let out = compile("var x: int = 1");
        let trig = &out["definition"]["triggers"]["manual"];
        assert_eq!(trig["type"], "Request");
        assert_eq!(trig["kind"], "Button");
    }

    #[test]
    fn slice22_terminate_succeeded_emits_minimal_inputs() {
        let out = compile("terminate succeeded");
        let action = &out["definition"]["actions"]["Terminate"];
        assert_eq!(action["type"], "Terminate");
        assert_eq!(action["inputs"]["runStatus"], "Succeeded");
        // No runError on succeeded.
        assert!(action["inputs"].get("runError").is_none());
    }

    #[test]
    fn slice22_terminate_failed_with_literal_message() {
        let out = compile(r#"terminate failed "validation failed""#);
        let action = &out["definition"]["actions"]["Terminate"];
        assert_eq!(action["inputs"]["runStatus"], "Failed");
        assert_eq!(action["inputs"]["runError"]["message"], "validation failed");
    }

    #[test]
    fn slice22_terminate_failed_with_expression_message() {
        // A mixed literal + expression message emits as PA's template form
        // ("failed at @{variables('step')}") rather than @concat(...).
        let out =
            compile("var step: string = \"validate\"\nterminate failed \"failed at \" & step");
        let action = &out["definition"]["actions"]["Terminate"];
        let msg = action["inputs"]["runError"]["message"].as_str().unwrap();
        assert_eq!(msg, "failed at @{variables('step')}");
    }

    #[test]
    fn slice29_until_emits_pa_until_action() {
        let out = compile(
            r#"var n: int = 0
until n > 5 {
  n += 1
}"#,
        );
        let u = &out["definition"]["actions"]["Until"];
        assert_eq!(u["type"], "Until");
        // Comparison op is already boolean -> no equals(_, true) auto-wrap.
        assert_eq!(
            u["expression"].as_str().unwrap(),
            "@greater(variables('n'), 5)"
        );
        // PA defaults: 60 iterations, PT1H.
        assert_eq!(u["limit"]["count"], 60);
        assert_eq!(u["limit"]["timeout"], "PT1H");
        // Body action nested inside Until, not at top level.
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(!actions.contains_key("Increment_n"));
        assert!(u["actions"]["Increment_n"].is_object());
    }

    #[test]
    fn slice34_until_max_overrides_count() {
        let out = compile(
            r#"var n: int = 0
until n > 5 max 10 {
  n += 1
}"#,
        );
        let u = &out["definition"]["actions"]["Until"];
        assert_eq!(u["limit"]["count"], 10);
        assert_eq!(u["limit"]["timeout"], "PT1H", "timeout stays default");
    }

    #[test]
    fn slice34_until_timeout_overrides_timeout() {
        let out = compile(
            r#"var n: int = 0
until n > 5 timeout "PT30M" {
  n += 1
}"#,
        );
        let u = &out["definition"]["actions"]["Until"];
        assert_eq!(u["limit"]["count"], 60, "count stays default");
        assert_eq!(u["limit"]["timeout"], "PT30M");
    }

    #[test]
    fn slice34_until_max_and_timeout_both_override() {
        let out = compile(
            r#"var n: int = 0
until n > 5 max 5 timeout "PT10M" {
  n += 1
}"#,
        );
        let u = &out["definition"]["actions"]["Until"];
        assert_eq!(u["limit"]["count"], 5);
        assert_eq!(u["limit"]["timeout"], "PT10M");
    }

    #[test]
    fn slice29_until_bare_ref_auto_wraps() {
        // A non-boolean expression (bare bool ref) gets the equals(_, true)
        // wrap same as Condition does.
        let out = compile(
            r#"var done: bool = false
until done {
  done = true
}"#,
        );
        let expr = &out["definition"]["actions"]["Until"]["expression"];
        assert_eq!(expr.as_str().unwrap(), "@equals(variables('done'), true)");
    }

    #[test]
    fn slice30_on_handler_emits_scope_with_runafter_status() {
        let out = compile(
            r#"var count: int = 0
scope try_work {
  count = 1
}
on failed try_work {
  count = 99
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(actions.contains_key("Scope_try_work"));
        assert!(actions.contains_key("On_failed_try_work"));
        let handler = &actions["On_failed_try_work"];
        assert_eq!(handler["type"], "Scope");
        assert_eq!(
            handler["runAfter"],
            json!({ "Scope_try_work": ["Failed"] }),
            "handler runs after target on Failed only"
        );
        // Handler body lives nested inside it, not at top level.
        assert!(handler["actions"]["Set_count_1"].is_object());
    }

    #[test]
    fn slice30_succeeded_handler_emits_succeeded_status() {
        let out = compile(
            r#"scope work {
}
on succeeded work {
}"#,
        );
        let handler = &out["definition"]["actions"]["On_succeeded_work"];
        assert_eq!(handler["runAfter"], json!({ "Scope_work": ["Succeeded"] }));
    }

    #[test]
    fn slice30_handler_does_not_join_main_chain() {
        let out = compile(
            r#"var n: int = 0
scope work {
}
on failed work {
}
n = 42"#,
        );
        let actions = &out["definition"]["actions"];
        // The Set_n after the handler must chain back to Scope_work, not to
        // the handler.
        assert_eq!(
            actions["Set_n"]["runAfter"],
            json!({ "Scope_work": ["Succeeded"] })
        );
    }

    #[test]
    fn slice33_on_handler_targets_pa_action_emits_runafter_at_pa_name() {
        let out = compile(
            r#"var n: int = 0
pa HTTP_Call
on failed HTTP_Call {
  n = 1
}
n = 99"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(actions.contains_key("HTTP_Call"));
        assert!(actions.contains_key("On_failed_HTTP_Call"));
        let handler = &actions["On_failed_HTTP_Call"];
        assert_eq!(
            handler["runAfter"],
            json!({ "HTTP_Call": ["Failed"] }),
            "handler runAfter points at the pa action's PA action name"
        );
        // The n = 99 after the handler must chain back to HTTP_Call, not
        // through the handler -- same off-main-chain rule as scope targets.
        // `Set_n` inside the handler body takes the bare name; the outer
        // n = 99 gets suffixed as Set_n_1.
        assert_eq!(
            actions["Set_n_1"]["runAfter"],
            json!({ "HTTP_Call": ["Succeeded"] })
        );
    }

    #[test]
    fn slice32_multi_status_handler_runafter_lists_every_status() {
        let out = compile(
            r#"var count: int = 0
scope try_work {
  count = 1
}
on failed or timedout try_work {
  count = 99
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(
            actions.contains_key("On_failed_timedout_try_work"),
            "action name joins status labels in source order"
        );
        let handler = &actions["On_failed_timedout_try_work"];
        assert_eq!(handler["type"], "Scope");
        assert_eq!(
            handler["runAfter"],
            json!({ "Scope_try_work": ["Failed", "TimedOut"] }),
            "runAfter array carries every status, PA-capitalized, source order"
        );
    }

    #[test]
    fn slice32_multi_status_handler_with_succeeded_still_off_main_chain() {
        // A handler that includes succeeded in its status list must still be
        // off the source-order sibling chain. The action following chains
        // back to the named scope, not the handler.
        let out = compile(
            r#"var n: int = 0
scope work {
}
on succeeded or failed work {
}
n = 42"#,
        );
        let actions = &out["definition"]["actions"];
        assert_eq!(
            actions["Set_n"]["runAfter"],
            json!({ "Scope_work": ["Succeeded"] }),
            "next action chains back to the scope, never through a handler"
        );
    }

    #[test]
    fn slice30_multiple_handlers_on_same_scope() {
        let out = compile(
            r#"scope work {
}
on failed work {
}
on succeeded work {
}
on timedout work {
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(actions.contains_key("On_failed_work"));
        assert!(actions.contains_key("On_succeeded_work"));
        assert!(actions.contains_key("On_timedout_work"));
    }

    #[test]
    fn slice28_scope_emits_pa_scope_action() {
        let out = compile(
            r#"var x: int = 0
scope {
  x = 1
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(actions.contains_key("Scope"));
        let scope = &actions["Scope"];
        assert_eq!(scope["type"], "Scope");
        // Body actions live under the scope, not at workflow top level.
        assert!(!actions.contains_key("Set_x"));
        assert!(scope["actions"].as_object().unwrap().contains_key("Set_x"));
    }

    #[test]
    fn slice28_scope_named_keys_by_name() {
        let out = compile(
            r#"scope try_work {
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(actions.contains_key("Scope_try_work"));
    }

    #[test]
    fn slice28_multiple_scopes_unique_names() {
        let out = compile(
            r#"scope {
}
scope {
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        assert!(actions.contains_key("Scope"));
        assert!(actions.contains_key("Scope_1"));
    }

    #[test]
    fn slice28_scope_chains_sibling_run_after() {
        // A statement after a Scope should chain its runAfter to the Scope,
        // not to anything inside it.
        let out = compile(
            r#"var x: int = 1
scope {
  x = 2
}
x = 99"#,
        );
        let actions = &out["definition"]["actions"];
        assert_eq!(
            actions["Set_x_1"]["runAfter"],
            json!({ "Scope": ["Succeeded"] })
        );
    }

    #[test]
    fn slice27_switch_emits_pa_switch_action() {
        let out = compile(
            r#"var status: string = "active"
switch status {
  case "active" {
  }
  case "pending" {
  }
  default {
  }
}"#,
        );
        let sw = &out["definition"]["actions"]["Switch"];
        assert_eq!(sw["type"], "Switch");
        assert_eq!(sw["expression"].as_str().unwrap(), "@variables('status')");
        let cases = sw["cases"].as_object().unwrap();
        assert!(cases.contains_key("Case"), "first case uses bare Case key");
        assert!(cases.contains_key("Case_1"), "second case uses Case_1");
        assert_eq!(cases["Case"]["case"], "active");
        assert_eq!(cases["Case_1"]["case"], "pending");
        assert!(
            sw["default"]["actions"].is_object(),
            "default block present"
        );
    }

    #[test]
    fn slice27_switch_with_int_case_values() {
        let out = compile(
            r#"var code: int = 1
switch code {
  case 1 {
  }
  case 2 {
  }
}"#,
        );
        let cases = &out["definition"]["actions"]["Switch"]["cases"];
        assert_eq!(cases["Case"]["case"], 1);
        assert_eq!(cases["Case_1"]["case"], 2);
    }

    #[test]
    fn slice27_switch_default_omitted_when_absent() {
        let out = compile(
            r#"var n: int = 1
switch n {
  case 1 {
  }
}"#,
        );
        let sw = &out["definition"]["actions"]["Switch"];
        assert!(
            sw.get("default").is_none(),
            "no source default -> no key emitted"
        );
    }

    #[test]
    fn slice27_switch_case_actions_nest_correctly() {
        // The assignments inside each case should appear in that case's
        // actions map, not at the workflow top level.
        let out = compile(
            r#"var n: int = 0
var tag: string = ""
switch n {
  case 1 {
    tag = "one"
  }
  case 2 {
    tag = "two"
  }
}"#,
        );
        let actions = out["definition"]["actions"].as_object().unwrap();
        // The only top-level actions are the two inits and the Switch itself.
        assert!(actions.contains_key("Initialize_n"));
        assert!(actions.contains_key("Initialize_tag"));
        assert!(actions.contains_key("Switch"));
        assert!(
            !actions.contains_key("Set_tag"),
            "case body action leaked to top level"
        );
        let case1 = &actions["Switch"]["cases"]["Case"]["actions"];
        assert!(case1.as_object().unwrap().contains_key("Set_tag"));
    }

    #[test]
    fn slice22_terminate_cancelled_emits_cancelled_status() {
        let out = compile("terminate cancelled");
        let action = &out["definition"]["actions"]["Terminate"];
        assert_eq!(action["inputs"]["runStatus"], "Cancelled");
        assert!(action["inputs"].get("runError").is_none());
    }
}
