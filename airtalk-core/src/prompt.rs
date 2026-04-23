//! Default LLM system prompt and file-based override.

use std::path::Path;

use anyhow::Context;

const DEFAULT: &str = include_str!("default_prompt.txt");

/// Load the system prompt. Returns the built-in default unless `path`
/// is Some and points to a readable file.
pub fn load(path: Option<&Path>) -> anyhow::Result<String> {
    match path {
        None => Ok(DEFAULT.to_string()),
        Some(p) => std::fs::read_to_string(p)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("reading prompt file {}", p.display())),
    }
}
