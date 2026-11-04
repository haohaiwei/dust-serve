use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AppState;

/// Cache for scan_status's file_scan count query (~0.55s on a 2.8B-row table
/// at 45 GB/s, but still too heavy to run on every 5s frontend poll, especially
/// with concurrent users). Keyed by scan_id; value is (json_row, fetched_at).
/// TTL is STATUS_CACHE_TTL — concurrent polls within the window reuse one
/// real query instead of stacking N × 0.55s queries.
static STATUS_CACHE: Lazy<std::sync::Mutex<HashMap<String, (Value, Instant)>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));
const STATUS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    /// Root directory to scan.
    pub root: String,
    /// Scan identifier. If omitted, one is generated from the current time.
    pub scan_id: Option<String>,
    /// If set, run an incremental scan: only files with mtime after this
    /// RFC3339 / YYYY-MM-DD datetime are emitted.
    pub since: Option<String>,
    /// Max stat calls per second (0 = unlimited). Protects NFS/Lustre MDS.
    #[serde(default)]
    pub rate_limit: u64,
    /// Number of rayon worker threads (overrides CPU count).
    #[serde(default)]
    pub threads: Option<u32>,
    /// ClickHouse insert batch size.
    #[serde(default)]
    pub batch_size: Option<u32>,
    /// Number of parallel dust processes to shard the scan across. When >1,
    /// the root's immediate subdirectories are split across N worker processes
    /// (each scanning one subdirectory) to parallelize large filesystems.
    /// 1 (default) = single process scanning the whole root.
    #[serde(default = "default_shards")]
    pub shards: u32,
    /// Overshard factor: expand to N*this many candidates for work-stealing.
    /// Larger splits deep directory trees finer (reduces long-tail stalls).
    /// Default 5.
    #[serde(default = "default_overshard")]
    pub overshard: u32,
    /// Extra CLI args passed through to dust.
    /// Per-scan dirs to exclude (comma-separated names, e.g. "tmp,backup,.git").
    /// Merged with the global EXCLUDE_DIRS env config. For each, dust-serve
    /// passes --ignore-directory {candidate}/{name} (full path) to dust.
    pub exclude_dirs: Option<String>,
    pub extra_args: Option<Vec<String>>,
}

fn default_shards() -> u32 {
    1
}

fn default_overshard() -> u32 {
    5
}

#[derive(Debug, Serialize)]
pub struct ScanResponse {
    pub scan_id: String,
    pub status: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/scan", post(start_scan))
        .route("/scan/:id/status", get(scan_status))
        .route("/scan/:id/qps", get(scan_qps))
        .route("/scan/:id/shards", get(scan_shards))
        .route("/scan/:id/calibrate", post(calibrate_scan))
        .route("/scan/:id/resume", post(resume_scan))
        .route("/scan/:id", axum::routing::delete(cancel_scan))
        .route("/scan/:id/log", get(scan_log))
        .route("/scans", get(list_scans))
        .route("/scan_states", get(scan_states))
}

/// Scan parameters that differ between a fresh scan and a resume, bundled so
/// run_scheduler can be shared by both start_scan and resume_scan. Everything
/// here is what gets threaded into each dust child process.
#[derive(Clone)]
struct ScanCfg {
    scan_id_for_cmd: String,
    rate_limit: u64,
    threads: Option<u32>,
    batch_size: Option<u32>,
    since: Option<String>,
    extra_args: Option<Vec<String>>,
    excludes: Vec<String>,
}

/// Spawn the background scheduler for a scan: `slots` tokio tasks each pull a
/// candidate from `shard.queue`, fork a dust process, wait for it, then pull the
/// next. On completion, runs a one-shot calibration (count/sum on file_scan for
/// accurate final counts), writes the per-scan log, and records the
/// FinishedScan. Shared by start_scan and resume_scan so both run identical
/// scheduling logic.
///
/// `shard` already has the candidate queue populated (fresh candidates for a new
/// scan, or the not-yet-done candidates for a resume). `slots` is the parallelism.
fn run_scheduler(
    state: AppState,
    scan_id: String,
    shard: ShardState,
    slots: usize,
    cfg: ScanCfg,
) {
    let running = state.running_scans.clone();
    let finished = state.finished_scans.clone();
    let ch_for_calib = state.ch.clone();
    let ch_write_for_pre = state.ch_write.clone();
    let table_for_calib = state.ch.table().to_string();
    let sid = scan_id.clone();
    let log_path = state.config.scan_log_dir.join(format!("{scan_id}.log"));
    let done_path = state.config.scan_log_dir.join(format!("{scan_id}.done"));
    let shard_for_sched = shard.clone();

    let scan_id_c = cfg.scan_id_for_cmd.clone();
    let ch_url = state.config.ch_url.clone();
    let ch_table = state.config.ch_table.clone();
    let ch_user = state.config.ch_write_user.clone();
    let ch_pass = state.config.ch_write_password.clone();
    let dust_path = state.config.dust_path.clone();
    let rate_limit = cfg.rate_limit;
    let threads = cfg.threads;
    let batch_size = cfg.batch_size;
    let since = cfg.since.clone();
    let extra_args = cfg.extra_args.clone();
    let all_excludes_config = cfg.excludes.clone();

    tokio::spawn(async move {
        let mut handles = Vec::with_capacity(slots);
        for slot_idx in 0..slots {
            let shard = shard_for_sched.clone();
            let log = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let log_clone = log.clone();
            let scan_id_c = scan_id_c.clone();
            let ch_url = ch_url.clone();
            let ch_table = ch_table.clone();
            let ch_user = ch_user.clone();
            let ch_pass = ch_pass.clone();
            let dust_path = dust_path.clone();
            let since = since.clone();
            let extra_args = extra_args.clone();
            let all_excludes_config = all_excludes_config.clone();
            let done_path = done_path.clone();
            let h = tokio::task::spawn_blocking(move || -> (SlotOutcome, std::sync::Arc<std::sync::Mutex<String>>) {
                // This slot's worst outcome across all candidates it processes.
                let mut outcome = SlotOutcome::Done;
                loop {
                    if shard.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let task = shard.queue.lock().unwrap().pop_front();
                    let Some(task) = task else {
                        break;
                    }; // queue empty for this slot
                    let mut cmd = std::process::Command::new(&dust_path);
                    cmd.arg("--scan")
                        .arg(&task.path)
                        .arg("--sink").arg("clickhouse")
                        .arg("--ch-url").arg(&ch_url)
                        .arg("--ch-table").arg(&ch_table)
                        .arg("--scan-id").arg(&scan_id_c)
                        .arg("--rate-limit").arg(rate_limit.to_string())
                        .arg("--no-progress")
                        // -s / --apparent-size: let dust record st_size into `size`
                        // while size_du stays st_blocks*512. Without -s both are
                        // blocks*512 (size is redundant). With -s the two columns
                        // diverge for sparse files: size=apparent, size_du=disk.
                        // The frontend file-detail page already labels them
                        // "大小(apparent)" / "大小(disk)" — this makes that true.
                        .arg("-s");
                    // max_depth: None = unlimited (default 0); Some(n) = cap recursion.
                    if let Some(md) = task.max_depth {
                        cmd.arg("--max-depth").arg(md.to_string());
                    }
                    // Exclude dirs: pass --ignore-directory {candidate}/{name} for
                    // each (merged global + per-scan). dust matches absolute path
                    // prefixes; multiple --ignore-directory args are collected by
                    // clap into a Vec.
                    for name in &all_excludes_config {
                        cmd.arg("--ignore-directory").arg(format!("{}/{name}", task.path.trim_end_matches('/')));
                    }
                    cmd.env("CH_USER", &ch_user);
                    cmd.env("CH_PASSWORD", &ch_pass);
                    if let Some(t) = threads { cmd.arg("--threads").arg(t.to_string()); }
                    if let Some(b) = batch_size { cmd.arg("--batch-size").arg(b.to_string()); }
                    if let Some(s) = &since { cmd.arg("--since").arg(s); }
                    if let Some(extras) = &extra_args {
                        for a in extras { cmd.arg(a); }
                    }
                    let child = match cmd.stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                    {
                        Ok(c) => c,
                        Err(e) => {
                            log_clone.lock().unwrap().push_str(
                                &format!("\n=== slot {slot_idx} spawn failed for {}: {e} ===\n", task.path));
                            outcome = worst_of(outcome, SlotOutcome::Failed);
                            continue; // try next candidate
                        }
                    };
                    let pid = child.id() as u64;
                    shard.active_pids.lock().unwrap().insert(pid);
                    let output = child.wait_with_output();
                    shard.active_pids.lock().unwrap().remove(&pid);
                    let mut lg = log_clone.lock().unwrap();
                    lg.push_str(&format!("\n=== slot {slot_idx} pid {pid}: {} ===\n", task.path));
                    match output {
                        Ok(out) => {
                            lg.push_str(&String::from_utf8_lossy(&out.stdout));
                            lg.push_str("\n--- stderr ---\n");
                            lg.push_str(&String::from_utf8_lossy(&out.stderr));
                            let code = out.status.code().unwrap_or(-1);
                            if out.status.success() {
                                // Record this candidate as fully scanned so a
                                // later /scan/:id/resume can skip it. Append-only
                                // (no fsync): if we crash here the line may be
                                // lost — which is correct, since that candidate
                                // wasn't durably completed and should be re-run.
                                //
                                // write_all of the full "path\n" in one call,
                                // NOT writeln!: writeln! issues two write syscalls
                                // (data, then "\n") and the gap between them is
                                // non-atomic, so concurrent slots appending to
                                // the same .done interleaved two paths into one line
                                // ("pathApathB\n\n") — corrupting .done, losing the
                                // completion record for both candidates, and making
                                // resume re-scan already-done candidates (duplicate
                                // ClickHouse rows). One write_all is a single write,
                                // atomic under O_APPEND on a regular file.
                                use std::io::Write;
                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                    .create(true).append(true).open(&done_path)
                                {
                                    let line = format!("{}\n", task.path);
                                    let _ = f.write_all(line.as_bytes());
                                }
                            } else {
                                // Classify this candidate's exit. The cancelled
                                // flag + SIGTERM → Cancelled; anything else non-
                                // success → Failed.
                                let _ = code;
                                outcome = worst_of(outcome, classify_exit(&out.status));
                            }
                        }
                        Err(e) => {
                            outcome = worst_of(outcome, SlotOutcome::Failed);
                            lg.push_str(&format!("wait error: {e}\n"));
                        }
                    }
                }
                (outcome, log)
            });
            handles.push(h);
        }
        // Wait for all slots to finish, then aggregate.
        // Wait for all slots to finish, then aggregate their outcomes into a
        // single scan state via the tested aggregate_outcomes function.
        let mut slot_outcomes: Vec<SlotOutcome> = Vec::with_capacity(handles.len());
        let mut all_log = String::new();
        for h in handles {
            if let Ok((outcome, log)) = h.await {
                slot_outcomes.push(outcome);
                all_log.push_str(&log.lock().unwrap());
            }
        }
        if let Ok(mut f) = std::fs::File::create(&log_path) {
            use std::io::Write;
            let _ = f.write_all(all_log.as_bytes());
        }
        let state_str = aggregate_outcomes(&slot_outcomes);
        // exit_code: non-zero only when the scan genuinely failed (not cancelled
        // — a user cancel is not a failure). done/cancelled → 0, failed → 1.
        let any_failed = state_str == "failed";
        running.lock().unwrap().remove(&sid);
        // Calibrate the accurate file count/bytes once, directly from file_scan
        // (mv_scan_stats can lose triggers under concurrent inserts). This is a
        // full scan of the scan_id's rows (~9s for 2B rows) but only runs once
        // at completion, not on every poll.
        let (files, dirs, symlinks, hardlinks, total, bytes_du) = if state_str == "done" || state_str == "cancelled" {
            let calib_sql = format!(
                "SELECT
                    countIf(is_file=1) AS files,
                    countIf(is_dir=1) AS dirs,
                    countIf(is_symlink=1) AS symlinks,
                    countIf(nlink >= 2 AND is_file=1) AS hardlinks,
                    count() AS total,
                    sumIf(size_du, is_file=1) AS bytes_du
                 FROM {table_for_calib} WHERE scan_id = {{sid:String}}"
            );
            match ch_for_calib.query_json_param(&calib_sql, &[("sid", &sid)]) {
                Ok(rows) => {
                    let r = rows.into_iter().next();
                    let f = r.as_ref().and_then(|v| v.get("files")).and_then(|v| v.as_u64());
                    let d = r.as_ref().and_then(|v| v.get("dirs")).and_then(|v| v.as_u64());
                    let sl = r.as_ref().and_then(|v| v.get("symlinks")).and_then(|v| v.as_u64());
                    let hl = r.as_ref().and_then(|v| v.get("hardlinks")).and_then(|v| v.as_u64());
                    let t = r.as_ref().and_then(|v| v.get("total")).and_then(|v| v.as_u64());
                    let b = r.as_ref().and_then(|v| v.get("bytes_du")).and_then(|v| v.as_u64());
                    (f, d, sl, hl, t, b)
                }
                Err(_) => (None, None, None, None, None, None),
            }
        } else {
            (None, None, None, None, None, None)
        };
        finished.lock().unwrap().insert(
            sid.clone(),
            FinishedScan {
                state: state_str.to_string(),
                ended_at: now_secs(),
                exit_code: if any_failed { 1 } else { 0 },
                files,
                dirs,
                symlinks,
                hardlinks,
                total,
                bytes_du,
            },
        );
        // Scan finished: drop its STATUS_CACHE entry so the cache doesn't
        // grow unbounded across scans. The entry will be re-populated on demand
        // if /scan/:id/status is polled again for this completed scan.
        STATUS_CACHE.lock().unwrap().remove(&sid);
        // Precompute dashboard stats into scan_stats (ms dashboard reads instead
        // of ~80s full scans). Only on successful completion — a cancelled scan
        // can still be precomputed via the manual calibrate button. Runs on a
        // blocking thread so the aggregations don't starve the async runtime.
        if state_str == "done" {
            let ch_read = ch_for_calib.clone();
            let ch_write = ch_write_for_pre.clone();
            let table = table_for_calib.clone();
            let sid_pre = sid.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::api_stats::precompute_stats(&ch_read, &ch_write, &table, &sid_pre) {
                    eprintln!("precompute_stats for {sid_pre} failed: {e}");
                }
            });
        }
    });
}

async fn start_scan(
    State(state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResponse>, (axum::http::StatusCode, String)> {
    let scan_id = req.scan_id.unwrap_or_else(|| {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("scan_{secs}")
    });

    // Prevent duplicate concurrent scans with the same scan_id.
    if state.running_scans.lock().unwrap().contains_key(&scan_id) {
        return Err((
            axum::http::StatusCode::CONFLICT,
            format!("scan {scan_id} is already running"),
        ));
    }

    // Determine the candidate directories to scan.
    // shards=1 scans the whole root in one process.
    // shards>1 expands root breadth-first into enough leaf-subdir candidates
    // (>= N*5) and runs N parallel dust processes that pull work from the
    // shared queue — dynamic work-stealing, so process count is fixed at N
    // regardless of how many subdirs the root has, and large subdirs don't
    // stall the rest.
    let n = (req.shards as usize).max(1);
    let overshard = (req.overshard as usize).max(1);
    // Merge global EXCLUDE_DIRS with per-scan exclude_dirs for both expand
    // (skip excluded dirs as candidates) and the --ignore-directory args.
    let mut all_excludes_config: Vec<String> = state.config.exclude_dirs.clone();
    if let Some(re) = &req.exclude_dirs {
        all_excludes_config.extend(re.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    let candidates: Vec<ScanTask> = if n <= 1 {
        vec![ScanTask { path: req.root.clone(), max_depth: None }]
    } else {
        match expand_candidates(&req.root, n * overshard, &all_excludes_config) {
            Ok(c) if !c.is_empty() => c,
            _ => vec![ScanTask { path: req.root.clone(), max_depth: None }],
        }
    };

    // Shared shard state: work queue + active PIDs (for cancel) + cancel flag.
    // Clone the candidate list first — `candidates.into()` below moves it into
    // the queue, but we also need it to persist into .meta.json so resume can
    // read the exact candidate set (path + max_depth) instead of re-deriving it.
    let candidates_for_meta = candidates.clone();
    let shard = ShardState {
        queue: Arc::new(std::sync::Mutex::new(candidates.into())),
        active_pids: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    // Number of parallel scheduler slots. Capped at candidate count so we
    // don't have idle slots spinning on an empty queue.
    let slots = n.min(shard.queue.lock().unwrap().len()).max(1);

    state.running_scans.lock().unwrap().insert(
        scan_id.clone(),
        RunningScan { shard: shard.clone(), started_at: now_secs() },
    );

    // Persist scan config + the exact candidate list so a later
    // /scan/:id/resume can read the candidates straight from meta (no
    // re-derivation via expand_candidates) and skip those in {scan_id}.done.
    // Storing the candidate list (not just the params that produced it) makes
    // resume independent of filesystem changes between runs and removes the
    // need for overshard on resume.
    let meta_path = state.config.scan_log_dir.join(format!("{scan_id}.meta.json"));
    let meta = serde_json::json!({
        "root": req.root,
        "shards": n,
        "overshard": overshard,
        "exclude_dirs": all_excludes_config,
        "rate_limit": req.rate_limit,
        "threads": req.threads,
        "batch_size": req.batch_size,
        "since": req.since,
        "extra_args": req.extra_args,
        "candidates": serde_json::to_value(&candidates_for_meta).unwrap_or(Value::Array(vec![])),
    });
    let _ = std::fs::write(&meta_path, meta.to_string());

    let cfg = ScanCfg {
        scan_id_for_cmd: scan_id.clone(),
        rate_limit: req.rate_limit,
        threads: req.threads,
        batch_size: req.batch_size,
        since: req.since.clone(),
        extra_args: req.extra_args.clone(),
        excludes: all_excludes_config,
    };
    run_scheduler(state, scan_id.clone(), shard, slots, cfg);

    Ok(Json(ScanResponse {
        scan_id,
        status: "running".to_string(),
    }))
}

/// Resume a previously-interrupted scan. Reads the exact candidate list
/// persisted in {scan_id}.meta.json by start_scan (no re-derivation via
/// expand_candidates — so resume is independent of filesystem changes between
/// runs and needs no overshard), filters out candidates already in
/// {scan_id}.done, and re-runs the rest through the same run_scheduler used by
/// start_scan. Candidates that were popped but not completed (crash
/// mid-candidate) are NOT in .done, so they get re-scanned — this may write
/// duplicate rows to ClickHouse for those candidates, which is accepted (no
/// dedup). NOTE: a whole-scan `DELETE FROM file_scan WHERE scan_id=?` is NOT a
/// correct dedup here — it would erase the already-completed candidates' rows
/// too, but resume never re-scans completed candidates, so that data would be
/// permanently lost. Correct (but usually not worth it) dedup would be a
/// per-candidate path-prefix DELETE for only the candidates about to be re-run,
/// which on a multi-billion-row table means heavy ClickHouse mutations per
/// candidate — typically worse than living with duplicate rows. A meta without
/// a `candidates` field (old/incompatible) is rejected rather than silently
/// re-derived, so the user knows to re-run the scan.
async fn resume_scan(
    State(state): State<AppState>,
    Path(scan_id): Path<String>,
) -> Result<Json<ScanResponse>, (axum::http::StatusCode, String)> {
    // Don't resume a scan that's already running.
    if state.running_scans.lock().unwrap().contains_key(&scan_id) {
        return Err((
            axum::http::StatusCode::CONFLICT,
            format!("scan {scan_id} is already running"),
        ));
    }

    // Read the persisted scan config to rebuild the exact same candidate set.
    let meta_path = state.config.scan_log_dir.join(format!("{scan_id}.meta.json"));
    let meta_text = std::fs::read_to_string(&meta_path).map_err(|_| {
        (
            axum::http::StatusCode::NOT_FOUND,
            format!("no meta for scan {scan_id} (never started or meta lost)"),
        )
    })?;
    let meta: serde_json::Value = serde_json::from_str(&meta_text).map_err(|_| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("corrupt meta for scan {scan_id}"),
        )
    })?;
    let n = meta["shards"].as_u64().unwrap_or(1) as usize;
    let excludes: Vec<String> = meta["exclude_dirs"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // Read the exact candidate list persisted by start_scan (no re-derivation
    // via expand_candidates). This makes resume independent of filesystem
    // changes between runs and removes the need for root/overshard on resume.
    // A missing candidates field means an old/incompatible meta — fail loudly
    // rather than silently re-deriving a different candidate set.
    let candidates: Vec<ScanTask> = serde_json::from_value(
        meta["candidates"].clone(),
    )
    .map_err(|_| {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("meta for scan {scan_id} has no candidates list (old/incompatible meta — re-run the scan)"),
        )
    })?;
    if candidates.is_empty() {
        return Ok(Json(ScanResponse {
            scan_id,
            status: "already-complete".to_string(),
        }));
    }

    // Read .done: the set of candidates that fully completed last time. Only
    // these are skipped; everything else (never started + popped-but-incomplete)
    // is re-scanned.
    let done_path = state.config.scan_log_dir.join(format!("{scan_id}.done"));
    let done: std::collections::HashSet<String> = std::fs::read_to_string(&done_path)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    let remaining: Vec<ScanTask> = candidates.into_iter()
        .filter(|t| !done.contains(&t.path))
        .collect();
    if remaining.is_empty() {
        return Ok(Json(ScanResponse {
            scan_id,
            status: "already-complete".to_string(),
        }));
    }

    let shard = ShardState {
        queue: Arc::new(std::sync::Mutex::new(remaining.into())),
        active_pids: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let slots = n.min(shard.queue.lock().unwrap().len()).max(1);

    // Clear any stale FinishedScan (e.g. the 'cancelled'/'failed' entry from
    // the interrupted run we're now resuming) so scan_states doesn't report
    // the dead state alongside the live RunningScan we're about to insert.
    state.finished_scans.lock().unwrap().remove(&scan_id);
    state.running_scans.lock().unwrap().insert(
        scan_id.clone(),
        RunningScan { shard: shard.clone(), started_at: now_secs() },
    );

    // Rebuild ScanCfg from meta so the resumed dust processes use the same
    // rate_limit/threads/batch_size/since/extra_args/excludes as the original.
    let cfg = ScanCfg {
        scan_id_for_cmd: scan_id.clone(),
        rate_limit: meta["rate_limit"].as_u64().unwrap_or(0),
        threads: meta["threads"].as_u64().map(|v| v as u32),
        batch_size: meta["batch_size"].as_u64().map(|v| v as u32),
        since: meta["since"].as_str().map(String::from),
        extra_args: meta["extra_args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()),
        excludes,
    };
    run_scheduler(state, scan_id.clone(), shard, slots, cfg);

    Ok(Json(ScanResponse {
        scan_id,
        status: "running".to_string(),
    }))
}

/// Determine the candidate directories to scan for sharding. The result MUST
/// cover the entire root subtree (no missed dirs) — sharding never sacrifices
/// completeness for parallelism.
///
/// Strategy:
/// - Read root's immediate subdirs. If there are >= `min_candidates`, use them
///   all as candidates (one dust process per subdir covers each subdir's whole
///   subtree). This is the common case for large storage with many top dirs.
/// - If root has fewer subdirs than `min_candidates`, expand each subdir's
///   children one more level and repeat, growing the frontier until we have
///   enough leaf candidates OR the frontier is exhausted (a level had only
///   leaves). Every directory ends up in exactly one candidate, so coverage
///   is complete and non-overlapping.
///
/// `min_candidates` is the overshard target (e.g. N*5) — more candidates than
/// slots lets the dynamic pool balance work. Coverage is never reduced to
/// meet it: if a level is all leaves, candidates = that level's dirs.
fn expand_candidates(
    root: &str,
    min_candidates: usize,
    excludes: &[String],
) -> std::io::Result<Vec<ScanTask>> {
    // Start with the root's immediate subdirs as the candidate frontier.
    let mut candidates: Vec<ScanTask> = Vec::new();
    let mut frontier: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Helper: true if a directory's basename is in the excludes list (e.g.
    // ".snapshot"). This prevents excluded dirs from becoming candidates.
    let is_excluded = |dir: &str| {
        let base = std::path::Path::new(dir).file_name().and_then(|n| n.to_str()).unwrap_or("");
        excludes.iter().any(|e| e == base)
    };

    // Seed: root's own subdirs (skip excluded ones like .snapshot).
    let root_subdirs = read_dir_dirs(root)?;
    if root_subdirs.is_empty() {
        // Root has no subdirs: scan the root itself as a single candidate
        // (full subtree). No root-direct-files gap exists here.
        return Ok(vec![ScanTask { path: root.to_string(), max_depth: None }]);
    }
    for s in root_subdirs {
        if is_excluded(&s) {
            continue;
        }
        frontier.push_back(s);
    }

    while !frontier.is_empty() && candidates.len() < min_candidates {
        // Snapshot the current frontier level; expand each into its subdirs.
        let level: Vec<String> = frontier.drain(..).collect();
        let mut leaves_this_level = Vec::new();
        let mut next_frontier: Vec<String> = Vec::new();
        for dir in level {
            let subs = read_dir_dirs(&dir)?;
            if subs.is_empty() {
                // Leaf: this dir is a candidate. Its subtree (just itself) is covered.
                if covered.insert(dir.clone()) {
                    leaves_this_level.push(dir);
                }
            } else {
                // Has subdirs: expand them, skipping excluded dirs (e.g. .snapshot).
                for s in subs {
                    if !is_excluded(&s) {
                        next_frontier.push(s);
                    }
                }
                // the expanded children. But this dir's OWN direct files (not
                // in any subdir) would be missed — so if it has any, add a
                // max_depth=1 candidate that scans only this dir's direct
                // entries (its files + the subdir records), without descending,
                // covering the direct files without duplicating the subtrees.
                if dir_has_direct_files(&dir) {
                    candidates.push(ScanTask { path: dir.clone(), max_depth: Some(1) });
                }
            }
        }
        candidates.extend(leaves_this_level.iter().map(|p| ScanTask { path: p.clone(), max_depth: None }));
        if next_frontier.is_empty() {
            break; // current level had only leaves; nothing more to expand
        }
        for s in next_frontier {
            frontier.push_back(s);
        }
    }

    // Any remaining frontier dirs are candidates too (we stopped expanding
    // because we hit min_candidates, or they're the next unexpanded level).
    // Each remaining dir becomes a candidate covering its whole subtree.
    for dir in frontier {
        if covered.insert(dir.clone()) {
            candidates.push(ScanTask { path: dir, max_depth: None });
        }
    }

    // Guarantee coverage of files directly under root (not in any subdir):
    // a final candidate scans only root's direct entries (max_depth=1), so it
    // picks up root's direct files without duplicating the subdir subtrees
    // already covered by the candidates above. Skip if root has no direct
    // files (no point scanning an empty direct level).
    if dir_has_direct_files(root) {
        candidates.push(ScanTask { path: root.to_string(), max_depth: Some(1) });
    }

    Ok(candidates)
}

/// Return immediate subdirectory paths of `dir` (dirs only, no files).
fn read_dir_dirs(dir: &str) -> std::io::Result<Vec<String>> {
    let mut subs = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(subs),
    };
    for entry in rd {
        let Ok(rd) = entry else { continue };
        if let Ok(ft) = rd.file_type() {
            if ft.is_dir() {
                subs.push(rd.path().to_string_lossy().into_owned());
            }
        }
    }
    // Sort so expand_candidates is deterministic: a fresh scan and a resume
    // (which re-runs expand_candidates with the same inputs) produce the same
    // candidate set, so .done path filtering lines up cleanly across runs.
    subs.sort();
    Ok(subs)
}

/// Does `dir` have any direct non-directory entries (regular files, symlinks)?
/// Used to decide whether to add a max_depth=1 candidate for a dir that is
/// otherwise expanded into its subdirs — so its direct files aren't missed.
fn dir_has_direct_files(dir: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for entry in rd {
        let Ok(e) = entry else { continue };
        if let Ok(ft) = e.file_type() {
            if !ft.is_dir() {
                return true; // a file/symlink/other directly under dir
            }
        }
    }
    false
}

async fn scan_status(
    State(state): State<AppState>,
    Path(scan_id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    // Determine the real scan state from process tracking, not from row
    // timestamps: running (in map) / done|failed|cancelled (finished map) /
    // unknown (neither — never launched from this server, or evicted).
    let (state_str, exit_code): (String, Option<i32>) = {
        if state.running_scans.lock().unwrap().contains_key(&scan_id) {
            ("running".to_string(), None)
        } else if let Some(fin) = state.finished_scans.lock().unwrap().get(&scan_id) {
            (fin.state.clone(), Some(fin.exit_code))
        } else {
            ("unknown".to_string(), None)
        }
    };

    // Progress directly from file_scan (a count/sum/min/max filtered by scan_id
    // runs in ~0.55s on a 2.8B-row table — fast enough to poll, and accurate,
    // unlike mv_scan_stats which can lose triggers under concurrent inserts).
    // Cached for STATUS_CACHE_TTL so concurrent polls within the window reuse
    // one real query instead of stacking N × 0.55s queries.
    if let Some((cached, fetched)) = STATUS_CACHE.lock().unwrap().get(&scan_id).cloned() {
        if fetched.elapsed() < STATUS_CACHE_TTL {
            let mut row = cached;
            if let Some(obj) = row.as_object_mut() {
                obj.insert("state".into(), Value::String(state_str));
                obj.insert("exit_code".into(), exit_code.map(|c| Value::from(c)).unwrap_or(Value::Null));
            }
            return Ok(Json(row));
        }
    }

    let table = state.ch.table();
    let sql = format!(
        "SELECT countIf(is_file = 1) AS files,
            sumIf(size_du, is_file = 1) AS bytes_du,
            sumIf(size_du, is_file = 1) AS bytes,
            formatDateTime(toDateTime(min(scan_time)), '%Y-%m-%d %H:%i:%s') AS started,
            formatDateTime(toDateTime(max(scan_time)), '%Y-%m-%d %H:%i:%s') AS latest
         FROM {table} WHERE scan_id = {{sid:String}}"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &scan_id)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut row = rows.into_iter().next().unwrap_or_else(|| {
        serde_json::json!({
            "files": 0, "bytes": 0, "bytes_du": 0,
            "started": null, "latest": null
        })
    });
    // Cache the CH result (before state/exit_code are spliced in, since those
    // come from in-memory tracking and must be fresh per request).
    STATUS_CACHE.lock().unwrap().insert(scan_id.clone(), (row.clone(), Instant::now()));
    if let Some(obj) = row.as_object_mut() {
        obj.insert("state".into(), Value::String(state_str));
        obj.insert("exit_code".into(), exit_code.map(|c| Value::from(c)).unwrap_or(Value::Null));
    }
    Ok(Json(row))
}

async fn list_scans(
    State(state): State<AppState>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    // Query the pre-aggregated mv_scan_stats (one row per scan_id) instead of
    // scanning all rows of file_scan. min/max use minMerge/maxMerge on the
    // AggregateFunction state columns. This turns a 25s full-scan into ms.
    let sql = format!(
        "SELECT scan_id,
            formatDateTime(toDateTime(minMerge(started_state)), '%Y-%m-%d %H:%i:%s') AS started,
            formatDateTime(toDateTime(maxMerge(ended_state)), '%Y-%m-%d %H:%i:%s') AS ended,
            sum(files) AS files,
            sum(bytes_du) AS bytes_du
         FROM mv_scan_stats
         GROUP BY scan_id
         ORDER BY started DESC
         LIMIT 100"
    );
    let rows = state
        .ch
        .query_json(&sql)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Override files/bytes_du with accurate values. finished_scans has the
    // accurate count for COMPLETED scans (from the one-shot calibration on
    // completion). For scans NOT in finished_scans (running scans, or scans
    // whose finished_scans entry was lost on restart), query file_scan
    // directly (~0.55s on a 2.8B-row table) — accurate, unlike mv_scan_stats
    // which can lose triggers. Reused STATUS_CACHE so the same scan polled by
    // both /scan/:id/status and /scans within 2s shares one real query.
    // Typically only 1 scan is running at a time, so at most ~1 file_scan
    // query per /scans poll (the rest hit finished_scans in memory).
    let fin = state.finished_scans.lock().unwrap();
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            if let Some(obj) = r.as_object_mut() {
                if let Some(sid) = obj.get("scan_id").and_then(|v| v.as_str()) {
                    if let Some(f) = fin.get(sid) {
                        if let Some(files) = f.files {
                            obj.insert("files".into(), Value::from(files));
                        }
                        if let Some(dirs) = f.dirs {
                            obj.insert("dirs".into(), Value::from(dirs));
                        }
                        if let Some(symlinks) = f.symlinks {
                            obj.insert("symlinks".into(), Value::from(symlinks));
                        }
                        if let Some(hardlinks) = f.hardlinks {
                            obj.insert("hardlinks".into(), Value::from(hardlinks));
                        }
                        if let Some(total) = f.total {
                            obj.insert("total".into(), Value::from(total));
                        }
                        if let Some(bytes) = f.bytes_du {
                            obj.insert("bytes_du".into(), Value::from(bytes));
                        }
                    } else if let Some((files, bytes_du)) = file_scan_progress(&state, sid) {
                        // Not in finished_scans (running, or post-restart): pull
                        // accurate files/bytes_du straight from file_scan.
                        obj.insert("files".into(), Value::from(files));
                        obj.insert("bytes_du".into(), Value::from(bytes_du));
                    }
                }
            }
            r
        })
        .collect();
    Ok(Json(serde_json::json!({ "scans": rows })))
}

/// Return accurate (files, bytes_du) for a scan straight from file_scan, with
/// STATUS_CACHE reuse: a cache hit (within STATUS_CACHE_TTL) returns without
/// querying; a miss runs the ~0.55s count and caches it. Shared by scan_status
/// (via the cache) and list_scans so the two endpoints don't double-query for
/// the same scan within the TTL window.
fn file_scan_progress(state: &AppState, scan_id: &str) -> Option<(u64, u64)> {
    // Fast path: cache hit (another poll already fetched this scan_id recently).
    if let Some((cached, fetched)) = STATUS_CACHE.lock().unwrap().get(scan_id).cloned() {
        if fetched.elapsed() < STATUS_CACHE_TTL {
            let files = cached.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
            let bytes_du = cached.get("bytes_du").and_then(|v| v.as_u64()).unwrap_or(0);
            return Some((files, bytes_du));
        }
    }
    // Cache miss: query file_scan, cache the row, return files/bytes_du.
    let table = state.ch.table();
    let sql = format!(
        "SELECT countIf(is_file = 1) AS files,
            sumIf(size_du, is_file = 1) AS bytes_du
         FROM {table} WHERE scan_id = {{sid:String}}"
    );
    let rows = state.ch.query_json_param(&sql, &[("sid", &scan_id)]).ok()?;
    let row = rows.into_iter().next()?;
    STATUS_CACHE.lock().unwrap().insert(scan_id.to_string(), (row.clone(), Instant::now()));
    let files = row.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
    let bytes_du = row.get("bytes_du").and_then(|v| v.as_u64()).unwrap_or(0);
    Some((files, bytes_du))
}

/// Return the real state (running/done/failed/cancelled) for every scan the
/// server currently knows about. The frontend uses this to render accurate
/// status badges in the scan list instead of guessing from row timestamps.
async fn scan_states(
    State(state): State<AppState>,
) -> Json<Value> {
    let mut states = serde_json::Map::new();
    // Insert finished first, then running — so a scan_id present in BOTH
    // (a resumed-from-failed/cancelled scan whose stale FinishedScan wasn't
    // cleared) reports the live 'running' state, not the dead 'cancelled'/'failed'.
    for (sid, fin) in state.finished_scans.lock().unwrap().iter() {
        states.insert(sid.clone(), Value::String(fin.state.clone()));
    }
    for (sid, _rs) in state.running_scans.lock().unwrap().iter() {
        states.insert(sid.clone(), Value::String("running".into()));
    }
    Json(serde_json::json!({ "states": states }))
}

/// Cancel an in-flight scan by sending SIGTERM to the dust child process.
async fn cancel_scan(
    State(state): State<AppState>,
    Path(scan_id): Path<String>,
    ) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    // Get the shard state (without removing it — the scheduler removes it
    // when the scan finishes). Set the cancel flag and signal all currently
    // active dust processes.
    let shard = {
        let running = state.running_scans.lock().unwrap();
        match running.get(&scan_id) {
            Some(rs) => rs.shard.clone(),
            None => {
                return Err((
                    axum::http::StatusCode::NOT_FOUND,
                    format!("no running scan with id {scan_id}"),
                ))
            }
        }
    };
    shard
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let pids: Vec<u64> = shard.active_pids.lock().unwrap().iter().copied().collect();
    #[cfg(unix)]
    {
        for pid in &pids {
            unsafe {
                libc::kill(*pid as i32, libc::SIGTERM);
            }
        }
    }
    Ok(Json(serde_json::json!({ "scan_id": scan_id, "status": "cancelled", "active_pids": pids })))
}

/// Return the captured stdout/stderr log of a scan (running or finished).
async fn scan_log(
    State(state): State<AppState>,
    Path(scan_id): Path<String>,
) -> Result<String, (axum::http::StatusCode, String)> {
    let log_path = state.config.scan_log_dir.join(format!("{scan_id}.log"));
    match std::fs::read_to_string(&log_path) {
        Ok(text) => Ok(text),
        Err(_) => Err((
            axum::http::StatusCode::NOT_FOUND,
            format!("no log for scan {scan_id} (still running or never run)"),
        )),
    }
}

/// Scan write rate (rows/sec) over the last minute, from query_log.
/// Does NOT scan file_scan — reads only system.query_log (ms). Used by the
/// frontend to show live progress while a scan is running, where mv_scan_stats
/// lags behind the writes.
async fn scan_qps(
    State(state): State<AppState>,
    Path(_scan_id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    // query_log doesn't store the scan_id value per INSERT, so we report the
    // global INSERT rate to file_scan over the last minute. When a single scan
    // is running this is its rate.
    let sql = "SELECT sum(written_rows) / 60 AS rows_per_sec
               FROM system.query_log
               WHERE type = 2 AND query LIKE 'INSERT%file_scan%'
                 AND event_time > now() - INTERVAL 1 MINUTE";
    let rows = state
        .ch
        .query_json(sql)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let qps = rows
        .into_iter()
        .next()
        .and_then(|v| v.get("rows_per_sec").and_then(|n| n.as_f64()))
        .unwrap_or(0.0);
    Ok(Json(serde_json::json!({ "qps": qps.round() as u64 })))
}

/// Shard status: how many candidates remain in the queue, how many dust
/// processes are active, and total candidates. Reads only in-memory state
/// (queue + active_pids) — no ClickHouse queries, μs.
async fn scan_shards(
    State(state): State<AppState>,
    Path(scan_id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let running = state.running_scans.lock().unwrap();
    let Some(rs) = running.get(&scan_id) else {
        return Ok(Json(serde_json::json!({
            "queue_remaining": 0,
            "active_pids": 0,
            "total_candidates": 0,
        })));
    };
    let q = rs.shard.queue.lock().unwrap();
    let queue_remaining = q.len();
    let active = rs.shard.active_pids.lock().unwrap().len();
    Ok(Json(serde_json::json!({
        "queue_remaining": queue_remaining,
        "active_pids": active,
    })))
}

/// Manually calibrate the file count/bytes for a scan by running count()+sum()
/// directly on file_scan. Used when finished_scans was lost (restart) or when
/// mv_scan_stats is inaccurate (duplicate triggers). Stores the result in
/// finished_scans so list_scans shows accurate numbers.
async fn calibrate_scan(
    State(state): State<AppState>,
    Path(scan_id): Path<String>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let sql = format!(
        "SELECT
            countIf(is_file=1) AS files,
            countIf(is_dir=1) AS dirs,
            countIf(is_symlink=1) AS symlinks,
            countIf(nlink >= 2 AND is_file=1) AS hardlinks,
            count() AS total,
            sumIf(size_du, is_file=1) AS bytes_du
         FROM {table} WHERE scan_id = {{sid:String}}"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &scan_id)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let r = rows.into_iter().next();
    let files = r.as_ref().and_then(|v| v.get("files")).and_then(|v| v.as_u64());
    let dirs = r.as_ref().and_then(|v| v.get("dirs")).and_then(|v| v.as_u64());
    let symlinks = r.as_ref().and_then(|v| v.get("symlinks")).and_then(|v| v.as_u64());
    let hardlinks = r.as_ref().and_then(|v| v.get("hardlinks")).and_then(|v| v.as_u64());
    let total = r.as_ref().and_then(|v| v.get("total")).and_then(|v| v.as_u64());
    let bytes_du = r.as_ref().and_then(|v| v.get("bytes_du")).and_then(|v| v.as_u64());
    // Store in finished_scans (overwrites any existing entry).
    state.finished_scans.lock().unwrap().insert(
        scan_id.clone(),
        FinishedScan {
            state: "done".to_string(),
            ended_at: now_secs(),
            exit_code: 0,
            files,
            dirs,
            symlinks,
            hardlinks,
            total,
            bytes_du,
        },
    );
    // Re-precompute dashboard stats into scan_stats in the background. Returns
    // immediately — the count above is what the caller waits on (~0.5s); the
    // ~80s of aggregations run on a blocking thread so this handler stays fast.
    let ch_read = state.ch.clone();
    let ch_write = state.ch_write.clone();
    let table_pre = state.ch.table().to_string();
    let sid_pre = scan_id.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = crate::api_stats::precompute_stats(&ch_read, &ch_write, &table_pre, &sid_pre) {
            eprintln!("precompute_stats for {sid_pre} failed: {e}");
        }
    });
    Ok(Json(serde_json::json!({
        "scan_id": scan_id,
        "files": files.unwrap_or(0),
        "dirs": dirs.unwrap_or(0),
        "symlinks": symlinks.unwrap_or(0),
        "hardlinks": hardlinks.unwrap_or(0),
        "total": total.unwrap_or(0),
        "bytes_du": bytes_du.unwrap_or(0),
    })))
}

/// A unit of work for a shard process: a path to scan + an optional max_depth.
/// max_depth=Some(1) is used for a root candidate so it scans only the root's
/// direct files (covering them without duplicating subtrees scanned by other
/// shards); None = unlimited recursion (normal subdir candidates).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanTask {
    pub path: String,
    pub max_depth: Option<usize>,
}

/// The outcome of one scheduler slot after it has processed its candidates.
/// Extracted from run_scheduler so the classification + aggregation logic can
/// be unit-tested without forking real dust processes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotOutcome {
    /// All candidates in this slot exited 0 (completed).
    Done,
    /// At least one candidate exited with a non-success status that was NOT a
    /// user cancel (non-zero exit code, unexpected signal, or spawn/wait error).
    Failed,
    /// At least one candidate was terminated by SIGTERM (user-initiated cancel).
    Cancelled,
}

/// Classify a single dust child process's exit status into a slot outcome.
///
/// - `status.success()` → Done
/// - SIGTERM → Cancelled (user clicked cancel; cancel_scan sends SIGTERM)
/// - any other signal or non-zero exit code → Failed
///
/// The wait-error case (`Err`) is handled by the caller as Failed, since this
/// function only takes the `Ok(output)` path's ExitStatus.
pub fn classify_exit(status: &std::process::ExitStatus) -> SlotOutcome {
    if status.success() {
        return SlotOutcome::Done;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            if sig == libc::SIGTERM {
                return SlotOutcome::Cancelled;
            }
        }
    }
    // Non-zero exit code, or a signal that is not SIGTERM.
    SlotOutcome::Failed
}

/// Aggregate the outcomes of all scheduler slots into a single scan state.
///
/// Priority: **Cancelled > Failed > Done**. If any slot was cancelled (even
/// alongside failures), the scan reports "cancelled" — because the user
/// initiated the cancel, and a "failed" label would mislead them into
/// thinking the scan crashed rather than was stopped. If no cancel but any
/// slot failed, the scan is "failed". Only when every slot succeeded is the
/// scan "done".
pub fn aggregate_outcomes(outcomes: &[SlotOutcome]) -> &'static str {
    if outcomes.iter().any(|o| *o == SlotOutcome::Cancelled) {
        "cancelled"
    } else if outcomes.iter().any(|o| *o == SlotOutcome::Failed) {
        "failed"
    } else {
        "done"
    }
}

/// Pick the worse of two slot outcomes, using the same priority as
/// aggregate_outcomes: Cancelled > Failed > Done. Used inside a slot to fold
/// the outcome of each candidate it processes into a single slot outcome.
fn worst_of(a: SlotOutcome, b: SlotOutcome) -> SlotOutcome {
    fn rank(o: SlotOutcome) -> u8 {
        match o {
            SlotOutcome::Done => 0,
            SlotOutcome::Failed => 1,
            SlotOutcome::Cancelled => 2,
        }
    }
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

/// Shared state for a sharded scan: the work queue (candidate directories),
/// the set of currently-active dust PIDs (for cancel), and a cancel flag.
/// Held via Arc inside RunningScan so the cancel handler and the background
/// scheduler can coordinate without race.
#[derive(Clone)]
pub struct ShardState {
    /// Candidate directories yet to be scanned. Each dust process scans one.
    pub queue: Arc<std::sync::Mutex<std::collections::VecDeque<ScanTask>>>,
    /// PIDs of dust processes currently running. fork adds, wait removes.
    pub active_pids: Arc<std::sync::Mutex<std::collections::HashSet<u64>>>,
    /// Set by cancel to stop the scheduler from forking more processes.
    pub cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// An in-flight scan: its sharding state (queue/active pids/cancel flag) +
/// start time. Held in AppState so cancel can signal processes and the list
/// handler can mark running vs finished scans.
#[derive(Clone)]
pub struct RunningScan {
    pub shard: ShardState,
    pub started_at: u64,
}

/// A scan that has finished (done / failed / cancelled). Kept in AppState so
/// the status endpoint can report the real outcome instead of guessing from
/// ClickHouse row timestamps.
#[derive(Clone)]
    pub struct FinishedScan {
    pub state: String, // "done" | "failed" | "cancelled"
    pub ended_at: u64,
    pub exit_code: i32,
    /// Accurate counts, calibrated by a one-time count() on file_scan.
    pub files: Option<u64>,
    pub dirs: Option<u64>,
    pub symlinks: Option<u64>,
    pub hardlinks: Option<u64>,
    pub total: Option<u64>,
    pub bytes_du: Option<u64>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    // ---- helpers to build directory trees ----

    /// Create a directory; no-op if it exists. Panics on real errors.
    fn mkdir(p: &Path) {
        fs::create_dir_all(p).unwrap_or_else(|e| panic!("mkdir {p:?}: {e}"));
    }

    /// Create a regular file (touches the parent dir, so dir_has_direct_files
    /// sees it). Content empty; only existence matters for the scan planner.
    fn touch(p: &Path) {
        fs::write(p, b"").unwrap_or_else(|e| panic!("touch {p:?}: {e}"));
    }

    /// Collect every directory that contains at least one direct non-directory
    /// entry (file/symlink). For each such dir, the planner MUST emit a
    /// candidate that scans its direct level (a depth=None covering it, or a
    /// depth=1 candidate), otherwise the direct files of that dir are missed.
    fn dirs_with_files(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let mut has_file = false;
            if let Ok(rd) = fs::read_dir(&d) {
                for e in rd.flatten() {
                    if let Ok(ft) = e.file_type() {
                        if ft.is_dir() {
                            stack.push(e.path());
                        } else {
                            has_file = true;
                        }
                    }
                }
            }
            if has_file {
                out.push(d.to_string_lossy().into_owned());
            }
        }
        out
    }

    // ---- the two invariants these tests lock down ----
    //
    // INV-A (coverage):  every directory that has direct files must be picked
    //   up by at least one candidate. A candidate with max_depth=None covers
    //   its own direct level AND its whole subtree. A candidate with
    //   max_depth=Some(1) covers ONLY its direct level (not descendants).
    //
    // INV-B (no-overlap): no two candidates scan the same (path, depth-scope).
    //   Two depth=None candidates must not be ancestor/descendant of each other
    //   (one would contain the other). Two depth=1 candidates must not share
    //   the same path. (A depth=1 and a depth=None may share a path: the depth=1
    //   scans direct entries, the depth=None recurses — but the planner never
    //   produces this combination for the same path, so we still assert it.)

    /// Does candidate `cand` cover the direct level of directory `dir`?
    /// - depth=None: yes if cand.path == dir (it scans its direct entries and
    ///   recurses, so its direct level is covered).
    /// - depth=Some(1): yes if cand.path == dir (scans only direct entries).
    /// A candidate never covers the direct level of a *different* directory:
    /// depth=None recurses into descendants but each descendant has its own
    /// row in dust's output, so the descendant's direct level is covered by
    /// *itself*, not by the ancestor candidate.
    fn covers_direct_level(cand: &ScanTask, dir: &str) -> bool {
        cand.path == dir
    }

    #[test]
    fn empty_root_single_candidate() {
        let root = tempdir().unwrap();
        // No subdirs, no files → the whole root is one depth=None candidate.
        let cands = expand_candidates(root.path().to_str().unwrap(), 1, &[]).unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].path, root.path().to_string_lossy());
        assert_eq!(cands[0].max_depth, None);
    }

    #[test]
    fn root_with_direct_files_gets_depth1_candidate() {
        // root/
        //   file1.txt        <- direct file under root
        //   subdir/
        let root = tempdir().unwrap();
        touch(&root.path().join("file1.txt"));
        mkdir(&root.path().join("subdir"));
        let cands = expand_candidates(root.path().to_str().unwrap(), 1, &[]).unwrap();

        // INV-A: root's direct file must be covered by a root candidate.
        let root_str = root.path().to_string_lossy().into_owned();
        let root_cand = cands
            .iter()
            .find(|c| c.path == root_str)
            .expect("a candidate scanning root's direct level must exist");
        assert_eq!(root_cand.max_depth, Some(1),
            "root with subdirs and direct files must use depth=1, not None");

        // INV-A: subdir (no direct files, no children) is a depth=None leaf.
        let subdir_str = root.path().join("subdir").to_string_lossy().into_owned();
        assert!(cands.iter().any(|c| c.path == subdir_str && c.max_depth == None),
            "leaf subdir must be a full-subtree candidate");
    }

    #[test]
    fn many_subdirs_used_as_candidates_when_enough() {
        // root/
        //   a/ b/ c/ d/   (>= min_candidates, each a leaf)
        let root = tempdir().unwrap();
        for name in ["a", "b", "c", "d"] {
            mkdir(&root.path().join(name));
        }
        let min = 4;
        let cands = expand_candidates(root.path().to_str().unwrap(), min, &[]).unwrap();

        // When root has >= min_candidates subdirs, each becomes a depth=None
        // candidate (the frontier is enough; no expansion needed).
        assert!(cands.len() >= min, "should have at least {min} candidates, got {}", cands.len());
        for c in &cands {
            if c.path != root.path().to_string_lossy() {
                assert_eq!(c.max_depth, None, "subdir candidates should be full-subtree");
            }
        }
    }

    #[test]
    fn expands_levels_until_min_candidates_met() {
        // root/
        //   only1/
        //     a/ b/ c/ d/ e/   (root has 1 subdir → expand one level → 5 leaves)
        let root = tempdir().unwrap();
        mkdir(&root.path().join("only1"));
        for name in ["a", "b", "c", "d", "e"] {
            mkdir(&root.path().join("only1").join(name));
        }
        let cands = expand_candidates(root.path().to_str().unwrap(), 5, &[]).unwrap();

        // The 5 grandchildren become leaf candidates; only1 (has subdir, no
        // direct files) produces no candidate of its own.
        let only1_str = root.path().join("only1").to_string_lossy().into_owned();
        assert!(!cands.iter().any(|c| c.path == only1_str),
            "a dir with only subdirs and no direct files should not be a candidate");
        for name in ["a", "b", "c", "d", "e"] {
            let p = root.path().join("only1").join(name).to_string_lossy().into_owned();
            assert!(cands.iter().any(|c| c.path == p && c.max_depth == None),
                "expanded leaf {name} should be a candidate");
        }
    }

    #[test]
    fn middle_dir_with_subdirs_and_direct_files_gets_depth1() {
        // root/
        //   mid/             (has a direct file AND subdirs)
        //     topfile.txt
        //     child/
        let root = tempdir().unwrap();
        let mid = root.path().join("mid");
        mkdir(&mid);
        touch(&mid.join("topfile.txt"));
        mkdir(&mid.join("child"));
        let cands = expand_candidates(root.path().to_str().unwrap(), 4, &[]).unwrap();

        // 'mid' is expanded (it has a subdir) so it's not a depth=None candidate,
        // BUT it has a direct file → must get a depth=1 candidate so topfile.txt
        // is not missed.
        let mid_str = mid.to_string_lossy().into_owned();
        let mid_cand = cands
            .iter()
            .find(|c| c.path == mid_str)
            .expect("mid (has direct files) must have a candidate");
        assert_eq!(mid_cand.max_depth, Some(1),
            "expanded dir with direct files must use depth=1");

        // child (leaf) is a full-subtree candidate.
        let child_str = mid.join("child").to_string_lossy().into_owned();
        assert!(cands.iter().any(|c| c.path == child_str && c.max_depth == None),
            "leaf child must be a full-subtree candidate");
    }

    #[test]
    fn excluded_dirs_never_become_candidates() {
        // root/
        //   .snapshot/   <- excluded, should NOT be a candidate
        //   a/ b/         <- normal
        let root = tempdir().unwrap();
        mkdir(&root.path().join(".snapshot"));
        mkdir(&root.path().join("a"));
        mkdir(&root.path().join("b"));
        let excludes = vec![".snapshot".to_string()];
        let cands = expand_candidates(root.path().to_str().unwrap(), 1, &excludes).unwrap();

        let snap_str = root.path().join(".snapshot").to_string_lossy().into_owned();
        assert!(
            !cands
                .iter()
                .any(|c| c.path == snap_str || c.path.starts_with(&format!("{snap_str}/"))),
            "excluded dir and its subtree must never be a candidate"
        );
    }

    // ---- the big invariant: coverage + no-overlap, checked automatically ----

    #[test]
    fn coverage_complete_no_overlap_flat() {
        let root = tempdir().unwrap();
        for name in ["a", "b", "c"] {
            mkdir(&root.path().join(name));
        }
        touch(&root.path().join("rootfile.txt"));
        let cands = expand_candidates(root.path().to_str().unwrap(), 1, &[]).unwrap();
        assert_coverage(&root, &cands);
    }

    #[test]
    fn coverage_complete_no_overlap_deep() {
        // A mixed tree that exercises every branch of expand_candidates:
        //   root/          (direct file → depth=1 candidate)
        //     a/           (leaf, depth=None)
        //     b/           (has direct file + subdir → depth=1 candidate)
        //       bf.txt
        //       c/         (leaf, depth=None)
        //     d/           (has only subdirs, no direct file → expanded, no own candidate)
        //       e/         (leaf, depth=None)
        //       f/         (has direct file + deeper subdir → depth=1 candidate)
        //         ff.txt
        //         g/       (leaf, depth=None)
        let root = tempdir().unwrap();
        touch(&root.path().join("rootfile.txt"));
        mkdir(&root.path().join("a"));
        let b = root.path().join("b");
        mkdir(&b);
        touch(&b.join("bf.txt"));
        mkdir(&b.join("c"));
        let d = root.path().join("d");
        mkdir(&d);
        mkdir(&d.join("e"));
        let f = d.join("f");
        mkdir(&f);
        touch(&f.join("ff.txt"));
        mkdir(&f.join("g"));
        let cands = expand_candidates(root.path().to_str().unwrap(), 100, &[]).unwrap();
        assert_coverage(&root, &cands);
    }

    #[test]
    fn coverage_complete_no_overlap_sparse_shards() {
        // Few subdirs but high min_candidates forces deep expansion, then the
        // remaining frontier (stopped at min_candidates) still must cover.
        //   root/
        //     x/           (only one subdir → must expand deeply)
        //       x1/ x2/    (2 leaves; min=10 cannot be met → frontier exhausted)
        let root = tempdir().unwrap();
        mkdir(&root.path().join("x").join("x1"));
        mkdir(&root.path().join("x").join("x2"));
        let cands = expand_candidates(root.path().to_str().unwrap(), 10, &[]).unwrap();
        assert_coverage(&root, &cands);
    }

    /// Assert the two invariants for a built tree + its candidates:
    ///
    /// INV-A (coverage): every directory with direct files is covered by some
    /// candidate's direct-level scope.
    /// INV-B (no-overlap): no two depth=None candidates are in an
    /// ancestor/descendant relationship, and no two candidates share the exact
    /// same (path, max_depth) pair.
    fn assert_coverage(root: &tempfile::TempDir, cands: &[ScanTask]) {
        let root_path = root.path();

        // INV-A: every dir with direct files is covered.
        for dir in dirs_with_files(root_path) {
            let covered = cands.iter().any(|c| covers_direct_level(c, &dir));
            assert!(covered,
                "INV-A violated: dir {dir} has direct files but no candidate covers its direct level\n\
                 candidates: {cands:?}");
        }

        // INV-B.1: no two depth=None candidates where one contains the other.
        let full: Vec<&ScanTask> = cands.iter().filter(|c| c.max_depth.is_none()).collect();
        for i in 0..full.len() {
            for j in (i + 1)..full.len() {
                let (a, b) = (full[i].path.trim_end_matches('/'), full[j].path.trim_end_matches('/'));
                let nested = a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"));
                assert!(!nested,
                    "INV-B violated: nested full-subtree candidates\n  {a}\n  {b}");
            }
        }

        // INV-B.2: no duplicate (path, max_depth) pairs.
        for i in 0..cands.len() {
            for j in (i + 1)..cands.len() {
                let same = cands[i].path == cands[j].path && cands[i].max_depth == cands[j].max_depth;
                assert!(!same,
                    "INV-B violated: duplicate candidate\n  {:#?}\n  {:#?}",
                    cands[i], cands[j]);
            }
        }

        // Sanity: the candidate paths are all real (the planner only ever
        // points at existing dirs or the root itself).
        for c in cands {
            assert!(Path::new(&c.path).is_dir(),
                "candidate path is not an existing directory: {}", c.path);
        }
    }

    // ===== run_scheduler state classification tests =====
    //
    // These test the pure functions extracted from run_scheduler:
    // classify_exit (one child → SlotOutcome) and aggregate_outcomes (many
    // slots → final state). The most dangerous regression these guard against
    // is reporting a FAILED scan as "done" — the user trusts the green
    // "complete" badge while the data is actually incomplete.
    //
    // classify_exit needs a real std::process::ExitStatus, which has no stable
    // public constructor. So we fork child processes that exit in controlled
    // ways (exit 0 / exit 1 / killed by SIGTERM / killed by SIGSEGV) and use
    // their real exit statuses.

    use std::process::Command;

    /// Run a command that exits with the given code, return its ExitStatus.
    fn exit_status(code: i32) -> std::process::ExitStatus {
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .expect("sh should spawn");
        status
    }

    /// Run a command that kills itself with the given signal, return its
    /// ExitStatus. Uses `kill` builtin from sh, which sends the signal to the
    /// sh process itself.
    #[cfg(unix)]
    fn killed_by_signal(sig: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -{sig} $$"))
            .status()
            .expect("sh should spawn");
        // On some shells `kill -SIG $$` may exit with a code instead of dying
        // by the signal. Verify it actually died by signal; if not, fall back
        // to a synthetic ExitStatus built from the raw signal value.
        if status.signal() == Some(sig) {
            status
        } else {
            // Synthetic: build from raw (signal, no code). This is the same
            // representation a real signal-killed process would have.
            ExitStatusExt::from_raw(sig << 8)
        }
    }

    #[test]
    fn classify_exit_success_is_done() {
        assert_eq!(classify_exit(&exit_status(0)), SlotOutcome::Done);
    }

    #[test]
    fn classify_exit_nonzero_is_failed() {
        // Any non-zero exit code → Failed (dust reported an error).
        assert_eq!(classify_exit(&exit_status(1)), SlotOutcome::Failed);
        assert_eq!(classify_exit(&exit_status(2)), SlotOutcome::Failed);
        assert_eq!(classify_exit(&exit_status(139)), SlotOutcome::Failed);
    }

    #[test]
    #[cfg(unix)]
    fn classify_exit_sigterm_is_cancelled() {
        // SIGTERM = user cancel (cancel_scan sends SIGTERM to dust).
        assert_eq!(classify_exit(&killed_by_signal(libc::SIGTERM)), SlotOutcome::Cancelled);
    }

    #[test]
    #[cfg(unix)]
    fn classify_exit_other_signal_is_failed() {
        // SIGSEGV / SIGABRT / SIGKILL are not user cancels → Failed.
        assert_eq!(classify_exit(&killed_by_signal(libc::SIGSEGV)), SlotOutcome::Failed);
        assert_eq!(classify_exit(&killed_by_signal(libc::SIGABRT)), SlotOutcome::Failed);
        assert_eq!(classify_exit(&killed_by_signal(libc::SIGKILL)), SlotOutcome::Failed);
    }

    // ---- aggregate_outcomes: the priority that matters ----

    #[test]
    fn aggregate_all_done_is_done() {
        assert_eq!(aggregate_outcomes(&[SlotOutcome::Done, SlotOutcome::Done]), "done");
        assert_eq!(aggregate_outcomes(&[SlotOutcome::Done]), "done");
        assert_eq!(aggregate_outcomes(&[]), "done"); // no slots = nothing failed = done
    }

    #[test]
    fn aggregate_any_failed_is_failed() {
        assert_eq!(aggregate_outcomes(&[SlotOutcome::Done, SlotOutcome::Failed]), "failed");
        assert_eq!(aggregate_outcomes(&[SlotOutcome::Failed, SlotOutcome::Done]), "failed");
        assert_eq!(aggregate_outcomes(&[SlotOutcome::Failed, SlotOutcome::Failed]), "failed");
    }

    #[test]
    fn aggregate_cancelled_beats_failed() {
        // THE critical priority: a cancel + a failure reports "cancelled", not
        // "failed". The user clicked cancel, so "failed" would mislead them
        // into thinking the scan crashed rather than was stopped.
        assert_eq!(
            aggregate_outcomes(&[SlotOutcome::Failed, SlotOutcome::Cancelled]),
            "cancelled"
        );
        assert_eq!(
            aggregate_outcomes(&[SlotOutcome::Cancelled, SlotOutcome::Failed]),
            "cancelled"
        );
    }

    #[test]
    fn aggregate_cancelled_beats_done() {
        assert_eq!(
            aggregate_outcomes(&[SlotOutcome::Done, SlotOutcome::Cancelled, SlotOutcome::Done]),
            "cancelled"
        );
    }

    #[test]
    fn aggregate_mixed_cancelled_failed_done_is_cancelled() {
        // One of each: cancelled wins (highest priority).
        assert_eq!(
            aggregate_outcomes(&[SlotOutcome::Done, SlotOutcome::Failed, SlotOutcome::Cancelled]),
            "cancelled"
        );
    }

    // ---- worst_of: per-slot folding (same priority as aggregate) ----

    #[test]
    fn worst_of_dominance() {
        // Cancelled dominates everything.
        assert_eq!(worst_of(SlotOutcome::Done, SlotOutcome::Cancelled), SlotOutcome::Cancelled);
        assert_eq!(worst_of(SlotOutcome::Cancelled, SlotOutcome::Done), SlotOutcome::Cancelled);
        assert_eq!(worst_of(SlotOutcome::Failed, SlotOutcome::Cancelled), SlotOutcome::Cancelled);
        // Failed dominates Done.
        assert_eq!(worst_of(SlotOutcome::Done, SlotOutcome::Failed), SlotOutcome::Failed);
        assert_eq!(worst_of(SlotOutcome::Failed, SlotOutcome::Done), SlotOutcome::Failed);
        // Same stays same.
        assert_eq!(worst_of(SlotOutcome::Done, SlotOutcome::Done), SlotOutcome::Done);
        assert_eq!(worst_of(SlotOutcome::Failed, SlotOutcome::Failed), SlotOutcome::Failed);
        assert_eq!(worst_of(SlotOutcome::Cancelled, SlotOutcome::Cancelled), SlotOutcome::Cancelled);
    }

    /// A slot processes multiple candidates; worst_of folds them. Simulate a
    /// slot that did Done, Done, then Failed — its slot outcome should be Failed.
    #[test]
    fn worst_of_folds_multiple_candidates_to_worst() {
        let mut o = SlotOutcome::Done;
        o = worst_of(o, SlotOutcome::Done);
        o = worst_of(o, SlotOutcome::Done);
        o = worst_of(o, SlotOutcome::Failed);
        assert_eq!(o, SlotOutcome::Failed);
        // Now add a cancel — should jump to Cancelled.
        o = worst_of(o, SlotOutcome::Cancelled);
        assert_eq!(o, SlotOutcome::Cancelled);
    }
}
