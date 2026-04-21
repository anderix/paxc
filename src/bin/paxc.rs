use chumsky::prelude::*;
use paxc::{emitter, lexer, parser, resolver};
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: paxc <file.pax>");
        process::exit(2);
    }
    let path = &args[1];
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("paxc: cannot read {path}: {e}");
            process::exit(1);
        }
    };

    let tokens = match lexer::lexer().parse(src.as_str()).into_result() {
        Ok(toks) => toks,
        Err(errs) => {
            for e in errs {
                eprintln!("lex error: {e}");
            }
            process::exit(1);
        }
    };

    let program = match parser::parser()
        .parse(
            tokens
                .as_slice()
                .map((src.len()..src.len()).into(), |(t, s)| (t, s)),
        )
        .into_result()
    {
        Ok(p) => p,
        Err(errs) => {
            for e in errs {
                eprintln!("parse error: {e:?}");
            }
            process::exit(1);
        }
    };

    let resolved = match resolver::resolve(&program) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    };

    let json = emitter::emit(&resolved);
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}
