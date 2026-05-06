//! Parser for the pax DSL.
//!
//! Slice 1 recognizes a sequence of `var <ident>: <type> = <literal>`
//! declarations and builds a `Program` AST.

use crate::ast::{
    AssignOp, BinOp, DebugArg, Expr, HandlerStatus, Literal, Program, Stmt, SubscriptKey,
    SwitchCase, TerminateStatus, Type, UnaryOp,
};
use crate::lexer::{Span, Token};
use chumsky::{input::ValueInput, prelude::*};

pub fn parser<'tokens, 'src: 'tokens, I>()
-> impl Parser<'tokens, I, Program, extra::Err<Rich<'tokens, Token<'src>, Span>>>
where
    I: ValueInput<'tokens, Token = Token<'src>, Span = Span>,
{
    let name = select! { Token::Ident(s) => s };

    let ty = select! {
        Token::Ident("int") => Type::Int,
        Token::Ident("float") => Type::Float,
        Token::Ident("string") => Type::String,
        Token::Ident("bool") => Type::Bool,
        Token::Ident("array") => Type::Array,
        Token::Ident("object") => Type::Object,
    }
    .labelled("type name (int, float, string, bool, array, or object)");

    let literal = recursive(|literal| {
        let scalar = select! {
            Token::Null => Literal::Null,
            Token::Int(n) => Literal::Int(n),
            Token::Float(x) => Literal::Float(x),
            Token::Str(s) => Literal::String(s),
            Token::Bool(b) => Literal::Bool(b),
        };

        let array = literal
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBracket), just(Token::RBracket))
            .map(Literal::Array);

        let key = select! { Token::Str(s) => s };
        let entry = key.then_ignore(just(Token::Colon)).then(literal.clone());
        let object = entry
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(Literal::Object);

        scalar.or(array).or(object)
    });

    // Expression parsing is wrapped in `recursive` because function-call args
    // can be arbitrary expressions (including more calls), so the inner layers
    // need to reference `expr` itself.
    //
    // Precedence, tightest to loosest:
    //   atom → unary (!, -) → product (*, /) → sum (+, -) → concat (&)
    //     → comparison (< <= > >= == !=, non-chaining) → and (&&) → or (||)
    // Binary operators are left-associative (except comparison, which is
    // non-chaining). `.boxed()` at each layer keeps chumsky's generic type
    // chain from growing exponentially across layers.
    let expr = recursive(|expr| {
        let ident_s = select! { Token::Ident(s) => s.to_string() };
        let ident_spanned = ident_s.map_with(|name, e| (name, e.span()));

        // A call is an ident followed immediately by `(args)`. We parse the
        // `(args)` optionally and decide Call vs Ref at the seed stage so
        // chumsky doesn't have to backtrack.
        let call_args = expr
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<Expr>>()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        let path_seed = ident_spanned
            .then(call_args.or_not())
            .map(|((name, span), maybe_args)| match maybe_args {
                Some(args) => Expr::Call { name, args },
                None => Expr::Ref { name, span },
            });

        // Path postfix: `.IDENT` (Member) or `?[<literal>]` (Subscript).
        // Both fold left-associatively onto the same path seed so chains like
        // `triggerBody()?['user']?['name']` and `obj.a?[0].b` work uniformly.
        enum PathTail {
            Field(String),
            Sub(SubscriptKey),
        }
        let field_tail = just(Token::Dot).ignore_then(ident_s).map(PathTail::Field);
        let subscript_key = select! {
            Token::Str(s) => SubscriptKey::String(s),
            Token::Int(n) => SubscriptKey::Index(n),
        };
        let subscript_tail = just(Token::Question)
            .ignore_then(just(Token::LBracket))
            .ignore_then(subscript_key)
            .then_ignore(just(Token::RBracket))
            .map(PathTail::Sub);
        let path_tail = field_tail.or(subscript_tail);

        let ref_path = path_seed.foldl(path_tail.repeated(), |target, tail| match tail {
            PathTail::Field(field) => Expr::Member {
                target: Box::new(target),
                field,
            },
            PathTail::Sub(key) => Expr::Subscript {
                target: Box::new(target),
                key,
            },
        });

        // Parenthesized subexpression. Lets the source disambiguate
        // precedence (`!(a == b)`, `(a + b) * c`) and lets the round-trip
        // decoder emit defensive parens without losing reparseability.
        let paren_expr = expr
            .clone()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        let atom = literal
            .clone()
            .map(Expr::Literal)
            .or(paren_expr)
            .or(ref_path)
            .boxed();

        let unary_op = select! {
            Token::Bang => UnaryOp::Not,
            Token::Minus => UnaryOp::Neg,
        };

        let unary = recursive(|unary| {
            unary_op
                .then(unary)
                .map(|(op, operand)| Expr::UnaryOp {
                    op,
                    operand: Box::new(operand),
                })
                .or(atom.clone())
        })
        .boxed();

        let mul_op = select! {
            Token::Star => BinOp::Mul,
            Token::Slash => BinOp::Div,
        };
        let add_op = select! {
            Token::Plus => BinOp::Add,
            Token::Minus => BinOp::Sub,
        };
        let concat_op = select! {
            Token::Amp => BinOp::Concat,
        };

        let product = unary
            .clone()
            .foldl(mul_op.then(unary).repeated(), |lhs, (op, rhs)| {
                Expr::BinaryOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            })
            .boxed();

        let sum = product
            .clone()
            .foldl(add_op.then(product).repeated(), |lhs, (op, rhs)| {
                Expr::BinaryOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            })
            .boxed();

        let concat = sum
            .clone()
            .foldl(concat_op.then(sum).repeated(), |lhs, (op, rhs)| {
                Expr::BinaryOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            })
            .boxed();

        // Non-chaining: `a < b` is allowed, `a < b < c` is a parse error.
        let comp_op = select! {
            Token::Lt => BinOp::Less,
            Token::Le => BinOp::LessEq,
            Token::Gt => BinOp::Greater,
            Token::Ge => BinOp::GreaterEq,
            Token::EqEq => BinOp::Equals,
            Token::BangEq => BinOp::NotEquals,
        };

        let comparison = concat
            .clone()
            .then(comp_op.then(concat).or_not())
            .map(|(lhs, tail)| match tail {
                None => lhs,
                Some((op, rhs)) => Expr::BinaryOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            })
            .boxed();

        let and_op = select! { Token::AmpAmp => BinOp::And };
        let or_op = select! { Token::PipePipe => BinOp::Or };

        let and_layer = comparison
            .clone()
            .foldl(and_op.then(comparison).repeated(), |lhs, (op, rhs)| {
                Expr::BinaryOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            })
            .boxed();

        and_layer
            .clone()
            .foldl(or_op.then(and_layer).repeated(), |lhs, (op, rhs)| {
                Expr::BinaryOp {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            })
            .boxed()
    });

    let stmt = recursive(|stmt| {
        let block = stmt
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace));

        let name_spanned = name.map_with(|n, e| (n.to_string(), e.span()));

        let var_init = just(Token::Eq).ignore_then(expr.clone()).or_not();
        let var_decl = just(Token::Var)
            .ignore_then(name_spanned)
            .then_ignore(just(Token::Colon))
            .then(ty)
            .then(var_init)
            .map(|(((name, name_span), ty), value)| Stmt::VarDecl {
                name,
                name_span,
                ty,
                value,
            });

        let assign_op = select! {
            Token::Eq => AssignOp::Set,
            Token::PlusEq => AssignOp::Add,
            Token::MinusEq => AssignOp::Subtract,
            Token::AmpEq => AssignOp::Concat,
        }
        .labelled("assignment operator");

        let assign = name_spanned.then(assign_op).then(expr.clone()).map(
            |(((name, name_span), op), value)| Stmt::Assign {
                name,
                name_span,
                op,
                value,
            },
        );

        // `pa <Name>` -- references an opaque PA action whose body lives at
        // `pa/<Name>.json` next to the source. The bare statement carries
        // no body in the source itself; the resolver reads the JSON file
        // and stores its content on the resolved action.
        let pa_stmt = just(Token::Pa)
            .ignore_then(
                select! { Token::Ident(s) => s.to_string() }.map_with(|s, e| (s, e.span())),
            )
            .map_with(|(name, name_span), e| Stmt::Pa {
                name,
                name_span,
                span: e.span(),
            });

        let let_decl = just(Token::Let)
            .ignore_then(name_spanned)
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
            .map(|((name, name_span), value)| Stmt::Let {
                name,
                name_span,
                value,
            });

        let if_stmt = recursive(|if_stmt| {
            let spanned_condition = expr.clone().map_with(|cond, e| (cond, e.span()));
            just(Token::If)
                .ignore_then(spanned_condition)
                .then(block.clone())
                .then(
                    just(Token::Else)
                        .ignore_then(if_stmt.map(|s| vec![s]).or(block.clone()))
                        .or_not(),
                )
                .map(
                    |(((condition, condition_span), true_branch), else_branch)| Stmt::If {
                        condition,
                        condition_span,
                        true_branch,
                        false_branch: else_branch.unwrap_or_default(),
                    },
                )
        });

        // `until <condition> [max N] [timeout "PT30M"] { body }` -- PA's
        // Until (do-while) loop. The condition is the exit condition. The
        // two optional trailers tune PA's Until `limit` block; both are
        // independent, both default to PA's built-in values when omitted.
        // Fixed order: `max` first, then `timeout` -- reading order matches
        // how users describe the loop ("run at most N times, for up to T").
        let until_max = just(Token::Ident("max"))
            .ignore_then(select! { Token::Int(n) => n }.map_with(|n, e| (n, e.span())))
            .labelled("max iteration count (integer literal)");

        let until_timeout = just(Token::Ident("timeout"))
            .ignore_then(select! { Token::Str(s) => s })
            .labelled("timeout (ISO 8601 duration string literal)");

        let until_stmt = just(Token::Until)
            .ignore_then(expr.clone().map_with(|cond, e| (cond, e.span())))
            .then(until_max.or_not())
            .then(until_timeout.or_not())
            .then(block.clone())
            .map_with(
                |((((condition, condition_span), max_clause), timeout_clause), body), e| {
                    let (limit_count, limit_count_span) = match max_clause {
                        Some((n, sp)) => (Some(n), Some(sp)),
                        None => (None, None),
                    };
                    Stmt::Until {
                        condition,
                        condition_span,
                        limit_count,
                        limit_count_span,
                        limit_timeout: timeout_clause,
                        body,
                        span: e.span(),
                    }
                },
            );

        let foreach_stmt = just(Token::Foreach)
            .ignore_then(name)
            .then_ignore(just(Token::In))
            .then(expr.clone())
            .then(block.clone())
            .map_with(|((iter, collection), body), e| Stmt::Foreach {
                iter: iter.to_string(),
                collection,
                body,
                span: e.span(),
            });

        // `debug(args)` -- each arg keeps its source span so paxr can
        // auto-label with the exact source slice the user wrote.
        let debug_arg = expr.clone().map_with(|expr, e| DebugArg {
            expr,
            span: e.span(),
        });

        let debug_stmt = just(Token::Debug)
            .ignore_then(
                debug_arg
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map_with(|args, e| Stmt::Debug {
                args,
                span: e.span(),
            });

        // `terminate <status> [message] [code <code-expr>]`. Status is one of
        // succeeded / failed / cancelled. Only `failed` accepts a trailing
        // message expression and/or `code` clause; the other statuses never
        // consume one, so `terminate succeeded` followed by a new statement
        // parses cleanly without the expr parser greedily eating the next
        // identifier.
        //
        // `code` is a contextual keyword: when followed by an expression in
        // this slot, it introduces the code clause. A bare `code` (no expr
        // after) falls through to be parsed as an identifier reference, so
        // `terminate failed code` (with `code` a string variable) still works
        // as a message-only form.
        let code_arg = just(Token::Ident("code"))
            .ignore_then(expr.clone())
            .labelled("`code` keyword followed by an expression");
        // Three shapes after `failed`:
        //   a) <code-arg>                    -- code only, no message
        //   b) <message-expr> [<code-arg>]   -- message, optional code
        //   c) (nothing)                     -- no message, no code
        // Order matters: try (a) first so `code "x"` doesn't get parsed as
        // a bare Ref("code") that then leaves "x" stranded.
        let failed_args = code_arg
            .clone()
            .map(|c| (None, Some(c)))
            .or(expr
                .clone()
                .then(code_arg.clone().or_not())
                .map(|(m, c)| (Some(m), c)))
            .or_not()
            .map(|opt| opt.unwrap_or((None, None)));

        let failed_form = select! { Token::Ident("failed") => () }
            .ignore_then(failed_args)
            .map(|(message, code)| (TerminateStatus::Failed, message, code));
        let succeeded_form = select! { Token::Ident("succeeded") => () }
            .map(|_| (TerminateStatus::Succeeded, None, None));
        let cancelled_form = select! { Token::Ident("cancelled") => () }
            .map(|_| (TerminateStatus::Cancelled, None, None));
        let terminate_body = failed_form
            .or(succeeded_form)
            .or(cancelled_form)
            .labelled("terminate status (succeeded, failed, or cancelled)");

        let terminate_stmt = just(Token::Terminate)
            .ignore_then(terminate_body)
            .map_with(|(status, message, code), e| Stmt::Terminate {
                status,
                message,
                code,
                span: e.span(),
            });

        // Case values are restricted to scalar literals (string / int / bool),
        // matching PA's constraint. Arbitrary expressions are not allowed.
        let case_literal = select! {
            Token::Int(n) => Literal::Int(n),
            Token::Str(s) => Literal::String(s),
            Token::Bool(b) => Literal::Bool(b),
        }
        .labelled("case value (string, int, or bool literal)");

        let switch_case = just(Token::Case)
            .ignore_then(case_literal)
            .then(block.clone())
            .map_with(|(value, body), e| SwitchCase {
                value,
                body,
                span: e.span(),
            });

        let default_arm = just(Token::Default).ignore_then(block.clone());

        let switch_body = switch_case
            .repeated()
            .collect::<Vec<_>>()
            .then(default_arm.or_not())
            .delimited_by(just(Token::LBrace), just(Token::RBrace));

        // `on <status> [or <status>]* <target> { body }` -- error-path
        // handler attached to a named scope. Status keyword follows the same
        // ident-select pattern as terminate's status (not reserved words, just
        // recognized by the parser). Multi-status form joins siblings with
        // `or`, mirroring the PA runAfter-status array under the hood.
        let handler_status = select! {
            Token::Ident("succeeded") => HandlerStatus::Succeeded,
            Token::Ident("failed") => HandlerStatus::Failed,
            Token::Ident("skipped") => HandlerStatus::Skipped,
            Token::Ident("timedout") => HandlerStatus::TimedOut,
        }
        .labelled("handler status (succeeded, failed, skipped, or timedout)");

        let handler_statuses = handler_status
            .then(
                just(Token::Ident("or"))
                    .ignore_then(handler_status)
                    .repeated()
                    .collect::<Vec<HandlerStatus>>(),
            )
            .map(|(first, rest)| {
                let mut v = Vec::with_capacity(1 + rest.len());
                v.push(first);
                v.extend(rest);
                v
            });

        let on_handler = just(Token::On)
            .ignore_then(handler_statuses)
            .then(name.map_with(|s, e| (s.to_string(), e.span())))
            .then(block.clone())
            .map_with(
                |((statuses, (target, target_span)), body), e| Stmt::OnHandler {
                    statuses,
                    target,
                    target_span,
                    body,
                    span: e.span(),
                },
            );

        // `scope [name] { ... }`. The optional name becomes part of the action
        // key. Without one, the resolver auto-suffixes `Scope`, `Scope_1`, ...
        let scope_stmt = just(Token::Scope)
            .ignore_then(name.or_not())
            .then(block.clone())
            .map_with(|(opt_name, body), e| Stmt::Scope {
                name: opt_name.map(|s| s.to_string()),
                body,
                span: e.span(),
            });

        let switch_stmt = just(Token::Switch)
            .ignore_then(expr.clone().map_with(|cond, e| (cond, e.span())))
            .then(switch_body)
            .map_with(
                |((subject, subject_span), (cases, default)), e| Stmt::Switch {
                    subject,
                    subject_span,
                    cases,
                    default,
                    span: e.span(),
                },
            );

        var_decl
            .or(let_decl)
            .or(if_stmt)
            .or(foreach_stmt)
            .or(until_stmt)
            .or(switch_stmt)
            .or(scope_stmt)
            .or(on_handler)
            .or(pa_stmt)
            .or(debug_stmt)
            .or(terminate_stmt)
            .or(assign)
    });

    // Programs no longer carry a trigger statement -- the trigger is
    // determined at resolve time by scanning the source directory's
    // `pa/` folder for a `*.trigger.json` file (or defaulting to a
    // generated manual trigger when none is present). The body is just
    // a sequence of statements.
    stmt.repeated()
        .collect::<Vec<_>>()
        .map(|statements| Program { statements })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lexer;

    fn parse(src: &str) -> Program {
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        parser()
            .parse(
                tokens
                    .as_slice()
                    .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .expect("parse failed")
    }

    #[test]
    fn slice1_var_decl() {
        let prog = parse("var counter: int = 1");
        assert_eq!(prog.statements.len(), 1);
        match &prog.statements[0] {
            Stmt::VarDecl {
                name, ty, value, ..
            } => {
                assert_eq!(name, "counter");
                assert!(matches!(ty, Type::Int));
                assert!(matches!(value, Some(Expr::Literal(Literal::Int(1)))));
            }
            _ => panic!("expected var decl"),
        }
    }

    #[test]
    fn slice44a_var_decl_no_initializer() {
        let prog = parse("var todo: string");
        assert_eq!(prog.statements.len(), 1);
        match &prog.statements[0] {
            Stmt::VarDecl {
                name, ty, value, ..
            } => {
                assert_eq!(name, "todo");
                assert!(matches!(ty, Type::String));
                assert!(value.is_none());
            }
            _ => panic!("expected var decl"),
        }
    }

    #[test]
    fn slice45a_subscript_string_key_parses() {
        let prog = parse(r#"let raw = obj?["body/email"]"#);
        match &prog.statements[0] {
            Stmt::Let { value, .. } => match value {
                Expr::Subscript { target, key } => {
                    assert!(matches!(target.as_ref(), Expr::Ref { name, .. } if name == "obj"));
                    assert!(matches!(key, SubscriptKey::String(s) if s == "body/email"));
                }
                other => panic!("expected subscript, got {other:?}"),
            },
            _ => panic!("expected let"),
        }
    }

    #[test]
    fn slice45a_subscript_int_key_parses() {
        let prog = parse("let first = arr?[0]");
        match &prog.statements[0] {
            Stmt::Let { value, .. } => match value {
                Expr::Subscript { key, .. } => {
                    assert!(matches!(key, SubscriptKey::Index(0)));
                }
                other => panic!("expected subscript, got {other:?}"),
            },
            _ => panic!("expected let"),
        }
    }

    #[test]
    fn slice45a_subscript_chains_with_dot_and_call() {
        // triggerBody() ?["body/email"] . local
        let prog = parse(r#"let v = triggerBody()?["body/email"].local"#);
        match &prog.statements[0] {
            Stmt::Let { value, .. } => match value {
                Expr::Member { target, field } => {
                    assert_eq!(field, "local");
                    assert!(matches!(target.as_ref(), Expr::Subscript { .. }));
                }
                other => panic!("expected outer member, got {other:?}"),
            },
            _ => panic!("expected let"),
        }
    }

    #[test]
    fn slice4_array_and_object() {
        let prog = parse(
            r#"var tasks: array = [
                { "title": "a", "done": true },
                { "title": "b", "done": false },
            ]"#,
        );
        match &prog.statements[0] {
            Stmt::VarDecl { ty, value, .. } => {
                assert!(matches!(ty, Type::Array));
                let Some(Expr::Literal(Literal::Array(items))) = value else {
                    panic!("expected array");
                };
                assert_eq!(items.len(), 2);
                let Literal::Object(entries) = &items[0] else {
                    panic!("expected object");
                };
                assert_eq!(entries[0].0, "title");
                assert!(matches!(&entries[0].1, Literal::String(s) if s == "a"));
                assert!(matches!(&entries[1].1, Literal::Bool(true)));
            }
            _ => panic!("expected var decl"),
        }
    }

    #[test]
    fn slice16_comparison_chain_is_parse_error() {
        let src = "var a: int = 1\nlet x = a < 2 < 3";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(result.has_errors(), "chained comparisons should not parse");
    }

    #[test]
    fn slice23_if_condition_span_covers_source() {
        // paxr's verbose trace uses this span to render
        // `condition? (<source>) = true/false`.
        let src = "var x: int = 1\nif x == 1 { x = 2 }";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let prog = parser()
            .parse(
                tokens
                    .as_slice()
                    .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .expect("parse failed");
        match &prog.statements[1] {
            Stmt::If { condition_span, .. } => {
                let slice = &src[condition_span.start..condition_span.end];
                assert_eq!(slice, "x == 1");
            }
            _ => panic!("expected if stmt"),
        }
    }

    #[test]
    fn slice20_debug_parses() {
        let prog = parse("var x: int = 1\ndebug()\ndebug(x)\ndebug(x, x + 1)");
        assert_eq!(prog.statements.len(), 4);
        match &prog.statements[1] {
            Stmt::Debug { args, .. } => assert_eq!(args.len(), 0),
            _ => panic!("expected debug breadcrumb"),
        }
        match &prog.statements[2] {
            Stmt::Debug { args, .. } => assert_eq!(args.len(), 1),
            _ => panic!("expected debug(x)"),
        }
        match &prog.statements[3] {
            Stmt::Debug { args, .. } => assert_eq!(args.len(), 2),
            _ => panic!("expected debug(x, x+1)"),
        }
    }

    #[test]
    fn slice20_debug_arg_span_covers_source_slice() {
        // Per-arg spans let paxr show `total - completed=<value>` rather
        // than just the evaluated number.
        let src = "var total: int = 0\nvar completed: int = 0\ndebug(total - completed)";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let prog = parser()
            .parse(
                tokens
                    .as_slice()
                    .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .expect("parse failed");
        match &prog.statements[2] {
            Stmt::Debug { args, .. } => {
                assert_eq!(args.len(), 1);
                let span = args[0].span;
                let slice = &src[span.start..span.end];
                assert_eq!(slice, "total - completed");
            }
            _ => panic!("expected debug"),
        }
    }

    #[test]
    fn slice6_assign_statement() {
        let prog = parse("var x: int = 1\nx += 2");
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[1] {
            Stmt::Assign {
                name, op, value, ..
            } => {
                assert_eq!(name, "x");
                assert!(matches!(op, AssignOp::Add));
                assert!(matches!(value, Expr::Literal(Literal::Int(2))));
            }
            _ => panic!("expected assign"),
        }
    }

    #[test]
    fn slice22_terminate_succeeded_no_message() {
        let prog = parse("terminate succeeded");
        assert!(matches!(
            &prog.statements[0],
            Stmt::Terminate {
                status: TerminateStatus::Succeeded,
                message: None,
                code: None,
                ..
            }
        ));
    }

    #[test]
    fn slice22_terminate_failed_with_string_message() {
        let prog = parse(r#"terminate failed "queue empty""#);
        match &prog.statements[0] {
            Stmt::Terminate {
                status: TerminateStatus::Failed,
                message: Some(Expr::Literal(Literal::String(s))),
                code: None,
                ..
            } => assert_eq!(s, "queue empty"),
            other => panic!("expected terminate failed with string message, got {other:?}"),
        }
    }

    #[test]
    fn slice22_terminate_failed_with_expression_message() {
        // Message is a full Expr, so `&` concat and var refs must work.
        let prog = parse("var n: int = 0\nterminate failed \"at \" & n");
        assert!(matches!(
            &prog.statements[1],
            Stmt::Terminate {
                status: TerminateStatus::Failed,
                message: Some(Expr::BinaryOp { .. }),
                code: None,
                ..
            }
        ));
    }

    #[test]
    fn slice22_terminate_cancelled_parses() {
        let prog = parse("terminate cancelled");
        assert!(matches!(
            &prog.statements[0],
            Stmt::Terminate {
                status: TerminateStatus::Cancelled,
                message: None,
                code: None,
                ..
            }
        ));
    }

    #[test]
    fn terminate_failed_with_message_and_code() {
        let prog = parse(r#"terminate failed "No title" code "Invalid item""#);
        match &prog.statements[0] {
            Stmt::Terminate {
                status: TerminateStatus::Failed,
                message: Some(Expr::Literal(Literal::String(m))),
                code: Some(Expr::Literal(Literal::String(c))),
                ..
            } => {
                assert_eq!(m, "No title");
                assert_eq!(c, "Invalid item");
            }
            other => panic!("expected message+code, got {other:?}"),
        }
    }

    #[test]
    fn terminate_failed_with_code_only() {
        let prog = parse(r#"terminate failed code "X""#);
        match &prog.statements[0] {
            Stmt::Terminate {
                status: TerminateStatus::Failed,
                message: None,
                code: Some(Expr::Literal(Literal::String(c))),
                ..
            } => assert_eq!(c, "X"),
            other => panic!("expected code-only, got {other:?}"),
        }
    }

    #[test]
    fn terminate_failed_message_with_var_named_code_still_works() {
        // `code` is a contextual keyword. A bare `code` (no expr after) is a
        // valid identifier reference, so `terminate failed code` (a string
        // var named `code`) parses as message-only.
        let prog = parse("var code: string = \"x\"\nterminate failed code");
        match &prog.statements[1] {
            Stmt::Terminate {
                status: TerminateStatus::Failed,
                message: Some(Expr::Ref { name, .. }),
                code: None,
                ..
            } => assert_eq!(name, "code"),
            other => panic!("expected message=Ref(code), code=None; got {other:?}"),
        }
    }

    #[test]
    fn slice22_terminate_message_on_succeeded_is_parse_error() {
        let src = "terminate succeeded \"nope\"";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(
            result.has_errors(),
            "expected parse error: message only valid with failed"
        );
    }

    #[test]
    fn slice29_until_parses() {
        let prog = parse("var n: int = 0\nuntil n > 5 { n += 1 }");
        match &prog.statements[1] {
            Stmt::Until {
                body,
                limit_count,
                limit_timeout,
                ..
            } => {
                assert_eq!(body.len(), 1);
                assert!(
                    limit_count.is_none() && limit_timeout.is_none(),
                    "bare until carries no user limits"
                );
            }
            _ => panic!("expected until"),
        }
    }

    #[test]
    fn slice34_until_max_clause_parses() {
        let prog = parse("var n: int = 0\nuntil n > 5 max 10 { n += 1 }");
        match &prog.statements[1] {
            Stmt::Until {
                limit_count,
                limit_timeout,
                ..
            } => {
                assert_eq!(*limit_count, Some(10));
                assert!(limit_timeout.is_none());
            }
            _ => panic!("expected until"),
        }
    }

    #[test]
    fn slice34_until_timeout_clause_parses() {
        let prog = parse("var n: int = 0\nuntil n > 5 timeout \"PT30M\" { n += 1 }");
        match &prog.statements[1] {
            Stmt::Until {
                limit_count,
                limit_timeout,
                ..
            } => {
                assert!(limit_count.is_none());
                assert_eq!(limit_timeout.as_deref(), Some("PT30M"));
            }
            _ => panic!("expected until"),
        }
    }

    #[test]
    fn slice34_until_max_and_timeout_both_parse() {
        let prog = parse("var n: int = 0\nuntil n > 5 max 10 timeout \"PT30M\" { n += 1 }");
        match &prog.statements[1] {
            Stmt::Until {
                limit_count,
                limit_timeout,
                ..
            } => {
                assert_eq!(*limit_count, Some(10));
                assert_eq!(limit_timeout.as_deref(), Some("PT30M"));
            }
            _ => panic!("expected until"),
        }
    }

    #[test]
    fn slice34_until_timeout_before_max_is_parse_error() {
        // Fixed order: `max` must come before `timeout` when both appear.
        let src = "var n: int = 0\nuntil n > 5 timeout \"PT30M\" max 10 { n += 1 }";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(result.has_errors());
    }

    #[test]
    fn slice29_until_condition_span_covers_source() {
        let src = "var n: int = 0\nuntil n > 5 { n += 1 }";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let prog = parser()
            .parse(
                tokens
                    .as_slice()
                    .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
            )
            .into_result()
            .expect("parse failed");
        match &prog.statements[1] {
            Stmt::Until { condition_span, .. } => {
                assert_eq!(&src[condition_span.start..condition_span.end], "n > 5");
            }
            _ => panic!("expected until"),
        }
    }

    #[test]
    fn slice30_on_handler_parses() {
        let prog = parse(
            r#"scope try_work {
  var x: int = 1
}
on failed try_work {
}"#,
        );
        match &prog.statements[1] {
            Stmt::OnHandler {
                statuses,
                target,
                body,
                ..
            } => {
                assert_eq!(statuses, &vec![HandlerStatus::Failed]);
                assert_eq!(target, "try_work");
                assert!(body.is_empty());
            }
            _ => panic!("expected OnHandler"),
        }
    }

    #[test]
    fn slice30_on_handler_all_statuses_parse() {
        for (kw, expected) in [
            ("succeeded", HandlerStatus::Succeeded),
            ("failed", HandlerStatus::Failed),
            ("skipped", HandlerStatus::Skipped),
            ("timedout", HandlerStatus::TimedOut),
        ] {
            let src = format!("scope foo {{\n}}\non {kw} foo {{\n}}");
            let prog = parse(&src);
            match &prog.statements[1] {
                Stmt::OnHandler { statuses, .. } => {
                    assert_eq!(statuses, &vec![expected], "kw: {kw}")
                }
                _ => panic!("expected OnHandler for {kw}"),
            }
        }
    }

    #[test]
    fn slice30_on_handler_unknown_status_is_parse_error() {
        let src = "scope foo {}\non weird foo {\n}";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(result.has_errors());
    }

    #[test]
    fn slice32_multi_status_handler_parses() {
        let prog = parse(
            r#"scope try_work {
}
on failed or timedout try_work {
}"#,
        );
        match &prog.statements[1] {
            Stmt::OnHandler {
                statuses, target, ..
            } => {
                assert_eq!(
                    statuses,
                    &vec![HandlerStatus::Failed, HandlerStatus::TimedOut],
                    "source order preserved"
                );
                assert_eq!(target, "try_work");
            }
            _ => panic!("expected OnHandler"),
        }
    }

    #[test]
    fn slice32_multi_status_handler_three_statuses() {
        let prog = parse(
            r#"scope work {
}
on failed or skipped or timedout work {
}"#,
        );
        match &prog.statements[1] {
            Stmt::OnHandler { statuses, .. } => {
                assert_eq!(
                    statuses,
                    &vec![
                        HandlerStatus::Failed,
                        HandlerStatus::Skipped,
                        HandlerStatus::TimedOut,
                    ]
                );
            }
            _ => panic!("expected OnHandler"),
        }
    }

    #[test]
    fn slice28_scope_unnamed_parses() {
        let prog = parse("scope {\n  var x: int = 1\n}");
        match &prog.statements[0] {
            Stmt::Scope { name, body, .. } => {
                assert!(name.is_none());
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected scope"),
        }
    }

    #[test]
    fn slice28_scope_named_parses() {
        let prog = parse("scope try_work {\n  var x: int = 1\n}");
        match &prog.statements[0] {
            Stmt::Scope { name, body, .. } => {
                assert_eq!(name.as_deref(), Some("try_work"));
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected scope"),
        }
    }

    #[test]
    fn slice28_scope_empty_body_parses() {
        let prog = parse("scope {}");
        match &prog.statements[0] {
            Stmt::Scope { name, body, .. } => {
                assert!(name.is_none());
                assert!(body.is_empty());
            }
            _ => panic!("expected scope"),
        }
    }

    #[test]
    fn slice27_switch_parses_with_cases_and_default() {
        let prog = parse(
            r#"var status: string = "active"
switch status {
  case "active" {
  }
  case "pending" {
  }
  default {
  }
}"#,
        );
        match &prog.statements[1] {
            Stmt::Switch { cases, default, .. } => {
                assert_eq!(cases.len(), 2);
                assert!(matches!(&cases[0].value, Literal::String(s) if s == "active"));
                assert!(matches!(&cases[1].value, Literal::String(s) if s == "pending"));
                assert!(
                    matches!(default, Some(v) if v.is_empty()),
                    "empty default block preserved as Some(vec![])"
                );
            }
            other => panic!("expected switch, got {other:?}"),
        }
    }

    #[test]
    fn slice27_switch_without_default_parses() {
        let prog = parse("var n: int = 1\nswitch n { case 1 { } case 2 { } }");
        match &prog.statements[1] {
            Stmt::Switch { cases, default, .. } => {
                assert_eq!(cases.len(), 2);
                assert!(default.is_none(), "default absent -> None");
            }
            _ => panic!("expected switch"),
        }
    }

    #[test]
    fn slice27_switch_empty_cases_parses() {
        // A switch with only a default arm is degenerate but legal.
        let prog = parse("var n: int = 1\nswitch n { default { } }");
        match &prog.statements[1] {
            Stmt::Switch { cases, default, .. } => {
                assert!(cases.is_empty());
                assert!(matches!(default, Some(v) if v.is_empty()));
            }
            _ => panic!("expected switch"),
        }
    }

    #[test]
    fn slice27_switch_rejects_expression_case_value() {
        // Case values must be literals, not expressions. PA enforces the
        // same constraint in its Switch action.
        let src = "var n: int = 1\nswitch n { case 1 + 1 { } }";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(result.has_errors(), "case values must be literals");
    }

    #[test]
    fn slice22_terminate_unknown_status_is_parse_error() {
        let src = "terminate bogus";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(
            result.has_errors(),
            "expected parse error: unknown status keyword"
        );
    }
}
