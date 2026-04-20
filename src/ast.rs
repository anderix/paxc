//! Abstract syntax tree for pax.
//!
//! Slice 1 covers only what's needed for a single `var` declaration with an
//! integer literal. Additional variants (more types, expressions, control
//! flow) are added slice by slice.

#[derive(Debug, Clone)]
pub struct Program {
    pub statements: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    VarDecl {
        name: String,
        ty: Type,
        value: Expr,
    },
}

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone)]
pub enum Literal {
    Int(i64),
    String(String),
    Bool(bool),
    Array(Vec<Literal>),
    Object(Vec<(String, Literal)>),
}
