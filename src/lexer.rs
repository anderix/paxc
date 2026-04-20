//! Lexer for the pax DSL.
//!
//! Slice 1 handles just the tokens needed for a single `var` declaration with
//! an integer literal: the `var` keyword, identifiers, `:`, `=`, integers,
//! whitespace, and `//` line comments.

use chumsky::prelude::*;

pub type Span = SimpleSpan;
pub type Spanned<T> = (T, Span);

#[derive(Clone, Debug, PartialEq)]
pub enum Token<'src> {
    Var,
    Ident(&'src str),
    Int(i64),
    Str(&'src str),
    Bool(bool),
    Colon,
    Eq,
}

pub fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<Spanned<Token<'src>>>, extra::Err<Rich<'src, char, Span>>> {
    let int = text::int(10)
        .to_slice()
        .from_str::<i64>()
        .unwrapped()
        .map(Token::Int);

    let str_ = just('"')
        .ignore_then(none_of('"').repeated().to_slice())
        .then_ignore(just('"'))
        .map(Token::Str);

    let ctrl = choice((just(':').to(Token::Colon), just('=').to(Token::Eq)));

    let ident = text::ascii::ident().map(|s: &str| match s {
        "var" => Token::Var,
        "true" => Token::Bool(true),
        "false" => Token::Bool(false),
        _ => Token::Ident(s),
    });

    let token = int.or(str_).or(ctrl).or(ident);

    let comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .padded();

    token
        .map_with(|tok, e| (tok, e.span()))
        .padded_by(comment.repeated())
        .padded()
        .repeated()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Token<'_>> {
        lexer()
            .parse(src)
            .into_result()
            .expect("lex failed")
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn slice1_var_decl() {
        assert_eq!(
            lex("var counter: int = 1"),
            vec![
                Token::Var,
                Token::Ident("counter"),
                Token::Colon,
                Token::Ident("int"),
                Token::Eq,
                Token::Int(1),
            ]
        );
    }

    #[test]
    fn slice2_string_and_bool() {
        assert_eq!(
            lex(r#"var greeting: string = "hello""#),
            vec![
                Token::Var,
                Token::Ident("greeting"),
                Token::Colon,
                Token::Ident("string"),
                Token::Eq,
                Token::Str("hello"),
            ]
        );
        assert_eq!(
            lex("var ok: bool = true"),
            vec![
                Token::Var,
                Token::Ident("ok"),
                Token::Colon,
                Token::Ident("bool"),
                Token::Eq,
                Token::Bool(true),
            ]
        );
    }

    #[test]
    fn skips_line_comment() {
        assert_eq!(lex("// hello\nvar x: int = 42"),
            vec![
                Token::Var,
                Token::Ident("x"),
                Token::Colon,
                Token::Ident("int"),
                Token::Eq,
                Token::Int(42),
            ]
        );
    }
}
