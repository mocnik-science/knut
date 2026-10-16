//! Memory-mapped reader and query engine for `.knutdb` files.
//!
//! All offsets read from the file are bounds checked; corrupt files produce errors, not
//! panics. See `SPEC.md` for the format.

#[cfg(feature = "arrow")]
pub mod arrow;
mod cache;
pub mod container;
mod file;
mod query;

pub use file::{DecodedBlock, OpenOptions, Reader, UserEntry, VerifyReport};
pub use knut_format::{BBox, Member, Meta, ObjType, Str, Tags};
pub use query::{Feature, Filter, Geometry, line_intersects};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Format(#[from] knut_format::Error),
    #[error("io: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt(msg: impl std::fmt::Display) -> Error {
    Error::Format(knut_format::Error::Corrupt(msg.to_string()))
}
