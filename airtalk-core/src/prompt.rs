//! Default LLM system prompt and file-based override.

use std::path::Path;

const DEFAULT: &str = include_str!("default_prompt.txt");

/// Load the system prompt. Returns the built-in default unless `path`
/// is Some and points to a readable file.
pub fn load(path: Option<&Path>) -> anyhow::Result<String> {
    match path {
        None => Ok(DEFAULT.to_string()),
        Some(p) => Ok(std::fs::read_to_string(p)?),
    }
}
