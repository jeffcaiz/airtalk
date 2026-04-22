//! Centralized file-system locations for airtalk's on-disk state.
//!
//! Everything lives under `%APPDATA%\airtalk\` (per-user, roams with the
//! profile on corporate domains). Directories are created lazily on
//! first access so the UI doesn't need an explicit init step.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};

/// `%APPDATA%\airtalk\` — the root for config, logs, and future preferences.
pub fn app_data_dir() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA").map_err(|_| anyhow!("APPDATA env var not set"))?;
    let dir = PathBuf::from(appdata).join("airtalk");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

/// `%APPDATA%\airtalk\logs\`. Currently holds `ui.log`, truncated each launch.
pub fn logs_dir() -> Result<PathBuf> {
    let d = app_data_dir()?.join("logs");
    std::fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
    Ok(d)
}

/// `%APPDATA%\airtalk\config.toml` — non-sensitive settings.
pub fn config_file() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("config.toml"))
}

/// `%APPDATA%\airtalk\hotwords.txt` — materialized hotwords content for core.
pub fn hotwords_file() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("hotwords.txt"))
}

/// `%APPDATA%\airtalk\prompt.txt` — materialized prompt content for core.
pub fn prompt_file() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("prompt.txt"))
}
