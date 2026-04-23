#![cfg(windows)]

mod model;
mod secrets;
mod spawn;
mod storage;
mod ui;

const ASR_CRED_TARGET: &str = "airtalk/asr_api_key";
const LLM_CRED_TARGET: &str = "airtalk/llm_api_key";
const ENV_ASR_KEY: &str = "AIRTALK_ASR_API_KEY";
const ENV_LLM_KEY: &str = "AIRTALK_LLM_API_KEY";

// DashScope region presets. The Region ComboBox in the Settings UI
// writes these literal URLs into the two base-url fields when a user
// picks a region. KEEP IN SYNC with the matching constants embedded
// in slint_ui.rs (Slint's slint!{} block can't reference Rust consts,
// so the strings are duplicated).
const MAINLAND_ASR_URL: &str =
    "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation";
const MAINLAND_LLM_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";

pub(crate) use model::{audio_choice_from_config, hotkey_config_from_config};
pub(crate) use spawn::build_spawn_config;
pub(crate) use storage::{load_snapshot, save_audio_choice, save_request};
pub(crate) use ui::{run_settings_window, SettingsEvent};
