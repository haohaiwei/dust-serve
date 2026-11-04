-- 迁移某个 scan_id 的数据到 ReplacingMergeTree 新表
-- 步骤:
--   1. 建新表
--   2. 从旧表导入指定 scan_id
--   3. 验证
--   4. (可选) 切换表名

-- 1. 建新表
CREATE TABLE IF NOT EXISTS file_scan_new (
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

-- 2. 从旧表导入指定 scan_id（替换 SCAN_ID_HERE）
--    ReplacingMergeTree 按 (scan_id, path) 去重，重复行自动合并
INSERT INTO file_scan_new
SELECT * FROM file_scan
WHERE scan_id = 'SCAN_ID_HERE';

-- 3. 验证
-- 总行数
SELECT count() FROM file_scan_new WHERE scan_id = 'SCAN_ID_HERE';
-- 唯一路径数（去重后应该等于路径数）
SELECT uniqExact(path) FROM file_scan_new WHERE scan_id = 'SCAN_ID_HERE';
-- 重复检查（合并后应为 0）
SELECT path, count() AS cnt
FROM file_scan_new WHERE scan_id = 'SCAN_ID_HERE'
GROUP BY path HAVING count() > 1
LIMIT 10;

-- 4. 强制合并（去重）
OPTIMIZE TABLE file_scan_new FINAL;

-- 5. 再次验证（合并后重复应为 0）
SELECT path, count() AS cnt
FROM file_scan_new WHERE scan_id = 'SCAN_ID_HERE'
GROUP BY path HAVING count() > 1
LIMIT 10;

-- 6. (可选) 切换表名
--    注意: 需要停止 dust-serve 再切换
-- RENAME TABLE file_scan TO file_scan_old;
-- RENAME TABLE file_scan_new TO file_scan;
