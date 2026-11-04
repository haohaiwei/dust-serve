-- dust-serve file_scan table — ReplacingMergeTree with (scan_id, path) dedup
-- Same scan_id + same path → deduplicated (fixes duplicate candidates).
-- Different scan_id + same path → kept (multiple scans preserved).
-- FINAL or argMax needed in queries to guarantee dedup at read time.

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
ENGINE = ReplacingMergeTree(scan_time)
PARTITION BY toYYYYMM(toDateTime(scan_time))
ORDER BY (scan_id, path)
TTL toDateTime(scan_time) + INTERVAL 180 DAY;
