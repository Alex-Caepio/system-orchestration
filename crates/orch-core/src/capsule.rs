//! Delegation capsule validation contract.
use crate::{Result, domain};
use serde_json::{Value, json};

const SECTIONS: &[&str] = &[
    "objective",
    "output format",
    "tool and source guidance",
    "boundaries",
    "decisions and assumptions",
];

pub fn validate_capsule_text(content: &str) -> Result<Vec<String>> {
    if content.trim().is_empty() {
        return Ok(vec!["capsule is empty".into()]);
    }
    let mut problems = Vec::new();
    let length = content.chars().count();
    if length > 8000 {
        problems.push(format!("capsule exceeds 8000 characters ({length})"));
    }
    let lowered = content.to_lowercase();
    for section in SECTIONS {
        if !lowered.contains(section) {
            problems.push(format!("missing mandatory section: {section}"));
        }
    }
    if let Err(error) = domain::guard_shared_text("delegation capsule", &[content.into()]) {
        problems.push(error.to_string());
    }
    Ok(problems)
}

pub fn capsule_report(content: &str) -> Result<Value> {
    let problems = validate_capsule_text(content)?;
    Ok(json!({
        "valid": problems.is_empty(),
        "length": content.chars().count(),
        "problems": problems,
    }))
}
