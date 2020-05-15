use std::fmt;

#[derive(Debug)]
pub enum Error {
    IOError(std::io::Error),
    NixError(nix::Error),
    NotATTY,
    InvalidColor,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::IOError(error)
    }
}

impl From<nix::Error> for Error {
    fn from(error: nix::Error) -> Self {
        Self::NixError(error)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
