use chumsky::prelude::*;
use paxc::{diagnostic, emitter, lexer, packager, parser, resolver};
use std::path::{Path, PathBuf};
use std::{env, fs, process};

struct Args {
    path: String,
    target: Option<packager::Target>,
    name: Option<String>,
    out: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "usage: paxc [--target <pa-legacy>] [--name <NAME>] [--out <PATH>] <file.pax>\n\
         \n\
         With no --target: writes the Power Automate flow definition JSON to stdout.\n\
         With --target pa-legacy: writes a legacy PA import package (.zip). Defaults:\n\
           --name  input file basename without .pax\n\
           --out   <name>.zip in the current directory"
    );
    process::exit(2);
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut target: Option<packager::Target> = None;
    let mut name: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--target" => {
                i += 1;
                let Some(v) = argv.get(i) else { usage() };
                target = Some(match v.as_str() {
                    "pa-legacy" => packager::Target::PaLegacy,
                    other => {
                        eprintln!("paxc: unknown target '{other}' (supported: pa-legacy)");
                        process::exit(2);
                    }
                });
            }
            "--name" => {
                i += 1;
                let Some(v) = argv.get(i) else { usage() };
                name = Some(v.clone());
            }
            "--out" => {
                i += 1;
                let Some(v) = argv.get(i) else { usage() };
                out = Some(PathBuf::from(v));
            }
            _ => positional.push(argv[i].clone()),
        }
        i += 1;
    }

    if positional.len() != 1 {
        usage();
    }
    Args {
        path: positional.into_iter().next().unwrap(),
        target,
        name,
        out,
    }
}

fn main() {
    let args = parse_args();
    let src = match fs::read_to_string(&args.path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("paxc: cannot read {}: {}", args.path, e);
            process::exit(1);
        }
    };

    let tokens = match lexer::lexer().parse(src.as_str()).into_result() {
        Ok(toks) => toks,
        Err(errs) => {
            for e in &errs {
                diagnostic::from_lex_error(e).report(&args.path, &src);
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
                diagnostic::from_parse_error(e).report(&args.path, &src);
            }
            process::exit(1);
        }
    };

    let resolved = match resolver::resolve(&program) {
        Ok(r) => r,
        Err(e) => {
            diagnostic::from_resolve_error(&e).report(&args.path, &src);
            process::exit(1);
        }
    };

    match args.target {
        None => {
            let json = emitter::emit(&resolved);
            println!("{}", serde_json::to_string_pretty(&json).unwrap());

            let dropped = emitter::count_debug_actions(&resolved.actions);
            if dropped > 0 {
                let plural = if dropped == 1 { "" } else { "s" };
                eprintln!("note: dropped {dropped} debug() statement{plural}");
            }
        }
        Some(target) => {
            let derived_name = args
                .name
                .unwrap_or_else(|| derive_name_from_path(&args.path));
            let out_path = args
                .out
                .unwrap_or_else(|| PathBuf::from(format!("{derived_name}.zip")));
            if let Err(e) = packager::package(&resolved, target, &derived_name, &out_path) {
                eprintln!("paxc: packaging failed: {e}");
                process::exit(1);
            }
            eprintln!("wrote {}", out_path.display());

            let dropped = emitter::count_debug_actions(&resolved.actions);
            if dropped > 0 {
                let plural = if dropped == 1 { "" } else { "s" };
                eprintln!("note: dropped {dropped} debug() statement{plural}");
            }
        }
    }
}

fn derive_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("flow")
        .to_string()
}
