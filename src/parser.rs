//! Parser for the pax DSL.
//!
//! Slice 1 recognizes a sequence of `var <ident>: <type> = <literal>`
//! declarations and builds a `Program` AST.

use crate::ast::{AssignOp, BinOp, Expr, Literal, Program, Stmt, Trigger, Type, UnaryOp};
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
    };

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

        // A call is an ident followed immediately by `(args)`. We parse the
        // `(args)` optionally and decide Call vs Ref at the seed stage so
        // chumsky doesn't have to backtrack.
        let call_args = expr
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<Expr>>()
            .delimited_by(just(Token::LParen), just(Token::RParen));

        let path_seed = ident_s.clone().then(call_args.or_not()).map(
            |(name, maybe_args)| match maybe_args {
                Some(args) => Expr::Call { name, args },
                None => Expr::Ref(name),
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

        let var_decl = just(Token::Var)
            .ignore_then(name)
            .then_ignore(just(Token::Colon))
            .then(ty)
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
            .map(|((name, ty), value)| Stmt::VarDecl {
                name: name.to_string(),
                ty,
                value,
            });

        let assign_op = select! {
            Token::Eq => AssignOp::Set,
            Token::PlusEq => AssignOp::Add,
            Token::MinusEq => AssignOp::Subtract,
            Token::AmpEq => AssignOp::Concat,
        };

        let assign = name
            .then(assign_op)
            .then(expr.clone())
            .map(|((name, op), value)| Stmt::Assign {
                name: name.to_string(),
                op,
                value,
            });

        let raw_stmt = just(Token::Raw)
            .ignore_then(select! { Token::Ident(s) => s.to_string() })
            .then(object_entries.clone())
            .map(|(name, body)| Stmt::Raw { name, body });

        let let_decl = just(Token::Let)
            .ignore_then(name)
            .then_ignore(just(Token::Eq))
            .then(expr.clone())
            .map(|(name, value)| Stmt::Let {
                name: name.to_string(),
                value,
            });

        let if_stmt = recursive(|if_stmt| {
            just(Token::If)
                .ignore_then(expr.clone())
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
                .map(|((condition, true_branch), else_branch)| Stmt::If {
                    condition,
                    true_branch,
                    false_branch: else_branch.unwrap_or_default(),
                })
        });

        let foreach_stmt = just(Token::Foreach)
            .ignore_then(name)
            .then_ignore(just(Token::In))
            .then(expr.clone())
            .then(block.clone())
            .map(|((iter, collection), body)| Stmt::Foreach {
                iter: iter.to_string(),
                collection,
                body,
            });

        var_decl
            .or(let_decl)
            .or(if_stmt)
            .or(foreach_stmt)
            .or(raw_stmt)
            .or(assign)
    });

    let trigger = just(Token::Trigger).ignore_then(select! {
        Token::Ident("manual") => Trigger::Manual,
    });

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
            Stmt::VarDecl { name, ty, value } => {
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
    fn slice6_assign_statement() {
        let prog = parse("var x: int = 1\nx += 2");
        assert_eq!(prog.statements.len(), 2);
        match &prog.statements[1] {
            Stmt::Assign { name, op, value } => {
                assert_eq!(name, "x");
                assert!(matches!(op, AssignOp::Add));
                assert!(matches!(value, Expr::Literal(Literal::Int(2))));
            }
            _ => panic!("expected assign"),
        }
    }
}
