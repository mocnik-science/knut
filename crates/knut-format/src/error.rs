use std::fmt::Display;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("truncated data: {0}")]
    Truncated(&'static str),
    #[error("corrupt data: {0}")]
    Corrupt(String),
    #[error("bad magic: {0}")]
    BadMagic(&'static str),
    #[error("checksum mismatch: {0}")]
    Checksum(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("compression error: {0}")]
    Compression(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt(msg: impl Display) -> Error {
    Error::Corrupt(msg.to_string())
}
