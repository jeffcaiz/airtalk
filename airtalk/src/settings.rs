#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

use crate::audio::{self, DeviceChoice};
use crate::core_client::SpawnConfig;
use crate::paths;
use crate::slint_ui::SettingsWindow;

const ASR_CRED_TARGET: &str = "airtalk/asr_api_key";
const LLM_CRED_TARGET: &str = "airtalk/llm_api_key";
const ENV_ASR_KEY: &str = "AIRTALK_ASR_API_KEY";
const ENV_LLM_KEY: &str = "AIRTALK_LLM_API_KEY";


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub asr: AsrConfig,
    pub llm: LlmConfig,
    pub audio: AudioConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsrConfig {
    pub lang: String,
    pub hotwords_content: String,
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            asr: AsrConfig {
                lang: "auto".into(),
                hotwords_content: String::new(),
            },
            llm: LlmConfig {
                enabled: false,
                base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
                model: "qwen-flash".into(),
            },
            audio: AudioConfig {
                input_device: "auto".into(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingsSnapshot {
    pub config: AppConfig,
    pub asr_key_saved: bool,
    pub llm_key_saved: bool,
    pub autostart_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct SaveRequest {
    pub config: AppConfig,
    pub new_asr_key: Option<String>,
    pub clear_asr_key: bool,
    pub new_llm_key: Option<String>,
    pub clear_llm_key: bool,
    pub autostart_enabled: bool,
}

#[derive(Debug)]
pub enum SettingsEvent {
    Applied(SettingsSnapshot),
    Cancelled,
    Failed(String),
}

pub fn load_snapshot() -> Result<SettingsSnapshot> {
    Ok(SettingsSnapshot {
        config: load_config()?,
        asr_key_saved: load_secret(ASR_CRED_TARGET)?.is_some(),
        llm_key_saved: load_secret(LLM_CRED_TARGET)?.is_some(),
        autostart_enabled: crate::autostart::is_enabled(),
    })
}

pub fn load_config() -> Result<AppConfig> {
    let path = paths::config_file()?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }

    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut config: AppConfig =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    normalize_config(&mut config);
    Ok(config)
}

pub fn save_request(req: SaveRequest) -> Result<()> {
    let mut config = req.config;
    normalize_config(&mut config);
    validate_config(&config)?;
    save_config(&config)?;

    if req.clear_asr_key {
        delete_secret(ASR_CRED_TARGET)?;
    }
    if let Some(value) = req.new_asr_key.as_deref() {
        store_secret(ASR_CRED_TARGET, value)?;
    }

    if req.clear_llm_key {
        delete_secret(LLM_CRED_TARGET)?;
    }
    if let Some(value) = req.new_llm_key.as_deref() {
        store_secret(LLM_CRED_TARGET, value)?;
    }

    crate::autostart::set(req.autostart_enabled)?;

    Ok(())
}

pub fn save_audio_choice(choice: &DeviceChoice) -> Result<()> {
    let mut config = load_config()?;
    config.audio.input_device = device_choice_to_config(choice);
    save_config(&config)
}

pub fn audio_choice_from_config(config: &AppConfig) -> DeviceChoice {
    config_to_device_choice(&config.audio.input_device)
}

pub fn build_spawn_config() -> Result<SpawnConfig> {
    let config = load_config()?;
    let support = materialize_support_files(&config)?;

    let mut spawn = SpawnConfig::default_sibling()?;
    let asr_key = load_secret(ASR_CRED_TARGET)?
        .or_else(|| std::env::var(ENV_ASR_KEY).ok())
        .context("ASR API key is not configured")?;
    spawn.env.push((ENV_ASR_KEY.into(), asr_key));
    spawn.args.push("--asr-lang".into());
    spawn.args.push(config.asr.lang.clone());

    if let Some(path) = support.hotwords_file {
        spawn.args.push("--hotwords-file".into());
        spawn.args.push(path.to_string_lossy().into_owned());
    }

    if config.llm.enabled {
        let llm_key = load_secret(LLM_CRED_TARGET)?
            .or_else(|| std::env::var(ENV_LLM_KEY).ok())
            .context("LLM API key is not configured")?;
        spawn.env.push((ENV_LLM_KEY.into(), llm_key));
        spawn.args.push("--llm-base-url".into());
        spawn.args.push(config.llm.base_url.clone());
        spawn.args.push("--llm-model".into());
        spawn.args.push(config.llm.model.clone());
    } else {
        spawn.args.push("--no-llm".into());
    }

    let _ = support;
    Ok(spawn)
}

struct SupportFiles {
    hotwords_file: Option<PathBuf>,
}

fn save_config(config: &AppConfig) -> Result<()> {
    let path = paths::config_file()?;
    let toml = toml::to_string_pretty(config).context("serialize config.toml")?;
    std::fs::write(&path, toml).with_context(|| format!("write {}", path.display()))?;
    materialize_support_files(config)?;
    Ok(())
}

fn materialize_support_files(config: &AppConfig) -> Result<SupportFiles> {
    let hotwords_file = write_optional_file(paths::hotwords_file()?, &config.asr.hotwords_content)?;
    // Clean up any prompt.txt left over from previous versions that exposed
    // a prompt editor in the UI — core now always uses its built-in prompt.
    if let Ok(path) = paths::prompt_file() {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(SupportFiles { hotwords_file })
}

fn write_optional_file(path: PathBuf, raw: &str) -> Result<Option<PathBuf>> {
    let content = raw.trim();
    if content.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("remove empty support file {}", path.display()))?;
        }
        return Ok(None);
    }
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(Some(path))
}

fn validate_config(config: &AppConfig) -> Result<()> {
    if config.asr.lang.trim().is_empty() {
        bail!("ASR language cannot be empty");
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
fn validate_hotwords(content: &str) -> std::result::Result<(), String> {
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
                    "Hotword line {}: character '{}' not allowed. Use letters, digits, spaces, and - _ . + # only — one term per line.",
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

fn normalize_config(config: &mut AppConfig) {
    config.asr.lang = trimmed_or_default(&config.asr.lang, "auto");
    config.asr.hotwords_content = normalize_multiline(&config.asr.hotwords_content);
    config.llm.base_url = trimmed_or_default(
        &config.llm.base_url,
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
    );
    config.llm.model = trimmed_or_default(&config.llm.model, "qwen-flash");
    config.audio.input_device = trimmed_or_default(&config.audio.input_device, "auto");
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

fn device_choice_to_config(choice: &DeviceChoice) -> String {
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

fn device_options(current: &AppConfig) -> (Vec<SharedString>, i32) {
    let mut options: Vec<SharedString> = Vec::with_capacity(1 + audio::list_input_devices().len());
    options.push("Auto (system default)".into());
    options.extend(audio::list_input_devices().into_iter().map(Into::into));

    let selected = match audio_choice_from_config(current) {
        DeviceChoice::Auto => 0,
        DeviceChoice::Named(name) => options
            .iter()
            .position(|s| s.as_str() == name)
            .map(|idx| idx as i32)
            .unwrap_or(0),
    };
    (options, selected)
}

pub(crate) fn run_settings_window() -> Result<Option<SaveRequest>> {
    let snapshot = load_snapshot()?;
    let window = SettingsWindow::new().map_err(|e| anyhow!(e.to_string()))?;
    let weak = window.as_weak();

    let (options, selected) = device_options(&snapshot.config);
    window.set_device_model(ModelRc::new(VecModel::from(options)));
    window.set_device_index(selected);
    window.set_autostart_enabled(snapshot.autostart_enabled);
    window.set_asr_lang(snapshot.config.asr.lang.clone().into());
    window.set_asr_hotwords(snapshot.config.asr.hotwords_content.clone().into());
    window.set_llm_enabled(snapshot.config.llm.enabled);
    window.set_llm_base_url(snapshot.config.llm.base_url.clone().into());
    window.set_llm_model(snapshot.config.llm.model.clone().into());
    window.set_asr_key_saved(snapshot.asr_key_saved);
    window.set_asr_key_pending_clear(false);
    window.set_llm_key_saved(snapshot.llm_key_saved);
    window.set_llm_key_pending_clear(false);
    window.set_status_text("".into());

    let save_flag = Arc::new(AtomicBool::new(false));

    {
        let weak = weak.clone();
        window.on_cancel_requested(move || {
            if let Some(window) = weak.upgrade() {
                let _ = window.hide();
            }
        });
    }
    {
        let weak = weak.clone();
        let save_flag = save_flag.clone();
        window.on_save_requested(move || {
            let Some(window) = weak.upgrade() else { return };
            // Pre-flight validation — keeps the window open on failure
            // so the user doesn't lose their edits to a modal dialog.
            let hotwords = window.get_asr_hotwords().to_string();
            if let Err(msg) = validate_hotwords(&hotwords) {
                window.set_status_text(msg.into());
                return;
            }
            window.set_status_text("".into());
            save_flag.store(true, Ordering::Release);
            let _ = window.hide();
        });
    }

    window.run().map_err(|e| anyhow!(e.to_string()))?;
    if !save_flag.load(Ordering::Acquire) {
        return Ok(None);
    }

    let config = AppConfig {
        asr: AsrConfig {
            lang: window.get_asr_lang().trim().to_string(),
            hotwords_content: window.get_asr_hotwords().to_string(),
        },
        llm: LlmConfig {
            enabled: window.get_llm_enabled(),
            base_url: window.get_llm_base_url().trim().to_string(),
            model: window.get_llm_model().trim().to_string(),
        },
        audio: AudioConfig {
            input_device: selected_device(window.get_device_index(), &snapshot.config),
        },
    };

    // `*-key-input` can carry stale text the user typed before clicking
    // Undo (Slint has no concept of "input is locked" beyond visual
    // disable — the stored string hangs around). Honor the UI state:
    // if the key is currently saved AND not pending-clear, the field
    // was locked, so any buffered text should be ignored.
    let asr_key_locked = snapshot.asr_key_saved && !window.get_asr_key_pending_clear();
    let new_asr_key = if asr_key_locked {
        None
    } else {
        optional_secret(window.get_asr_key_input().to_string())
    };
    let llm_key_locked = snapshot.llm_key_saved && !window.get_llm_key_pending_clear();
    let new_llm_key = if llm_key_locked {
        None
    } else {
        optional_secret(window.get_llm_key_input().to_string())
    };
    Ok(Some(SaveRequest {
        config,
        new_asr_key,
        clear_asr_key: window.get_asr_key_pending_clear(),
        new_llm_key,
        clear_llm_key: window.get_llm_key_pending_clear(),
        autostart_enabled: window.get_autostart_enabled(),
    }))
}

fn selected_device(index: i32, current: &AppConfig) -> String {
    if index <= 0 {
        return "auto".into();
    }
    let devices = audio::list_input_devices();
    devices
        .get((index as usize).saturating_sub(1))
        .cloned()
        .unwrap_or_else(|| current.audio.input_device.clone())
}

fn optional_secret(raw: String) -> Option<String> {
    let trimmed = raw.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn store_secret(target: &str, value: &str) -> Result<()> {
    let mut target_w = widestring(target);
    let mut blob = value.as_bytes().to_vec();
    let cred = CREDENTIALW {
        Flags: CRED_FLAGS(0),
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target_w.as_mut_ptr()),
        Comment: PWSTR::null(),
        LastWritten: Default::default(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: std::ptr::null_mut(),
        TargetAlias: PWSTR::null(),
        UserName: PWSTR::null(),
    };
    unsafe {
        CredWriteW(&cred, 0).ok().context("CredWriteW")?;
    }
    Ok(())
}

fn load_secret(target: &str) -> Result<Option<String>> {
    let target_w = widestring(target);
    let mut raw = std::ptr::null_mut();
    let found = unsafe {
        CredReadW(
            PCWSTR(target_w.as_ptr()),
            CRED_TYPE_GENERIC,
            Some(0),
            &mut raw,
        )
    };
    if found.is_err() {
        return Ok(None);
    }

    let value = unsafe {
        let cred = &*raw;
        let bytes =
            std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize);
        String::from_utf8(bytes.to_vec()).context("Credential Manager payload is not UTF-8")?
    };
    unsafe {
        CredFree(raw as _);
    }
    Ok(Some(value))
}

fn delete_secret(target: &str) -> Result<()> {
    let target_w = widestring(target);
    let res = unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, Some(0)) };
    if res.is_err() {
        return Ok(());
    }
    Ok(())
}

fn widestring(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
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
