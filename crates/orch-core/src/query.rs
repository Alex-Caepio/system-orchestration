use sha2::{Digest, Sha256};

pub fn sha256(bytes: &[u8]) -> String {
    crate::domain::hex_lower(Sha256::digest(bytes))
}

pub fn chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

pub fn folded(value: &str) -> String {
    crate::unicode_fold::fold(value)
}
