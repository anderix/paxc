//! Emitter: turns a resolved pax program into a Power Automate flow definition.
//!
//! The emitter walks a `ResolvedProgram` and produces the standard
//! `Microsoft.Logic/workflows` outer shape with a manual-button trigger and
//! an `actions` map. Action keys and `runAfter` predecessors come from the
//! resolver, so this layer is purely a tree-to-JSON translation.

use crate::ast::{Expr, Literal, Type};
use crate::resolver::{ActionKind, ResolvedAction, ResolvedProgram};
use serde_json::{Map, Value, json};

const SCHEMA_URL: &str =
    "https://schema.management.azure.com/providers/Microsoft.Logic/schemas/2016-06-01/workflowdefinition.json#";

pub fn emit(program: &ResolvedProgram) -> Value {
    let mut actions = Map::new();
    for action in &program.actions {
        actions.insert(action.name.clone(), emit_action(action));
    }

    json!({
        "definition": {
            "$schema": SCHEMA_URL,
            "contentVersion": "1.0.0.0",
            "triggers": {
                "manual": {
                    "type": "Request",
                    "kind": "Button",
                    "inputs": {}
                }
            },
            "actions": Value::Object(actions),
        }
    })
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
    };
    splice_run_after(body, &action.run_after)
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
        Expr::Ref(var_name) => json!(format!("@{{variables('{var_name}')}}")),
    }
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
        let tokens = lexer().parse(src).into_result().expect("lex failed");
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
