use std::path::PathBuf;

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConfigKey {
    Claude,
    Codex,
    GeminiSettings,
    GeminiEnv,
    OpenCode,
    OpenClaw,
    Hermes,
    Pi,
}

impl ConfigKey {
    pub(crate) fn path(self) -> Result<PathBuf> {
        Ok(match self {
            Self::Claude => crate::config::get_claude_settings_path(),
            Self::Codex => crate::codex_config::get_codex_config_path(),
            Self::GeminiSettings => crate::gemini_config::get_gemini_settings_path(),
            Self::GeminiEnv => crate::gemini_config::get_gemini_env_path(),
            Self::OpenCode => crate::opencode_config::get_opencode_config_path()?,
            Self::OpenClaw => crate::openclaw_config::get_openclaw_config_path(),
            Self::Hermes => crate::hermes_config::get_hermes_config_path(),
            Self::Pi => crate::pi_config::get_pi_models_path()?,
        })
    }

    fn validate(self, contents: &str) -> Result<()> {
        ensure!(
            contents.len() <= MAX_BYTES,
            "The application config exceeds 1 MiB"
        );
        ensure!(
            !contents.contains('\0'),
            "The application config contains a NUL byte"
        );
        match self {
            Self::Claude | Self::GeminiSettings | Self::Pi => ensure!(
                serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(contents)
                    .is_ok(),
                "The application config must be a valid JSON object"
            ),
            Self::OpenCode | Self::OpenClaw => ensure!(
                json5::from_str::<serde_json::Map<String, serde_json::Value>>(contents).is_ok(),
                "The application config must be a valid JSON or JSONC object"
            ),
            Self::Codex => ensure!(
                toml::from_str::<toml::Table>(contents).is_ok(),
                "The application config must be valid TOML"
            ),
            Self::Hermes => ensure!(
                serde_yaml::from_str::<serde_yaml::Mapping>(contents).is_ok(),
                "The application config must be a valid YAML mapping"
            ),
            Self::GeminiEnv => {}
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct RemoteConfigFile {
    path: String,
    contents: String,
    revision: String,
    exists: bool,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ConfigWrite {
    contents: String,
    revision: String,
}

impl ConfigWrite {
    pub(crate) fn validate(&self, key: ConfigKey) -> Result<()> {
        ensure!(
            self.revision.len() == 64 && self.revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid remote configuration revision"
        );
        key.validate(&self.contents)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WriteResult {
    backup_path: Option<String>,
}

#[cfg(all(unix, any(not(feature = "desktop"), test)))]
mod agent;

#[cfg(not(feature = "desktop"))]
pub(crate) use agent::handle;
