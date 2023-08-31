//! knut language: lexer, parser, AST and diagnostics (see `KNUT_LANG.md`).
//!
//! *knut* is short for *knutpunkt*, the Swedish word for ‘junction’ or ‘node’, which is
//! the building blocks of OpenStreetMap data.

pub mod ast;
pub mod dates;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod units;

pub use parser::parse;
