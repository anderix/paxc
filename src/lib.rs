//! paxc — a compiler for the pax DSL that emits Power Automate cloud flow definitions.
//!
//! This is the library crate. The binary entry point lives in `src/main.rs`.

pub mod ast;
pub mod emitter;
pub mod interpreter;
pub mod lexer;
pub mod parser;
pub mod resolver;
