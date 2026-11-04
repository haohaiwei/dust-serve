use axum::{
    extract::{Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub path: Option<String>,
    /// "file" | "dir" | "symlink" | "all"
    #[serde(default = "default_kind")]
    pub kind: String,
    /// If true, restrict to broken symlinks (symlink_target not in scanned paths).
    #[serde(default)]
    pub broken_symlink: bool,
    /// If true, restrict to sparse files: apparent size (size) > disk usage
    /// (size_du). Only meaningful when scans run with --apparent-size so that
    /// size = st_size and size_du = st_blocks*512; without it both are equal
    /// and this filter matches nothing.
    #[serde(default)]
    pub sparse: bool,
    pub limit: Option<u32>,
    /// Filters (all optional, empty = no filter)
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// mtime bounds in seconds since epoch
    pub mtime_after: Option<i64>,
    pub mtime_before: Option<i64>,
    pub atime_after: Option<i64>,
    pub atime_before: Option<i64>,
    /// owner uid (exact match)
    pub uid: Option<u32>,
    /// extension exact match, lowercase, no dot
    pub extension: Option<String>,
    /// Find files by inode (optionally also by device). Used to list every
    /// path of a hardlink group when only inode (+device) is known.
    pub inode: Option<u64>,
    pub device: Option<u64>,
    /// Minimum hardlink count (nlink). nlink > 1 means the file has multiple
    /// paths; nlink >= 3 finds heavily hardlinked files.
    pub min_nlink: Option<u64>,
    /// Optional scan_id to query (defaults to the latest scan when empty).
    pub scan_id: Option<String>,
    pub order_by: Option<String>,
    pub order_dir: Option<String>,
}

fn default_kind() -> String {
    "all".to_string()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/search", get(search))
        .route("/dir", get(list_dir))
        .route("/file", get(file_detail))
        .route("/subdirs", get(subdirs))
        .route("/owners", get(owners))
        .route("/extensions", get(extensions))
        .route("/uid_lookup", get(uid_lookup))
}

async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let latest = resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(serde_json::json!({ "results": [] })));
    }

    // kind_filter / path_filter / q_filter are trusted static strings inserted
    // verbatim into the final SQL via the {x_filter} format substitutions. Since
    // format! does not re-process the *contents* of interpolated variables, the
    // ClickHouse parameter placeholders inside them must use single braces
    // ({q:String}), NOT the {{...}} double-brace escaping (which only works for
    // literal text in the format string itself).
    let kind_filter = match q.kind.as_str() {
        "file" => "AND is_file = 1",
        "dir" => "AND is_dir = 1",
        "symlink" => "AND is_symlink = 1",
        _ => "",
    };

    // Broken symlinks: symlink_target not among the scanned paths. Compare by
    // suffix (endsWith) to be robust to mount-prefix mismatches in containerized
    // scans (target is a host path, scanned path may have a /host/ prefix).
    // {table} is replaced here via .replace; the {sid:String} stays as a
    // literal ClickHouse param — the outer where_clause format! does NOT
    // re-process the contents of this inserted String, so single braces are
    // preserved for ClickHouse.
    let broken_symlink_filter = if q.broken_symlink {
        // Use s.symlink_target (correlated to outer table alias s). The subquery
        // count can return NULL in some ClickHouse correlated cases, so wrap
        // with coalesce to treat it as 0 (no matching target = broken).
        "AND is_symlink = 1 AND notEmpty(s.symlink_target) AND coalesce((SELECT count() FROM {table} AS c WHERE c.scan_id = {sid:String} AND endsWith(c.path, s.symlink_target)), 0) = 0".replace("{table}", &table)
    } else {
        String::new()
    };

    // Sparse files: apparent size (size = st_size) exceeds disk usage
    // (size_du = st_blocks*512). Files only (dirs/symlinks don't carry a
    // meaningful apparent/disk split). Requires scans run with --apparent-size;
    // otherwise size == size_du for every row and this matches nothing.
    let sparse_filter = if q.sparse {
        "AND is_file = 1 AND size > size_du"
    } else {
        ""
    };

    let path_filter = if q.path.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        "AND startsWith(path, {path:String})"
    } else {
        ""
    };

    let q_filter = if q.q.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        // Match against name OR full path, so a path fragment like "/tmp/"
        // also returns hits (not just filename substrings).
        "AND (positionCaseInsensitive(name, {q:String}) > 0 OR positionCaseInsensitive(path, {q:String}) > 0)"
    } else {
        ""
    };

    let min_size_filter = if q.min_size.is_some() {
        "AND size_du >= {min_size:UInt64}"
    } else {
        ""
    };
    let max_size_filter = if q.max_size.is_some() {
        "AND size_du <= {max_size:UInt64}"
    } else {
        ""
    };
    let mtime_after_filter = if q.mtime_after.is_some() {
        "AND mtime >= {mtime_after:Int64}"
    } else {
        ""
    };
    let mtime_before_filter = if q.mtime_before.is_some() {
        "AND mtime <= {mtime_before:Int64}"
    } else {
        ""
    };
    let atime_after_filter = if q.atime_after.is_some() {
        "AND atime >= {atime_after:Int64}"
    } else {
        ""
    };
    let atime_before_filter = if q.atime_before.is_some() {
        "AND atime <= {atime_before:Int64}"
    } else {
        ""
    };
    let uid_filter = if q.uid.is_some() {
        "AND uid = {uid:UInt32}"
    } else {
        ""
    };
    let extension_filter = if q.extension.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        "AND extension = {extension:String}"
    } else {
        ""
    };
    // inode/device: Nullable columns — match only when not null. Used to list
    // all paths of a hardlink group (same inode+device = same physical file).
    let inode_filter = if q.inode.is_some() {
        "AND isNotNull(inode) AND inode = {inode:UInt64}"
    } else {
        ""
    };
    let device_filter = if q.device.is_some() {
        "AND isNotNull(device) AND device = {device:UInt64}"
    } else {
        ""
    };
    // min_nlink finds hard-linked FILES (nlink > 1 means multiple paths to the
    // same file). Directories also have nlink > 1 (it counts subdirs + 2), but
    // that's not a "hardlink" — so this filter always restricts to files only,
    // regardless of the kind filter.
    let nlink_filter = if q.min_nlink.is_some() {
        "AND is_file = 1 AND nlink >= {min_nlink:UInt64}"
    } else {
        ""
    };

    // Validate order_by against an allowlist to prevent SQL injection — the
    // value is interpolated literally into ORDER BY.
    let order_field = match q.order_by.as_deref().unwrap_or("size_du") {
        "size_du" => "size_du",
        "size" => "size",
        "mtime" => "mtime",
        "atime" => "atime",
        "name" => "name",
        _ => "size_du",
    };
    let order_dir = match q.order_dir.as_deref().unwrap_or("desc") {
        "asc" => "ASC",
        _ => "DESC",
    };

    // The literal SQL below uses {table} and {limit} as real format! args, so
    // any literal braces that must survive into the output (the ClickHouse
    // {sid:String} placeholder) are escaped as {{...}} here.
    let where_clause = format!(
        "WHERE scan_id = {{sid:String}}
         {kind_filter}
         {path_filter}
         {q_filter}
         {min_size_filter}
         {max_size_filter}
         {mtime_after_filter}
         {mtime_before_filter}
         {atime_after_filter}
         {atime_before_filter}
         {uid_filter}
         {extension_filter}
         {inode_filter}
         {device_filter}
         {nlink_filter}
         {broken_symlink_filter}
         {sparse_filter}"
    );
    let sql = format!(
        "SELECT path, name, extension, parent_path, size, size_du, mtime, atime, ctime,
            nlink, uid, gid, inode, device, is_dir, is_file, is_symlink, symlink_target, depth
         FROM {table} AS s
         {where_clause}
         ORDER BY {order_field} {order_dir}
         LIMIT {limit}"
    );

    let mut params: Vec<(&str, String)> = vec![("sid", latest)];
    if let Some(p) = &q.path {
        if !p.is_empty() {
            params.push(("path", p.clone()));
        }
    }
    if let Some(qs) = &q.q {
        if !qs.is_empty() {
            params.push(("q", qs.clone()));
        }
    }
    if let Some(v) = q.min_size {
        params.push(("min_size", v.to_string()));
    }
    if let Some(v) = q.max_size {
        params.push(("max_size", v.to_string()));
    }
    if let Some(v) = q.mtime_after {
        params.push(("mtime_after", v.to_string()));
    }
    if let Some(v) = q.mtime_before {
        params.push(("mtime_before", v.to_string()));
    }
    if let Some(v) = q.atime_after {
        params.push(("atime_after", v.to_string()));
    }
    if let Some(v) = q.atime_before {
        params.push(("atime_before", v.to_string()));
    }
    if let Some(v) = q.uid {
        params.push(("uid", v.to_string()));
    }
    if let Some(v) = &q.extension {
        if !v.is_empty() {
            params.push(("extension", v.clone()));
        }
    }
    if let Some(v) = q.inode {
        params.push(("inode", v.to_string()));
    }
    if let Some(v) = q.device {
        params.push(("device", v.to_string()));
    }
    if let Some(v) = q.min_nlink {
        params.push(("min_nlink", v.to_string()));
    }
    let param_refs: Vec<(&str, &str)> =
        params.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let rows = state
        .ch
        .query_json_param(&sql, &param_refs)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Total matching rows (same WHERE, no LIMIT/ORDER). Lets the frontend show
    // "N of M" without paging millions of rows.
    let count_sql = format!("SELECT count() FROM {table} AS s {where_clause}");
    let total = state
        .ch
        .query_scalar_param(&count_sql, &param_refs)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let total: u64 = total.trim().parse().unwrap_or(0);

    Ok(Json(serde_json::json!({ "results": rows, "total": total })))
}

async fn list_dir(
    State(state): State<AppState>,
    Query(q): Query<DirQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let latest = resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(serde_json::json!({ "entries": [] })));
    }
    let path = q.path.unwrap_or_default();
    // Use arg_max to pick the per-directory size: directories themselves
    // carry their own size_du from the scan, and we want the row with the
    // latest scan_time in case of partial scans. Parent_path filtering gives
    // the immediate children.
    let sql = format!(
        "SELECT name, size, size_du, mtime, is_dir, is_file, is_symlink, nlink, uid, gid
         FROM {table}
         WHERE scan_id = {{sid:String}}
           AND parent_path = {{path:String}}
         ORDER BY is_dir DESC, size_du DESC
         LIMIT 5000"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &latest), ("path", &path)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "entries": rows })))
}

#[derive(Debug, Deserialize)]
pub struct DirQuery {
    pub path: Option<String>,
    #[serde(default)]
    pub scan_id: Option<String>,
}

/// Query param for endpoints that only need to select a scan_id.
#[derive(Debug, Deserialize)]
pub struct ScanIdQuery {
    #[serde(default)]
    pub scan_id: Option<String>,
}

async fn file_detail(
    State(state): State<AppState>,
    Query(q): Query<FileQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let latest = resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(serde_json::json!({})));
    }
    let path = q.path.unwrap_or_default();
    let sql = format!(
        "SELECT path, name, size, size_du, mtime, atime, ctime, nlink, uid, gid,
            inode, device, is_dir, is_file, is_symlink, symlink_target, depth
         FROM {table}
         WHERE scan_id = {{sid:String}} AND path = {{path:String}}
         LIMIT 1"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &latest), ("path", &path)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(rows.into_iter().next().unwrap_or_default()))
}

#[derive(Debug, Deserialize)]
pub struct FileQuery {
    pub path: Option<String>,
    #[serde(default)]
    pub scan_id: Option<String>,
}

/// List immediate subdirectories of a path, each with a child count.
/// Used to render the collapsible directory tree.
async fn subdirs(
    State(state): State<AppState>,
    Query(q): Query<DirQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let table = state.ch.table();
    let latest = resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(serde_json::json!({ "dirs": [] })));
    }
    let parent = q.path.unwrap_or_default();
    // When path is empty, the user wants the scan root's immediate subdirs.
    // The root's subdirs have parent_path = the scan root path (e.g.
    // /mnt/moonshot-fs01), not an empty string. So we resolve the root path
    // by finding the shallowest parent_path in the scan.
    // Exclude .snapshot paths: old scans may contain snapshot data with
    // shorter paths, which would resolve the root incorrectly.
    let parent = if parent.is_empty() {
        let root_sql = format!(
            "SELECT argMin(parent_path, length(parent_path))
             FROM {table} WHERE scan_id = {{sid:String}} AND is_dir = 1
               AND parent_path NOT LIKE '%.snapshot%'"
        );
        state
            .ch
            .query_scalar_param(&root_sql, &[("sid", &latest)])
            .unwrap_or_default()
    } else {
        parent
    };
    // Children whose parent_path equals the given path. For each child dir,
    // also count its own subdirectories via a correlated count.
    let sql = format!(
        "SELECT
            name,
            size_du,
            mtime,
            (SELECT count() FROM {table} c
             WHERE c.scan_id = {{sid:String}}
               AND c.is_dir = 1
               AND c.parent_path = concat({{parent:String}}, '/', name)
            ) AS child_count
         FROM {table}
         WHERE scan_id = {{sid:String}}
           AND is_dir = 1
           AND parent_path = {{parent:String}}
         ORDER BY size_du DESC
         LIMIT 1000"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &latest), ("parent", &parent)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "dirs": rows })))
}

/// List distinct uids present in the scan, for the owner filter dropdown.
async fn owners(
    State(state): State<AppState>,
    Query(q): Query<ScanIdQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let latest = resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(serde_json::json!({ "owners": [] })));
    }
    let sql = format!(
        "SELECT uid, sum(cnt) AS cnt, sum(bytes) AS bytes
         FROM mv_owner_stats
         WHERE scan_id = {{sid:String}}
         GROUP BY uid ORDER BY bytes DESC LIMIT 100"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &latest)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "owners": rows })))
}

/// List distinct extensions present in the scan, for the extension filter.
async fn extensions(
    State(state): State<AppState>,
    Query(q): Query<ScanIdQuery>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    let latest = resolve_scan_id(&state, &q.scan_id);
    if latest.is_empty() {
        return Ok(Json(serde_json::json!({ "extensions": [] })));
    }
    let sql = format!(
        "SELECT extension, sum(cnt) AS cnt, sum(bytes) AS bytes
         FROM mv_extension_stats
         WHERE scan_id = {{sid:String}}
         GROUP BY extension ORDER BY bytes DESC LIMIT 100"
    );
    let rows = state
        .ch
        .query_json_param(&sql, &[("sid", &latest)])
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(serde_json::json!({ "extensions": rows })))
}

#[derive(Debug, Deserialize)]
pub struct UidLookupQuery {
    /// comma-separated uids, e.g. "0,1000,1002"
    pub uids: Option<String>,
    pub gids: Option<String>,
}

/// Resolve uid/gid numbers to user/group names on the *display* node (where
/// dust-serve runs and can read /etc/passwd). This avoids storing names at
/// scan time, which would be wrong if the scan node lacks the user. If a uid
/// can't be resolved here, the raw number is returned so the frontend can
/// fall back to "uid=N".
async fn uid_lookup(
    State(_state): State<AppState>,
    Query(q): Query<UidLookupQuery>,
) -> Json<Value> {
    let mut uid_map = serde_json::Map::new();
    let mut gid_map = serde_json::Map::new();
    if let Some(uids) = &q.uids {
        for s in uids.split(',') {
            if let Ok(uid) = s.trim().parse::<u32>() {
                let name = lookup_user(uid).unwrap_or_else(|| format!("uid={uid}"));
                uid_map.insert(s.trim().to_string(), Value::String(name));
            }
        }
    }
    if let Some(gids) = &q.gids {
        for s in gids.split(',') {
            if let Ok(gid) = s.trim().parse::<u32>() {
                let name = lookup_group(gid).unwrap_or_else(|| format!("gid={gid}"));
                gid_map.insert(s.trim().to_string(), Value::String(name));
            }
        }
    }
    Json(serde_json::json!({ "users": uid_map, "groups": gid_map }))
}

#[cfg(target_family = "unix")]
fn lookup_user(uid: u32) -> Option<String> {
    use std::ffi::CStr;
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            None
        } else {
            let name = CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned();
            Some(name)
        }
    }
}

#[cfg(target_family = "unix")]
fn lookup_group(gid: u32) -> Option<String> {
    use std::ffi::CStr;
    unsafe {
        let gr = libc::getgrgid(gid);
        if gr.is_null() {
            None
        } else {
            let name = CStr::from_ptr((*gr).gr_name).to_string_lossy().into_owned();
            Some(name)
        }
    }
}

#[cfg(not(target_family = "unix"))]
fn lookup_user(_uid: u32) -> Option<String> {
    None
}
#[cfg(not(target_family = "unix"))]
fn lookup_group(_gid: u32) -> Option<String> {
    None
}

/// Return the most recent scan_id (by max scan_time). Cached per-request.
fn latest_scan_id(state: &AppState) -> Option<String> {
    let table = state.ch.table();
    let sql = format!(
        "SELECT scan_id FROM {table}
         ORDER BY scan_time DESC
         LIMIT 1"
    );
    state.ch.query_scalar(&sql).ok().filter(|s| !s.is_empty())
}

/// Resolve the scan_id to query: prefer an explicit selection, else fall back
/// to the latest scan. Returns "" when there's no scan at all.
pub fn resolve_scan_id(state: &AppState, explicit: &Option<String>) -> String {
    explicit
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| latest_scan_id(state))
        .unwrap_or_default()
}
