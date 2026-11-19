pub mod clock;
pub mod json;
mod privacy;

use std::collections::BTreeSet;

pub use privacy::{
    guard_shared_text, indexed_secret_policy_reason, mask_secrets, policy_pattern_matches,
    sanitize_automatic_query, secret_label, secret_policy_fingerprint, sensitive_label,
    source_path_problem,
};

pub fn utc_now() -> String {
    clock::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

pub fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn is_uuid4(text: &str) -> bool {
    if text.len() != 36
        || ![8, 13, 18, 23]
            .iter()
            .all(|index| text.as_bytes()[*index] == b'-')
    {
        return false;
    }
    uuid::Uuid::parse_str(text)
        .is_ok_and(|id| id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122)
}

pub fn python_date(text: &str) -> Option<chrono::NaiveDate> {
    for format in ["%Y-%m-%d", "%Y%m%d", "%G-W%V-%u", "%GW%V%u"] {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(text, format) {
            return Some(date);
        }
    }
    if text.len() == 8
        && text.as_bytes().get(4) == Some(&b'-')
        && text.as_bytes().get(5) == Some(&b'W')
    {
        return chrono::NaiveDate::parse_from_str(&format!("{text}-1"), "%G-W%V-%u").ok();
    }
    if text.len() == 7 && text.as_bytes().get(4) == Some(&b'W') {
        return chrono::NaiveDate::parse_from_str(&format!("{text}1"), "%GW%V%u").ok();
    }
    None
}

pub fn rstrip(text: &str) -> &str {
    text.char_indices()
        .rev()
        .find(|(_, character)| {
            !character.is_whitespace() && !matches!(character, '\u{1c}'..='\u{1f}')
        })
        .map(|(index, character)| &text[..index + character.len_utf8()])
        .unwrap_or("")
}

pub fn unique_strings(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

/// Lowercase hexadecimal for raw bytes, independent of digest-array traits.
pub fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}
