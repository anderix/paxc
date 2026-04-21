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

use crate::ast::{AssignOp, DebugArg, Expr, Literal, Program, Stmt, Trigger, Type};
use crate::lexer::Span;
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
    /// Span of the source statement. paxr uses this to attribute runtime
    /// errors back to the originating statement in diagnostic output.
    pub span: Span,
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
        /// The original user-facing `let` binding name. paxr uses this for
        /// the state dump; the emitter keys actions by `ResolvedAction.name`,
        /// which may differ (e.g. `Compose_remaining_1` when suffixed).
        name: String,
        value: Expr,
    },
    Raw {
        body: Vec<(String, Literal)>,
    },
    Condition {
        condition: Expr,
        /// Source span of the condition, threaded through from the parser
        /// so paxr can render `condition? (source) = true/false` in verbose
        /// traces. The emitter ignores this field.
        condition_span: Span,
        true_branch: Vec<ResolvedAction>,
        false_branch: Vec<ResolvedAction>,
    },
    Foreach {
        collection: Expr,
        /// User-facing iterator name (e.g. `task` from `foreach task in ...`),
        /// used by paxr verbose traces. The emitter keys by `action.name`.
        iter_name: String,
        body: Vec<ResolvedAction>,
    },
    /// `debug(args)` diagnostic. paxc skips these at emit time and counts
    /// them for the end-of-compile note. paxr evaluates them and prints.
    Debug {
        args: Vec<DebugArg>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
enum Binding {
    Var { ty: Type },
    Let { action_name: String },
    Iterator { action_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    DuplicateVariable { name: String, span: Span },
    UndefinedVariable { name: String, span: Span },
    InvalidOperation { op: AssignOp, name: String, ty: Type, span: Span },
    CannotAssignToImmutable { name: String, span: Span },
    /// `var` declarations must be at the top level of the flow. PA's
    /// `InitializeVariable` action is only valid at the workflow scope,
    /// so nesting one inside a Condition or Apply_to_each produces an
    /// invalid definition.
    NestedVarDeclaration { name: String, span: Span },
}

impl ResolveError {
    pub fn span(&self) -> Span {
        match self {
            ResolveError::DuplicateVariable { span, .. }
            | ResolveError::UndefinedVariable { span, .. }
            | ResolveError::InvalidOperation { span, .. }
            | ResolveError::CannotAssignToImmutable { span, .. }
            | ResolveError::NestedVarDeclaration { span, .. } => *span,
        }
    }

    /// Short label to attach to the offending span in ariadne output.
    pub fn label(&self) -> &'static str {
        match self {
            ResolveError::DuplicateVariable { .. } => "already declared",
            ResolveError::UndefinedVariable { .. } => "not defined",
            ResolveError::InvalidOperation { .. } => "invalid operation",
            ResolveError::CannotAssignToImmutable { .. } => "immutable binding",
            ResolveError::NestedVarDeclaration { .. } => "must be top-level",
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::DuplicateVariable { name, .. } => {
                write!(f, "`{name}` is already declared")
            }
            ResolveError::UndefinedVariable { name, .. } => {
                write!(f, "`{name}` is not defined")
            }
            ResolveError::InvalidOperation { op, name, ty, .. } => {
                let op_str = match op {
                    AssignOp::Set => "=",
                    AssignOp::Add => "+=",
                    AssignOp::Subtract => "-=",
                    AssignOp::Concat => "&=",
                };
                let ty_str = type_name(ty);
                write!(
                    f,
                    "cannot apply `{op_str}` to variable `{name}` of type `{ty_str}`"
                )
            }
            ResolveError::CannotAssignToImmutable { name, .. } => {
                write!(
                    f,
                    "cannot assign to `{name}`: `let` bindings are immutable"
                )
            }
            ResolveError::NestedVarDeclaration { name, .. } => {
                write!(
                    f,
                    "`var {name}` must be declared at the top level of the flow"
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
    let mut env: HashMap<String, Binding> = HashMap::new();
    let mut name_counts: HashMap<String, u32> = HashMap::new();
    let actions = resolve_statements(&program.statements, &mut env, &mut name_counts, true)?;
    Ok(ResolvedProgram {
        trigger: program.trigger.clone(),
        actions,
    })
}

/// `top_level` controls whether `var` declarations are permitted. PA requires
/// `InitializeVariable` actions at workflow scope, so nested `var` decls must
/// be rejected at compile time. `let` / assign / raw are fine at any depth.
fn resolve_statements(
    statements: &[Stmt],
    env: &mut HashMap<String, Binding>,
    name_counts: &mut HashMap<String, u32>,
    top_level: bool,
) -> Result<Vec<ResolvedAction>, ResolveError> {
    let mut actions: Vec<ResolvedAction> = Vec::with_capacity(statements.len());
    let mut prev_name: Option<String> = None;

    for stmt in statements {
        let (action_name, kind, stmt_span) = match stmt {
            Stmt::VarDecl { name, name_span, ty, value } => {
                if !top_level {
                    return Err(ResolveError::NestedVarDeclaration {
                        name: name.clone(),
                        span: *name_span,
                    });
                }
                if env.contains_key(name) {
                    return Err(ResolveError::DuplicateVariable {
                        name: name.clone(),
                        span: *name_span,
                    });
                }
                let value = resolve_expr(value, env)?;
                env.insert(name.clone(), Binding::Var { ty: ty.clone() });
                let action_name =
                    unique_name(&format!("Initialize_{name}"), name_counts);
                let kind = ActionKind::InitializeVariable {
                    var: name.clone(),
                    ty: ty.clone(),
                    value,
                };
                (action_name, kind, *name_span)
            }
            Stmt::Let { name, name_span, value } => {
                if env.contains_key(name) {
                    return Err(ResolveError::DuplicateVariable {
                        name: name.clone(),
                        span: *name_span,
                    });
                }
                let value = resolve_expr(value, env)?;
                let action_name = unique_name(&format!("Compose_{name}"), name_counts);
                env.insert(
                    name.clone(),
                    Binding::Let {
                        action_name: action_name.clone(),
                    },
                );
                (
                    action_name,
                    ActionKind::Compose {
                        name: name.clone(),
                        value,
                    },
                    *name_span,
                )
            }
            Stmt::Assign { name, name_span, op, value } => {
                let value = resolve_expr(value, env)?;
                match env.get(name) {
                    Some(Binding::Var { ty }) => {
                        let ty = ty.clone();
                        let (base, kind) = lower_assign(name, *name_span, *op, &ty, value)?;
                        (unique_name(&base, name_counts), kind, *name_span)
                    }
                    Some(Binding::Let { .. }) | Some(Binding::Iterator { .. }) => {
                        return Err(ResolveError::CannotAssignToImmutable {
                            name: name.clone(),
                            span: *name_span,
                        });
                    }
                    None => {
                        return Err(ResolveError::UndefinedVariable {
                            name: name.clone(),
                            span: *name_span,
                        });
                    }
                }
            }
            Stmt::Raw { name, body, span } => {
                let action_name = unique_name(name, name_counts);
                (action_name, ActionKind::Raw { body: body.clone() }, *span)
            }
            Stmt::If {
                condition,
                condition_span,
                true_branch,
                false_branch,
            } => {
                let condition = resolve_expr(condition, env)?;
                let action_name = unique_name("Condition", name_counts);
                // Branches scope `let` bindings to themselves: save the env,
                // resolve each branch against it, restore after. name_counts
                // stays shared since PA action names are globally unique.
                let saved_env = env.clone();
                let true_actions =
                    resolve_statements(true_branch, env, name_counts, false)?;
                *env = saved_env.clone();
                let false_actions =
                    resolve_statements(false_branch, env, name_counts, false)?;
                *env = saved_env;
                (
                    action_name,
                    ActionKind::Condition {
                        condition,
                        condition_span: *condition_span,
                        true_branch: true_actions,
                        false_branch: false_actions,
                    },
                    *condition_span,
                )
            }
            Stmt::Foreach {
                iter,
                collection,
                body,
                span,
            } => {
                let collection = resolve_expr(collection, env)?;
                let action_name = unique_name("Apply_to_each", name_counts);
                // Iterator and any body-local `let` are scoped to the loop body.
                let saved_env = env.clone();
                env.insert(
                    iter.clone(),
                    Binding::Iterator {
                        action_name: action_name.clone(),
                    },
                );
                let body_actions =
                    resolve_statements(body, env, name_counts, false)?;
                *env = saved_env;
                (
                    action_name,
                    ActionKind::Foreach {
                        collection,
                        iter_name: iter.clone(),
                        body: body_actions,
                    },
                    *span,
                )
            }
            Stmt::Debug { args, span } => {
                let mut resolved_args = Vec::with_capacity(args.len());
                for arg in args {
                    let expr = resolve_expr(&arg.expr, env)?;
                    resolved_args.push(DebugArg { expr, span: arg.span });
                }
                (
                    String::new(),
                    ActionKind::Debug {
                        args: resolved_args,
                        span: *span,
                    },
                    *span,
                )
            }
        };

        // Debug actions are diagnostic-only: paxc drops them, and real
        // actions after a debug must chain runAfter back to the last real
        // action, so we skip updating prev_name here.
        if matches!(&kind, ActionKind::Debug { .. }) {
            actions.push(ResolvedAction {
                name: action_name,
                run_after: Vec::new(),
                kind,
                span: stmt_span,
            });
            continue;
        }

        let run_after = match &prev_name {
            Some(n) => vec![n.clone()],
            None => Vec::new(),
        };
        prev_name = Some(action_name.clone());
        actions.push(ResolvedAction {
            name: action_name,
            run_after,
            kind,
            span: stmt_span,
        });
    }

    Ok(actions)
}

fn lower_assign(
    name: &str,
    name_span: Span,
    op: AssignOp,
    ty: &Type,
    value: Expr,
) -> Result<(String, ActionKind), ResolveError> {
    let invalid = || ResolveError::InvalidOperation {
        op,
        name: name.to_string(),
        ty: ty.clone(),
        span: name_span,
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
        AssignOp::Concat => match ty {
            Type::String => Ok((
                format!("Append_to_{name}"),
                ActionKind::AppendToStringVariable {
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
        Expr::Ref { name, span } => match env.get(name) {
            Some(Binding::Var { .. }) => Ok(Expr::VarRef(name.clone())),
            Some(Binding::Let { action_name }) => Ok(Expr::ComposeRef(action_name.clone())),
            Some(Binding::Iterator { action_name }) => {
                Ok(Expr::IteratorRef(action_name.clone()))
            }
            None => Err(ResolveError::UndefinedVariable {
                name: name.clone(),
                span: *span,
            }),
        },
        Expr::Member { target, field } => {
            let resolved_target = resolve_expr(target, env)?;
            Ok(Expr::Member {
                target: Box::new(resolved_target),
                field: field.clone(),
            })
        }
        Expr::VarRef(_) | Expr::ComposeRef(_) | Expr::IteratorRef(_) => Ok(expr.clone()),
        Expr::BinaryOp { op, lhs, rhs } => {
            let lhs = resolve_expr(lhs, env)?;
            let rhs = resolve_expr(rhs, env)?;
            Ok(Expr::BinaryOp {
                op: *op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        Expr::UnaryOp { op, operand } => {
            let operand = resolve_expr(operand, env)?;
            Ok(Expr::UnaryOp {
                op: *op,
                operand: Box::new(operand),
            })
        }
        Expr::Call { name, args } => {
            let resolved_args: Result<Vec<_>, _> =
                args.iter().map(|a| resolve_expr(a, env)).collect();
            Ok(Expr::Call {
                name: name.clone(),
                args: resolved_args?,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Literal, Trigger, Type};

    fn sp() -> Span {
        (0..0).into()
    }

    fn rref(name: &str) -> Expr {
        Expr::Ref {
            name: name.to_string(),
            span: sp(),
        }
    }

    fn var(name: &str) -> Stmt {
        Stmt::VarDecl {
            name: name.to_string(),
            name_span: sp(),
            ty: Type::Int,
            value: Expr::Literal(Literal::Int(0)),
        }
    }

    fn var_ty(name: &str, ty: Type) -> Stmt {
        Stmt::VarDecl {
            name: name.to_string(),
            name_span: sp(),
            ty,
            value: Expr::Literal(Literal::Int(0)),
        }
    }

    fn assign(name: &str, op: AssignOp, value: Expr) -> Stmt {
        Stmt::Assign {
            name: name.to_string(),
            name_span: sp(),
            op,
            value,
        }
    }

    fn let_stmt(name: &str, value: Expr) -> Stmt {
        Stmt::Let {
            name: name.to_string(),
            name_span: sp(),
            value,
        }
    }

    fn debug(args: Vec<Expr>) -> Stmt {
        Stmt::Debug {
            args: args
                .into_iter()
                .map(|expr| DebugArg {
                    expr,
                    span: (0..0).into(),
                })
                .collect(),
            span: (0..0).into(),
        }
    }

    #[test]
    fn slice20_debug_does_not_chain_run_after() {
        // var a; debug(); var b --  b.runAfter must point at a, not at debug.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![var("a"), debug(vec![]), var("b")],
        };
        let resolved = resolve(&prog).expect("resolve should succeed");
        assert_eq!(resolved.actions.len(), 3);
        assert_eq!(resolved.actions[0].name, "Initialize_a");
        assert!(matches!(resolved.actions[1].kind, ActionKind::Debug { .. }));
        assert!(
            resolved.actions[1].run_after.is_empty(),
            "debug action should have empty runAfter"
        );
        assert_eq!(resolved.actions[2].name, "Initialize_b");
        assert_eq!(
            resolved.actions[2].run_after,
            vec!["Initialize_a"],
            "action after debug must chain back to prior real action"
        );
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
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::DuplicateVariable { name, .. } if name == "x"
        ));
    }

    #[test]
    fn accepts_valid_reference() {
        let ref_y = Stmt::VarDecl {
            name: "y".to_string(),
            name_span: sp(),
            ty: Type::Int,
            value: rref("x"),
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
            name_span: sp(),
            ty: Type::Int,
            value: rref("nope"),
        };
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![ref_y],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "nope"
        ));
    }

    #[test]
    fn rejects_forward_reference() {
        let ref_y = Stmt::VarDecl {
            name: "y".to_string(),
            name_span: sp(),
            ty: Type::Int,
            value: rref("x"),
        };
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![ref_y, var("x")],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "x"
        ));
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
    fn add_rejects_string_bool_and_object() {
        for ty in [Type::String, Type::Bool, Type::Object] {
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
    fn concat_assign_dispatches_to_string_append() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("msg", Type::String),
                assign(
                    "msg",
                    AssignOp::Concat,
                    Expr::Literal(Literal::String("!".to_string())),
                ),
            ],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[1].name, "Append_to_msg");
        assert!(matches!(
            resolved.actions[1].kind,
            ActionKind::AppendToStringVariable { .. }
        ));
    }

    #[test]
    fn concat_assign_rejects_non_string() {
        for ty in [Type::Int, Type::Bool, Type::Array, Type::Object] {
            let prog = Program {
                trigger: Trigger::Manual,
                statements: vec![
                    var_ty("x", ty.clone()),
                    assign(
                        "x",
                        AssignOp::Concat,
                        Expr::Literal(Literal::String("".to_string())),
                    ),
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
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "nope"
        ));
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
                    name_span: sp(),
                    ty: Type::Int,
                    value: rref("total"),
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
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::CannotAssignToImmutable { name, .. } if name == "immut"
        ));
    }

    #[test]
    fn nested_var_decl_in_if_branch_is_error() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("flag", Type::Bool),
                Stmt::If {
                    condition: rref("flag"),
                    condition_span: sp(),
                    true_branch: vec![var("inner")],
                    false_branch: vec![],
                },
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::NestedVarDeclaration { name, .. } if name == "inner"
        ));
    }

    #[test]
    fn nested_var_decl_in_foreach_body_is_error() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("items", Type::Array),
                Stmt::Foreach {
                    iter: "x".to_string(),
                    collection: rref("items"),
                    body: vec![var("inner")],
                    span: sp(),
                },
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::NestedVarDeclaration { name, .. } if name == "inner"
        ));
    }

    #[test]
    fn let_in_if_branch_does_not_leak_to_outer_scope() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("flag", Type::Bool),
                Stmt::If {
                    condition: rref("flag"),
                    condition_span: sp(),
                    true_branch: vec![let_stmt(
                        "inner",
                        Expr::Literal(Literal::Int(1)),
                    )],
                    false_branch: vec![],
                },
                // Reference `inner` from outer scope: should fail, because
                // the `let` was scoped to the true branch.
                let_stmt("leak", rref("inner")),
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "inner"
        ));
    }

    #[test]
    fn let_in_true_branch_does_not_leak_to_false_branch() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("flag", Type::Bool),
                Stmt::If {
                    condition: rref("flag"),
                    condition_span: sp(),
                    true_branch: vec![let_stmt(
                        "inner",
                        Expr::Literal(Literal::Int(1)),
                    )],
                    false_branch: vec![let_stmt(
                        "copy",
                        rref("inner"),
                    )],
                },
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "inner"
        ));
    }

    #[test]
    fn let_in_foreach_body_does_not_leak_to_outer_scope() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("items", Type::Array),
                Stmt::Foreach {
                    iter: "x".to_string(),
                    collection: rref("items"),
                    body: vec![let_stmt(
                        "per_item",
                        Expr::Literal(Literal::Int(1)),
                    )],
                    span: sp(),
                },
                let_stmt("leak", rref("per_item")),
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "per_item"
        ));
    }

    #[test]
    fn nested_let_can_still_reference_outer_vars() {
        // Regression guard: scoping a nested `let` must not break its ability
        // to see outer-scope declarations.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("outer", Type::Int),
                Stmt::If {
                    condition: Expr::Literal(Literal::Bool(true)),
                    condition_span: sp(),
                    true_branch: vec![let_stmt(
                        "copy",
                        rref("outer"),
                    )],
                    false_branch: vec![],
                },
            ],
        };
        assert!(resolve(&prog).is_ok());
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
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::DuplicateVariable { name, .. } if name == "x"
        ));
    }
}
