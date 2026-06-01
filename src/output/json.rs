//! JSON output helper.
use anyhow::Result;
use serde::Serialize;
pub fn to_string<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}
