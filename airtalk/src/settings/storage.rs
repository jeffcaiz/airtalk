use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::audio::DeviceChoice;
use crate::paths;

use super::model::{device_choice_to_config, normalize_config, validate_config, AppConfig};
use super::secrets::{delete_secret, load_secret, store_secret};
use super::{ASR_CRED_TARGET, LLM_CRED_TARGET};

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

pub fn load_snapshot() -> Result<SettingsSnapshot> {
    Ok(SettingsSnapshot {
        config: load_config()?,
        asr_key_saved: load_secret(ASR_CRED_TARGET)?.is_some(),
        llm_key_saved: load_secret(LLM_CRED_TARGET)?.is_some(),
        autostart_enabled: crate::autostart::is_enabled(),
    })
}

pub(crate) fn load_config() -> Result<AppConfig> {
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

pub(crate) struct SupportFiles {
    pub hotwords_file: Option<PathBuf>,
}

pub(crate) fn materialize_support_files(config: &AppConfig) -> Result<SupportFiles> {
    let hotwords_file = write_optional_file(paths::hotwords_file()?, &config.asr.hotwords_content)?;
    // Clean up any prompt.txt left over from previous versions that exposed
    // a prompt editor in the UI; core now always uses its built-in prompt.
    if let Ok(path) = paths::prompt_file() {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(SupportFiles { hotwords_file })
}

fn save_config(config: &AppConfig) -> Result<()> {
    let path = paths::config_file()?;
    let toml = toml::to_string_pretty(config).context("serialize config.toml")?;
    std::fs::write(&path, toml).with_context(|| format!("write {}", path.display()))?;
    materialize_support_files(config)?;
    Ok(())
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
