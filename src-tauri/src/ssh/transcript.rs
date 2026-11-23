use std::{fmt, io::Read};

use anyhow::{Context, Result};
use serde::{
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer,
};

use crate::session_manager::{
    cache::{CHUNK_MAX_BYTES, CHUNK_MAX_MESSAGES},
    model::SessionMessage,
};

pub(super) fn read(
    reader: impl Read,
    send: impl FnMut(usize, &[SessionMessage]) -> Result<()>,
) -> Result<u64> {
    let mut json = serde_json::Deserializer::from_reader(reader);
    let bytes = json
        .deserialize_map(Transcript(send))
        .context("Invalid remote transcript")?;
    json.end().context("Invalid remote transcript")?;
    Ok(bytes)
}

struct Transcript<F>(F);

impl<'de, F> Visitor<'de> for Transcript<F>
where
    F: FnMut(usize, &[SessionMessage]) -> Result<()>,
{
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a transcript with a messages array")
    }

    fn visit_map<A: MapAccess<'de>>(mut self, mut map: A) -> Result<u64, A::Error> {
        let mut bytes = None;
        while let Some(field) = map.next_key::<String>()? {
            if field == "messages" {
                if bytes.is_some() {
                    return Err(de::Error::duplicate_field("messages"));
                }
                bytes = Some(map.next_value_seed(Messages(&mut self.0))?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        bytes.ok_or_else(|| de::Error::missing_field("messages"))
    }
}

struct Messages<F>(F);

impl<'de, F> DeserializeSeed<'de> for Messages<F>
where
    F: FnMut(usize, &[SessionMessage]) -> Result<()>,
{
    type Value = u64;

    fn deserialize<D: Deserializer<'de>>(self, json: D) -> Result<u64, D::Error> {
        json.deserialize_seq(self)
    }
}

#[derive(Deserialize)]
struct RemoteMessage {
    role: String,
    content: String,
    ts: Option<i64>,
}

impl<'de, F> Visitor<'de> for Messages<F>
where
    F: FnMut(usize, &[SessionMessage]) -> Result<()>,
{
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of messages")
    }

    fn visit_seq<A: SeqAccess<'de>>(mut self, mut seq: A) -> Result<u64, A::Error> {
        let mut batch = Vec::new();
        let mut start = 0;
        let mut batch_bytes = 0;
        let mut total_bytes = 0;
        while let Some(msg) = seq.next_element::<RemoteMessage>()? {
            let size = msg.content.len() + msg.role.len() + 48;
            if !batch.is_empty() && batch_bytes + size > CHUNK_MAX_BYTES {
                (self.0)(start, &batch).map_err(de::Error::custom)?;
                start += batch.len();
                batch.clear();
                batch_bytes = 0;
            }
            batch.push(SessionMessage {
                role: msg.role,
                content: msg.content,
                ts: msg.ts,
                ..SessionMessage::default()
            });
            batch_bytes += size;
            total_bytes += size as u64;
            // Show the first message immediately, then batch to reduce IPC traffic.
            if start == 0 || batch_bytes >= CHUNK_MAX_BYTES || batch.len() >= CHUNK_MAX_MESSAGES {
                (self.0)(start, &batch).map_err(de::Error::custom)?;
                start += batch.len();
                batch.clear();
                batch_bytes = 0;
            }
        }
        if !batch.is_empty() {
            (self.0)(start, &batch).map_err(de::Error::custom)?;
        }
        Ok(total_bytes)
    }
}
