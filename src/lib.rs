//! Altonic: pure-Rust database data cleaning, subsetting, and replication.
//!
//! Zero external dependencies: JSON, CSV, regex, SHA-256/HMAC, and the PRNG are
//! all implemented in this crate. See SPEC.md for the full design.

pub mod cli;
pub mod config;
pub mod connector;
pub mod crypto;
pub mod csv;
pub mod engine;
pub mod expr;
pub mod generators;
pub mod json;
pub mod regex;
pub mod rng;
pub mod scan;
pub mod schema;
pub mod subset;
pub mod value;

/// Crate-wide error type. Variants distinguish validation (config/schema)
/// failures from runtime failures so the CLI can exit 1 vs 2.
#[derive(Debug)]
pub enum Error {
    Validation(String),
    Runtime(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Validation(m) => write!(f, "validation error: {m}"),
            Error::Runtime(m) => write!(f, "error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Runtime(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Shorthand constructors used throughout the crate.
pub fn verr(msg: impl Into<String>) -> Error {
    Error::Validation(msg.into())
}
pub fn rerr(msg: impl Into<String>) -> Error {
    Error::Runtime(msg.into())
}
