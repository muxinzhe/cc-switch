use std::{fmt, io::Read};

use anyhow::{Context, Result};
use serde::{
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserializer,
};

use crate::session_manager::{
    cache::{serialized_message_len, CHUNK_MAX_BYTES, CHUNK_MAX_MESSAGES},
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
        while let Some(msg) = seq.next_element::<SessionMessage>()? {
            let size = serialized_message_len(&msg);
            if !batch.is_empty() && batch_bytes + size > CHUNK_MAX_BYTES {
                (self.0)(start, &batch).map_err(de::Error::custom)?;
                start += batch.len();
                batch.clear();
                batch_bytes = 0;
            }
            batch.push(msg);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::model::SessionBlock;

    #[test]
    fn streams_rich_messages_in_bounded_batches_without_losing_wire_fields() {
        let mut expected: Vec<SessionMessage> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/sessions/claude.messages.json"
        ))
        .unwrap();
        let large = "界".repeat(CHUNK_MAX_BYTES / 3 + 1);
        expected.insert(
            1,
            SessionMessage::from_blocks("user", None, vec![SessionBlock::text(large)]),
        );
        let wire = serde_json::to_vec(&serde_json::json!({ "messages": expected })).unwrap();
        let mut received = Vec::new();
        let size = read(wire.as_slice(), |start, batch| {
            assert_eq!(start, received.len());
            assert!(
                batch.len() == 1
                    || batch.iter().map(serialized_message_len).sum::<usize>() <= CHUNK_MAX_BYTES
            );
            received.extend_from_slice(batch);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            size,
            expected.iter().map(serialized_message_len).sum::<usize>() as u64
        );
        assert_eq!(
            serde_json::to_value(received).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}
