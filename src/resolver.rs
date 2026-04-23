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

use crate::ast::{
    AssignOp, DebugArg, Expr, HandlerStatus, Literal, Program, Stmt, TerminateStatus, Trigger, Type,
};
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
    pub run_after: Vec<RunAfterEntry>,
    pub kind: ActionKind,
    /// Span of the source statement. paxr uses this to attribute runtime
    /// errors back to the originating statement in diagnostic output.
    pub span: Span,
}

/// One predecessor in PA's `runAfter` map: an action name plus the statuses
/// under which it triggers this action. Most entries are built via
/// [`RunAfterEntry::succeeded`] (the source-order sibling chain); error-path
/// handlers (`on failed foo { ... }`) use other statuses.
#[derive(Debug, Clone)]
pub struct RunAfterEntry {
    pub action_name: String,
    /// PA-capitalized statuses: `Succeeded`, `Failed`, `Skipped`, `TimedOut`.
    pub statuses: Vec<String>,
}

impl RunAfterEntry {
    /// The default sibling-chain edge: "this action runs after <name> has
    /// succeeded." Used by the source-order runAfter chain.
    pub fn succeeded(action_name: String) -> Self {
        Self {
            action_name,
            statuses: vec!["Succeeded".to_string()],
        }
    }
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
    /// `terminate <status> [message]`. Compiles to a PA Terminate action;
    /// paxr halts execution on reaching one. Message is only present when
    /// status is Failed (parser-enforced).
    Terminate {
        status: TerminateStatus,
        message: Option<Expr>,
    },
    /// `scope [name] { body }` -- a PA Scope action. Pure container; the
    /// interpreter walks the body like any other block. Only the name form
    /// varies the action key.
    Scope {
        body: Vec<ResolvedAction>,
    },
    /// `until <condition> { body }` -- PA's Until loop. The condition is the
    /// exit condition; body runs at least once. Emitter uses PA's default
    /// limit (60 iterations, PT1H timeout); paxr caps iterations at 60 too,
    /// producing a notice and stopping cleanly to mirror PA's cap behavior.
    Until {
        condition: Expr,
        /// Source span of the condition expression, mirrors Condition's span.
        condition_span: Span,
        body: Vec<ResolvedAction>,
    },
    /// `on <status> [or <status>]* <target> { body }` -- handler attached to
    /// a named scope (or, in the future, any named action). Compiles to a PA
    /// Scope action whose `runAfter` points at the target with one or more
    /// statuses. The handler does not participate in the source-order sibling
    /// chain: the resolver skips updating `prev_name` when emitting it, so
    /// the next real statement chains back to whatever came before.
    OnHandler {
        /// One or more handler statuses, in source order, guaranteed unique
        /// by the resolver's duplicate check.
        statuses: Vec<HandlerStatus>,
        /// PA action name of the target (e.g. `Scope_try_work`), already
        /// resolved from the source-level label (`try_work`).
        target_action_name: String,
        body: Vec<ResolvedAction>,
    },
    /// `switch subject { case L { ... } ... default { ... } }`. Each case
    /// has a literal value and a resolved body; `default` is `None` when the
    /// source omitted the default arm.
    Switch {
        subject: Expr,
        /// Source span of the subject expression, threaded through for paxr's
        /// verbose trace (parallel to Condition's `condition_span`).
        subject_span: Span,
        cases: Vec<ResolvedSwitchCase>,
        default: Option<Vec<ResolvedAction>>,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedSwitchCase {
    /// PA action key for this case (e.g. `Case`, `Case_1`). Lives inside the
    /// parent Switch's `cases` map; name counts are shared with the top-level
    /// counter for simplicity, so these are globally unique too.
    pub action_name: String,
    pub value: Literal,
    pub body: Vec<ResolvedAction>,
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
    /// `on <status> <target> { ... }` named a target that no named scope
    /// resolves to. Future versions may allow raw blocks or other named
    /// actions as targets too; for now only `scope <name>` registers.
    UnknownHandlerTarget { name: String, span: Span },
    /// Multi-status handler `on a or b or ... <target>` listed the same
    /// status twice. Redundant but usually a typo, so reject rather than
    /// silently dedup.
    DuplicateHandlerStatus { status: HandlerStatus, span: Span },
}

impl ResolveError {
    pub fn span(&self) -> Span {
        match self {
            ResolveError::DuplicateVariable { span, .. }
            | ResolveError::UndefinedVariable { span, .. }
            | ResolveError::InvalidOperation { span, .. }
            | ResolveError::CannotAssignToImmutable { span, .. }
            | ResolveError::NestedVarDeclaration { span, .. }
            | ResolveError::UnknownHandlerTarget { span, .. }
            | ResolveError::DuplicateHandlerStatus { span, .. } => *span,
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
            ResolveError::UnknownHandlerTarget { .. } => "not a named scope",
            ResolveError::DuplicateHandlerStatus { .. } => "duplicate status",
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
            ResolveError::UnknownHandlerTarget { name, .. } => {
                write!(
                    f,
                    "`{name}` is not a named scope; `on` handlers attach to `scope <name> {{ ... }}`"
                )
            }
            ResolveError::DuplicateHandlerStatus { status, .. } => {
                write!(
                    f,
                    "status `{}` appears more than once in this handler",
                    status.as_label()
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

fn type_name(ty: &Type) -> &'static str {
    match ty {
        Type::Int => "int",
        Type::Float => "float",
        Type::String => "string",
        Type::Bool => "bool",
        Type::Array => "array",
        Type::Object => "object",
    }
}

pub fn resolve(program: &Program) -> Result<ResolvedProgram, ResolveError> {
    let mut env: HashMap<String, Binding> = HashMap::new();
    let mut name_counts: HashMap<String, u32> = HashMap::new();
    // Source name -> PA action name for every user-labeled scope. Populated
    // as we resolve each `scope <name> { ... }`; consumed when an `on` handler
    // references a target by name.
    let mut named_scopes: HashMap<String, String> = HashMap::new();
    let actions = resolve_statements(
        &program.statements,
        &mut env,
        &mut name_counts,
        &mut named_scopes,
        true,
    )?;
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
    named_scopes: &mut HashMap<String, String>,
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
                // Branches scope `let` bindings and named-scope registrations
                // to themselves: save both, resolve each branch against them,
                // restore after. name_counts stays shared since PA action
                // names are globally unique. Rolling back named_scopes here
                // matters because a `scope foo { }` inside a branch creates
                // a PA action nested in the Condition -- letting `foo` leak
                // to outer resolution would allow an `on failed foo` at top
                // level to emit a runAfter targeting an action that isn't
                // at workflow root, which PA rejects on import.
                let saved_env = env.clone();
                let saved_named_scopes = named_scopes.clone();
                let true_actions =
                    resolve_statements(true_branch, env, name_counts, named_scopes, false)?;
                *env = saved_env.clone();
                *named_scopes = saved_named_scopes.clone();
                let false_actions =
                    resolve_statements(false_branch, env, name_counts, named_scopes, false)?;
                *env = saved_env;
                *named_scopes = saved_named_scopes;
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
                // Same named_scopes rollback rationale as the Condition arm.
                let saved_env = env.clone();
                let saved_named_scopes = named_scopes.clone();
                env.insert(
                    iter.clone(),
                    Binding::Iterator {
                        action_name: action_name.clone(),
                    },
                );
                let body_actions =
                    resolve_statements(body, env, name_counts, named_scopes, false)?;
                *env = saved_env;
                *named_scopes = saved_named_scopes;
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
            Stmt::Until {
                condition,
                condition_span,
                body,
                span,
            } => {
                let condition = resolve_expr(condition, env)?;
                let action_name = unique_name("Until", name_counts);
                // Body scopes lets and named-scope registrations to itself,
                // same as foreach/if-branches.
                let saved_env = env.clone();
                let saved_named_scopes = named_scopes.clone();
                let body_actions = resolve_statements(body, env, name_counts, named_scopes, false)?;
                *env = saved_env;
                *named_scopes = saved_named_scopes;
                (
                    action_name,
                    ActionKind::Until {
                        condition,
                        condition_span: *condition_span,
                        body: body_actions,
                    },
                    *span,
                )
            }
            Stmt::Scope {
                name: scope_name,
                body,
                span,
            } => {
                let base = match scope_name {
                    Some(n) => format!("Scope_{n}"),
                    None => "Scope".to_string(),
                };
                let action_name = unique_name(&base, name_counts);
                // Register named scopes so `on <status> <name>` handlers can
                // resolve the source label to a PA action name. Duplicate
                // source names silently overwrite (first one wins would be
                // odd -- PA's own action names stay unique via suffixing, so
                // a handler attaches to the most recent declaration). Not
                // ideal; may revisit with a duplicate-name diagnostic.
                if let Some(n) = scope_name {
                    named_scopes.insert(n.clone(), action_name.clone());
                }
                // Scope body scopes its own `let` bindings and inner named-
                // scope registrations like if/foreach. The scope name we
                // just registered was added BEFORE the clone, so it stays
                // visible to siblings after this scope returns -- only names
                // registered *inside* the body get rolled back.
                let saved_env = env.clone();
                let saved_named_scopes = named_scopes.clone();
                let body_actions = resolve_statements(body, env, name_counts, named_scopes, false)?;
                *env = saved_env;
                *named_scopes = saved_named_scopes;
                (action_name, ActionKind::Scope { body: body_actions }, *span)
            }
            Stmt::OnHandler {
                statuses,
                target,
                target_span,
                body,
                span,
            } => {
                // Parser guarantees `statuses` is non-empty. Reject repeats
                // up front: they usually mean a typo like `failed or failed`.
                let mut seen: Vec<HandlerStatus> = Vec::with_capacity(statuses.len());
                for s in statuses {
                    if seen.contains(s) {
                        return Err(ResolveError::DuplicateHandlerStatus {
                            status: *s,
                            span: *span,
                        });
                    }
                    seen.push(*s);
                }
                let target_action_name = named_scopes.get(target).cloned().ok_or_else(|| {
                    ResolveError::UnknownHandlerTarget {
                        name: target.clone(),
                        span: *target_span,
                    }
                })?;
                // Action name joins status labels in source order, so
                // `on failed or timedout try_work` → `On_failed_timedout_try_work`.
                let labels = statuses
                    .iter()
                    .map(|s| s.as_label())
                    .collect::<Vec<_>>()
                    .join("_");
                let action_name =
                    unique_name(&format!("On_{labels}_{target}"), name_counts);
                // Handler body scopes its own registrations like any other
                // block. Same rationale as Condition/Foreach/Until.
                let saved_env = env.clone();
                let saved_named_scopes = named_scopes.clone();
                let body_actions = resolve_statements(body, env, name_counts, named_scopes, false)?;
                *env = saved_env;
                *named_scopes = saved_named_scopes;
                (
                    action_name,
                    ActionKind::OnHandler {
                        statuses: statuses.clone(),
                        target_action_name,
                        body: body_actions,
                    },
                    *span,
                )
            }
            Stmt::Switch {
                subject,
                subject_span,
                cases,
                default,
                span,
            } => {
                let subject = resolve_expr(subject, env)?;
                let action_name = unique_name("Switch", name_counts);
                // Cases and default each scope their `let` bindings and
                // named-scope registrations to their own branch, like
                // if/else-branches. Clone both before each branch and
                // restore after.
                let saved_env = env.clone();
                let saved_named_scopes = named_scopes.clone();
                let mut resolved_cases = Vec::with_capacity(cases.len());
                for case in cases {
                    let case_action_name = unique_name("Case", name_counts);
                    *env = saved_env.clone();
                    *named_scopes = saved_named_scopes.clone();
                    let body = resolve_statements(&case.body, env, name_counts, named_scopes, false)?;
                    resolved_cases.push(ResolvedSwitchCase {
                        action_name: case_action_name,
                        value: case.value.clone(),
                        body,
                    });
                }
                let resolved_default = match default {
                    Some(stmts) => {
                        *env = saved_env.clone();
                        *named_scopes = saved_named_scopes.clone();
                        Some(resolve_statements(stmts, env, name_counts, named_scopes, false)?)
                    }
                    None => None,
                };
                *env = saved_env;
                *named_scopes = saved_named_scopes;
                (
                    action_name,
                    ActionKind::Switch {
                        subject,
                        subject_span: *subject_span,
                        cases: resolved_cases,
                        default: resolved_default,
                    },
                    *span,
                )
            }
            Stmt::Terminate { status, message, span } => {
                let resolved_message = match message {
                    Some(m) => Some(resolve_expr(m, env)?),
                    None => None,
                };
                let action_name = unique_name("Terminate", name_counts);
                (
                    action_name,
                    ActionKind::Terminate {
                        status: *status,
                        message: resolved_message,
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

        // `on` handlers are off the main sibling chain: their runAfter points
        // at their target + filter statuses (set here), and the statement
        // after the handler chains back to whatever came before, not the
        // handler.
        if let ActionKind::OnHandler {
            statuses,
            target_action_name,
            ..
        } = &kind
        {
            let entry = RunAfterEntry {
                action_name: target_action_name.clone(),
                statuses: statuses.iter().map(|s| s.as_pa_str().to_string()).collect(),
            };
            actions.push(ResolvedAction {
                name: action_name,
                run_after: vec![entry],
                kind,
                span: stmt_span,
            });
            continue;
        }

        let run_after = match &prev_name {
            Some(n) => vec![RunAfterEntry::succeeded(n.clone())],
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
            Type::Int | Type::Float => Ok((
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
            Type::Int | Type::Float => Ok((
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
        let after = &resolved.actions[2].run_after;
        assert_eq!(after.len(), 1, "expected one predecessor");
        assert_eq!(
            after[0].action_name, "Initialize_a",
            "action after debug must chain back to prior real action"
        );
        assert_eq!(after[0].statuses, vec!["Succeeded".to_string()]);
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
        assert_eq!(resolved.actions[1].run_after[0].action_name, "Initialize_a");
        assert_eq!(resolved.actions[2].name, "Initialize_c");
        assert_eq!(resolved.actions[2].run_after[0].action_name, "Initialize_b");
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
            (Type::Float, "Increment_x"),
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
        for ty in [Type::Int, Type::Float, Type::Bool, Type::Array, Type::Object] {
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
    fn subtract_only_on_numeric() {
        // String rejected; Int and Float accepted.
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
        for ty in [Type::Int, Type::Float] {
            let prog = Program {
                trigger: Trigger::Manual,
                statements: vec![
                    var_ty("x", ty),
                    assign("x", AssignOp::Subtract, Expr::Literal(Literal::Int(1))),
                ],
            };
            let resolved = resolve(&prog).unwrap();
            assert_eq!(resolved.actions[1].name, "Decrement_x");
        }
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
    fn slice30_on_handler_resolves_target_to_action_name() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                Stmt::Scope {
                    name: Some("try_work".to_string()),
                    body: vec![],
                    span: sp(),
                },
                Stmt::OnHandler {
                    statuses: vec![HandlerStatus::Failed],
                    target: "try_work".to_string(),
                    target_span: sp(),
                    body: vec![],
                    span: sp(),
                },
            ],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[0].name, "Scope_try_work");
        assert_eq!(resolved.actions[1].name, "On_failed_try_work");
        match &resolved.actions[1].kind {
            ActionKind::OnHandler {
                statuses,
                target_action_name,
                ..
            } => {
                assert_eq!(statuses, &vec![HandlerStatus::Failed]);
                assert_eq!(target_action_name, "Scope_try_work");
            }
            _ => panic!("expected OnHandler"),
        }
        // RunAfter points at Scope_try_work with Failed status.
        assert_eq!(resolved.actions[1].run_after.len(), 1);
        assert_eq!(
            resolved.actions[1].run_after[0].action_name,
            "Scope_try_work"
        );
        assert_eq!(
            resolved.actions[1].run_after[0].statuses,
            vec!["Failed".to_string()]
        );
    }

    #[test]
    fn slice30_unknown_handler_target_errors() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![Stmt::OnHandler {
                statuses: vec![HandlerStatus::Failed],
                target: "nope".to_string(),
                target_span: sp(),
                body: vec![],
                span: sp(),
            }],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UnknownHandlerTarget { name, .. } if name == "nope"
        ));
    }

    #[test]
    fn slice30_nested_scope_name_does_not_leak_to_outer_handler() {
        // Regression guard: a scope declared inside an if / foreach / switch
        // case / until / scope / on-handler body must not be visible as a
        // target from the enclosing source level. Letting `conditional_work`
        // leak would emit an `on` handler at workflow root pointing at a
        // Scope action nested inside the Condition -- invalid PA graph.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("flag", Type::Bool),
                Stmt::If {
                    condition: rref("flag"),
                    condition_span: sp(),
                    true_branch: vec![Stmt::Scope {
                        name: Some("conditional_work".to_string()),
                        body: vec![],
                        span: sp(),
                    }],
                    false_branch: vec![],
                },
                Stmt::OnHandler {
                    statuses: vec![HandlerStatus::Failed],
                    target: "conditional_work".to_string(),
                    target_span: sp(),
                    body: vec![],
                    span: sp(),
                },
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UnknownHandlerTarget { name, .. } if name == "conditional_work"
        ));
    }

    #[test]
    fn slice30_handler_within_same_scope_body_still_resolves() {
        // Complement to the leak test: a handler that references a sibling
        // scope declared in the SAME body resolves fine. Named scopes are
        // visible to siblings within a block; they're rolled back only at
        // the end of the enclosing block.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![Stmt::Scope {
                name: Some("outer".to_string()),
                body: vec![
                    Stmt::Scope {
                        name: Some("inner".to_string()),
                        body: vec![],
                        span: sp(),
                    },
                    Stmt::OnHandler {
                        statuses: vec![HandlerStatus::Failed],
                        target: "inner".to_string(),
                        target_span: sp(),
                        body: vec![],
                        span: sp(),
                    },
                ],
                span: sp(),
            }],
        };
        assert!(
            resolve(&prog).is_ok(),
            "handler on sibling inner scope should resolve"
        );
    }

    #[test]
    fn slice30_handler_does_not_participate_in_sibling_chain() {
        // A handler between two regular actions must not appear in the next
        // action's runAfter. The next action chains back to the scope (the
        // last real action on the main path).
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                Stmt::Scope {
                    name: Some("work".to_string()),
                    body: vec![],
                    span: sp(),
                },
                Stmt::OnHandler {
                    statuses: vec![HandlerStatus::Failed],
                    target: "work".to_string(),
                    target_span: sp(),
                    body: vec![],
                    span: sp(),
                },
                var("after"),
            ],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[2].name, "Initialize_after");
        assert_eq!(resolved.actions[2].run_after.len(), 1);
        assert_eq!(
            resolved.actions[2].run_after[0].action_name,
            "Scope_work",
            "next action must chain to the scope, not the handler"
        );
    }

    #[test]
    fn slice32_multi_status_handler_joins_statuses_in_runafter() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                Stmt::Scope {
                    name: Some("try_work".to_string()),
                    body: vec![],
                    span: sp(),
                },
                Stmt::OnHandler {
                    statuses: vec![HandlerStatus::Failed, HandlerStatus::TimedOut],
                    target: "try_work".to_string(),
                    target_span: sp(),
                    body: vec![],
                    span: sp(),
                },
            ],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(
            resolved.actions[1].name, "On_failed_timedout_try_work",
            "action name joins status labels in source order"
        );
        match &resolved.actions[1].kind {
            ActionKind::OnHandler { statuses, .. } => {
                assert_eq!(
                    statuses,
                    &vec![HandlerStatus::Failed, HandlerStatus::TimedOut]
                );
            }
            _ => panic!("expected OnHandler"),
        }
        let entry = &resolved.actions[1].run_after[0];
        assert_eq!(entry.action_name, "Scope_try_work");
        assert_eq!(
            entry.statuses,
            vec!["Failed".to_string(), "TimedOut".to_string()],
            "both statuses appear in PA-capitalized form, source order preserved"
        );
    }

    #[test]
    fn slice32_duplicate_handler_status_errors() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                Stmt::Scope {
                    name: Some("try_work".to_string()),
                    body: vec![],
                    span: sp(),
                },
                Stmt::OnHandler {
                    statuses: vec![HandlerStatus::Failed, HandlerStatus::Failed],
                    target: "try_work".to_string(),
                    target_span: sp(),
                    body: vec![],
                    span: sp(),
                },
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::DuplicateHandlerStatus { status, .. }
                if status == HandlerStatus::Failed
        ));
    }

    #[test]
    fn slice28_scope_unnamed_gets_bare_action_key() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![Stmt::Scope {
                name: None,
                body: vec![],
                span: sp(),
            }],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[0].name, "Scope");
        assert!(matches!(resolved.actions[0].kind, ActionKind::Scope { .. }));
    }

    #[test]
    fn slice28_scope_named_keys_by_name() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![Stmt::Scope {
                name: Some("try_api".to_string()),
                body: vec![],
                span: sp(),
            }],
        };
        let resolved = resolve(&prog).unwrap();
        assert_eq!(resolved.actions[0].name, "Scope_try_api");
    }

    #[test]
    fn slice28_scope_nested_var_decl_is_error() {
        // Like if/foreach, Scope bodies reject nested `var`.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![Stmt::Scope {
                name: None,
                body: vec![var("inner")],
                span: sp(),
            }],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::NestedVarDeclaration { name, .. } if name == "inner"
        ));
    }

    #[test]
    fn slice28_scope_let_does_not_leak() {
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                Stmt::Scope {
                    name: None,
                    body: vec![let_stmt("inner", Expr::Literal(Literal::Int(1)))],
                    span: sp(),
                },
                let_stmt("leak", rref("inner")),
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "inner"
        ));
    }

    #[test]
    fn slice27_switch_resolves_with_unique_case_names() {
        // Multiple cases should get auto-suffixed action names via the shared
        // name counter, parallel to PA's Case / Case_1 convention.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("status", Type::String),
                Stmt::Switch {
                    subject: rref("status"),
                    subject_span: sp(),
                    cases: vec![
                        crate::ast::SwitchCase {
                            value: Literal::String("a".to_string()),
                            body: vec![],
                            span: sp(),
                        },
                        crate::ast::SwitchCase {
                            value: Literal::String("b".to_string()),
                            body: vec![],
                            span: sp(),
                        },
                    ],
                    default: None,
                    span: sp(),
                },
            ],
        };
        let resolved = resolve(&prog).unwrap();
        match &resolved.actions[1].kind {
            ActionKind::Switch { cases, default, .. } => {
                assert_eq!(cases[0].action_name, "Case");
                assert_eq!(cases[1].action_name, "Case_1");
                assert!(default.is_none(), "empty default source -> None");
            }
            _ => panic!("expected Switch"),
        }
    }

    #[test]
    fn slice27_switch_nested_var_decl_is_error() {
        // `var` in a case body must be rejected like in if/foreach bodies.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("status", Type::String),
                Stmt::Switch {
                    subject: rref("status"),
                    subject_span: sp(),
                    cases: vec![crate::ast::SwitchCase {
                        value: Literal::String("a".to_string()),
                        body: vec![var("inner")],
                        span: sp(),
                    }],
                    default: None,
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
    fn slice27_switch_let_in_case_does_not_leak() {
        // Regression: per-case `let` bindings must scope to their own branch,
        // paralleling if-branch scoping.
        let prog = Program {
            trigger: Trigger::Manual,
            statements: vec![
                var_ty("status", Type::String),
                Stmt::Switch {
                    subject: rref("status"),
                    subject_span: sp(),
                    cases: vec![crate::ast::SwitchCase {
                        value: Literal::String("a".to_string()),
                        body: vec![let_stmt("inner", Expr::Literal(Literal::Int(1)))],
                        span: sp(),
                    }],
                    default: None,
                    span: sp(),
                },
                let_stmt("leak", rref("inner")),
            ],
        };
        assert!(matches!(
            resolve(&prog).unwrap_err(),
            ResolveError::UndefinedVariable { name, .. } if name == "inner"
        ));
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
