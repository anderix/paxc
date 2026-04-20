//! Emitter: turns a resolved pax program into a Power Automate flow definition.
//!
//! The emitter walks a `ResolvedProgram` and produces the standard
//! `Microsoft.Logic/workflows` outer shape with a manual-button trigger and
//! an `actions` map. Action keys and `runAfter` predecessors come from the
//! resolver, so this layer is purely a tree-to-JSON translation.

use crate::ast::{BinOp, Expr, Literal, Trigger, Type};
use crate::resolver::{ActionKind, ResolvedAction, ResolvedProgram};
use serde_json::{Map, Value, json};

const SCHEMA_URL: &str =
    "https://schema.management.azure.com/providers/Microsoft.Logic/schemas/2016-06-01/workflowdefinition.json#";

pub fn emit(program: &ResolvedProgram) -> Value {
    json!({
        "definition": {
            "$schema": SCHEMA_URL,
            "contentVersion": "1.0.0.0",
            "triggers": emit_trigger(&program.trigger),
            "actions": build_actions_map(&program.actions),
        }
    })
}

fn build_actions_map(actions: &[ResolvedAction]) -> Value {
    let mut map = Map::new();
    for action in actions {
        map.insert(action.name.clone(), emit_action(action));
    }
    Value::Object(map)
}

fn emit_trigger(trigger: &Trigger) -> Value {
    match trigger {
        Trigger::Manual => json!({
            "manual": {
                "type": "Request",
                "kind": "Button",
                "inputs": {}
            }
        }),
    }
}

fn emit_action(action: &ResolvedAction) -> Value {
    let body = match &action.kind {
        ActionKind::InitializeVariable { var, ty, value } => emit_initialize(var, ty, value),
        ActionKind::SetVariable { var, value } => emit_mutation("SetVariable", var, value),
        ActionKind::IncrementVariable { var, value } => {
            emit_mutation("IncrementVariable", var, value)
        }
        ActionKind::DecrementVariable { var, value } => {
            emit_mutation("DecrementVariable", var, value)
        }
        ActionKind::AppendToStringVariable { var, value } => {
            emit_mutation("AppendToStringVariable", var, value)
        }
        ActionKind::AppendToArrayVariable { var, value } => {
            emit_mutation("AppendToArrayVariable", var, value)
        }
        ActionKind::Compose { value } => emit_compose(value),
        ActionKind::Raw { body } => emit_raw(body),
        ActionKind::Condition {
            condition,
            true_branch,
            false_branch,
        } => emit_condition(condition, true_branch, false_branch),
        ActionKind::Foreach { collection, body } => emit_foreach(collection, body),
    };
    splice_run_after(body, &action.run_after)
}

fn emit_foreach(collection: &Expr, body: &[ResolvedAction]) -> Value {
    json!({
        "type": "Foreach",
        "foreach": expr_to_pa_field(collection),
        "actions": build_actions_map(body),
    })
}

fn emit_condition(
    condition: &Expr,
    true_branch: &[ResolvedAction],
    false_branch: &[ResolvedAction],
) -> Value {
    let expression = format!("@equals({}, true)", pa_expr(condition));
    json!({
        "type": "If",
        "expression": expression,
        "actions": build_actions_map(true_branch),
        "else": {
            "actions": build_actions_map(false_branch),
        }
    })
}

fn emit_compose(value: &Expr) -> Value {
    json!({
        "type": "Compose",
        "inputs": expr_to_json(value),
    })
}

fn emit_raw(body: &[(String, Literal)]) -> Value {
    let mut map = Map::new();
    for (k, v) in body {
        map.insert(k.clone(), literal_to_json(v));
    }
    Value::Object(map)
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
        _ => json!(format!("@{{{}}}", pa_expr(value))),
    }
}

/// Builds the inner text of a PA expression (everything that goes inside `@{...}`).
fn pa_expr(expr: &Expr) -> String {
    match expr {
        Expr::VarRef(name) => format!("variables('{name}')"),
        Expr::ComposeRef(action_name) => format!("outputs('{action_name}')"),
        Expr::IteratorRef(action_name) => format!("items('{action_name}')"),
        Expr::Member { target, field } => format!("{}?['{}']", pa_expr(target), field),
        Expr::Literal(lit) => pa_literal(lit),
        Expr::BinaryOp { op, lhs, rhs } => {
            let fn_name = match op {
                BinOp::Concat => "concat",
                BinOp::Add => "add",
                BinOp::Sub => "sub",
                BinOp::Mul => "mul",
                BinOp::Div => "div",
            };
            format!("{}({}, {})", fn_name, pa_expr(lhs), pa_expr(rhs))
        }
        Expr::Ref(_) => {
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

fn emit_initialize(name: &str, ty: &Type, value: &Expr) -> Value {
    let ty_str = match ty {
        Type::Int => "Integer",
        Type::String => "String",
        Type::Bool => "Boolean",
        Type::Array => "Array",
        Type::Object => "Object",
    };
    let value_json = expr_to_json(value);
    json!({
        "type": "InitializeVariable",
        "inputs": {
            "variables": [
                { "name": name, "type": ty_str, "value": value_json }
            ]
        }
    })
}

fn literal_to_json(lit: &Literal) -> Value {
    match lit {
        Literal::Null => Value::Null,
        Literal::Int(n) => json!(n),
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

fn splice_run_after(mut action_body: Value, predecessors: &[String]) -> Value {
    let mut run_after = Map::new();
    for pred in predecessors {
        run_after.insert(pred.clone(), json!(["Succeeded"]));
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
        let src = format!("trigger manual\n{src}");
        let tokens = lexer().parse(src.as_str()).into_result().expect("lex failed");
        let program = parser()
            .parse(
                tokens
                    .as_slice()
                    .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .expect("parse failed");
        let resolved = resolve(&program).expect("resolve failed");
        emit(&resolved)
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
    fn slice15_arithmetic_emits_pa_functions() {
        let out = compile("var x: int = 2 + 3");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@{add(2, 3)}");

        let out = compile("var x: int = 10 - 4");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@{sub(10, 4)}");

        let out = compile("var x: int = 6 * 7");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@{mul(6, 7)}");

        let out = compile("var x: int = 20 / 4");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@{div(20, 4)}");
    }

    #[test]
    fn slice15_precedence_multiplicative_binds_tighter() {
        let out = compile("var x: int = 2 + 3 * 4");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@{add(2, mul(3, 4))}");
    }

    #[test]
    fn slice15_left_associative() {
        let out = compile("var x: int = 10 - 4 - 1");
        let v = &out["definition"]["actions"]["Initialize_x"]["inputs"]["variables"][0]["value"];
        assert_eq!(v.as_str().unwrap(), "@{sub(sub(10, 4), 1)}");
    }

    #[test]
    fn slice15_concat_binds_looser_than_arithmetic() {
        let out = compile(
            r#"var total: int = 5
var msg: string = "count: " & total + 1"#,
        );
        let v = &out["definition"]["actions"]["Initialize_msg"]["inputs"]["variables"][0]["value"];
        assert_eq!(
            v.as_str().unwrap(),
            "@{concat('count: ', add(variables('total'), 1))}"
        );
    }

    #[test]
    fn slice14_concat_emits_concat_function() {
        let out = compile(
            r#"var greeting: string = "hello"
var message: string = greeting & ", world""#,
        );
        let msg_value = &out["definition"]["actions"]["Initialize_message"]["inputs"]
            ["variables"][0]["value"];
        assert_eq!(
            msg_value.as_str().unwrap(),
            "@{concat(variables('greeting'), ', world')}"
        );
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
}
