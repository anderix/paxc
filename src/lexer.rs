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
    Let,
    If,
    Else,
    Foreach,
    In,
    Trigger,
    Raw,
    Null,
    Ident(&'src str),
    Int(i64),
    Str(String),
    Bool(bool),
    Colon,
    Eq,
    PlusEq,
    MinusEq,
    AmpEq,
    Amp,
    Plus,
    Minus,
    Star,
    Slash,
    Comma,
    Dot,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
}

pub fn lexer<'src>()
-> impl Parser<'src, &'src str, Vec<Spanned<Token<'src>>>, extra::Err<Rich<'src, char, Span>>> {
    let int = text::int(10)
        .to_slice()
        .from_str::<i64>()
        .unwrapped()
        .map(Token::Int);

    let escape = just('\\').ignore_then(choice((
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
        just('"').to('"'),
        just('\\').to('\\'),
    )));

    let str_char = escape.or(none_of("\\\""));

    let str_ = just('"')
        .ignore_then(str_char.repeated().collect::<String>())
        .then_ignore(just('"'))
        .map(Token::Str);

    let compound = choice((
        just("+=").to(Token::PlusEq),
        just("-=").to(Token::MinusEq),
        just("&=").to(Token::AmpEq),
    ));

    let ctrl = choice((
        just(':').to(Token::Colon),
        just('=').to(Token::Eq),
        just('&').to(Token::Amp),
        just('+').to(Token::Plus),
        just('-').to(Token::Minus),
        just('*').to(Token::Star),
        just('/').to(Token::Slash),
        just(',').to(Token::Comma),
        just('.').to(Token::Dot),
        just('[').to(Token::LBracket),
        just(']').to(Token::RBracket),
        just('{').to(Token::LBrace),
        just('}').to(Token::RBrace),
    ));

    let ident = text::ascii::ident().map(|s: &str| match s {
        "var" => Token::Var,
        "let" => Token::Let,
        "if" => Token::If,
        "else" => Token::Else,
        "foreach" => Token::Foreach,
        "in" => Token::In,
        "trigger" => Token::Trigger,
        "raw" => Token::Raw,
        "null" => Token::Null,
        "true" => Token::Bool(true),
        "false" => Token::Bool(false),
        _ => Token::Ident(s),
    });

    let token = int.or(str_).or(compound).or(ctrl).or(ident);

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
                Token::Str("hello".to_string()),
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
    fn slice6_compound_assign_ops() {
        assert_eq!(
            lex("counter += 1"),
            vec![
                Token::Ident("counter"),
                Token::PlusEq,
                Token::Int(1),
            ]
        );
        assert_eq!(
            lex("counter -= 1"),
            vec![
                Token::Ident("counter"),
                Token::MinusEq,
                Token::Int(1),
            ]
        );
    }

    #[test]
    fn slice13_string_escapes() {
        assert_eq!(
            lex(r#""a\nb\tc\"d\\e\re""#),
            vec![Token::Str("a\nb\tc\"d\\e\re".to_string())]
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
