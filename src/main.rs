use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: paxc <file.pax>");
        process::exit(2);
    }
    eprintln!("paxc: not yet implemented (input: {})", args[1]);
    process::exit(1);
}
