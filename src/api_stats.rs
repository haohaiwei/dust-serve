use std::collections::HashMap;

use axum::{
    extract::State,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use crate::ch_client::ClickHouse;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/stats", get(stats_quick))
        .route("/stats/quick", get(stats_quick))
        .route("/stats/heavy", get(stats_heavy))
}

// --- SQL for the precomputed stats -------------------------------------
// Shared by precompute_stats (writes scan_stats) and the fallback handlers
// (read file_scan live when a scan has no precomputed rows). Kept here so the
// SQL stays in sync between the write and read paths.

fn sql_by_age(table: &str) -> String {
    format!(
        "SELECT multiIf(
            mtime > now() - INTERVAL 30 DAY, '0-30d',
            mtime > now() - INTERVAL 90 DAY, '30-90d',
            mtime > now() - INTERVAL 180 DAY, '90-180d',
            mtime > now() - INTERVAL 365 DAY, '180d-1y',
            '>1y') AS age,
            sum(size_du) AS bytes
         FROM {table}
         WHERE scan_id = {{sid:String}} AND is_file = 1
         GROUP BY age ORDER BY age"
    )
}

fn sql_top_dirs(table: &str) -> String {
    format!(
        "SELECT parent_path AS dir, sum(size_du) AS bytes, count() AS files
         FROM {table}
         WHERE scan_id = {{sid:String}} AND is_file = 1
         GROUP BY parent_path ORDER BY bytes DESC LIMIT 10"
    )
}

fn sql_top_by_filecount(table: &str) -> String {
    format!(
        "SELECT parent_path AS dir, count() AS files, sum(size_du) AS bytes
         FROM {table}
         WHERE scan_id = {{sid:String}} AND is_file = 1
         GROUP BY parent_path ORDER BY files DESC LIMIT 10"
    )
}

fn sql_hardlink_stats(table: &str) -> String {
    format!(
        "SELECT
            countIf(link_count > 1) AS hardlinked_files,
            sum(greatest(link_count - 1, 0) * one_size) AS wasted_bytes,
            sum(one_size) AS hardlinked_total_bytes
         FROM (
            SELECT device, inode,
                max(nlink) AS link_count,
                any(size_du) AS one_size
             FROM {table}
             WHERE scan_id = {{sid:String}} AND is_file = 1
               AND nlink > 1 AND isNotNull(device) AND isNotNull(inode)
             GROUP BY device, inode
         )"
    )
}

fn sql_top_hardlinks(table: &str) -> String {
    format!(
        "SELECT device, inode, link_count, size_du, example_path,
                greatest(link_count - 1, 0) * size_du AS wasted_bytes
         FROM (
            SELECT device, inode,
                max(nlink) AS link_count,
                any(size_du) AS size_du,
                any(path) AS example_path
             FROM {table}
             WHERE scan_id = {{sid:String}} AND is_file = 1
               AND nlink > 1 AND isNotNull(device) AND isNotNull(inode)
             GROUP BY device, inode
         )
         ORDER BY wasted_bytes DESC
         LIMIT 10"
    )
}

/// cold_data: scalar sum → wrapped as {"cold_data_bytes": "<n>"} to match the
/// stats_quick response shape.
fn sql_cold_data(table: &str) -> String {
    format!(
        "SELECT sum(size_du) FROM {table}
         WHERE scan_id = {{sid:String}} AND atime < now() - INTERVAL 365 DAY AND is_file = 1"
    )
}

fn sql_symlink_stats(table: &str) -> String {
    format!(
        "SELECT countIf(is_symlink = 1) AS total_symlinks
         FROM {table}
         WHERE scan_id = {{sid:String}}"
    )
}

/// Read all precomputed stat rows for a scan_id from scan_stats. Returns None
/// when the scan has no precomputed rows (or the read fails), so the caller
/// falls back to the live full-scan queries. Each stat_value is parsed back
/// from its stored JSON string into a Value.
///
/// Uses argMax(stat_value, updated_at) so re-precomputing (e.g. calibrate after
/// completion) returns the latest value per key without waiting for
/// ReplacingMergeTree to merge the duplicate rows.
fn read_precomputed(state: &AppState, scan_id: &str) -> Option<HashMap<String, Value>> {
    let rows = state
        .ch
        .query_json_param(
            "SELECT stat_key, argMax(stat_value, updated_at) AS stat_value
             FROM scan_stats WHERE scan_id = {sid:String}
             GROUP BY stat_key",
            &[("sid", scan_id)],
        )
        .ok()?;
    if rows.is_empty() {
        return None;
    }
    let mut map = HashMap::new();
    for r in rows {
        let key = r.get("stat_key").and_then(|v| v.as_str()).map(String::from);
        let val = r.get("stat_value").and_then(|v| v.as_str());
        if let (Some(k), Some(v)) = (key, val) {
            if let Ok(parsed) = serde_json::from_str::<Value>(v) {
                map.insert(k, parsed);
            }
        }
    }
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

/// Precompute all dashboard stats for a scan and store them in scan_stats (one
/// row per stat_key, stat_value as a JSON string). Called from run_scheduler on
/// scan completion and from calibrate_scan on manual calibrate — both via
/// spawn_blocking, since this runs 7 full-scan aggregations (~80s for 2B rows).
///
/// Error-tolerant: each stat is computed independently; only successfully
/// computed stats are inserted. If every query fails (e.g. CH down) nothing is
/// inserted, so the read path falls back to the live queries instead of
/// showing zeros.
pub fn precompute_stats(
    ch_read: &ClickHouse,
    ch_write: &ClickHouse,
    table: &str,
    scan_id: &str,
) -> anyhow::Result<()> {
    let mut pairs: Vec<(&str, Value)> = Vec::new();

    if let Ok(rows) = ch_read.query_json_param(&sql_by_age(table), &[("sid", scan_id)]) {
        pairs.push(("by_age", Value::Array(rows)));
    }
    if let Ok(rows) = ch_read.query_json_param(&sql_top_dirs(table), &[("sid", scan_id)]) {
        pairs.push(("top_dirs", Value::Array(rows)));
    }
    if let Ok(rows) = ch_read.query_json_param(&sql_top_by_filecount(table), &[("sid", scan_id)]) {
        pairs.push(("top_by_filecount", Value::Array(rows)));
    }
    if let Ok(rows) = ch_read.query_json_param(&sql_hardlink_stats(table), &[("sid", scan_id)]) {
        pairs.push((
            "hardlink_stats",
            rows.into_iter()
                .next()
                .unwrap_or(json!({"hardlinked_files": 0, "wasted_bytes": 0, "hardlinked_total_bytes": 0})),
        ));
    }
    if let Ok(rows) = ch_read.query_json_param(&sql_top_hardlinks(table), &[("sid", scan_id)]) {
        pairs.push(("top_hardlinks", Value::Array(rows)));
    }
    if let Ok(s) = ch_read.query_scalar_param(&sql_cold_data(table), &[("sid", scan_id)]) {
        pairs.push(("cold_data", json!({ "cold_data_bytes": s })));
    }
    if let Ok(rows) = ch_read.query_json_param(&sql_symlink_stats(table), &[("sid", scan_id)]) {
        pairs.push((
            "symlink_stats",
            rows.into_iter()
                .next()
                .unwrap_or(json!({"total_symlinks": 0, "broken_symlinks": 0})),
        ));
    }

    // All queries failed → leave scan_stats empty so the read path falls back.
    if pairs.is_empty() {
        return Ok(());
    }

    // Build a JSONEachRow body: one line per stat_key. stat_value is a String
    // column, so the inner JSON is serialized to a string and embedded as a JSON
    // string value (serde_json quotes/escapes it).
    let mut body = String::new();
    for (key, val) in &pairs {
        let row = json!({
            "scan_id": scan_id,
            "stat_key": key,
            "stat_value": serde_json::to_string(val).unwrap_or_default(),
        });
        body.push_str(&serde_json::to_string(&row).unwrap_or_default());
        body.push('\n');
    }
    ch_write.insert_json_each_row("scan_stats", &body)?;
    Ok(())
}

/// Quick stats: by_extension (MV, ms) + cold_data + symlink_stats.
/// cold_data/symlink_stats read from scan_stats when precomputed (ms); otherwise
/// fall back to the live aggregations on file_scan.
async fn stats_quick(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<crate::api_search::ScanIdQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let latest = crate::api_search::resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(json!({
            "scan_id": null,
            "by_extension": [],
            "cold_data_bytes": "0",
            "symlink_stats": { "total_symlinks": 0, "broken_symlinks": 0 },
        })));
    }

    // by_extension: MV-backed, ms — always live (not precomputed).
    let by_extension = query(&state, &format!(
        "SELECT extension, sum(cnt) AS cnt, sum(bytes) AS bytes
         FROM mv_extension_stats
         WHERE scan_id = {{sid:String}}
         GROUP BY extension ORDER BY bytes DESC LIMIT 10"
    ), &latest)?;

    // cold_data + symlink_stats: precomputed if available, else live.
    let (cold_data_bytes, symlink_stats) = match read_precomputed(&state, &latest) {
        Some(m) => {
            let c = m
                .get("cold_data")
                .and_then(|v| v.get("cold_data_bytes"))
                .cloned()
                .unwrap_or(json!("0"));
            let s = m
                .get("symlink_stats")
                .cloned()
                .unwrap_or(json!({"total_symlinks": 0, "broken_symlinks": 0}));
            (c, s)
        }
        None => {
            let cold = state
                .ch
                .query_scalar_param(&sql_cold_data(table), &[("sid", &latest)])
                .unwrap_or_else(|_| "0".to_string());
            let sym = state
                .ch
                .query_json_param(&sql_symlink_stats(table), &[("sid", &latest)])
                .ok()
                .and_then(|mut rows| rows.pop())
                .unwrap_or(json!({"total_symlinks": 0, "broken_symlinks": 0}));
            (json!(cold), sym)
        }
    };

    Ok(Json(json!({
        "scan_id": latest,
        "by_extension": by_extension,
        "cold_data_bytes": cold_data_bytes,
        "symlink_stats": symlink_stats,
    })))
}

/// Heavy stats: by_age, top_dirs, top_by_filecount, hardlink_stats, top_hardlinks.
/// Read from scan_stats when precomputed (ms); otherwise fall back to the live
/// full-scan aggregations (~80s for 2B rows).
async fn stats_heavy(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<crate::api_search::ScanIdQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let latest = crate::api_search::resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(json!({
            "by_age": [],
            "top_dirs": [],
            "top_by_filecount": [],
            "hardlink_stats": { "hardlinked_files": 0, "wasted_bytes": 0, "hardlinked_total_bytes": 0 },
            "top_hardlinks": [],
        })));
    }

    let (by_age, top_dirs, top_by_filecount, hardlink_stats, top_hardlinks) = match read_precomputed(&state, &latest) {
        Some(m) => (
            m.get("by_age").cloned().unwrap_or(json!([])),
            m.get("top_dirs").cloned().unwrap_or(json!([])),
            m.get("top_by_filecount").cloned().unwrap_or(json!([])),
            m.get("hardlink_stats").cloned().unwrap_or(
                json!({"hardlinked_files": 0, "wasted_bytes": 0, "hardlinked_total_bytes": 0}),
            ),
            m.get("top_hardlinks").cloned().unwrap_or(json!([])),
        ),
        None => {
            // Fallback: live full-scan aggregations.
            (
                json!(query(&state, &sql_by_age(table), &latest)?),
                json!(query(&state, &sql_top_dirs(table), &latest)?),
                json!(query(&state, &sql_top_by_filecount(table), &latest)?),
                query(&state, &sql_hardlink_stats(table), &latest)?
                    .into_iter()
                    .next()
                    .unwrap_or(json!({"hardlinked_files": 0, "wasted_bytes": 0, "hardlinked_total_bytes": 0})),
                json!(query(&state, &sql_top_hardlinks(table), &latest)?),
            )
        }
    };

    Ok(Json(json!({
        "by_age": by_age,
        "top_dirs": top_dirs,
        "top_by_filecount": top_by_filecount,
        "hardlink_stats": hardlink_stats,
        "top_hardlinks": top_hardlinks,
    })))
}

fn query(
    state: &AppState,
    sql: &str,
    sid: &str,
) -> Result<Vec<Value>, (axum::http::StatusCode, String)> {
    state
        .ch
        .query_json_param(sql, &[("sid", sid)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}
