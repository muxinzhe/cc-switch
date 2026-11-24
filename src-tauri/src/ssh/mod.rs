pub(crate) mod agent;
mod config;
pub(crate) mod config_file;
mod connection;
pub(crate) mod session_content;
mod transcript;
pub(crate) mod usage;

use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use tauri::{
    ipc::{Channel, IpcResponse},
    State,
};
use tokio::sync::{Mutex, OnceCell};

use crate::config::{get_app_config_dir, read_json_file, write_json_file_private};
use crate::session_manager::model::TranscriptChunk;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Endpoint {
    destination: String,
    port: Option<u16>,
    identity_file: Option<String>,
}

impl Endpoint {
    fn validate(mut self) -> Result<Self, String> {
        self.destination = self.destination.trim().to_string();
        if !config::valid_destination(&self.destination) {
            return Err("Invalid SSH host or user@host".into());
        }
        if self.port == Some(0) {
            return Err("SSH port must be between 1 and 65535".into());
        }
        if let Some(path) = self.identity_file.take() {
            let path = path.trim();
            if !path.is_empty() {
                let path = if let Some(relative) = path.strip_prefix("~/") {
                    crate::config::get_home_dir().join(relative)
                } else {
                    std::path::PathBuf::from(path)
                };
                if !path.is_absolute() {
                    return Err("The SSH identity must be an absolute path or start with ~/".into());
                }
                self.identity_file = Some(path.to_string_lossy().into_owned());
            }
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Host {
    id: String,
    name: String,
    enabled: bool,
    #[serde(flatten)]
    endpoint: Endpoint,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HostInput {
    id: Option<String>,
    name: String,
    enabled: bool,
    #[serde(flatten)]
    endpoint: Endpoint,
}

#[derive(Default)]
pub(crate) struct Registry(Mutex<HashMap<String, Arc<OnceCell<connection::RemoteAgent>>>>);

fn load() -> Result<Vec<Host>, String> {
    let path = get_app_config_dir().join("ssh-hosts.json");
    if path.exists() {
        return read_json_file(&path).map_err(|e| e.to_string());
    }
    Ok(Vec::new())
}

fn persist(list: &[Host]) -> Result<(), String> {
    let path = get_app_config_dir().join("ssh-hosts.json");
    write_json_file_private(&path, &list).map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) async fn ssh_aliases() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| config::aliases(&crate::config::get_home_dir()))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn ssh_hosts(state: State<'_, Registry>) -> Result<Vec<Host>, String> {
    let _guard = state.0.lock().await;
    load()
}

#[tauri::command]
pub(crate) async fn ssh_save_host(
    state: State<'_, Registry>,
    input: HostInput,
) -> Result<Host, String> {
    let endpoint = input.endpoint.validate()?;
    let mut connections = state.0.lock().await;
    let mut hosts = load()?;
    let id = match input.id {
        Some(id) if hosts.iter().any(|host| host.id == id) => id,
        Some(_) => return Err("Unknown SSH host".into()),
        None => uuid::Uuid::new_v4().to_string(),
    };
    if hosts
        .iter()
        .any(|host| host.id != id && host.endpoint == endpoint)
    {
        return Err("This SSH connection is already in the host list".into());
    }

    let name = input.name.trim();
    let host = Host {
        id,
        name: if name.is_empty() {
            endpoint.destination.clone()
        } else {
            name.into()
        },
        enabled: input.enabled,
        endpoint,
    };
    if let Some(existing) = hosts.iter_mut().find(|item| item.id == host.id) {
        *existing = host.clone();
    } else {
        hosts.push(host.clone());
    }
    persist(&hosts)?;
    connections.remove(&host.id);
    Ok(host)
}

#[tauri::command]
pub(crate) async fn ssh_remove_host(state: State<'_, Registry>, id: String) -> Result<(), String> {
    let mut connections = state.0.lock().await;
    let mut hosts = load()?;
    hosts.retain(|host| host.id != id);
    persist(&hosts)?;
    connections.remove(&id);
    Ok(())
}

async fn remote(state: &Registry, host_id: &str) -> Result<connection::RemoteAgent, String> {
    let (endpoint, connection) = {
        let mut connections = state.0.lock().await;
        let host = load()?
            .into_iter()
            .find(|host| host.enabled && host.id == host_id)
            .ok_or("SSH host is disabled or missing")?;
        let connection = connections.entry(host.id).or_default().clone();
        (host.endpoint, connection)
    };
    connection
        .get_or_try_init(|| connection::RemoteAgent::connect(endpoint))
        .await
        .cloned()
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) async fn ssh_connect(
    state: State<'_, Registry>,
    host_id: String,
) -> Result<connection::HostInfo, String> {
    Ok(remote(&state, &host_id).await?.info())
}

#[tauri::command]
pub(crate) async fn ssh_sessions(
    state: State<'_, Registry>,
    host_id: String,
) -> Result<Vec<crate::session_manager::SessionMeta>, String> {
    remote(&state, &host_id)
        .await?
        .sessions()
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) async fn ssh_stream_session_messages(
    state: State<'_, Registry>,
    host_id: String,
    app: String,
    source_path: String,
    on_chunk: Channel,
) -> Result<(), String> {
    // A batch only needs to stay borrowed through serialization.
    let on_batch = on_chunk.clone();
    let result = async {
        remote(&state, &host_id)
            .await?
            .stream_messages(&app, &source_path, move |start, messages| {
                on_batch
                    .send(TranscriptChunk::Messages { start, messages }.body()?)
                    .map_err(Into::into)
            })
            .await
            .map_err(|e| format!("{e:#}"))
    }
    .await;
    let send =
        |chunk: TranscriptChunk<'_>| on_chunk.send(chunk.body()?).map_err(anyhow::Error::from);
    match result {
        Ok(payload_bytes) => {
            send(TranscriptChunk::Done { payload_bytes }).map_err(|e| e.to_string())
        }
        Err(message) => {
            let _ = send(TranscriptChunk::Error { message: &message });
            Err(message)
        }
    }
}
