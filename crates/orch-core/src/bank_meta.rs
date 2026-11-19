//! Standalone knowledge-chunk frontmatter validation (no Memory Bank coupling).
use crate::{Result, RuntimeError, domain, fs};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::Path,
};

const REQUIRED: [&str; 12] = [
    "id",
    "title",
    "type",
    "status",
    "scope",
    "tags",
    "created",
    "last_verified",
    "review_after",
    "sources",
    "supersedes",
    "superseded_by",
];
const OPTIONAL: [&str; 3] = ["valid_from", "valid_to", "source_digests"];
const TYPES: [&str; 7] = [
    "architecture",
    "constraint",
    "convention",
    "decision",
    "domain",
    "integration",
    "operations",
];
const STATUSES: [&str; 4] = ["active", "archived", "needs-review", "superseded"];

fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::invalid(message)
}

fn string_list(value: &Value, key: &str, allow_empty: bool) -> Result<Vec<String>> {
    let items = value[key]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .filter(|text| !text.trim().is_empty())
                        .map(String::from)
                })
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| invalid(format!("{key} must be a list of non-empty strings")))?;
    if !allow_empty && items.is_empty() {
        return Err(invalid(format!("{key} must not be empty")));
    }
    Ok(items)
}

fn date_of(value: &Value, key: &str) -> Result<chrono::NaiveDate> {
    let text = value[key]
        .as_str()
        .ok_or_else(|| invalid(format!("{key} must be an ISO date string")))?;
    domain::python_date(text).ok_or_else(|| invalid(format!("{key} must use YYYY-MM-DD")))
}

fn valid_id(id: &str) -> bool {
    if let Some(rest) = id.strip_prefix("MEM-") {
        let parts: Vec<&str> = rest.split('-').collect();
        if parts.len() == 2
            && parts[0].len() == 8
            && parts[0].bytes().all(|b| b.is_ascii_digit())
            && parts[1].len() == 8
            && parts[1].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return true;
        }
        if parts.len() == 1
            && !parts[0].is_empty()
            && parts[0].len() <= 8
            && parts[0].bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

fn filename_id(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".md")?;
    let rest = stem.strip_prefix("MEM-")?;
    let mut parts = rest.splitn(3, '-');
    let first = parts.next()?;
    let second = parts.next()?;
    if first.len() == 8
        && first.bytes().all(|b| b.is_ascii_digit())
        && second.len() == 8
        && second.bytes().all(|b| b.is_ascii_hexdigit())
        && parts.next().is_some_and(|slug| !slug.is_empty())
    {
        return Some(format!("MEM-{first}-{second}"));
    }
    if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) && !second.is_empty() {
        return Some(format!("MEM-{first}"));
    }
    None
}

pub fn validate_metadata(root: &Path, path: &str, value: &Value, warnings: bool) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("frontmatter must be an object"))?;
    let missing = REQUIRED
        .into_iter()
        .filter(|key| !object.contains_key(*key))
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        return Err(invalid(format!(
            "missing metadata keys: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let extra = object
        .keys()
        .map(String::as_str)
        .filter(|key| !REQUIRED.contains(key) && !OPTIONAL.contains(key))
        .collect::<BTreeSet<_>>();
    if !extra.is_empty() {
        return Err(invalid(format!(
            "unexpected metadata keys: {}",
            extra.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let Some(bound) = filename_id(name) else {
        return Err(invalid(
            "filename must be MEM-YYYYMMDD-xxxxxxxx-short-slug.md (or legacy MEM-0001-short-slug.md)",
        ));
    };
    let id = value["id"]
        .as_str()
        .filter(|id| valid_id(id))
        .ok_or_else(|| invalid("id must use MEM-YYYYMMDD-xxxxxxxx (or legacy MEM-0001) format"))?;
    if bound != id {
        return Err(invalid("frontmatter id does not match filename id"));
    }
    if value["title"].as_str().is_none_or(|s| s.trim().is_empty()) {
        return Err(invalid("title must be a non-empty string"));
    }
    if !TYPES.contains(&value["type"].as_str().unwrap_or("")) {
        return Err(invalid(format!(
            "type must be one of: {}",
            TYPES.join(", ")
        )));
    }
    let status = value["status"].as_str().unwrap_or("");
    if !STATUSES.contains(&status) {
        return Err(invalid(format!(
            "status must be one of: {}",
            STATUSES.join(", ")
        )));
    }
    string_list(value, "scope", false)?;
    string_list(value, "tags", false)?;
    let sources = string_list(value, "sources", false)?;
    string_list(value, "supersedes", true)?;
    let created = date_of(value, "created")?;
    let verified = date_of(value, "last_verified")?;
    let review = date_of(value, "review_after")?;
    let today = domain::clock::local_today();
    if created > verified {
        return Err(invalid("created must not be later than last_verified"));
    }
    if verified > domain::clock::latest_today() {
        return Err(invalid("last_verified must not be in the future"));
    }
    if review < verified {
        return Err(invalid(
            "review_after must not be earlier than last_verified",
        ));
    }
    if status == "active" && review < today {
        return Err(invalid("active chunk is overdue for review"));
    }
    let start = object
        .contains_key("valid_from")
        .then(|| date_of(value, "valid_from"))
        .transpose()?;
    let end = (!value["valid_to"].is_null())
        .then(|| date_of(value, "valid_to"))
        .transpose()?;
    if let Some(end) = end {
        if start.is_some_and(|start| end < start) {
            return Err(invalid("valid_to must not be earlier than valid_from"));
        }
        if status == "active" && end < today {
            return Err(invalid("active chunk is past its valid_to date"));
        }
    }
    let local = sources
        .iter()
        .filter(|source| !source.starts_with("https://") && !source.starts_with("http://"))
        .map(|source| source.split('#').next().unwrap_or(source))
        .collect::<Vec<_>>();
    if let Some(digests) = value.get("source_digests").filter(|value| !value.is_null()) {
        let rows = digests
            .as_array()
            .ok_or_else(|| invalid("source_digests must be a list"))?;
        for row in rows {
            if row.as_object().is_none_or(|entry| {
                entry.len() != 2 || !entry.contains_key("path") || !entry.contains_key("sha256")
            }) {
                return Err(invalid("each source_digests entry must be {path, sha256}"));
            }
            if row["path"].as_str().is_none_or(str::is_empty) {
                return Err(invalid("source_digests path must be a non-empty string"));
            }
            if !row["sha256"].as_str().is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) {
                return Err(invalid("source_digests sha256 must be a hex digest"));
            }
        }
        if rows
            .iter()
            .any(|row| !local.contains(&row["path"].as_str().unwrap_or("")))
        {
            return Err(invalid(
                "source_digests may only cover paths listed in sources",
            ));
        }
    }
    let replacement = &value["superseded_by"];
    if !replacement.is_null() && replacement.as_str().is_none_or(|id| !valid_id(id)) {
        return Err(invalid("superseded_by must be null or a memory ID"));
    }
    if status == "superseded" && replacement.is_null() {
        return Err(invalid("superseded chunks require superseded_by"));
    }
    if status != "superseded" && !replacement.is_null() {
        return Err(invalid("only superseded chunks may set superseded_by"));
    }
    let terminal =
        ["superseded", "archived"].contains(&status) || end.is_some_and(|end| end < today);
    let canonical_root = fs::canonicalize(root)?;
    for source in local {
        let candidate = canonical_root.join(source);
        let resolved = fs::canonical_or_missing(&candidate)?;
        let inside = resolved == canonical_root || resolved.starts_with(&canonical_root);
        let exists = inside && candidate.exists();
        if !exists && !(warnings && terminal) {
            return Err(invalid(format!("Missing source reference: {source}")));
        }
    }
    Ok(())
}
