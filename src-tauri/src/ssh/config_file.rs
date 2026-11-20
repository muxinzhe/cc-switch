use std::{fs::File, io::Read, path::PathBuf};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use tauri::State;

use super::{remote, Registry};

const MAX_BYTES: usize = 1024 * 1024;

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
    fn local_path(self) -> Result<PathBuf> {
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

#[derive(Serialize)]
pub(crate) struct LocalConfigFile {
    path: PathBuf,
    contents: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ConfigWrite {
    contents: String,
    revision: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WriteResult {
    backup_path: Option<String>,
}

#[tauri::command]
pub(crate) async fn ssh_read_config_file(
    state: State<'_, Registry>,
    host_id: String,
    key: ConfigKey,
) -> Result<RemoteConfigFile, String> {
    remote(&state, &host_id)
        .await?
        .config_file(key, None)
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub(crate) async fn ssh_read_local_config_file(key: ConfigKey) -> Result<LocalConfigFile, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = key.local_path()?;
        let info = path
            .metadata()
            .context("Cannot read the local application config")?;
        ensure!(
            info.is_file(),
            "The local application config must be a regular file"
        );
        let mut contents = String::new();
        File::open(&path)?
            .take(MAX_BYTES as u64 + 1)
            .read_to_string(&mut contents)
            .context("The local application config must use UTF-8")?;
        ensure!(
            contents.len() <= MAX_BYTES,
            "The application config exceeds 1 MiB"
        );
        Ok(LocalConfigFile { path, contents })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error: anyhow::Error| format!("{error:#}"))
}

#[tauri::command]
pub(crate) async fn ssh_write_config_file(
    state: State<'_, Registry>,
    host_id: String,
    key: ConfigKey,
    draft: ConfigWrite,
) -> Result<WriteResult, String> {
    key.validate(&draft.contents)
        .map_err(|error| error.to_string())?;
    ensure_revision(&draft.revision)?;
    remote(&state, &host_id)
        .await?
        .config_file(key, Some(&draft))
        .await
        .map_err(|error| format!("{error:#}"))
}

fn ensure_revision(revision: &str) -> Result<(), String> {
    if revision.len() == 64 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("Invalid remote configuration revision".into())
    }
}
