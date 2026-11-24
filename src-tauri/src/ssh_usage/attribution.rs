//! Match Codex session usage to a configured provider using successful HTTP requests
//! in Codex's native trace. Persist only proven identities so they survive trace rotation.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::BufRead,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::Result;
use regex::Regex;
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use url::Url;

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct UsageKey {
    pub request: String,
    pub session: String,
    pub stamp: i64,
}

#[derive(Clone)]
pub(crate) struct Provider {
    pub id: String,
    pub name: String,
}

pub(crate) type Matches = HashMap<UsageKey, Provider>;

struct Candidate {
    key: UsageKey,
    tokens: [i64; 3],
}

pub(crate) struct Evidence {
    // An endpoint maps to None when multiple configured providers share it.
    endpoints: HashMap<Url, Option<Provider>>,
    pending: HashMap<String, BTreeMap<i64, Vec<Candidate>>>,
    counts: HashMap<(String, i64), usize>,
    rollouts: HashMap<String, Vec<PathBuf>>,
    pub cached: Matches,
}

fn endpoint(value: &str, base: bool) -> Option<Url> {
    let mut url = Url::parse(value.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    let mut path = url.path().trim_end_matches('/').to_owned();
    if base {
        path.push_str("/responses");
    }
    url.set_path(&path);
    Some(url)
}

pub(crate) fn init_cache(cache: &Connection) -> rusqlite::Result<()> {
    cache.execute_batch(
        "CREATE TABLE IF NOT EXISTS attribution (
        request_id TEXT NOT NULL, session_id TEXT NOT NULL, created_at INTEGER NOT NULL,
        provider_id TEXT NOT NULL, provider_name TEXT NOT NULL,
        PRIMARY KEY(request_id, session_id, created_at))",
    )
}

pub(crate) fn store(cache: &Connection, matches: &Matches) -> rusqlite::Result<()> {
    let mut insert = cache.prepare_cached(
        "INSERT OR IGNORE INTO attribution
        (request_id,session_id,created_at,provider_id,provider_name) VALUES (?1,?2,?3,?4,?5)",
    )?;
    for (key, provider) in matches {
        insert.execute(params![
            key.request,
            key.session,
            key.stamp,
            provider.id,
            provider.name
        ])?;
    }
    Ok(())
}

impl Evidence {
    pub(crate) fn load(source: &Connection, cache: &Connection) -> Result<Self> {
        let mut endpoints = HashMap::new();
        let mut names = HashMap::new();
        let mut providers = source.prepare(
            "SELECT id,name,category,settings_config FROM providers WHERE app_type='codex'",
        )?;
        let mut rows = providers.query([])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let name: String = row.get(1)?;
            let category: Option<String> = row.get(2)?;
            let settings = serde_json::from_str::<Value>(row.get_ref(3)?.as_str()?).ok();
            let doc = settings
                .as_ref()
                .and_then(|s| s["config"].as_str())
                .and_then(|s| s.parse::<toml::Table>().ok());
            let base = doc
                .as_ref()
                .and_then(|doc| {
                    let active = doc.get("model_provider").and_then(toml::Value::as_str);
                    active
                        .and_then(|id| doc.get("model_providers")?.get(id)?.get("base_url"))
                        .or_else(|| doc.get("base_url"))
                        .or_else(|| doc.get("openai_base_url"))
                        .and_then(toml::Value::as_str)
                })
                .or_else(|| {
                    (category.as_deref() == Some("official"))
                        .then_some("https://chatgpt.com/backend-api/codex")
                });
            if let Some(url) = base.and_then(|v| endpoint(v, true)) {
                endpoints
                    .entry(url)
                    .and_modify(|p| *p = None)
                    .or_insert_with(|| {
                        Some(Provider {
                            id: id.clone(),
                            name: name.clone(),
                        })
                    });
            }
            names.insert(id, name);
        }

        let mut saved = cache.prepare(
            "SELECT request_id,session_id,created_at,provider_id,provider_name FROM attribution",
        )?;
        let mut known = saved
            .query_map([], |row| {
                let id: String = row.get(3)?;
                let name = names.get(&id).cloned().unwrap_or(row.get(4)?);
                Ok((
                    UsageKey {
                        request: row.get(0)?,
                        session: row.get(1)?,
                        stamp: row.get(2)?,
                    },
                    Provider { id, name },
                ))
            })?
            .collect::<rusqlite::Result<Matches>>()?;
        let mut state = Self {
            endpoints,
            pending: HashMap::new(),
            counts: HashMap::new(),
            rollouts: HashMap::new(),
            cached: HashMap::new(),
        };
        let mut candidates = source.prepare("SELECT request_id,session_id,created_at,input_tokens,output_tokens,cache_read_tokens
            FROM proxy_request_logs WHERE app_type='codex' AND data_source='codex_session' AND provider_id='_codex_session' AND session_id IS NOT NULL")?;
        let mut rows = candidates.query([])?;
        while let Some(row) = rows.next()? {
            let key = UsageKey {
                request: row.get(0)?,
                session: row.get(1)?,
                stamp: row.get(2)?,
            };
            *state
                .counts
                .entry((key.session.clone(), key.stamp))
                .or_default() += 1;
            if let Some(provider) = known.remove(&key) {
                state.cached.insert(key, provider);
            } else {
                state
                    .pending
                    .entry(key.session.clone())
                    .or_default()
                    .entry(key.stamp)
                    .or_default()
                    .push(Candidate {
                        key,
                        tokens: [row.get(3)?, row.get(4)?, row.get(5)?],
                    });
            }
        }

        if !state.pending.is_empty() && !state.endpoints.is_empty() {
            static ROLLOUT: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"([0-9a-fA-F-]{36})(?:_[0-9a-fA-F-]{36})?\.jsonl(?:\.zst)?$").unwrap()
            });
            let mut paths = source.prepare("SELECT file_path FROM session_log_sync")?;
            for path in paths.query_map([], |row| row.get::<_, String>(0))? {
                let path = path?;
                if let Some(caps) = ROLLOUT.captures(&path) {
                    let session = &caps[1];
                    if state.pending.contains_key(session) {
                        state
                            .rollouts
                            .entry(session.to_owned())
                            .or_default()
                            .push(PathBuf::from(path));
                    }
                }
            }
        }
        Ok(state)
    }

    pub(crate) fn resolve(&self, codex_home: &Path) -> Matches {
        let mut found = Matches::new();
        if self.pending.is_empty() || self.endpoints.is_empty() {
            return found;
        }
        let Ok(trace) = Connection::open_with_flags(
            codex_home.join("logs_2.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) else {
            return found;
        };
        for (session, by_time) in &self.pending {
            let first = *by_time.first_key_value().unwrap().0;
            let last = *by_time.last_key_value().unwrap().0;
            // Missing and older trace formats are normal; session totals still work.
            let Ok(evidence) = TurnTrace::read(&trace, session, first, last) else {
                continue;
            };
            let mut compactions = BTreeMap::new();
            for (&stamp, candidates) in by_time {
                let count = self.counts[&(session.clone(), stamp)];
                let sampled = evidence
                    .samples
                    .get(&stamp)
                    .filter(|samples| samples.len() == count)
                    .and_then(|samples| unique(samples.iter()))
                    .and_then(Option::as_ref)
                    .and_then(|url| self.endpoints.get(url))
                    .and_then(Option::as_ref);
                if let Some(provider) = sampled {
                    for row in candidates {
                        found.insert(row.key.clone(), provider.clone());
                    }
                } else if count == 1 {
                    compactions.insert(stamp, &candidates[0]);
                }
            }
            // Most sessions only have normal sampling or WebSocket requests.
            // Open a potentially large rollout only after trace evidence puts a
            // pending row between two nearby HTTP calls to the same provider.
            let mut hints = HashMap::new();
            for (turn, calls) in &evidence.calls {
                if calls.len() < 2 {
                    continue;
                }
                let Some(provider) = unique(calls.iter().map(|(_, url)| url))
                    .and_then(|url| self.endpoints.get(url))
                    .and_then(Option::as_ref)
                else {
                    continue;
                };
                for (&stamp, &candidate) in compactions.range(calls[0].0..=calls.last().unwrap().0)
                {
                    let before = calls.partition_point(|(ts, _)| *ts < stamp);
                    let after = calls.partition_point(|(ts, _)| *ts <= stamp);
                    if before > 0
                        && after < calls.len()
                        && stamp.abs_diff(calls[before - 1].0) <= 120
                        && stamp.abs_diff(calls[after].0) <= 120
                    {
                        hints.insert((stamp, turn.as_str()), (candidate, provider));
                    }
                }
            }
            if hints.is_empty() {
                continue;
            }
            let mut confirmed: HashMap<UsageKey, Option<Provider>> = HashMap::new();
            for file in self.rollouts.get(session).into_iter().flatten() {
                read_compactions(file, |stamp, payload| {
                    if payload["thread_id"].as_str() != Some(session) {
                        return;
                    }
                    let Some(turn) = payload["turn_id"].as_str() else {
                        return;
                    };
                    let Some(&(candidate, provider)) = hints.get(&(stamp, turn)) else {
                        return;
                    };
                    let usage = &payload["usage"];
                    if ["input_tokens", "output_tokens", "cached_input_tokens"]
                        .map(|k| usage[k].as_i64())
                        != candidate.tokens.map(Some)
                    {
                        return;
                    }
                    confirmed
                        .entry(candidate.key.clone())
                        .and_modify(|p| {
                            if p.as_ref().is_some_and(|p| p.id != provider.id) {
                                *p = None;
                            }
                        })
                        .or_insert_with(|| Some(provider.clone()));
                });
            }
            found.extend(confirmed.into_iter().filter_map(|(k, p)| Some((k, p?))));
        }
        found
    }
}

fn unique<T: PartialEq>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let first = values.next()?;
    values.all(|v| v == first).then_some(first)
}

#[derive(Default)]
struct TurnTrace {
    samples: HashMap<i64, Vec<Option<Url>>>,
    calls: HashMap<String, Vec<(i64, Url)>>,
}

impl TurnTrace {
    fn read(conn: &Connection, session: &str, first: i64, last: i64) -> Result<Self> {
        static TURN: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r#"\bturn_id="?([0-9a-fA-F-]{36})(?:["\s},]|$)"#).unwrap());
        static HTTP: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?:^|\}: )Request completed method=POST url=(\S+) status=2\d{2}\b")
                .unwrap()
        });
        static SAMPLE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"(?:^|\}: )post sampling token usage\b").unwrap());

        let mut result = Self::default();
        let mut pending: HashMap<String, HashSet<Url>> = HashMap::new();
        let mut stmt = conn.prepare_cached("SELECT ts,target,feedback_log_body FROM logs
            WHERE thread_id=?1 AND ts BETWEEN ?2 AND ?3 AND feedback_log_body IS NOT NULL
            AND target IN ('codex_http_client::client','codex_core::session::turn') ORDER BY ts,ts_nanos,id")?;
        let mut rows = stmt.query(params![
            session,
            first.saturating_sub(600),
            last.saturating_add(120)
        ])?;
        while let Some(row) = rows.next()? {
            let stamp: i64 = row.get(0)?;
            let target = row.get_ref(1)?.as_str()?;
            let body = row.get_ref(2)?.as_str()?;
            let Some(turn) = unique(TURN.captures_iter(body).map(|c| c.get(1).unwrap().as_str()))
            else {
                continue;
            };
            if target == "codex_http_client::client" {
                let Some(url) = HTTP
                    .captures(body)
                    .and_then(|c| endpoint(&c[1], false))
                    .filter(|url| url.path().ends_with("/responses"))
                else {
                    continue;
                };
                result
                    .calls
                    .entry(turn.to_owned())
                    .or_default()
                    .push((stamp, url.clone()));
                pending.entry(turn.to_owned()).or_default().insert(url);
            } else if SAMPLE.is_match(body) {
                // Consume the evidence. A subsequent WebSocket sample cannot inherit the previous address.
                let url = pending
                    .remove(turn)
                    .and_then(|urls| unique(urls.into_iter()));
                result.samples.entry(stamp).or_default().push(url);
            }
        }
        Ok(result)
    }
}

fn read_compactions(file: &Path, mut consume: impl FnMut(i64, &Value)) {
    let Ok(reader) = crate::codex_rollout_file::open_reader(file) else {
        return;
    };
    let mut previous = None;
    for line in reader.lines().map_while(std::result::Result::ok) {
        if !(line.contains("\"token_usage_record\"") || line.contains("\"compacted\"")) {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let stamp = event["timestamp"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp());
        match event["type"].as_str() {
            Some("token_usage_record") => previous = stamp.map(|t| (t, event["payload"].clone())),
            Some("compacted") => {
                if let (Some((time, payload)), Some(stamp)) = (previous.take(), stamp) {
                    if (0..=1).contains(&(stamp - time)) {
                        consume(time, &payload);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::lock_conn;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn trace_matches_sampling_and_verified_compaction_and_survives_rotation() -> Result<()> {
        let home = tempfile::tempdir()?;
        let db = crate::Database::memory()?;
        let conn = lock_conn!(db.conn);
        init_cache(&conn)?;
        let session = "12345678-1234-1234-1234-123456789abc";
        let turn = "a2345678-1234-1234-1234-123456789abc";
        let rollout = home.path().join(format!("rollout-{session}.jsonl"));
        let mut out = std::fs::File::create(&rollout)?;
        for (second, kind) in [
            (20, "token_usage_record"),
            (21, "compacted"),
            (22, "token_usage_record"),
            (23, "compacted"),
        ] {
            writeln!(
                out,
                "{}",
                json!({"type":kind, "timestamp":format!("1970-01-01T00:00:{second}Z"), "payload":{
                    "thread_id": session,"turn_id": turn,"usage":{"input_tokens":12,"output_tokens":8,"cached_input_tokens":4}
                }})
            )?;
        }
        conn.execute(
            "INSERT INTO session_log_sync(file_path,last_modified,last_line_offset,last_synced_at)
            VALUES (?1,0,0,0)",
            [rollout.to_str().unwrap()],
        )?;
        conn.execute("INSERT INTO providers(id,app_type,name,settings_config) VALUES ('relay','codex','Relay',?1)",
            [json!({"config":"model_provider='r'\n[model_providers.r]\nbase_url='https://relay.example/v1/'"}).to_string()])?;
        for (id, stamp, inputs) in [
            ("sampling", 10, 12),
            ("ws", 11, 12),
            ("compact", 20, 12),
            ("wrong-tokens", 22, 13),
        ] {
            conn.execute("INSERT INTO proxy_request_logs(request_id,provider_id,app_type,model,latency_ms,status_code,session_id,created_at,data_source,input_tokens,output_tokens,cache_read_tokens)
               VALUES (?1,'_codex_session','codex','model',1,200,?2,?3,'codex_session',?4,8,4)", params![id, session,stamp,inputs])?;
        }
        let trace_path = home.path().join("logs_2.sqlite");
        let trace = Connection::open(&trace_path)?;
        trace.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,thread_id TEXT,target TEXT,feedback_log_body TEXT)")?;
        for (stamp, http) in [(9, true), (10, false), (11, false), (19, true), (24, true)] {
            let (target, message) = if http {
                ("codex_http_client::client","Request completed method=POST url=https://relay.example/v1/responses?x=1 status=200")
            } else {
                ("codex_core::session::turn", "post sampling token usage")
            };
            trace.execute("INSERT INTO logs(ts,ts_nanos,thread_id,target,feedback_log_body) VALUES (?1,0,?2,?3,?4)",
                params![stamp,session,target,format!("span{{turn_id={turn}}}: {message}")])?;
        }
        drop(trace);
        let found = Evidence::load(&conn, &conn)?.resolve(home.path());
        assert_eq!(found.len(), 2);
        assert!(found.keys().any(|k| k.request == "sampling"));
        assert!(found.keys().any(|k| k.request == "compact"));
        assert!(found.values().all(|p| p.id == "relay"));
        store(&conn, &found)?;
        std::fs::remove_file(trace_path)?;
        conn.execute("DELETE FROM providers WHERE id='relay'", [])?;
        let again = Evidence::load(&conn, &conn)?;
        assert!(again.resolve(home.path()).is_empty());
        assert_eq!(again.cached.len(), 2);
        assert!(again.cached.values().all(|p| p.name == "Relay"));
        Ok(())
    }
}
