use std::fmt;
use std::io;

/// Crate-wide error type.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Format(&'static str),
    Corrupt(String),
    Unsupported(&'static str),
    NotFound(String),
    Safety(String),
    WriteDenied(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o: {e}"),
            Error::Format(m) => write!(f, "format: {m}"),
            Error::Corrupt(m) => write!(f, "corrupt: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Safety(m) => write!(f, "safety: {m}"),
            Error::WriteDenied(m) => write!(f, "write denied: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
