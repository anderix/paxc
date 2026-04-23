//! Abstract syntax tree for pax.
//!
//! Slice 1 covers only what's needed for a single `var` declaration with an
//! integer literal. Additional variants (more types, expressions, control
//! flow) are added slice by slice.

use crate::lexer::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub trigger: Trigger,
    pub statements: Vec<Stmt>,
}

/// One argument to a `debug(...)` statement. Carries the expression itself
/// plus the source span so paxr can show the arg in `name=value` form using
/// the literal source slice as the name.
#[derive(Debug, Clone)]
pub struct DebugArg {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Manual,
    Schedule { frequency: Frequency, interval: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
}

impl Frequency {
    /// PA's canonical capitalization for the `frequency` field value.
    pub fn as_pa_str(self) -> &'static str {
        match self {
            Frequency::Second => "Second",
            Frequency::Minute => "Minute",
            Frequency::Hour => "Hour",
            Frequency::Day => "Day",
            Frequency::Week => "Week",
            Frequency::Month => "Month",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    VarDecl {
        name: String,
        /// Span of the declared name identifier, used by diagnostics.
        name_span: Span,
        ty: Type,
        value: Expr,
    },
    Assign {
        name: String,
        /// Span of the assigned-to name identifier, used by diagnostics.
        name_span: Span,
        op: AssignOp,
        value: Expr,
    },
    Raw {
        name: String,
        body: Vec<(String, Literal)>,
        /// Span of the whole statement, used for runtime-error localization
        /// when paxr executes the resolved action.
        span: Span,
    },
    Let {
        name: String,
        /// Span of the bound name identifier, used by diagnostics.
        name_span: Span,
        value: Expr,
    },
    If {
        condition: Expr,
        /// Source span of the condition expression -- used by paxr in
        /// verbose mode to render `condition? (source) = true/false` traces.
        condition_span: Span,
        true_branch: Vec<Stmt>,
        false_branch: Vec<Stmt>,
    },
    Foreach {
        iter: String,
        collection: Expr,
        body: Vec<Stmt>,
        /// Span of the whole statement, used for runtime-error localization.
        span: Span,
    },
    /// `debug(args)` diagnostic. paxc drops these with an end-of-compile note;
    /// paxr evaluates them and prints `debug: <source>=value at line X`. The
    /// span covers the whole statement so paxr can recover the line number.
    Debug {
        args: Vec<DebugArg>,
        span: Span,
    },
    /// `terminate <status> [message]` early-exit. `message` is only valid when
    /// status is `Failed` (parser-enforced). Compiles to PA's `Terminate`
    /// action; paxr halts execution on reaching it.
    Terminate {
        status: TerminateStatus,
        message: Option<Expr>,
        span: Span,
    },
    /// `on <status> [or <status>]* <target> { body }` -- error-path (or
    /// success-path) handler attached to a named scope. Compiles to a PA
    /// Scope action with a `runAfter` pointing at the target under each of
    /// the listed statuses. The handler does NOT become part of the
    /// source-order sibling chain; statements following the handler chain
    /// their runAfter back to the last real action before any handlers,
    /// like `debug()` does.
    OnHandler {
        /// One or more handler statuses, in source order. Parser guarantees
        /// the vector is non-empty. Duplicates are rejected by the resolver.
        statuses: Vec<HandlerStatus>,
        target: String,
        /// Span of the target identifier, used by the resolver's diagnostic
        /// when the target is unknown.
        target_span: Span,
        body: Vec<Stmt>,
        /// Span of the whole statement, used for runtime-error localization.
        span: Span,
    },
    /// `until <condition> [max N] [timeout "PT30M"] { body }` -- PA's Until
    /// (do-while) loop. The condition is the EXIT condition: PA runs the body
    /// first, evaluates the expression, and exits when it becomes true. When
    /// `max` or `timeout` are omitted, paxc emits PA's defaults (60 iterations,
    /// PT1H timeout). `max` must be a positive integer literal; `timeout` must
    /// be a string literal holding an ISO 8601 duration.
    Until {
        condition: Expr,
        /// Source span of the condition expression, mirrors `If::condition_span`
        /// so paxr's verbose trace can show the source slice.
        condition_span: Span,
        /// Optional `max N` iteration-count override, stored as the raw i64
        /// from the int literal. Resolver validates range and promotes to u32.
        /// None means "use PA's default count".
        limit_count: Option<i64>,
        /// Source span of the `max N` clause (the whole `max 5` text), for
        /// diagnostics when the count is out of range.
        limit_count_span: Option<Span>,
        /// Optional `timeout "PT30M"` override. String literal content only;
        /// paxc does not interpret ISO 8601 here -- PA validates it at import.
        /// None means "use PA's default timeout".
        limit_timeout: Option<String>,
        body: Vec<Stmt>,
        /// Span of the whole statement, used for runtime-error localization.
        span: Span,
    },
    /// `scope [<name>] { ... }` -- a no-op container action that groups
    /// statements. Lowers to PA's Scope action. Body follows the same
    /// nested-statement rules as if/foreach bodies (no nested `var`).
    /// An unnamed scope lowers to a PA action keyed `Scope` (auto-suffixed
    /// if repeated); a named scope keys `Scope_<name>`.
    Scope {
        /// Optional source label; the resolver uses this in the PA action key.
        name: Option<String>,
        body: Vec<Stmt>,
        /// Span of the whole statement, used for runtime-error localization.
        span: Span,
    },
    /// `switch <subject> { case <literal> { ... } ... default { ... } }`.
    /// Lowers to PA's Switch action. Case values are scalar literals only
    /// (string / int / bool), matching PA's constraint -- no arbitrary
    /// expressions in the case clause. `default` is `None` when the source
    /// omitted the default arm entirely, `Some(vec![])` when the source
    /// wrote an explicitly empty `default { }` block -- PA emits the default
    /// key based on source intent.
    Switch {
        subject: Expr,
        /// Span of the subject expression, used by paxr verbose traces.
        subject_span: Span,
        cases: Vec<SwitchCase>,
        default: Option<Vec<Stmt>>,
        /// Span of the whole statement, used for runtime-error localization.
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct SwitchCase {
    pub value: Literal,
    pub body: Vec<Stmt>,
    /// Span of the case keyword + value, used for diagnostics.
    pub span: Span,
}

/// Status filter for an `on` handler. Maps to PA's runAfter status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerStatus {
    Succeeded,
    Failed,
    Skipped,
    TimedOut,
}

impl HandlerStatus {
    /// PA-canonical capitalization for the runAfter status array.
    pub fn as_pa_str(self) -> &'static str {
        match self {
            HandlerStatus::Succeeded => "Succeeded",
            HandlerStatus::Failed => "Failed",
            HandlerStatus::Skipped => "Skipped",
            HandlerStatus::TimedOut => "TimedOut",
        }
    }

    /// Lowercase label used in paxr trace / notice output.
    pub fn as_label(self) -> &'static str {
        match self {
            HandlerStatus::Succeeded => "succeeded",
            HandlerStatus::Failed => "failed",
            HandlerStatus::Skipped => "skipped",
            HandlerStatus::TimedOut => "timedout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateStatus {
    Succeeded,
    Failed,
    Cancelled,
}

impl TerminateStatus {
    /// PA's canonical capitalization for the `runStatus` field.
    pub fn as_pa_str(self) -> &'static str {
        match self {
            TerminateStatus::Succeeded => "Succeeded",
            TerminateStatus::Failed => "Failed",
            TerminateStatus::Cancelled => "Cancelled",
        }
    }

    /// Lowercase label used in paxr trace output.
    pub fn as_label(self) -> &'static str {
        match self {
            TerminateStatus::Succeeded => "succeeded",
            TerminateStatus::Failed => "failed",
            TerminateStatus::Cancelled => "cancelled",
        }
    }
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
    Float,
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
    Ref { name: String, span: Span },
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
    Float(f64),
    String(String),
    Bool(bool),
    Array(Vec<Literal>),
    Object(Vec<(String, Literal)>),
}
