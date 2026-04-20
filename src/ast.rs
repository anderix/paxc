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
    Raw {
        name: String,
        body: Vec<(String, Literal)>,
    },
    Let {
        name: String,
        value: Expr,
    },
    If {
        condition: Expr,
        true_branch: Vec<Stmt>,
        false_branch: Vec<Stmt>,
    },
    Foreach {
        iter: String,
        collection: Expr,
        body: Vec<Stmt>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Set,
    Add,
    Subtract,
    Concat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Equals,
    NotEquals,
    And,
    Or,
}

impl BinOp {
    /// True when this operator's result is a boolean (comparisons + logical).
    pub fn is_boolean(self) -> bool {
        matches!(
            self,
            BinOp::Less
                | BinOp::LessEq
                | BinOp::Greater
                | BinOp::GreaterEq
                | BinOp::Equals
                | BinOp::NotEquals
                | BinOp::And
                | BinOp::Or
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
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
    /// Unresolved identifier reference emitted by the parser. The resolver
    /// rewrites each occurrence into either `VarRef` or `ComposeRef`.
    Ref(String),
    /// Reference to a pax variable. Emits `@{variables('x')}`.
    VarRef(String),
    /// Reference to a `let` binding. The payload is the Compose action key
    /// the resolver assigned to it. Emits `@{outputs('Compose_x')}`.
    ComposeRef(String),
    /// Member access `target.field`. Chains via nested Member nodes.
    Member {
        target: Box<Expr>,
        field: String,
    },
    /// Reference to a foreach iterator. Payload is the `Apply_to_each` action
    /// key the iterator belongs to. Emits `items('action_name')`.
    IteratorRef(String),
    /// Binary operator expression. Emits as a PA function call, e.g. `&` → `concat(lhs, rhs)`.
    BinaryOp {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Unary operator expression. `!x` → `not(x)`; `-x` → `sub(0, x)`.
    UnaryOp {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    /// PA expression function call: `name(args...)`. The name is passed
    /// through unchecked -- any valid PA function or user-defined one works.
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone)]
pub enum Literal {
    Null,
    Int(i64),
    String(String),
    Bool(bool),
    Array(Vec<Literal>),
    Object(Vec<(String, Literal)>),
}
