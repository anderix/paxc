//! paxr — the pax runner (interpreter).
//!
//! Reads a .pax file, parses and resolves it the same way paxc does, and
//! then executes it in-process so the developer can exercise their logic
//! without going through Power Automate. Lives alongside paxc in the same
//! crate, sharing the lexer / parser / resolver via the library.

use chumsky::prelude::*;
use paxc::{diagnostic, interpreter, lexer, parser, resolver};
use std::{env, fs, process};

fn main() {
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut verbose = false;
    let mut quiet = false;
    let mut debug_only = false;
    let mut positional: Vec<String> = Vec::new();
    for arg in argv {
        match arg.as_str() {
            "--verbose" | "-v" => verbose = true,
            "--quiet" | "-q" => quiet = true,
            "--debug" | "-d" => debug_only = true,
            _ => positional.push(arg),
        }
    }
    // --verbose, --quiet, --debug are pairwise mutually exclusive.
    let mode_count = [verbose, quiet, debug_only].iter().filter(|b| **b).count();
    if mode_count > 1 {
        eprintln!("paxr: --verbose, --quiet, and --debug are mutually exclusive");
        process::exit(2);
    }
    if positional.len() != 1 {
        eprintln!("usage: paxr [--verbose | --quiet | --debug] <file.pax>");
        process::exit(2);
    }
    let path = &positional[0];
    let src = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("paxr: cannot read {path}: {e}");
            process::exit(1);
        }
    };

    let tokens = match lexer::lexer().parse(src.as_str()).into_result() {
        Ok(toks) => toks,
        Err(errs) => {
            for e in &errs {
                diagnostic::from_lex_error(e).report(path, &src);
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
            for e in &errs {
                diagnostic::from_parse_error(e).report(path, &src);
            }
            process::exit(1);
        }
    };

    let resolved = match resolver::resolve(&program) {
        Ok(r) => r,
        Err(e) => {
            diagnostic::from_resolve_error(&e).report(path, &src);
            process::exit(1);
        }
    };

    let config = interpreter::Config {
        verbose,
        quiet,
        debug_only,
    };
    let state = match interpreter::interpret_with(&src, &resolved, config) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("runtime error: {e}");
            process::exit(1);
        }
    };

    if !quiet && !debug_only {
        let dump = interpreter::format_state_dump(&state);
        if !dump.is_empty() {
            println!();
            println!("end state:");
            print!("{}", dump);
        }
    }
}
