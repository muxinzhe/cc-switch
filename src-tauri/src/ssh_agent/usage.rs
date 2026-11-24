use anyhow::Result;
use rusqlite::types::ValueRef;
use serde_json::Value;

use crate::{
    database::{lock_conn, Database},
    services::session_usage::{session_sync_mutex, sync_all_unlocked},
    ssh_usage::{attribution, Rows, LOG_COLUMNS, ROLLUP_COLUMNS},
};

pub(super) fn snapshot(db: &Database, scan: bool) -> Result<Rows> {
    if scan {
        let _guard = session_sync_mutex().blocking_lock();
        let result = sync_all_unlocked(db);
        for error in result.errors {
            log::warn!("SSH usage scan: {error}");
        }
    }
    export(db, &crate::codex_config::get_codex_config_dir())
}

fn export(db: &Database, codex_home: &std::path::Path) -> Result<Rows> {
    let evidence = {
        let conn = lock_conn!(db.conn);
        attribution::init_cache(&conn)?;
        attribution::Evidence::load(&conn, &conn)?
    };
    // Codex trace lookup and reading native rollouts can be slow. Neither needs
    // to hold the agent's database mutex while a provider or proxy request runs.
    let discovered = evidence.resolve(codex_home);
    let mut conn = lock_conn!(db.conn);
    let tx = conn.transaction()?;
    attribution::store(&tx, &discovered)?;

    // Join against the persisted match so concurrent snapshots see the same
    // provider even after Codex rotates its trace or a provider is deleted.
    let fields = LOG_COLUMNS
        .split(',')
        .map(|column| {
            if column == "provider_id" {
                "COALESCE(a.provider_id,l.provider_id)".to_owned()
            } else {
                format!("l.{column}")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let logs = sql_rows(&mut tx.prepare(&format!(
        "SELECT {fields} FROM proxy_request_logs l
        LEFT JOIN attribution a ON a.request_id=l.request_id AND a.session_id=l.session_id
           AND a.created_at=l.created_at AND l.app_type='codex' AND l.data_source='codex_session'
           AND l.provider_id='_codex_session'"
    ))?)?;
    let rollups =
        sql_rows(&mut tx.prepare(&format!("SELECT {ROLLUP_COLUMNS} FROM usage_daily_rollups"))?)?;
    let providers = tx
        .prepare(
            "SELECT id,app_type,name FROM providers UNION ALL
        SELECT a.provider_id,'codex',a.provider_name FROM attribution a
        WHERE NOT EXISTS (SELECT 1 FROM providers p WHERE p.id=a.provider_id AND p.app_type='codex')
        GROUP BY a.provider_id HAVING MAX(a.created_at) IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tx.commit()?;
    Ok(Rows {
        logs,
        rollups,
        providers,
    })
}

fn sql_rows(statement: &mut rusqlite::Statement<'_>) -> Result<Vec<Vec<Value>>> {
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(columns);
        for column in 0..columns {
            values.push(match row.get_ref(column)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => value.into(),
                ValueRef::Real(value) => value.into(),
                ValueRef::Text(bytes) => std::str::from_utf8(bytes)?.into(),
                ValueRef::Blob(_) => anyhow::bail!("Unexpected binary value in a usage record"),
            });
        }
        result.push(values);
    }
    Ok(result)
}
