use std::{
    collections::VecDeque,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

use anyhow::{ensure, Context, Result};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use super::{remote, Registry};
use crate::{
    database::{lock_conn, Database},
    store::AppState,
};

pub(super) const LOG_COLUMNS: &str = "request_id,provider_id,app_type,model,request_model,pricing_model,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_token_semantics,input_cost_usd,output_cost_usd,cache_read_cost_usd,cache_creation_cost_usd,total_cost_usd,latency_ms,first_token_ms,duration_ms,status_code,error_message,session_id,provider_type,is_streaming,cost_multiplier,created_at,data_source";
pub(super) const ROLLUP_COLUMNS: &str = "provider_id,date,app_type,model,request_model,pricing_model,request_count,success_count,input_tokens,output_tokens,cache_read_tokens,cache_creation_tokens,input_token_semantics,total_cost_usd,avg_latency_ms";

#[derive(Deserialize)]
pub(super) struct Rows {
    logs: Vec<Vec<Value>>,
    rollups: Vec<Vec<Value>>,
    #[serde(default)]
    codex_providers: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct CodexAttributions {
    requests: Vec<(String, String, i64, String)>,
    providers: Vec<(String, String)>,
}

type Snapshot = (String, Arc<Database>);

#[derive(Default)]
struct SnapshotCache {
    recent: VecDeque<Snapshot>,
    latest_local: Option<Snapshot>,
    latest_all: Option<Snapshot>,
}

#[derive(Default)]
pub struct Snapshots(Mutex<SnapshotCache>);

#[derive(Serialize)]
pub(crate) struct SnapshotInfo {
    id: String,
    failures: Vec<String>,
}

impl Snapshots {
    pub(crate) fn get(&self, id: &str) -> Option<Arc<Database>> {
        let snapshots = self.0.lock().unwrap();
        snapshots
            .recent
            .iter()
            .chain(snapshots.latest_local.iter())
            .chain(snapshots.latest_all.iter())
            .find(|(key, _)| key == id)
            .map(|(_, db)| Arc::clone(db))
    }

    fn insert(&self, db: Database, include_local: bool, include_remote: bool) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let mut snapshots = self.0.lock().unwrap();
        let snapshot = (id.clone(), Arc::new(db));
        // The sidebar uses the all-machine snapshot while the dashboard may
        // refresh a single remote host at a faster cadence.
        match (include_local, include_remote) {
            (true, true) => snapshots.latest_all = Some(snapshot.clone()),
            (true, false) => snapshots.latest_local = Some(snapshot.clone()),
            (false, _) => {}
        }
        snapshots.recent.push_back(snapshot);
        // Keep recent generations so requests already dispatched by the
        // dashboard can finish even if it has switched to a newer snapshot.
        if snapshots.recent.len() > 4 {
            snapshots.recent.pop_front();
        }
        id
    }
}

#[tauri::command]
pub(crate) async fn ssh_usage_snapshot(
    state: State<'_, Registry>,
    snapshots: State<'_, Snapshots>,
    app: State<'_, AppState>,
    host_ids: Vec<String>,
    include_local: bool,
    force_scan: Option<bool>,
) -> Result<SnapshotInfo, String> {
    let scan =
        force_scan.unwrap_or_default() || crate::settings::get_settings().session_auto_sync_enabled;
    let scope: std::collections::BTreeSet<_> = host_ids.into_iter().collect();
    let include_remote = !scope.is_empty();
    let names = super::load()?;
    let reports = futures::future::join_all(scope.into_iter().map(|id| {
        let state = &state;
        let name = names
            .iter()
            .find(|host| host.id == id)
            .map(|host| host.name.clone())
            .unwrap_or_else(|| id.clone());
        async move {
            let outcome = match remote(state, &id).await {
                Ok(cli) => cli
                    .usage_rows(scan)
                    .await
                    .map_err(|error| format!("{error:#}")),
                Err(error) => Err(error),
            };
            (id, name, outcome)
        }
    }))
    .await;
    let (mut rows, mut failures) = (Vec::new(), Vec::new());
    for (id, name, outcome) in reports {
        match outcome {
            Ok(data) => rows.push((id, data)),
            Err(error) => failures.push(format!("{name}: {error}")),
        }
    }

    if !include_local && rows.is_empty() && !failures.is_empty() {
        return Err(failures.join("\n"));
    }

    let local = Arc::clone(&app.db);
    let db =
        tauri::async_runtime::spawn_blocking(move || create_snapshot(&local, include_local, rows))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:#}"))?;
    Ok(SnapshotInfo {
        id: snapshots.insert(db, include_local, include_remote),
        failures,
    })
}

pub(super) fn create_snapshot(
    local: &Database,
    include_local: bool,
    machines: Vec<(String, Rows)>,
) -> Result<Database> {
    let mut conn = if include_local {
        local.snapshot_to_memory()?
    } else {
        let conn = Connection::open_in_memory()?;
        Database::create_tables_on_conn(&conn)?;
        copy_model_pricing(local, &conn)?;
        conn
    };
    // No update hook is installed on this connection. Imports and metric
    // queries operate on this temporary snapshot, outside desktop app data.
    let tx = conn.transaction()?;
    if include_local {
        let path = lock_conn!(local.conn).path().map(str::to_owned);
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            let codex_home = crate::codex_config::get_codex_config_dir();
            match read_local_attributions(Path::new(&path), &codex_home) {
                Ok(Some(matches)) => apply_local_attributions(&tx, matches)?,
                Ok(None) => {}
                Err(error) => log::warn!("Could not attribute local Codex usage: {error:#}"),
            }
        }
    }
    for (machine, rows) in machines {
        let mut insert_provider = tx.prepare(
            "INSERT INTO providers (id, app_type, name, settings_config)
             VALUES (?1, 'codex', ?2, '{}')",
        )?;
        for (id, name) in rows.codex_providers {
            insert_provider.execute(rusqlite::params![format!("ssh:{machine}:{id}"), name])?;
        }
        append(
            &tx,
            "proxy_request_logs",
            LOG_COLUMNS,
            rows.logs,
            &machine,
            2,
        )?;
        append(
            &tx,
            "usage_daily_rollups",
            ROLLUP_COLUMNS,
            rows.rollups,
            &machine,
            1,
        )?;
    }
    tx.commit()?;
    // The remote CLI can have an older pricing catalog. Use desktop prices to
    // fill gaps before any aggregate or per-request query sees the snapshot.
    Database::backfill_missing_usage_costs_on_conn(&conn, None)?;
    Ok(Database {
        conn: Mutex::new(conn),
        log_count_cache: Mutex::new(None),
    })
}

fn read_local_attributions(
    database: &Path,
    codex_home: &Path,
) -> Result<Option<CodexAttributions>> {
    if !codex_home.join("logs_2.sqlite").is_file()
        && !database
            .with_file_name("codex-usage-attribution.db")
            .is_file()
    {
        return Ok(None);
    }

    let mut command = Command::new("python3");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command
        .args([
            "-I",
            "-c",
            include_str!("usage_snapshot.py"),
            "--attributions",
        ])
        .arg(database)
        .arg(codex_home)
        .output()
        .context("Starting the Codex attribution script with Python 3")?;
    ensure!(
        output.status.success(),
        "Codex attribution script returned {}",
        output.status
    );
    Ok(Some(serde_json::from_slice(&output.stdout)?))
}

fn apply_local_attributions(tx: &Transaction<'_>, matches: CodexAttributions) -> Result<()> {
    let mut insert_provider = tx.prepare(
        "INSERT OR IGNORE INTO providers (id, app_type, name, settings_config)
         VALUES (?1, 'codex', ?2, '{}')",
    )?;
    for (id, name) in matches.providers {
        insert_provider.execute(params![id, name])?;
    }
    let mut update = tx.prepare(
        "UPDATE proxy_request_logs SET provider_id = ?4
         WHERE request_id = ?1 AND session_id = ?2 AND created_at = ?3
         AND app_type = 'codex' AND data_source = 'codex_session' AND provider_id = '_codex_session'",
    )?;
    for (request, session, stamp, provider) in matches.requests {
        update.execute(params![request, session, stamp, provider])?;
    }
    Ok(())
}

fn copy_model_pricing(local: &Database, snapshot: &Connection) -> Result<()> {
    const COLUMNS: &str = "model_id, display_name, input_cost_per_million, output_cost_per_million, cache_read_cost_per_million, cache_creation_cost_per_million";
    let local = lock_conn!(local.conn);
    let mut select = local.prepare(&format!("SELECT {COLUMNS} FROM model_pricing"))?;
    let prices = select.query_map([], |row| {
        Ok([
            row.get::<_, String>(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ])
    })?;
    let mut insert = snapshot.prepare(&format!(
        "INSERT INTO model_pricing ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    ))?;
    for price in prices {
        insert.execute(params_from_iter(price?))?;
    }
    Ok(())
}

fn append(
    tx: &Transaction<'_>,
    table: &str,
    columns: &str,
    rows: Vec<Vec<Value>>,
    machine: &str,
    prefix_count: usize,
) -> Result<()> {
    let count = columns.split(',').count();
    let params = vec!["?"; count].join(",");
    let mut insert = tx.prepare(&format!(
        "INSERT INTO {table} ({columns}) VALUES ({params})"
    ))?;
    for row in rows {
        ensure!(
            row.len() == count,
            "Unexpected column count in remote usage"
        );
        let values = row
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                if index < prefix_count {
                    let id = value.as_str().context("Invalid remote usage identifier")?;
                    return Ok(SqlValue::Text(format!("ssh:{machine}:{id}")));
                }
                match value {
                    Value::Null => Ok(SqlValue::Null),
                    Value::String(text) => Ok(SqlValue::Text(text)),
                    Value::Number(value) => value
                        .as_i64()
                        .map(SqlValue::Integer)
                        .context("Invalid remote usage number"),
                    _ => anyhow::bail!("Invalid remote usage field"),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        insert.execute(params_from_iter(values))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use serde_json::json;

    #[test]
    fn remote_only_refreshes_keep_the_sidebar_snapshot_available() -> Result<()> {
        let snapshots = Snapshots::default();
        let all_machines = snapshots.insert(Database::memory()?, true, true);
        for _ in 0..5 {
            snapshots.insert(Database::memory()?, false, true);
        }
        assert!(snapshots.get(&all_machines).is_some());
        Ok(())
    }

    fn session_usage(id: &str, model: &str, cost: &str, multiplier: &str) -> Vec<Value> {
        json!([
            id,
            "_codex_session",
            "codex",
            model,
            null,
            model,
            1_000_000,
            1_000_000,
            0,
            0,
            0,
            "0",
            "0",
            "0",
            "0",
            cost,
            0,
            null,
            null,
            200,
            null,
            null,
            null,
            0,
            multiplier,
            1_700_000_000,
            "codex_session"
        ])
        .as_array()
        .unwrap()
        .clone()
    }

    #[test]
    fn snapshots_fill_missing_costs_before_summary_using_desktop_prices() -> Result<()> {
        let local = Database::memory()?;
        lock_conn!(local.conn).execute(
            "INSERT INTO model_pricing VALUES ('test-ssh-model', 'Test SSH Model', '2', '8', '0', '0')",
            [],
        )?;
        let host_id = "12345678-1234-1234-1234-123456789abc";
        for include_local in [false, true] {
            let snapshot = create_snapshot(
                &local,
                include_local,
                vec![(
                    host_id.to_owned(),
                    Rows {
                        logs: vec![
                            session_usage("unpriced", "test-ssh-model", "0", "1.5"),
                            session_usage("priced", "test-ssh-model", "3", "1"),
                            session_usage("free", "test-ssh-model", "0", "0"),
                            session_usage("unknown", "zz-model-without-a-price", "0", "1"),
                        ],
                        rollups: vec![],
                        codex_providers: vec![],
                    },
                )],
            )?;

            let summary = snapshot.get_usage_summary(None, None, None, None, None)?;
            assert_eq!(summary.total_requests, 4);
            assert_eq!(summary.total_cost.parse::<Decimal>()?, Decimal::from(18));
            for (id, expected) in [("unpriced", 15), ("priced", 3), ("free", 0), ("unknown", 0)] {
                let detail = snapshot
                    .get_request_detail(&format!("ssh:{host_id}:{id}"))?
                    .expect("remote request");
                assert_eq!(
                    detail.total_cost_usd.parse::<Decimal>()?,
                    Decimal::from(expected)
                );
            }
        }
        assert_eq!(
            local
                .get_usage_summary(None, None, None, None, None)?
                .total_requests,
            0
        );
        Ok(())
    }

    #[test]
    fn snapshots_filter_codex_usage_by_remote_provider_name() -> Result<()> {
        let local = Database::memory()?;
        lock_conn!(local.conn).execute(
            "INSERT INTO providers (id, app_type, name, settings_config)
             VALUES ('relay', 'codex', 'Local Relay', '{}')",
            [],
        )?;

        let host_id = "12345678-1234-1234-1234-123456789abc";
        let mut attributed = session_usage("attributed", "gpt-6.2-astra", "0", "1");
        attributed[1] = json!("relay");
        let snapshot = create_snapshot(
            &local,
            true,
            vec![(
                host_id.to_owned(),
                Rows {
                    logs: vec![
                        attributed,
                        session_usage("unknown", "gpt-6.2-astra", "0", "1"),
                    ],
                    rollups: vec![],
                    codex_providers: vec![("relay".to_owned(), "Remote Relay".to_owned())],
                },
            )],
        )?;

        let summary =
            snapshot.get_usage_summary(None, None, Some("codex"), Some("Remote Relay"), None)?;
        assert_eq!(summary.total_requests, 1);
        let filters = crate::services::usage_stats::LogFilters {
            provider_name: Some("Remote Relay".to_owned()),
            ..Default::default()
        };
        let logs = snapshot.get_request_logs(&filters, 0, 10)?;
        assert_eq!(logs.total, 1);
        assert_eq!(logs.data[0].request_id, format!("ssh:{host_id}:attributed"));
        assert_eq!(
            snapshot
                .get_usage_summary(None, None, Some("codex"), Some("Codex (Session)"), None)?
                .total_requests,
            1
        );
        Ok(())
    }
}
