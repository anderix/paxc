//! Parser for the pax DSL.
//!
//! Slice 1 recognizes a sequence of `var <ident>: <type> = <literal>`
//! declarations and builds a `Program` AST.

use crate::ast::{
    AssignOp, BinOp, DebugArg, Expr, Frequency, Literal, Program, Stmt, SwitchCase,
    TerminateStatus, Trigger, Type, UnaryOp,
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
        Token::Ident("string") => Type::String,
        Token::Ident("bool") => Type::Bool,
        Token::Ident("array") => Type::Array,
        Token::Ident("object") => Type::Object,
    }
    .labelled("type name (int, string, bool, array, or object)");

    let literal = recursive(|literal| {
        let scalar = select! {
            Token::Null => Literal::Null,
            Token::Int(n) => Literal::Int(n),
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

    let object_entries = {
        let key = select! { Token::Str(s) => s };
        let entry = key.then_ignore(just(Token::Colon)).then(literal.clone());
        entry
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
    };

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

        let path_seed = ident_spanned.then(call_args.or_not()).map(
            |((name, span), maybe_args)| match maybe_args {
                Some(args) => Expr::Call { name, args },
                None => Expr::Ref { name, span },
            },
        );

        let field = just(Token::Dot).ignore_then(ident_s);

        let ref_path = path_seed.foldl(field.repeated(), |target, field| Expr::Member {
            target: Box::new(target),
            field,
        });

        let atom = literal.clone().map(Expr::Literal).or(ref_path).boxed();

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

        let var_decl = just(Token::Var)
            .ignore_then(name_spanned)
            .then_ignore(just(Token::Colon))
            .then(ty)
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
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

        let assign = name_spanned
            .then(assign_op)
            .then(expr.clone())
            .map(|(((name, name_span), op), value)| Stmt::Assign {
                name,
                name_span,
                op,
                value,
            });

        let raw_stmt = just(Token::Raw)
            .ignore_then(select! { Token::Ident(s) => s.to_string() })
            .then(object_entries.clone())
            .map_with(|(name, body), e| Stmt::Raw { name, body, span: e.span() });

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
            let spanned_condition = expr
                .clone()
                .map_with(|cond, e| (cond, e.span()));
            just(Token::If)
                .ignore_then(spanned_condition)
                .then(block.clone())
                .then(
                    just(Token::Else)
                        .ignore_then(
                            if_stmt
                                .map(|s| vec![s])
                                .or(block.clone()),
                        )
                        .or_not(),
                )
                .map(|(((condition, condition_span), true_branch), else_branch)| Stmt::If {
                    condition,
                    condition_span,
                    true_branch,
                    false_branch: else_branch.unwrap_or_default(),
                })
        });

        // `until <condition> { body }` -- PA's Until (do-while) loop. The
        // condition is the exit condition.
        let until_stmt = just(Token::Until)
            .ignore_then(expr.clone().map_with(|cond, e| (cond, e.span())))
            .then(block.clone())
            .map_with(|((condition, condition_span), body), e| Stmt::Until {
                condition,
                condition_span,
                body,
                span: e.span(),
            });

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
        let debug_arg = expr
            .clone()
            .map_with(|expr, e| DebugArg { expr, span: e.span() });

        let debug_stmt = just(Token::Debug)
            .ignore_then(
                debug_arg
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map_with(|args, e| Stmt::Debug { args, span: e.span() });

        // `terminate <status> [message]`. Status is one of succeeded / failed /
        // cancelled. Only `failed` accepts a trailing message expression; the
        // other statuses never consume one, so a plain `terminate succeeded`
        // followed by a new statement on the next line parses cleanly without
        // the expr parser greedily eating the next identifier.
        let failed_form = select! { Token::Ident("failed") => () }
            .ignore_then(expr.clone().or_not())
            .map(|msg| (TerminateStatus::Failed, msg));
        let succeeded_form = select! { Token::Ident("succeeded") => () }
            .map(|_| (TerminateStatus::Succeeded, None));
        let cancelled_form = select! { Token::Ident("cancelled") => () }
            .map(|_| (TerminateStatus::Cancelled, None));
        let terminate_body = failed_form
            .or(succeeded_form)
            .or(cancelled_form)
            .labelled("terminate status (succeeded, failed, or cancelled)");

        let terminate_stmt = just(Token::Terminate)
            .ignore_then(terminate_body)
            .map_with(|(status, message), e| Stmt::Terminate {
                status,
                message,
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
            .ignore_then(
                expr.clone().map_with(|cond, e| (cond, e.span())),
            )
            .then(switch_body)
            .map_with(|((subject, subject_span), (cases, default)), e| Stmt::Switch {
                subject,
                subject_span,
                cases,
                default,
                span: e.span(),
            });

        var_decl
            .or(let_decl)
            .or(if_stmt)
            .or(foreach_stmt)
            .or(until_stmt)
            .or(switch_stmt)
            .or(scope_stmt)
            .or(raw_stmt)
            .or(debug_stmt)
            .or(terminate_stmt)
            .or(assign)
    });

    // Unit keyword → Frequency. Singular and plural both accepted.
    let frequency_unit = select! {
        Token::Ident("second") => Frequency::Second,
        Token::Ident("seconds") => Frequency::Second,
        Token::Ident("minute") => Frequency::Minute,
        Token::Ident("minutes") => Frequency::Minute,
        Token::Ident("hour") => Frequency::Hour,
        Token::Ident("hours") => Frequency::Hour,
        Token::Ident("day") => Frequency::Day,
        Token::Ident("days") => Frequency::Day,
        Token::Ident("week") => Frequency::Week,
        Token::Ident("weeks") => Frequency::Week,
        Token::Ident("month") => Frequency::Month,
        Token::Ident("months") => Frequency::Month,
    }
    .labelled("time unit (second, minute, hour, day, week, or month)");

    // `every [N]? <unit>`: N defaults to 1 when omitted.
    let schedule_body = select! { Token::Ident("every") => () }
        .ignore_then(select! { Token::Int(n) => n }.or_not())
        .then(frequency_unit)
        .try_map(|(n_opt, frequency), span| {
            let interval = n_opt.unwrap_or(1);
            if interval < 1 {
                return Err(Rich::custom(span, "schedule interval must be at least 1"));
            }
            let interval: u32 = interval.try_into().map_err(|_| {
                Rich::custom(span, "schedule interval is too large (must fit in u32)")
            })?;
            Ok(Trigger::Schedule {
                frequency,
                interval,
            })
        });

    let manual_body = select! { Token::Ident("manual") => Trigger::Manual };
    let schedule_kw = select! { Token::Ident("schedule") => () }.ignore_then(schedule_body);

    let trigger = just(Token::Trigger).ignore_then(manual_body.or(schedule_kw));

    trigger
        .then(stmt.repeated().collect::<Vec<_>>())
        .map(|(trigger, statements)| Program {
            trigger,
            statements,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lexer;

    fn parse(src: &str) -> Program {
        // Prepend a manual trigger so slice-specific tests can focus on their
        // new syntax without every test having to repeat the trigger line.
        let src = format!("trigger manual\n{src}");
        let tokens = lexer().parse(src.as_str()).into_result().expect("lex failed");
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
            Stmt::VarDecl { name, ty, value, .. } => {
                assert_eq!(name, "counter");
                assert!(matches!(ty, Type::Int));
                assert!(matches!(value, Expr::Literal(Literal::Int(1))));
            }
            _ => panic!("expected var decl"),
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
                let Expr::Literal(Literal::Array(items)) = value else {
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
        let src = "trigger manual\nvar a: int = 1\nlet x = a < 2 < 3";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(
            result.has_errors(),
            "chained comparisons should not parse"
        );
    }

    #[test]
    fn slice23_if_condition_span_covers_source() {
        // paxr's verbose trace uses this span to render
        // `condition? (<source>) = true/false`.
        let src = "trigger manual\nvar x: int = 1\nif x == 1 { x = 2 }";
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
        let src = "trigger manual\nvar total: int = 0\nvar completed: int = 0\ndebug(total - completed)";
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
            Stmt::Assign { name, op, value, .. } => {
                assert_eq!(name, "x");
                assert!(matches!(op, AssignOp::Add));
                assert!(matches!(value, Expr::Literal(Literal::Int(2))));
            }
            _ => panic!("expected assign"),
        }
    }

    fn parse_full(src: &str) -> Program {
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
    fn slice21_schedule_trigger_every_n_plural() {
        let prog = parse_full("trigger schedule every 5 minutes");
        assert_eq!(
            prog.trigger,
            Trigger::Schedule {
                frequency: Frequency::Minute,
                interval: 5,
            }
        );
    }

    #[test]
    fn slice21_schedule_trigger_every_singular_defaults_to_one() {
        let prog = parse_full("trigger schedule every hour");
        assert_eq!(
            prog.trigger,
            Trigger::Schedule {
                frequency: Frequency::Hour,
                interval: 1,
            }
        );
    }

    #[test]
    fn slice21_schedule_accepts_all_units() {
        for (src, expected) in [
            ("trigger schedule every 30 seconds", Frequency::Second),
            ("trigger schedule every 2 days", Frequency::Day),
            ("trigger schedule every week", Frequency::Week),
            ("trigger schedule every 6 months", Frequency::Month),
        ] {
            let prog = parse_full(src);
            match prog.trigger {
                Trigger::Schedule { frequency, .. } => assert_eq!(frequency, expected),
                _ => panic!("expected schedule"),
            }
        }
    }

    #[test]
    fn slice21_schedule_rejects_interval_zero() {
        let src = "trigger schedule every 0 minutes";
        let tokens = lexer().parse(src).into_result().expect("lex failed");
        let result = parser().parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        );
        assert!(result.has_errors(), "expected parse error for interval 0");
    }

    #[test]
    fn slice21_manual_trigger_still_parses() {
        let prog = parse_full("trigger manual\nvar x: int = 1");
        assert_eq!(prog.trigger, Trigger::Manual);
        assert_eq!(prog.statements.len(), 1);
    }

    #[test]
    fn slice22_terminate_succeeded_no_message() {
        let prog = parse("terminate succeeded");
        assert!(matches!(
            &prog.statements[0],
            Stmt::Terminate {
                status: TerminateStatus::Succeeded,
                message: None,
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
                ..
            }
        ));
    }

    #[test]
    fn slice22_terminate_message_on_succeeded_is_parse_error() {
        let src = "trigger manual\nterminate succeeded \"nope\"";
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
            Stmt::Until { body, .. } => assert_eq!(body.len(), 1),
            _ => panic!("expected until"),
        }
    }

    #[test]
    fn slice29_until_condition_span_covers_source() {
        let src = "trigger manual\nvar n: int = 0\nuntil n > 5 { n += 1 }";
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
        let src = "trigger manual\nvar n: int = 1\nswitch n { case 1 + 1 { } }";
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
        let src = "trigger manual\nterminate bogus";
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
