//! Name resolution and runAfter graph construction.
//!
//! The resolver is the pass between parser and emitter. It assigns each
//! statement a Power Automate action key, links actions by source order
//! (so the emitter can set `runAfter`), and detects duplicate declarations
//! that would produce conflicting action keys.

use crate::ast::{Program, Stmt};
use std::collections::HashSet;
use std::fmt;

#[derive(Debug, Clone)]
pub struct ResolvedProgram {
    pub actions: Vec<ResolvedAction>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAction {
    pub name: String,
    pub run_after: Vec<String>,
    pub stmt: Stmt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    DuplicateVariable { name: String },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::DuplicateVariable { name } => {
                write!(f, "variable `{name}` declared more than once")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

pub fn resolve(program: &Program) -> Result<ResolvedProgram, ResolveError> {
    let mut actions: Vec<ResolvedAction> = Vec::with_capacity(program.statements.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut prev_name: Option<String> = None;

    for stmt in &program.statements {
        match stmt {
            Stmt::VarDecl { name, .. } => {
                if !seen.insert(name.clone()) {
                    return Err(ResolveError::DuplicateVariable { name: name.clone() });
                }
                let action_name = format!("Initialize_{name}");
                let run_after = match &prev_name {
                    Some(n) => vec![n.clone()],
                    None => Vec::new(),
                };
                prev_name = Some(action_name.clone());
                actions.push(ResolvedAction {
                    name: action_name,
                    run_after,
                    stmt: stmt.clone(),
                });
            }
        }
    }

    Ok(ResolvedProgram { actions })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Literal, Type};

    fn var(name: &str) -> Stmt {
        Stmt::VarDecl {
            name: name.to_string(),
            ty: Type::Int,
            value: Expr::Literal(Literal::Int(0)),
        }
    }

    #[test]
    fn chains_in_source_order() {
        let prog = Program {
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
            statements: vec![var("x"), var("x")],
        };
        assert_eq!(
            resolve(&prog).unwrap_err(),
            ResolveError::DuplicateVariable {
                name: "x".to_string()
            }
        );
    }
}
