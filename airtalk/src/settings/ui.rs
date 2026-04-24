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

/// Build the ComboBox model + selected index.
///
/// Layout, in order:
///   * **0** — Auto, labeled with whatever the OS default resolves to
///     right now (`"Auto (Realtek …)"`), so the user sees the concrete
///     device Auto currently binds to.
///   * **1** — *Only when* the user's preferred `Named(x)` is not in
///     the current enumeration, a synthetic `"<x> [inactive]"` row.
///     Selected by default so the user's sticky choice is visible even
///     while the device is disconnected; re-saving preserves it.
///     Slint ComboBox has no per-item disabled state, so the row is
///     selectable — but selecting the already-selected current choice
///     is a no-op, and picking a real device / Auto overrides it
///     explicitly, which is the whole point.
///   * **2..** (or **1..** if no inactive row) — live devices from cpal,
///     in enumeration order.
fn device_options(current: &AppConfig) -> (Vec<SharedString>, i32) {
    let choice = audio_choice_from_config(current);
    let devices = audio::list_input_devices();

    let auto_label = match audio::current_default_input_name() {
        Some(name) => format!("Auto ({})", audio::display_device_name(&name)),
        None => "Auto (no device)".to_string(),
    };

    let preferred_missing = match &choice {
        DeviceChoice::Named(name) => !devices.iter().any(|d| d == name),
        DeviceChoice::Auto => false,
    };

    let mut options: Vec<SharedString> = Vec::with_capacity(2 + devices.len());
    options.push(auto_label.into());
    if preferred_missing {
        if let DeviceChoice::Named(name) = &choice {
            options.push(format!("{} [inactive]", audio::display_device_name(name)).into());
        }
    }
    for name in &devices {
        options.push(SharedString::from(audio::display_device_name(name)));
    }

    // Match against the raw enumeration (not `options`, which now
    // carries display-stripped labels) so the stored Named(x) — which
    // is always the Windows-full name — still resolves to the right row.
    let selected = match &choice {
        DeviceChoice::Auto => 0,
        DeviceChoice::Named(_) if preferred_missing => 1,
        DeviceChoice::Named(name) => devices
            .iter()
            .position(|d| d == name)
            .map(|idx| (idx as i32) + 1)
            .unwrap_or(0),
    };
    (options, selected)
}

fn selected_device(index: i32, current: &AppConfig) -> String {
    if index <= 0 {
        return "auto".into();
    }
    let choice = audio_choice_from_config(current);
    let devices = audio::list_input_devices();
    let preferred_missing = match &choice {
        DeviceChoice::Named(name) => !devices.iter().any(|d| d == name),
        DeviceChoice::Auto => false,
    };

    // Match the layout built in `device_options`: the inactive row sits
    // at index 1 when present, real devices start one slot later.
    if preferred_missing && index == 1 {
        if let DeviceChoice::Named(name) = &choice {
            return name.clone();
        }
    }
    let device_offset = if preferred_missing { 2 } else { 1 };
    devices
        .get((index as usize).saturating_sub(device_offset))
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
