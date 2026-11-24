use std::{fs::File, io::Read, path::PathBuf};

use anyhow::{ensure, Context, Result};
use serde::Serialize;
use serde_json::json;
use tauri::State;

use super::{remote, Registry};

use crate::native_config::MAX_BYTES;
use crate::native_config::{ConfigKey, ConfigWrite, RemoteConfigFile, WriteResult};

#[derive(Serialize)]
pub(crate) struct LocalConfigFile {
    path: PathBuf,
    contents: String,
}

#[tauri::command]
pub(crate) async fn ssh_read_config_file(
    state: State<'_, Registry>,
    host_id: String,
    key: ConfigKey,
) -> Result<RemoteConfigFile, String> {
    remote(&state, &host_id)
        .await?
        .rpc("native_config", json!({ "key": key }))
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub(crate) async fn ssh_read_local_config_file(key: ConfigKey) -> Result<LocalConfigFile, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = key.path()?;
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
    draft.validate(key).map_err(|error| error.to_string())?;
    remote(&state, &host_id)
        .await?
        .rpc("native_config", json!({ "key": key, "draft": draft }))
        .await
        .map_err(|error| format!("{error:#}"))
}
