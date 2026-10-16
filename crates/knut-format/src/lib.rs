//! knut file format: logical model, codecs, block encoding and on-disk layouts.
//!
//! This crate performs no file I/O. See `SPEC.md` in the repository root for the
//! byte-level specification.

pub mod block;
pub mod bloom;
pub mod codec;
pub mod container;
pub mod diff;
mod error;
pub mod hilbert;
pub mod layout;
pub mod model;
pub mod strings;
pub mod time;

pub use error::{Error, Result};
pub use model::*;
