//! Standalone OS custody, strict JSON, Git, privacy, and local dispatch primitives.
pub mod bank_meta;
pub mod capsule;
pub mod dispatch;
pub mod domain;
pub mod error;
pub mod fs;
pub mod git;
pub mod native;
pub mod publication;
pub mod query;
pub mod security;
pub mod strict_json;
pub mod unicode_fold;

pub use error::{ErrorKind, Result, RuntimeError};

pub fn collapse(value: &str) -> String {
    value
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
