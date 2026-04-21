//! Persistent preference for the microphone input device.
//!
//! Mirrors koe's `~/.koe/input-device.txt`: a plain-text file with either
//! the literal `auto` (follow the system default device) or a cpal device
//! name. Load always succeeds — missing / corrupt file returns `Auto`.

use anyhow::{Context, Result};

use crate::audio::DeviceChoice;
use crate::paths;

pub fn load() -> DeviceChoice {
    match paths::input_device_pref_file() {
        Ok(p) => match std::fs::read_to_string(&p) {
            Ok(raw) => parse(raw.trim()),
            Err(_) => DeviceChoice::Auto,
        },
        Err(_) => DeviceChoice::Auto,
    }
}

pub fn save(choice: &DeviceChoice) -> Result<()> {
    let path = paths::input_device_pref_file()?;
    let content = match choice {
        DeviceChoice::Auto => "auto".to_string(),
        DeviceChoice::Named(name) => name.clone(),
    };
    std::fs::write(&path, content)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn parse(s: &str) -> DeviceChoice {
    if s.is_empty() || s.eq_ignore_ascii_case("auto") {
        DeviceChoice::Auto
    } else {
        DeviceChoice::Named(s.to_string())
    }
}
