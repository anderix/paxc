//! Parser for the pax DSL.
//!
//! Slice 1 recognizes a sequence of `var <ident>: <type> = <literal>`
//! declarations and builds a `Program` AST.

use crate::ast::{AssignOp, Expr, Literal, Program, Stmt, Trigger, Type};
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
            Token::Int(n) => Literal::Int(n),
            Token::Str(s) => Literal::String(s.to_string()),
            Token::Bool(b) => Literal::Bool(b),
        };

        let array = literal
            .clone()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBracket), just(Token::RBracket))
            .map(Literal::Array);

        let key = select! { Token::Str(s) => s.to_string() };
        let entry = key.then_ignore(just(Token::Colon)).then(literal.clone());
        let object = entry
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(Literal::Object);

        scalar.or(array).or(object)
    });

    let reference = select! { Token::Ident(s) => Expr::Ref(s.to_string()) };

    let expr = literal.map(Expr::Literal).or(reference);

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
    };

    let assign = name
        .then(assign_op)
        .then(expr)
        .map(|((name, op), value)| Stmt::Assign {
            name: name.to_string(),
            op,
            value,
        });

    let stmt = var_decl.or(assign);

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
