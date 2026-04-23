use anyhow::{Context, Result};

use crate::core_client::SpawnConfig;

use super::secrets::load_secret;
use super::storage::{load_config, materialize_support_files};
use super::{ASR_CRED_TARGET, ENV_ASR_KEY, ENV_LLM_KEY, LLM_CRED_TARGET};

pub fn build_spawn_config() -> Result<SpawnConfig> {
    let config = load_config()?;
    let support = materialize_support_files(&config)?;

    let mut spawn = SpawnConfig::default_sibling()?;
    let asr_key = load_secret(ASR_CRED_TARGET)?
        .or_else(|| std::env::var(ENV_ASR_KEY).ok())
        .context("ASR API key is not configured")?;
    spawn.env.push((ENV_ASR_KEY.into(), asr_key));
    spawn.args.push("--asr-base-url".into());
    spawn.args.push(config.asr.base_url.clone());
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

    Ok(spawn)
}
