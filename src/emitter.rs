//! Emitter: turns a resolved pax program into a Power Automate flow definition.
//!
//! The emitter walks a `ResolvedProgram` and produces the standard
//! `Microsoft.Logic/workflows` outer shape with a manual-button trigger and
//! an `actions` map. Action keys and `runAfter` predecessors come from the
//! resolver, so this layer is purely a tree-to-JSON translation.

use crate::ast::{Expr, Literal, Stmt, Type};
use crate::resolver::{ResolvedAction, ResolvedProgram};
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
    let body = match &action.stmt {
        Stmt::VarDecl { name, ty, value } => emit_var_decl(name, ty, value),
    };
    splice_run_after(body, &action.run_after)
}

fn emit_var_decl(name: &str, ty: &Type, value: &Expr) -> Value {
    let ty_str = match ty {
        Type::Int => "Integer",
        Type::String => "String",
        Type::Bool => "Boolean",
    };
    let value_json = match value {
        Expr::Literal(Literal::Int(n)) => json!(n),
        Expr::Literal(Literal::String(s)) => json!(s),
        Expr::Literal(Literal::Bool(b)) => json!(b),
    };
    json!({
        "type": "InitializeVariable",
        "inputs": {
            "variables": [
                { "name": name, "type": ty_str, "value": value_json }
            ]
        }
    })
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
