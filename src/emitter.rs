//! Emitter: turns a resolved pax program into a Power Automate flow definition.
//!
//! The emitter walks a `ResolvedProgram` and produces the standard
//! `Microsoft.Logic/workflows` outer shape with a manual-button trigger and
//! an `actions` map. Action keys and `runAfter` predecessors come from the
//! resolver, so this layer is purely a tree-to-JSON translation.

use crate::ast::{BinOp, Expr, Literal, Trigger, Type, UnaryOp};
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
            _ => {}
        }
    }
    n
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
        ActionKind::Compose { value, .. } => emit_compose(value),
        ActionKind::Raw { body } => emit_raw(body),
        ActionKind::Condition {
            condition,
            true_branch,
            false_branch,
        } => emit_condition(condition, true_branch, false_branch),
        ActionKind::Foreach { collection, body } => emit_foreach(collection, body),
        ActionKind::Debug { .. } => {
            // Filtered out in `build_actions_map` before reaching here.
            unreachable!("debug action reached emitter");
        }
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
    // Skip the `equals(_, true)` auto-wrap when the condition is already a
    // boolean-producing expression (a comparison op). Bare refs, member access,
    // etc. still need the wrap so PA sees a boolean.
    let expression = if is_boolean_expr(condition) {
        format!("@{}", pa_expr(condition))
    } else {
        format!("@equals({}, true)", pa_expr(condition))
    };
    json!({
        "type": "If",
        "expression": expression,
        "actions": build_actions_map(true_branch),
        "else": {
            "actions": build_actions_map(false_branch),
        }
    })
}

fn is_boolean_expr(expr: &Expr) -> bool {
    match expr {
        Expr::BinaryOp { op, .. } => op.is_boolean(),
        Expr::UnaryOp { op: UnaryOp::Not, .. } => true,
        _ => false,
    }
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
        Expr::Member { target, field } => {
            format!("{}?['{}']", pa_expr(target), field.replace('\'', "''"))
        }
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
            let expected = format!("@{{{}(variables('a'), 10)}}", fn_name);
            assert_eq!(v.as_str().unwrap(), expected, "op: {op}");
        }
    }

    #[test]
    fn slice16_not_equals_synthesized_via_not() {
        let out = compile("var a: int = 5\nlet check = a != 10");
        let v = &out["definition"]["actions"]["Compose_check"]["inputs"];
        assert_eq!(
            v.as_str().unwrap(),
            "@{not(equals(variables('a'), 10))}"
        );
    }

    #[test]
    fn slice16_condition_with_comparison_skips_autowrap() {
        let out = compile(
            r#"var a: int = 5
if a > 0 {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(cond.as_str().unwrap(), "@greater(variables('a'), 0)");
    }

    #[test]
    fn slice16_condition_with_bare_ref_still_autowraps() {
        let out = compile(
            r#"var ok: bool = true
if ok {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(cond.as_str().unwrap(), "@equals(variables('ok'), true)");
    }

    #[test]
    fn slice17_logical_and_or_emit_pa_functions() {
        let out = compile("var a: bool = true\nvar b: bool = false\nlet c = a && b");
        let v = &out["definition"]["actions"]["Compose_c"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{and(variables('a'), variables('b'))}");

        let out = compile("var a: bool = true\nvar b: bool = false\nlet c = a || b");
        let v = &out["definition"]["actions"]["Compose_c"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{or(variables('a'), variables('b'))}");
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
            "@{or(variables('a'), and(variables('b'), variables('c')))}"
        );
    }

    #[test]
    fn slice17_unary_not_emits_not() {
        let out = compile("var a: bool = true\nlet x = !a");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{not(variables('a'))}");
    }

    #[test]
    fn slice17_unary_neg_emits_sub_from_zero() {
        let out = compile("var a: int = 5\nlet x = -a");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{sub(0, variables('a'))}");
    }

    #[test]
    fn slice17_condition_with_logical_skips_autowrap() {
        let out = compile(
            r#"var a: int = 5
var b: int = 3
if a > 0 && b > 0 {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond.as_str().unwrap(),
            "@and(greater(variables('a'), 0), greater(variables('b'), 0))"
        );
    }

    #[test]
    fn slice17_condition_with_not_skips_autowrap() {
        let out = compile(
            r#"var ok: bool = true
if !ok {
}"#,
        );
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(cond.as_str().unwrap(), "@not(variables('ok'))");
    }

    #[test]
    fn slice18_function_call_emits_pa_function() {
        let out = compile(
            r#"var a: int = 5
let n = length("hello")"#,
        );
        let v = &out["definition"]["actions"]["Compose_n"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{length('hello')}");
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
            "@{concat('done ', variables('completed'), ' of ', variables('total'))}"
        );
    }

    #[test]
    fn slice18_nested_calls() {
        let out = compile(
            r#"var a: int = 5
let n = length(concat("x", "y"))"#,
        );
        let v = &out["definition"]["actions"]["Compose_n"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{length(concat('x', 'y'))}");
    }

    #[test]
    fn slice18_call_can_be_a_condition() {
        let out = compile(
            r#"var items: array = []
if empty(items) {
}"#,
        );
        // Bare call result — we don't know its type, so auto-wrap kicks in.
        let cond = &out["definition"]["actions"]["Condition"]["expression"];
        assert_eq!(
            cond.as_str().unwrap(),
            "@equals(empty(variables('items')), true)"
        );
    }

    #[test]
    fn v1_1_bool_literals_inside_expressions() {
        let out = compile("var flag: bool = true\nlet x = flag == true");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{equals(variables('flag'), true)}");

        let out = compile("var flag: bool = true\nlet y = flag == false");
        let v = &out["definition"]["actions"]["Compose_y"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{equals(variables('flag'), false)}");
    }

    #[test]
    fn v1_1_null_literal_inside_expression() {
        let out = compile("var results: object = {}\nlet z = results == null");
        let v = &out["definition"]["actions"]["Compose_z"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{equals(variables('results'), null)}");
    }

    #[test]
    fn v1_1_chained_unary_not() {
        let out = compile("var a: bool = true\nlet x = !!a");
        let v = &out["definition"]["actions"]["Compose_x"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{not(not(variables('a')))}");
    }

    #[test]
    fn v1_1_chained_unary_neg() {
        let out = compile("var n: int = 5\nlet y = --n");
        let v = &out["definition"]["actions"]["Compose_y"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{sub(0, sub(0, variables('n')))}");
    }

    #[test]
    fn v1_1_concat_with_literal_on_left() {
        let out = compile(r#"var name: string = "world"
let greeting = "hello " & name"#);
        let v = &out["definition"]["actions"]["Compose_greeting"]["inputs"];
        assert_eq!(v.as_str().unwrap(), "@{concat('hello ', variables('name'))}");
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
            "@{and(not(equals(variables('a'), variables('b'))), greater(variables('a'), 0))}"
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
            "@{or(not(equals(variables('a'), variables('b'))), equals(variables('a'), 0))}"
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
