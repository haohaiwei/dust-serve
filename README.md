# dust-serve

文件系统快速扫描工具，基于dust 提供Web 前端 + 扫描管理 API, 扫描 + ClickHouse 文件索引方案提供服务层。

## 架构

```
浏览器 (React SPA)
   │  HTTP + JWT
   ▼
dust-serve (Rust, axum)  ─── 单一二进制
   ├─ /api/login        认证(用户名密码 + JWT)
   ├─ /api/scan         fork dust 进程启动扫描
   ├─ /api/scans        历史扫描列表
   ├─ /api/search       文件搜索(参数化 SQL,防注入)
   ├─ /api/dir          目录浏览
   ├─ /api/file         文件详情
   ├─ /api/stats        仪表盘聚合
   └─ /*                托管前端静态文件 (web/dist)
   │
   ▼
ClickHouse :8123
   ├─ file_scan 表         dust scan 写入(主表,按 scan_time 月分区)
   ├─ mv_scan_stats 等 MV  预聚合视图(加速扫描列表/统计/属主/扩展名)
   ├─ scan_stats 表        预计算仪表盘统计(扫描完成后异步写入)
   └─ dust_users 表        认证用户
```

前端不直连 ClickHouse,所有查询经 dust-serve 代理,SQL 由后端用参数化绑定构造,前端无法注入任意 SQL。

## 规模与性能

本系统面向**十亿级文件系统**(实测 28 亿行 `file_scan`、具体扫描性能取决于你的后端，支持限速，避免打爆后端，海量数据建议使用nvme ssd 以提供更优秀的查询性能。

| 操作 | 十亿级行上的耗时 | 说明 |
|------|------------------|------|
| 单次扫描状态轮询(count) | ~0.55s | 2.8B 行表;2s TTL 缓存避免每次轮询都查 |
| 扫描完成后校准(准确计数) | ~9s | 2B 行,只在完成时跑一次 |
| 仪表盘 7 项聚合 | ~80s | 20亿 行全扫;完成后预计算,之后毫秒读 |
| 扫描列表 | ms | 走物化视图 `mv_scan_stats`,而非全表扫描 |

支撑十亿级的几个设计:

- **物化视图**:`mv_scan_stats`/`mv_extension_stats`/`mv_owner_stats` 把按扫描/扩展名/属主的聚合从全表扫描降到毫秒(见 `setup.sql`)。
- **预计算仪表盘**:扫描完成后异步把 7 项统计写入 `scan_stats` 表,仪表盘读预计算(ms)而非实时聚合(~80s)。
- **分片扫描**:root 展开成 N×overshard 个候选,N 个并行 dust 进程动态 work-stealing,避免大目录长尾拖慢整体。
- **限速保护**:`--rate-limit` 控制 stat 调用频率,保护 NFS/Lustre MDS 不被压垮。
- **断点续扫**:候选清单 + `.done` 记录持久化,中断后只重跑未完成部分。

> 以上数据来自开发注释中的实测基准,具体表现取决于硬件、ClickHouse 配置和文件系统结构。

## 部署注意事项

- **生产必改**:`JWT_SECRET`(改长随机串)、`ADMIN_PASSWORD`、ClickHouse `readonly`/`dust_writer` 密码。默认值仅供首次体验。
- **表结构单一真相源**:所有 ClickHouse 建表/建账户语句在 [`setup.sql`](./setup.sql),README 不再内联 DDL(避免与实现漂移)。Docker 部署由 `ch-init` 服务自动执行;手动部署按下方"ClickHouse 准备"操作。
- **容器路径**:Docker 默认只读挂载 `/abc:/abc:ro`,web 上扫描时填**容器内路径**(如 `/abc/data`),不是宿主路径。改挂载点见 docker-compose.yml。
- **续扫不自动去重**:断点续扫重跑的候选可能写入重复行(同 `scan_id`+`path`)。当前接受此行为;若需去重见 `docs/file_scan_replacing.sql` 的 `ReplacingMergeTree` 方案。
- **进程重启丢失运行态**:正在进行的扫描状态(`running_scans`/`finished_scans`)存内存,重启后丢失,会回退到查 `file_scan` 推断。

## 构建

### 后端

```bash
cargo build --release
# 产物: target/release/dust-serve
```

### 前端

```bash
cd web
npm install
npm run build
# 产物: web/dist/
```

### 部署

把 `dust-serve` 二进制、`web/dist/` 目录、`dust` 二进制放一起:

```
/opt/dust-serve/
  ├─ dust-serve        # 后端二进制
  ├─ dust              # 扫描器二进制(扫描时被 fork)
  ├─ web/dist/         # 前端静态文件
  └─ dust-serve.toml   # 配置
```

## 配置

复制 `dust-serve.toml.example` 为 `dust-serve.toml` 并编辑,或用环境变量:

| 配置项 | 环境变量 | 说明 |
|--------|---------|------|
| listen | LISTEN | 监听地址,默认 0.0.0.0:8080 |
| dust_path | DUST_PATH | dust 二进制路径 |
| ch_url | CH_URL | ClickHouse HTTP 地址 |
| ch_read_user | CH_READ_USER | 读账户(前端查询用) |
| ch_read_password | CH_READ_PASSWORD | |
| ch_write_user | CH_WRITE_USER | 写账户(dust scan + 用户表写入) |
| ch_write_password | CH_WRITE_PASSWORD | |
| ch_table | CH_TABLE | 文件表名,默认 file_scan |
| web_dir | WEB_DIR | 前端目录,默认 web/dist |
| jwt_secret | JWT_SECRET | JWT 签名密钥(必填,生产请改长随机串) |
| admin_user | ADMIN_USER | 初始管理员,默认 admin |
| admin_password | ADMIN_PASSWORD | 初始密码,默认 changeme |

## ClickHouse 准备

用 `setup.sql` 一键建好(需要管理员权限):

```bash
clickhouse-client --multiquery < setup.sql
```

`setup.sql` 会建好:

- `file_scan` 表 — 主表,`MergeTree` 按 `scan_time` 月分区,TTL 180 天,`ORDER BY (parent_path, name)`
- `mv_scan_stats` / `mv_extension_stats` / `mv_owner_stats` — 物化视图,加速扫描列表/扩展名/属主聚合(详见"规模与性能")
- `scan_stats` 表 — 预计算仪表盘统计(`ReplacingMergeTree`,扫描完成后由 dust-serve 写入)
- `dust_users` 表 — 认证用户
- `readonly` / `dust_writer` 两个账户及权限(读写分离;`readonly` 额外需 `SELECT system.query_log` 供实时 QPS 端点)

> Docker 部署无需手动跑:`docker-compose.yml` 的 `ch-init` 服务会自动执行 `setup.sql`。

> 若需要按 `(scan_id, path)` 去重(解决续扫重复行),见 [`docs/file_scan_replacing.sql`](./docs/file_scan_replacing.sql) 的 `ReplacingMergeTree` 方案及 [`docs/migrate_scan_id.sql`](./docs/migrate_scan_id.sql) 迁移脚本。

## 运行

```bash
./dust-serve
# 首次启动自动建 dust_users 表并写入 admin 用户
# 浏览器访问 http://localhost:8080
# 用 admin / changeme 登录(生产请立即改密码)
```

## 扫描流程

1. Web 页面"扫描管理"填根目录,点启动 → `POST /api/scan`
2. dust-serve fork `dust --scan /data --sink clickhouse ...`
3. dust 进程遍历文件系统,批量写 ClickHouse
4. 前端轮询 `/api/scans` 看进度和状态
5. 完成后在"文件浏览"和"仪表盘"查看结果

## 增量扫描

"扫描管理"页的"自"字段填日期(如 2025-07-01),dust-serve 传 `--since` 给 dust,
只输出 mtime 晚于此的文件。详见 dust 文档的增量扫描章节。

## 安全

- 前端不直连 ClickHouse,查询全经 dust-serve 代理
- SQL 用 ClickHouse 参数化绑定(`{name:Type}` + `param_name=value`),防注入
- 写操作只有 dust scan 进程(dust_writer 账户),前端无写权限
- JWT 24 小时过期,生产请改强密钥
- 认证用户表存 ClickHouse,bcrypt 哈希密码

## Docker 部署(docker-compose)

一键拉起 ClickHouse + dust-serve(含前端、dust 扫描器):

```bash
cd dust-serve
docker compose up -d --build   # 首次或代码改动后,加 --build 重新构建镜像
# 之后:docker compose up -d      # 用已构建的镜像,无需 --build
```

启动后:
- ClickHouse 在 8123(`ch-init` 服务自动跑 `setup.sql` 建全部表/物化视图/账户)
- dust-serve 在 http://localhost:8080
- 登录:admin / changeme(在 docker-compose.yml 改 ADMIN_PASSWORD)

### 扫描宿主目录

docker-compose.yml 默认把宿主 `/abc` 只读挂载到容器的 `/abc`:

```yaml
volumes:
  - /abc:/abc:ro
```

所以 web 上扫描宿主的 `/abc/data` 时,直接填 **`/abc/data`**(容器内路径与宿主一致)。
要改挂载点,编辑 volumes,如只挂载 `/data`:

```yaml
- /data:/data:ro     # web 上填 /data
```

### 配置

环境变量在 docker-compose.yml 的 dust-serve 服务里改:

| 变量 | 默认 | 说明 |
|------|------|------|
| CH_URL | http://clickhouse:8123 | CH 地址(容器间用服务名) |
| CH_READ_USER/PASSWORD | readonly | 读账户 |
| CH_WRITE_USER/PASSWORD | dust_writer | 写账户 |
| ADMIN_USER/PASSWORD | admin/changeme | 登录账户 |
| JWT_SECRET | change-me... | JWT 密钥(生产必改) |

### 数据目录(bind mount 到宿主)

数据和日志用 bind mount 挂到宿主目录,可直接查看/备份/迁移:

- `./data/clickhouse`:ClickHouse 数据(容器内 `/var/lib/clickhouse`)
- `./scan-logs`:dust 扫描日志(`*.log`,每扫描一个文件)

```bash
ls ./data/clickhouse           # 看 CH 数据(sudo,属 docker uid)
ls ./scan-logs/                # 看扫描日志
tar czf backup.tar.gz ./data/clickhouse ./scan-logs   # 备份

docker compose down            # 停(数据保留在宿主目录)
docker compose up -d           # 起(数据自动恢复)
docker compose logs -f dust-serve   # 看运行日志
```

### 只用 dust-serve 镜像(自带 CH)

如果已有 ClickHouse,只用 dust-serve 镜像:

```bash
docker build -t dust-serve -f dust-serve/Dockerfile .  
docker run -d -p 8080:8080 \
  -e CH_URL=http://your-ch:8123 \
  -e CH_READ_USER=readonly -e CH_READ_PASSWORD=readonly \
  -e CH_WRITE_USER=dust_writer -e CH_WRITE_PASSWORD=changeme \
  -e JWT_SECRET=your-secret -e ADMIN_PASSWORD=changeme \
  -v /abc:/abc:ro \
  dust-serve
```
## UI
<img width="2263" height="269" alt="image" src="https://github.com/user-attachments/assets/76802bfd-050b-4e69-a234-a0b9a7db32f9" />
