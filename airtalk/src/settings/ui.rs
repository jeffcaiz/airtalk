use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::audio::{self, DeviceChoice};
use crate::slint_ui::SettingsWindow;

use super::model::{
    audio_choice_from_config, mode_from_index, mode_to_index, trigger_from_index, trigger_to_index,
    validate_hotwords, AppConfig, AsrConfig, AudioConfig, HotkeyConfig, LlmConfig,
};
use super::storage::{load_snapshot, SaveRequest, SettingsSnapshot};

#[derive(Debug)]
pub enum SettingsEvent {
    Applied(SettingsSnapshot),
    Cancelled,
    Failed(String),
}

pub(crate) fn run_settings_window() -> Result<Option<SaveRequest>> {
    let snapshot = load_snapshot()?;
    let window = SettingsWindow::new().map_err(|e| anyhow!(e.to_string()))?;
    let weak = window.as_weak();

    let (options, selected) = device_options(&snapshot.config);
    window.set_device_model(ModelRc::new(VecModel::from(options)));
    window.set_device_index(selected);
    window.set_instant_record(snapshot.config.audio.instant_record);
    window.set_autostart_enabled(snapshot.autostart_enabled);
    window.set_asr_lang(snapshot.config.asr.lang.clone().into());
    window.set_asr_base_url(snapshot.config.asr.base_url.clone().into());
    window.set_asr_hotwords(snapshot.config.asr.hotwords_content.clone().into());
    // Region ComboBox is a UI convenience preset that rewrites both
    // ASR and LLM base URLs. Derive its initial selection from the
    // LLM URL (single source for the heuristic; if user mixed
    // mainland + intl urls by hand, we prefer the LLM one).
    let region_index = if snapshot.config.llm.base_url.contains("dashscope-intl") {
        1
    } else {
        0
    };
    window.set_region_index(region_index);
    window.set_llm_enabled(snapshot.config.llm.enabled);
    window.set_llm_base_url(snapshot.config.llm.base_url.clone().into());
    window.set_llm_model(snapshot.config.llm.model.clone().into());
    window.set_asr_key_saved(snapshot.asr_key_saved);
    window.set_asr_key_pending_clear(false);
    window.set_llm_key_saved(snapshot.llm_key_saved);
    window.set_llm_key_pending_clear(false);
    window.set_hotkey_trigger_index(trigger_to_index(&snapshot.config.hotkey.trigger));
    window.set_hotkey_mode_index(mode_to_index(&snapshot.config.hotkey.mode));
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
            // Pre-flight validation keeps the window open on failure
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
            base_url: window.get_asr_base_url().trim().to_string(),
        },
        llm: LlmConfig {
            enabled: window.get_llm_enabled(),
            base_url: window.get_llm_base_url().trim().to_string(),
            model: window.get_llm_model().trim().to_string(),
        },
        audio: AudioConfig {
            input_device: selected_device(window.get_device_index(), &snapshot.config),
            instant_record: window.get_instant_record(),
        },
        hotkey: HotkeyConfig {
            trigger: trigger_from_index(window.get_hotkey_trigger_index()),
            mode: mode_from_index(window.get_hotkey_mode_index()),
        },
    };

    // `*-key-input` can carry stale text the user typed before clicking
    // Undo (Slint has no concept of "input is locked" beyond visual
    // disable; the stored string hangs around). Honor the UI state:
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

fn device_options(current: &AppConfig) -> (Vec<SharedString>, i32) {
    let devices = audio::list_input_devices();
    let mut options: Vec<SharedString> = Vec::with_capacity(1 + devices.len());
    options.push("Auto (system default)".into());
    options.extend(devices.into_iter().map(Into::into));

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
