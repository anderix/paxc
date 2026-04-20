//! Name resolution and runAfter graph construction.
//!
//! The resolver is the pass between parser and emitter. It assigns each
//! statement a Power Automate action key (uniqued by suffix when needed),
//! links actions by source order (so the emitter can set `runAfter`),
//! tracks each binding in an environment, and lowers each AST statement
//! into a concrete `ActionKind` the emitter can render directly.
//!
//! Expression resolution happens here: every `Expr::Ref(name)` emitted by
//! the parser is rewritten to either `Expr::VarRef` (pointing at a pax
//! variable) or `Expr::ComposeRef` (pointing at a `let` binding's Compose
//! action key), so the emitter never sees an unresolved reference.

use crate::ast::{AssignOp, Expr, Literal, Program, Stmt, Trigger, Type};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone)]
pub struct ResolvedProgram {
    pub trigger: Trigger,
    pub actions: Vec<ResolvedAction>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub name: String,
    pub run_after: Vec<String>,
    pub kind: ActionKind,
}

#[derive(Debug, Clone)]
pub enum ActionKind {
    InitializeVariable {
        var: String,
        ty: Type,
        value: Expr,
    },
    SetVariable {
        var: String,
        value: Expr,
    },
    IncrementVariable {
        var: String,
        value: Expr,
    },
    DecrementVariable {
        var: String,
        value: Expr,
    },
    AppendToStringVariable {
        var: String,
        value: Expr,
    },
    AppendToArrayVariable {
        var: String,
        value: Expr,
    },
    Compose {
        value: Expr,
    },
    Raw {
        body: Vec<(String, Literal)>,
    },
}

#[derive(Debug, Clone)]
enum Binding {
    Var { ty: Type },
    Let { action_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    DuplicateVariable { name: String },
    UndefinedVariable { name: String },
    InvalidOperation { op: AssignOp, name: String, ty: Type },
    CannotAssignToImmutable { name: String },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::DuplicateVariable { name } => {
                write!(f, "`{name}` is already declared")
            }
            ResolveError::UndefinedVariable { name } => {
                write!(f, "`{name}` is not defined")
            }
            ResolveError::InvalidOperation { op, name, ty } => {
                let op_str = match op {
                    AssignOp::Set => "=",
                    AssignOp::Add => "+=",
                    AssignOp::Subtract => "-=",
                };
                let ty_str = type_name(ty);
                write!(
                    f,
                    "cannot apply `{op_str}` to variable `{name}` of type `{ty_str}`"
                )
            }
            ResolveError::CannotAssignToImmutable { name } => {
                write!(
                    f,
                    "cannot assign to `{name}`: `let` bindings are immutable"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

fn type_name(ty: &Type) -> &'static str {
    match ty {
        Type::Int => "int",
        Type::String => "string",
        Type::Bool => "bool",
        Type::Array => "array",
        Type::Object => "object",
    }
}

pub fn resolve(program: &Program) -> Result<ResolvedProgram, ResolveError> {
    let mut actions: Vec<ResolvedAction> = Vec::with_capacity(program.statements.len());
    let mut env: HashMap<String, Binding> = HashMap::new();
    let mut name_counts: HashMap<String, u32> = HashMap::new();
    let mut prev_name: Option<String> = None;

    for stmt in &program.statements {
        let (action_name, kind) = match stmt {
            Stmt::VarDecl { name, ty, value } => {
                let value = resolve_expr(value, &env)?;
                if env.contains_key(name) {
                    return Err(ResolveError::DuplicateVariable { name: name.clone() });
                }
                env.insert(name.clone(), Binding::Var { ty: ty.clone() });
                let action_name =
                    unique_name(&format!("Initialize_{name}"), &mut name_counts);
                let kind = ActionKind::InitializeVariable {
                    var: name.clone(),
                    ty: ty.clone(),
                    value,
                };
                (action_name, kind)
            }
            Stmt::Let { name, value } => {
                let value = resolve_expr(value, &env)?;
                if env.contains_key(name) {
                    return Err(ResolveError::DuplicateVariable { name: name.clone() });
                }
                let action_name = unique_name(&format!("Compose_{name}"), &mut name_counts);
                env.insert(
                    name.clone(),
                    Binding::Let {
                        action_name: action_name.clone(),
                    },
                );
                (action_name, ActionKind::Compose { value })
            }
            Stmt::Assign { name, op, value } => {
                let value = resolve_expr(value, &env)?;
                match env.get(name) {
                    Some(Binding::Var { ty }) => {
                        let ty = ty.clone();
                        let (base, kind) = lower_assign(name, *op, &ty, value)?;
                        (unique_name(&base, &mut name_counts), kind)
                    }
                    Some(Binding::Let { .. }) => {
                        return Err(ResolveError::CannotAssignToImmutable {
                            name: name.clone(),
                        });
                    }
                    None => {
                        return Err(ResolveError::UndefinedVariable { name: name.clone() });
                    }
                }
            }
            Stmt::Raw { name, body } => {
                let action_name = unique_name(name, &mut name_counts);
                (action_name, ActionKind::Raw { body: body.clone() })
            }
        };

        let run_after = match &prev_name {
            Some(n) => vec![n.clone()],
            None => Vec::new(),
        };
        prev_name = Some(action_name.clone());
        actions.push(ResolvedAction {
            name: action_name,
            run_after,
            kind,
        });
    }

    Ok(ResolvedProgram {
        trigger: program.trigger.clone(),
        actions,
    })
}

fn lower_assign(
    name: &str,
    op: AssignOp,
    ty: &Type,
    value: Expr,
) -> Result<(String, ActionKind), ResolveError> {
    let invalid = || ResolveError::InvalidOperation {
        op,
        name: name.to_string(),
        ty: ty.clone(),
    };
    match op {
        AssignOp::Set => Ok((
            format!("Set_{name}"),
            ActionKind::SetVariable {
                var: name.to_string(),
                value,
            },
        )),
        AssignOp::Add => match ty {
            Type::Int => Ok((
                format!("Increment_{name}"),
                ActionKind::IncrementVariable {
                    var: name.to_string(),
                    value,
                },
            )),
            Type::String => Ok((
                format!("Append_to_{name}"),
                ActionKind::AppendToStringVariable {
                    var: name.to_string(),
                    value,
                },
            )),
            Type::Array => Ok((
                format!("Append_to_{name}"),
                ActionKind::AppendToArrayVariable {
                    var: name.to_string(),
                    value,
                },
            )),
            _ => Err(invalid()),
        },
        AssignOp::Subtract => match ty {
            Type::Int => Ok((
                format!("Decrement_{name}"),
                ActionKind::DecrementVariable {
                    var: name.to_string(),
                    value,
                },
            )),
            _ => Err(invalid()),
        },
    }
}

/// Assigns a unique action name using PA's convention: first occurrence bare,
/// subsequent occurrences suffixed `_1`, `_2`, ...
fn unique_name(base: &str, counts: &mut HashMap<String, u32>) -> String {
    let count = counts.entry(base.to_string()).or_insert(0);
    let name = if *count == 0 {
        base.to_string()
    } else {
        format!("{base}_{count}")
    };
    *count += 1;
    name
}

fn resolve_expr(expr: &Expr, env: &HashMap<String, Binding>) -> Result<Expr, ResolveError> {
    match expr {
        Expr::Literal(l) => Ok(Expr::Literal(l.clone())),
        Expr::Ref(name) => match env.get(name) {
            Some(Binding::Var { .. }) => Ok(Expr::VarRef(name.clone())),
            Some(Binding::Let { action_name }) => Ok(Expr::ComposeRef(action_name.clone())),
            None => Err(ResolveError::UndefinedVariable { name: name.clone() }),
        },
        Expr::VarRef(_) | Expr::ComposeRef(_) => Ok(expr.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Literal, Trigger, Type};

    fn var(name: &str) -> Stmt {
        Stmt::VarDecl {
            name: name.to_string(),
            ty: Type::Int,
            value: Expr::Literal(Literal::Int(0)),
        }
    }

    fn var_ty(name: &str, ty: Type) -> Stmt {
        Stmt::VarDecl {
            name: name.to_string(),
            ty,
            value: Expr::Literal(Literal::Int(0)),
        }
    }

    fn assign(name: &str, op: AssignOp, value: Expr) -> Stmt {
        Stmt::Assign {
            name: name.to_string(),
            op,
            value,
        }
    }

    fn let_stmt(name: &str, value: Expr) -> Stmt {
        Stmt::Let {
            name: name.to_string(),
            value,
        }
    }

    #[test]
    fn chains_in_source_order() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![var("a"), var("b"), var("c")],
        };
        let resolved = resolve(&prog).expect("resolve should succeed");
        assert_eq!(resolved.actions.len(), 3);
        assert_eq!(resolved.actions[0].name, "Initialize_a");
        assert!(resolved.actions[0].run_after.is_empty());
        assert_eq!(resolved.actions[1].name, "Initialize_b");
        assert_eq!(resolved.actions[1].run_after, vec!["Initialize_a"]);
        assert_eq!(resolved.actions[2].name, "Initialize_c");
        assert_eq!(resolved.actions[2].run_after, vec!["Initialize_b"]);
    }

    #[test]
    fn rejects_duplicate_variable() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![var("x"), var("x")],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::DuplicateVariable {
                name: "x".to_string()
            }
        );
    }

    #[test]
    fn accepts_valid_reference() {
        let ref_y = Stmt::VarDecl {
            name: "y".to_string(),
            ty: Type::Int,
            value: Expr::Ref("x".to_string()),
        };
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![var("x"), ref_y],
        };
        assert!(resolve(&prog).is_ok());
    }

    #[test]
    fn rejects_undefined_reference() {
        let ref_y = Stmt::VarDecl {
            name: "y".to_string(),
            ty: Type::Int,
            value: Expr::Ref("nope".to_string()),
        };
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![ref_y],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable {
                name: "nope".to_string()
            }
        );
    }

    #[test]
    fn rejects_forward_reference() {
        let ref_y = Stmt::VarDecl {
            name: "y".to_string(),
            ty: Type::Int,
            value: Expr::Ref("x".to_string()),
        };
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![ref_y, var("x")],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable {
                name: "x".to_string()
            }
        );
    }

    #[test]
    fn set_on_any_type() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("x", Type::Bool),
                assign("x", AssignOp::Set, Expr::Literal(Literal::Bool(true))),
            ],
        };
        let resolved = resolve(&prog).unwrap();
        assert!(matches!(
            resolved.actions[1].kind,
            ActionKind::SetVariable { .. }
        ));
        assert_eq!(resolved.actions[1].name, "Set_x");
    }

    #[test]
    fn add_dispatches_by_type() {
        let cases = [
            (Type::Int, "Increment_x"),
            (Type::String, "Append_to_x"),
            (Type::Array, "Append_to_x"),
        ];
        for (ty, expected_name) in cases {
            let prog = Program {
                trigger: Trigger::Manual,
                statements: vec![
                    var_ty("x", ty.clone()),
                    assign("x", AssignOp::Add, Expr::Literal(Literal::Int(1))),
                ],
            };
            let resolved = resolve(&prog).unwrap();
            assert_eq!(resolved.actions[1].name, expected_name);
        }
    }

    #[test]
    fn add_rejects_bool_and_object() {
        for ty in [Type::Bool, Type::Object] {
            let prog = Program {
                trigger: Trigger::Manual,
                statements: vec![
                    var_ty("x", ty.clone()),
                    assign("x", AssignOp::Add, Expr::Literal(Literal::Int(1))),
                ],
            };
            assert!(matches!(
                resolve(&prog).unwrap_err(),
                ResolveError::InvalidOperation { .. }
            ));
        }
    }

    #[test]
    fn subtract_only_on_int() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("x", Type::String),
                assign("x", AssignOp::Subtract, Expr::Literal(Literal::Int(1))),
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::InvalidOperation { .. }
        ));
    }

    #[test]
    fn assign_to_undefined_is_error() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![assign("nope", AssignOp::Set, Expr::Literal(Literal::Int(1)))],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable {
                name: "nope".to_string()
            }
        );
    }

    #[test]
    fn auto_suffix_follows_zero_indexed_convention() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("x", Type::Int),
                assign("x", AssignOp::Add, Expr::Literal(Literal::Int(1))),
                assign("x", AssignOp::Add, Expr::Literal(Literal::Int(1))),
                assign("x", AssignOp::Add, Expr::Literal(Literal::Int(1))),
            ],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[1].name, "Increment_x");
        assert_eq!(resolved.actions[2].name, "Increment_x_1");
        assert_eq!(resolved.actions[3].name, "Increment_x_2");
    }

    #[test]
    fn let_becomes_compose_action() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![let_stmt("doubled", Expr::Literal(Literal::Int(42)))],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[0].name, "Compose_doubled");
        assert!(matches!(resolved.actions[0].kind, ActionKind::Compose { .. }));
    }

    #[test]
    fn let_is_referenced_as_compose_output() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                let_stmt("total", Expr::Literal(Literal::Int(10))),
                Stmt::VarDecl {
                    name: "mirror".to_string(),
                    ty: Type::Int,
                    value: Expr::Ref("total".to_string()),
                },
            ],
        };
        let resolved = resolve(&prog).unwrap();
        let var_action = &resolved.actions[1];
        match &var_action.kind {
            ActionKind::InitializeVariable { value, .. } => {
                assert!(matches!(value, Expr::ComposeRef(s) if s == "Compose_total"));
            }
            _ => panic!("expected InitializeVariable"),
        }
    }

    #[test]
    fn cannot_assign_to_let() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                let_stmt("immut", Expr::Literal(Literal::Int(1))),
                assign("immut", AssignOp::Set, Expr::Literal(Literal::Int(2))),
            ],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::CannotAssignToImmutable {
                name: "immut".to_string()
            }
        );
    }

    #[test]
    fn var_and_let_share_namespace() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var("x"),
                let_stmt("x", Expr::Literal(Literal::Int(1))),
            ],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::DuplicateVariable {
                name: "x".to_string()
            }
        );
    }
}
