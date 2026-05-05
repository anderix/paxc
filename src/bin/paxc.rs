use chumsky::prelude::*;
use paxc::pa::{decoder, emitter, packager};
use paxc::{diagnostic, lexer, parser, resolver};
use std::path::{Path, PathBuf};
use std::{env, fs, process};

struct Args {
    path: String,
    target: Option<packager::Target>,
    name: Option<String>,
    out: Option<PathBuf>,
    decode: bool,
    out_dir: Option<PathBuf>,
}

fn usage() -> ! {
    eprintln!(
        "usage: paxc [--target <pa-legacy>] [--name <NAME>] [--out <PATH>] <file.pax>\n\
         \n\
         With no --target: writes the Power Automate flow definition JSON to stdout.\n\
         With --target pa-legacy: writes a legacy PA import package (.zip). Defaults:\n\
           --name  input file basename without .pax (or pa/flow.json's displayName when present)\n\
           --out   <name>.zip in the current directory\n\
         \n\
         Decode mode (round-trip ingest):\n\
           paxc --decode <flow.json> [--out-dir <DIR>]\n\
         Reads an exported PA flow JSON and writes a .pax source file plus a\n\
         pa/ folder of opaque action bodies to <DIR> (defaults to the input's\n\
         parent directory)."
    );
    process::exit(2);
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut target: Option<packager::Target> = None;
    let mut name: Option<String> = None;
    let mut out: Option<PathBuf> = None;
    let mut decode = false;
    let mut out_dir: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--version" | "-V" => {
                println!("paxc {}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
            }
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
            "--decode" => {
                decode = true;
            }
            "--out-dir" => {
                i += 1;
                let Some(v) = argv.get(i) else { usage() };
                out_dir = Some(PathBuf::from(v));
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
        decode,
        out_dir,
    }
}

fn main() {
    let args = parse_args();

    if args.decode {
        run_decode(&args);
        return;
    }

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

    let source_dir = Path::new(&args.path).parent();
    let resolved = match resolver::resolve(&program, source_dir) {
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
            // Default --name: pa/flow.json's displayName when present, else
            // the input file basename. Lets a decoded flow round-trip back to
            // its original PA displayName without re-typing it on encode.
            let derived_name = args.name.unwrap_or_else(|| {
                read_display_name(source_dir).unwrap_or_else(|| derive_name_from_path(&args.path))
            });
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

fn run_decode(args: &Args) {
    let input_path = Path::new(&args.path);
    let out_dir = args
        .out_dir
        .clone()
        .or_else(|| input_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    match decoder::decode_file(input_path, &out_dir) {
        Ok(report) => {
            for w in &report.warnings {
                eprintln!("{w}");
            }
            eprintln!("wrote {}", report.pax_path.display());
            for f in &report.pa_files_written {
                eprintln!("wrote {}", f.display());
            }
        }
        Err(e) => {
            eprintln!("paxc: decode failed: {e}");
            process::exit(1);
        }
    }
}

/// Returns the displayName from `<source_dir>/pa/flow.json` when present, so
/// the encode side can default `--name` to it. Silent on any failure (file
/// missing, malformed, etc.) — caller falls back to the input basename.
fn read_display_name(source_dir: Option<&Path>) -> Option<String> {
    let dir = source_dir?;
    let bytes = fs::read(dir.join("pa/flow.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("displayName")?.as_str().map(str::to_string)
}

fn derive_name_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("flow")
        .to_string()
}
