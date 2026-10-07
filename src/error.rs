//! A single user-facing error type.

use std::fmt;

/// An error with a human readable message (printed as `roc: <message>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.trim_end())
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Error(s)
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Error(s.to_string())
    }
}

impl From<crate::state::StateError> for Error {
    fn from(e: crate::state::StateError) -> Self {
        Error(e.to_string())
    }
}

impl From<crate::paths::MountErrors> for Error {
    fn from(e: crate::paths::MountErrors) -> Self {
        Error(e.to_string())
    }
}

impl From<crate::pool::SelectError> for Error {
    fn from(e: crate::pool::SelectError) -> Self {
        Error(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

/// Result alias.
pub type Result<T> = std::result::Result<T, Error>;
