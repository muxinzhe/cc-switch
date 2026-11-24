use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::session_manager::{
    self, content,
    model::{ContentRef, ImageRef, SessionMessage},
    SessionMeta,
};

pub(super) fn list() -> Vec<SessionMeta> {
    let mut sessions = session_manager::scan_sessions_with_status().sessions;
    for session in &mut sessions {
        session.resume_command = None;
    }
    sessions
}

#[derive(Serialize)]
pub(super) struct Messages {
    messages: Vec<SessionMessage>,
}

pub(super) fn messages(app: &str, source_path: &str) -> Result<Messages> {
    let source = content::validate_source(app, source_path).map_err(anyhow::Error::msg)?;
    let messages =
        session_manager::load_messages(app, &source.load_path()).map_err(anyhow::Error::msg)?;
    Ok(Messages { messages })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockRequest {
    content_ref: ContentRef,
    offset: Option<u32>,
    limit: Option<u32>,
}

pub(super) fn detail(command: &str, mut args: Map<String, Value>) -> Result<Value> {
    let app = args
        .get("app")
        .and_then(Value::as_str)
        .context("Missing session application")?;
    let path = args
        .get("sourcePath")
        .and_then(Value::as_str)
        .context("Missing session source")?;
    let source = content::validate_source(app, path).map_err(anyhow::Error::msg)?;
    match command {
        "session_block_content" => {
            let request: BlockRequest = serde_json::from_value(Value::Object(args))?;
            let text = content::resolve_content_ref(&source, &request.content_ref)
                .map_err(anyhow::Error::msg)?;
            Ok(serde_json::to_value(content::paginate(
                &text,
                request.offset,
                request.limit,
            ))?)
        }
        "session_image" => {
            let image: ImageRef =
                serde_json::from_value(args.remove("image").context("Missing session image")?)?;
            let bytes = content::load_image(&source, &image).map_err(anyhow::Error::msg)?;
            Ok(Value::String(STANDARD.encode(bytes)))
        }
        _ => anyhow::bail!("Unknown session detail command"),
    }
}
