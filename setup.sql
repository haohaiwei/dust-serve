-- dust-serve end-to-end test setup
-- Run via: clickhouse-client --multiquery < setup.sql  or  curl each statement

-- 1. file_scan table (matches dust FileRecord output)
CREATE TABLE IF NOT EXISTS file_scan (
    path            String,
    name            String,
    extension       LowCardinality(String),
    parent_path     String,
    size            UInt64,
    size_du         UInt64,
    inode           Nullable(UInt64),
    device          Nullable(UInt64),
    mtime           Int64,
    atime           Int64,
    ctime           Int64,
    nlink           UInt64 DEFAULT 0,
    uid             UInt32 DEFAULT 0,
    gid             UInt32 DEFAULT 0,
    is_dir          Bool,
    is_file         Bool,
    is_symlink      Bool,
    symlink_target  Nullable(String),
    depth           UInt32,
    scan_id         LowCardinality(String),
    scan_time       Int64
)
ENGINE = MergeTree()
PARTITION BY toYYYYMM(toDateTime(scan_time))
ORDER BY (parent_path, name)
TTL toDateTime(scan_time) + INTERVAL 180 DAY;

-- 1b. Materialized views for fast aggregation (avoid full-scan of file_scan).
-- Each MV pre-aggregates by scan_id (or scan_id+dim) so list_scans / stats /
-- owners / extensions return in milliseconds instead of scanning all rows.

-- /api/scans: per-scan summary (files, bytes, started, ended).
-- min/max need AggregatingMergeTree + minState/maxState + minMerge/maxMerge.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_scan_stats
ENGINE = AggregatingMergeTree() ORDER BY scan_id
AS SELECT
    scan_id,
    count() AS files,
    sum(size_du) AS bytes_du,
    minState(scan_time) AS started_state,
    maxState(scan_time) AS ended_state
FROM file_scan GROUP BY scan_id;

-- /api/stats by_extension + /api/extensions: per-scan, per-extension counts/bytes.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_extension_stats
ENGINE = SummingMergeTree() ORDER BY (scan_id, extension)
AS SELECT
    scan_id,
    extension,
    count() AS cnt,
    sum(size_du) AS bytes
FROM file_scan WHERE is_file = 1 GROUP BY scan_id, extension;

-- /api/owners: per-scan, per-uid counts/bytes.
CREATE MATERIALIZED VIEW IF NOT EXISTS mv_owner_stats
ENGINE = SummingMergeTree() ORDER BY (scan_id, uid)
AS SELECT
    scan_id,
    uid,
    count() AS cnt,
    sum(size_du) AS bytes
FROM file_scan GROUP BY scan_id, uid;

-- 1c. Pre-computed dashboard stats (one row per scan_id + stat_key, JSON value).
-- Populated by dust-serve after a scan completes and on manual calibrate, so
-- the dashboard reads scan_stats (ms) instead of scanning file_scan (~80s for
-- 2B rows). ReplacingMergeTree dedups by (scan_id, stat_key); latest updated_at
-- wins, so re-precomputing overwrites stale values.
CREATE TABLE IF NOT EXISTS scan_stats (
    scan_id     String,
    stat_key    LowCardinality(String),
    stat_value  String,          -- JSON string (array or object, per stat_key)
    updated_at  DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (scan_id, stat_key);

-- 2. users for read/write separation
CREATE USER IF NOT EXISTS readonly IDENTIFIED WITH plaintext_password BY 'readonly';
GRANT SELECT ON default.file_scan TO readonly;
GRANT SELECT ON default.dust_users TO readonly;
-- readonly also needs SELECT on the materialized views (used by list_scans/stats/owners/extensions).
GRANT SELECT ON default.mv_scan_stats TO readonly;
GRANT SELECT ON default.mv_extension_stats TO readonly;
GRANT SELECT ON default.mv_owner_stats TO readonly;
-- readonly needs SELECT on system.query_log for the live-scan QPS endpoint.
GRANT SELECT ON system.query_log TO readonly;
-- readonly reads precomputed dashboard stats.
GRANT SELECT ON default.scan_stats TO readonly;

CREATE USER IF NOT EXISTS dust_writer IDENTIFIED WITH plaintext_password BY 'changeme';
GRANT SELECT, INSERT ON default.file_scan TO dust_writer;
GRANT SELECT, INSERT, CREATE TABLE ON default.dust_users TO dust_writer;
-- dust_writer populates scan_stats after scans (precompute on completion/calibrate).
GRANT SELECT, INSERT ON default.scan_stats TO dust_writer;

-- 3. dust_users table will be auto-created by dust-serve on first start,
--    but create it here too so readonly can be granted before the app boots.
CREATE TABLE IF NOT EXISTS dust_users (
    username String,
    pass_hash String,
    created_at DateTime DEFAULT now()
) ENGINE = ReplacingMergeTree() ORDER BY username;

-- sanity check
SELECT 'setup ok' AS status, count() AS tables
FROM system.tables
WHERE database = 'default' AND name IN ('file_scan', 'dust_users');

-- 4. Backfill materialized views from existing file_scan data (one-time).
-- New inserts populate MVs automatically; this covers data already present.
-- (Run once. Re-running duplicates rows in MVs; to redo, TRUNCATE the MVs first.)
INSERT INTO mv_scan_stats
SELECT scan_id, count() AS files, sum(size_du) AS bytes_du,
       minState(scan_time) AS started_state, maxState(scan_time) AS ended_state
FROM file_scan GROUP BY scan_id;

INSERT INTO mv_extension_stats
SELECT scan_id, extension, count() AS cnt, sum(size_du) AS bytes
FROM file_scan WHERE is_file = 1 GROUP BY scan_id, extension;

INSERT INTO mv_owner_stats
SELECT scan_id, uid, count() AS cnt, sum(size_du) AS bytes
FROM file_scan GROUP BY scan_id, uid;

-- 5. System log tables TTL — prevent disk exhaustion.
-- The stock ClickHouse image creates these system.*_log tables WITHOUT a TTL,
-- so they grow forever. Under a 2.8B-row scan the volume is significant
-- (trace_log/text_log alone reach hundreds of MB on a near-idle test instance).
-- Add a TTL so old partitions are dropped automatically. These are ALTERs on
-- system tables; they need admin (the ch-init service runs as admin).
ALTER TABLE system.query_log              MODIFY TTL event_date + INTERVAL 30 DAY;
ALTER TABLE system.trace_log              MODIFY TTL event_date + INTERVAL 7 DAY;
ALTER TABLE system.text_log               MODIFY TTL event_date + INTERVAL 7 DAY;
ALTER TABLE system.part_log               MODIFY TTL event_date + INTERVAL 30 DAY;
ALTER TABLE system.metric_log             MODIFY TTL event_date + INTERVAL 30 DAY;
ALTER TABLE system.asynchronous_metric_log MODIFY TTL event_date + INTERVAL 7 DAY;
ALTER TABLE system.query_views_log         MODIFY TTL event_date + INTERVAL 30 DAY;
ALTER TABLE system.processors_profile_log  MODIFY TTL event_date + INTERVAL 7 DAY;
ALTER TABLE system.background_schedule_pool_log MODIFY TTL event_date + INTERVAL 7 DAY;
ALTER TABLE system.error_log               MODIFY TTL event_date + INTERVAL 30 DAY;
