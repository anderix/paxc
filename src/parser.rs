//! Parser for the pax DSL.
//!
//! Slice 1 recognizes a sequence of `var <ident>: <type> = <literal>`
//! declarations and builds a `Program` AST.

use crate::ast::{Expr, Literal, Program, Stmt, Type};
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
    };

    let literal = select! {
        Token::Int(n) => Expr::Literal(Literal::Int(n)),
        Token::Str(s) => Expr::Literal(Literal::String(s.to_string())),
        Token::Bool(b) => Expr::Literal(Literal::Bool(b)),
    };

    let var_decl = just(Token::Var)
        .ignore_then(name)
        .then_ignore(just(Token::Colon))
        .then(ty)
        .then_ignore(just(Token::Eq))
        .then(literal)
        .map(|((name, ty), value)| Stmt::VarDecl {
            name: name.to_string(),
            ty,
            value,
        });

    var_decl
        .repeated()
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
            Stmt::VarDecl { name, ty, value } => {
                assert_eq!(name, "counter");
                assert!(matches!(ty, Type::Int));
                assert!(matches!(value, Expr::Literal(Literal::Int(1))));
            }
        }
    }
}
