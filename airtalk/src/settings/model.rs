use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::audio::DeviceChoice;
use crate::hotkey;

use super::{MAINLAND_ASR_URL, MAINLAND_LLM_URL};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub asr: AsrConfig,
    pub llm: LlmConfig,
    pub audio: AudioConfig,
    // serde(default) keeps old config.toml files (written before the
    // hotkey section existed) parsing cleanly; missing section falls
    // back to Right Alt + Combo.
    #[serde(default)]
    pub hotkey: HotkeyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsrConfig {
    pub lang: String,
    pub hotwords_content: String,
    // `serde(default)` so existing config.toml files (written before
    // this field existed) still deserialize; missing value falls
    // back to the DashScope mainland endpoint.
    #[serde(default = "default_asr_base_url")]
    pub base_url: String,
}

fn default_asr_base_url() -> String {
    MAINLAND_ASR_URL.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    pub input_device: String,
    // `serde(default)` so existing config.toml files (written before
    // this field existed) still deserialize; missing value falls back
    // to false (pause/play per session, mic indicator only while
    // recording). True = keep the capture stream running continuously
    // for press-to-first-sample with no warm-up; Windows shows
    // mic-in-use whenever AirTalk is running.
    #[serde(default)]
    pub instant_record: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    // Stored as strings (see `TRIGGERS` / `MODES` tables below) rather
    // than enum variants so the TOML stays human-readable and robust
    // to enum reordering.
    #[serde(default = "default_hotkey_trigger")]
    pub trigger: String,
    #[serde(default = "default_hotkey_mode")]
    pub mode: String,
}

fn default_hotkey_trigger() -> String {
    "right_alt".into()
}

fn default_hotkey_mode() -> String {
    "combo".into()
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            trigger: default_hotkey_trigger(),
            mode: default_hotkey_mode(),
        }
    }
}

// Maps between the persisted strings, the UI ComboBox index, and the
// `hotkey::Trigger` / `hotkey::Mode` enum. The order of these tables
// is the order shown in the ComboBox; must stay in sync with the
// hardcoded `model:` lists in slint_ui.rs.
const TRIGGERS: &[(&str, hotkey::Trigger)] = &[
    ("right_alt", hotkey::Trigger::RightAlt),
    ("left_alt", hotkey::Trigger::LeftAlt),
    ("right_ctrl", hotkey::Trigger::RightCtrl),
    ("left_ctrl", hotkey::Trigger::LeftCtrl),
    ("right_shift", hotkey::Trigger::RightShift),
    ("left_shift", hotkey::Trigger::LeftShift),
    ("right_win", hotkey::Trigger::RightWin),
    ("left_win", hotkey::Trigger::LeftWin),
    ("caps_lock", hotkey::Trigger::CapsLock),
];

const MODES: &[(&str, hotkey::Mode)] = &[
    ("combo", hotkey::Mode::Combo),
    ("hold", hotkey::Mode::Hold),
    ("tap", hotkey::Mode::Tap),
];

pub fn hotkey_config_from_config(config: &AppConfig) -> hotkey::Config {
    let trigger = TRIGGERS
        .iter()
        .find_map(|(k, t)| (*k == config.hotkey.trigger).then_some(*t))
        .unwrap_or(hotkey::Trigger::RightAlt);
    let mode = MODES
        .iter()
        .find_map(|(k, m)| (*k == config.hotkey.mode).then_some(*m))
        .unwrap_or(hotkey::Mode::Combo);
    hotkey::Config { trigger, mode }
}

pub fn audio_choice_from_config(config: &AppConfig) -> DeviceChoice {
    config_to_device_choice(&config.audio.input_device)
}

pub(crate) fn trigger_to_index(key: &str) -> i32 {
    TRIGGERS
        .iter()
        .position(|(k, _)| *k == key)
        .map(|i| i as i32)
        .unwrap_or(0)
}

pub(crate) fn mode_to_index(key: &str) -> i32 {
    MODES
        .iter()
        .position(|(k, _)| *k == key)
        .map(|i| i as i32)
        .unwrap_or(0)
}

pub(crate) fn trigger_from_index(idx: i32) -> String {
    TRIGGERS
        .get(idx.max(0) as usize)
        .map(|(k, _)| (*k).to_string())
        .unwrap_or_else(default_hotkey_trigger)
}

pub(crate) fn mode_from_index(idx: i32) -> String {
    MODES
        .get(idx.max(0) as usize)
        .map(|(k, _)| (*k).to_string())
        .unwrap_or_else(default_hotkey_mode)
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            asr: AsrConfig {
                lang: "auto".into(),
                hotwords_content: String::new(),
                base_url: MAINLAND_ASR_URL.into(),
            },
            llm: LlmConfig {
                enabled: false,
                base_url: MAINLAND_LLM_URL.into(),
                model: "qwen-flash".into(),
            },
            audio: AudioConfig {
                input_device: "auto".into(),
                instant_record: false,
            },
            hotkey: HotkeyConfig::default(),
        }
    }
}

pub(crate) fn validate_config(config: &AppConfig) -> Result<()> {
    if config.asr.lang.trim().is_empty() {
        bail!("ASR language cannot be empty");
    }
    if config.asr.base_url.trim().is_empty() {
        bail!("ASR base URL cannot be empty");
    }
    if let Err(msg) = validate_hotwords(&config.asr.hotwords_content) {
        bail!("{msg}");
    }
    if config.llm.enabled {
        if config.llm.base_url.trim().is_empty() {
            bail!("LLM base URL cannot be empty when cleanup is enabled");
        }
        if config.llm.model.trim().is_empty() {
            bail!("LLM model cannot be empty when cleanup is enabled");
        }
    }
    Ok(())
}

/// Validate hotword file contents: one term per line, restricted
/// character set. Core auto-wraps the list with a "拼写保留：" (or
/// "Preserve spelling:") prefix, so users must not sneak their own
/// prefix or punctuation in.
///
/// Allowed: Unicode letters/digits, single inner spaces, and `-_.+#`.
pub(crate) fn validate_hotwords(content: &str) -> std::result::Result<(), String> {
    for (idx, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for ch in line.chars() {
            let ok = ch.is_alphanumeric()
                || ch == ' '
                || ch == '-'
                || ch == '_'
                || ch == '.'
                || ch == '+'
                || ch == '#';
            if !ok {
                return Err(format!(
                    "Hotword line {}: character '{}' not allowed. Use letters, digits, spaces, and - _ . + # only - one term per line.",
                    idx + 1,
                    ch
                ));
            }
        }
        if line.contains("  ") {
            return Err(format!(
                "Hotword line {} has multiple consecutive spaces.",
                idx + 1
            ));
        }
    }
    Ok(())
}

pub(crate) fn normalize_config(config: &mut AppConfig) {
    config.asr.lang = trimmed_or_default(&config.asr.lang, "auto");
    config.asr.hotwords_content = normalize_multiline(&config.asr.hotwords_content);
    config.asr.base_url = trimmed_or_default(&config.asr.base_url, MAINLAND_ASR_URL);
    config.llm.base_url = trimmed_or_default(&config.llm.base_url, MAINLAND_LLM_URL);
    config.llm.model = trimmed_or_default(&config.llm.model, "qwen-flash");
    config.audio.input_device = trimmed_or_default(&config.audio.input_device, "auto");
    // Reject unknown keys; fall back to defaults so a hand-edited
    // config.toml with a typo doesn't leave the user stuck with an
    // unresponsive hotkey.
    if !TRIGGERS.iter().any(|(k, _)| *k == config.hotkey.trigger) {
        config.hotkey.trigger = default_hotkey_trigger();
    }
    if !MODES.iter().any(|(k, _)| *k == config.hotkey.mode) {
        config.hotkey.mode = default_hotkey_mode();
    }
}

pub(crate) fn device_choice_to_config(choice: &DeviceChoice) -> String {
    match choice {
        DeviceChoice::Auto => "auto".into(),
        DeviceChoice::Named(name) => name.clone(),
    }
}

fn config_to_device_choice(raw: &str) -> DeviceChoice {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        DeviceChoice::Auto
    } else {
        DeviceChoice::Named(trimmed.to_string())
    }
}

fn trimmed_or_default(value: &str, default: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_multiline(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotwords_ok_simple_terms() {
        let input = "React\nVite\nTypeScript\nNode.js\nC++\nC#\n接口\n函数";
        assert!(validate_hotwords(input).is_ok());
    }

    #[test]
    fn hotwords_ok_inner_space_and_trailing_blank() {
        let input = "Model 3\niPhone 15 Pro\n";
        assert!(validate_hotwords(input).is_ok());
    }

    #[test]
    fn hotwords_ok_comment_and_blank_lines_ignored() {
        let input = "# domain terms\n\nReact\n\n# trailing comment\nVite\n";
        assert!(validate_hotwords(input).is_ok());
    }

    #[test]
    fn hotwords_rejects_user_prefix() {
        let err = validate_hotwords("拼写保留：React, Vite").unwrap_err();
        assert!(err.contains("line 1"), "got: {err}");
        assert!(err.contains("：") || err.contains(","), "got: {err}");
    }

    #[test]
    fn hotwords_rejects_slash() {
        let err = validate_hotwords("a/b").unwrap_err();
        assert!(err.contains("'/'"));
    }

    #[test]
    fn hotwords_rejects_brace() {
        let err = validate_hotwords("p{").unwrap_err();
        assert!(err.contains("'{'"));
    }

    #[test]
    fn hotwords_rejects_double_space() {
        let err = validate_hotwords("Model  3").unwrap_err();
        assert!(err.contains("consecutive spaces"));
    }

    #[test]
    fn hotwords_rejects_colon() {
        assert!(validate_hotwords("a:b").is_err());
        assert!(validate_hotwords("a：b").is_err());
    }
}
