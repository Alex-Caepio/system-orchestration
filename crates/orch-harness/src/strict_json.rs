use crate::Result;
use serde_json::Value;
pub fn parse(bytes: &[u8], limit: usize) -> Result<Value> {
    orch_host::strict_json::parse(bytes, limit)
}
