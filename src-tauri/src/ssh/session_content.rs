use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::json;
use tauri::{ipc::Response, State};

use super::{remote, Registry};
use crate::session_manager::{
    content::BlockContent,
    model::{ContentRef, ImageRef},
};

#[tauri::command]
pub(crate) async fn ssh_session_block_content(
    state: State<'_, Registry>,
    host_id: String,
    app: String,
    source_path: String,
    content_ref: ContentRef,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<BlockContent, String> {
    remote(&state, &host_id)
        .await?
        .rpc(
            "session_block_content",
            json!({
                "app": app, "sourcePath": source_path, "contentRef": content_ref,
                "offset": offset, "limit": limit
            }),
        )
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub(crate) async fn ssh_session_image(
    state: State<'_, Registry>,
    host_id: String,
    app: String,
    source_path: String,
    image: ImageRef,
) -> Result<Response, String> {
    let bytes: String = remote(&state, &host_id)
        .await?
        .rpc(
            "session_image",
            json!({ "app": app, "sourcePath": source_path, "image": image }),
        )
        .await
        .map_err(|e| format!("{e:#}"))?;
    STANDARD
        .decode(bytes)
        .map(Response::new)
        .map_err(|e| e.to_string())
}
