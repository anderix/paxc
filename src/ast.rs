//! Abstract syntax tree for pax.
//!
//! Slice 1 covers only what's needed for a single `var` declaration with an
//! integer literal. Additional variants (more types, expressions, control
//! flow) are added slice by slice.

#[derive(Debug, Clone)]
pub struct Program {
    pub trigger: Trigger,
    pub statements: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Manual,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    VarDecl {
        name: String,
        ty: Type,
        value: Expr,
    },
    Assign {
        name: String,
        op: AssignOp,
        value: Expr,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Subtract,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    String,
    Bool,
    Array,
    Object,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Literal),
    Ref(String),
}

#[derive(Debug, Clone)]
pub enum Literal {
    Int(i64),
    String(String),
    Bool(bool),
    Array(Vec<Literal>),
    Object(Vec<(String, Literal)>),
}
