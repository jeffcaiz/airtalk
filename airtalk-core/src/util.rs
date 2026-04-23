//! Small helpers shared across provider clients.

/// Truncate to at most `max_chars` characters, appending an ellipsis
/// marker when cut. Used to bound error-context strings so a huge
/// HTML error page or runaway response doesn't blow up logs.
pub fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…[truncated]")
    } else {
        head
    }
}

/// Replace every occurrence of `api_key` in `s` with a placeholder so
/// upstream error bodies can't smuggle the caller's credential into
/// logs. No-op when the key is empty (dev / unset).
pub fn redact_api_key(s: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        return s.to_string();
    }
    s.replace(api_key, "[redacted-api-key]")
}
