use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Address to listen on, e.g. "0.0.0.0:8080"
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Path to the dust binary used to launch scans.
    #[serde(default = "default_dust_path")]
    pub dust_path: String,

    /// ClickHouse HTTP endpoint, e.g. "http://localhost:8123"
    pub ch_url: String,

    /// ClickHouse account used by the API for read queries.
    pub ch_read_user: String,
    pub ch_read_password: String,

    /// ClickHouse account used by the dust scan process for writes.
    pub ch_write_user: String,
    pub ch_write_password: String,

    /// ClickHouse table name (default: file_scan)
    #[serde(default = "default_table")]
    pub ch_table: String,

    /// Directory of the built frontend static files.
    #[serde(default = "default_web_dir")]
    pub web_dir: String,

    /// Directory for per-scan log files (one file per scan_id).
    #[serde(default = "default_scan_log_dir")]
    pub scan_log_dir: std::path::PathBuf,

    /// Directory names to exclude from scans (e.g. snapshot dirs like .snapshot).
    /// Each scan command gets --ignore-directory <candidate>/<name> for these.
    #[serde(default)]
    pub exclude_dirs: Vec<String>,

    /// JWT signing secret. Must be set to a long random string in production.
    pub jwt_secret: String,

    /// Default admin credentials (created on first start if no users exist).
    pub admin_user: String,
    pub admin_password: String,
}

fn default_listen() -> String {
    "0.0.0.0:8080".to_string()
}
fn default_dust_path() -> String {
    "dust".to_string()
}
fn default_table() -> String {
    "file_scan".to_string()
}
fn default_web_dir() -> String {
    "web/dist".to_string()
}
fn default_scan_log_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("scan_logs")
}

impl Config {
    /// Load config from dust-serve.toml in the current directory, with env var overrides.
    pub fn load() -> anyhow::Result<Self> {
        let mut cfg = if std::path::Path::new("dust-serve.toml").exists() {
            let text = std::fs::read_to_string("dust-serve.toml")?;
            toml::from_str(&text)?
        } else {
            Config {
                listen: default_listen(),
                dust_path: default_dust_path(),
                ch_url: expect_env("CH_URL"),
                ch_read_user: expect_env("CH_READ_USER"),
                ch_read_password: expect_env("CH_READ_PASSWORD"),
                ch_write_user: expect_env("CH_WRITE_USER"),
                ch_write_password: expect_env("CH_WRITE_PASSWORD"),
                ch_table: default_table(),
                web_dir: default_web_dir(),
                scan_log_dir: default_scan_log_dir(),
                exclude_dirs: std::env::var("EXCLUDE_DIRS")
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect(),
                jwt_secret: expect_env("JWT_SECRET"),
                admin_user: std::env::var("ADMIN_USER").unwrap_or_else(|_| "admin".to_string()),
                admin_password: std::env::var("ADMIN_PASSWORD")
                    .unwrap_or_else(|_| "changeme".to_string()),
            }
        };

        // Env overrides for the common fields.
        if let Ok(v) = std::env::var("LISTEN") {
            cfg.listen = v;
        }
        if let Ok(v) = std::env::var("DUST_PATH") {
            cfg.dust_path = v;
        }
        if let Ok(v) = std::env::var("CH_URL") {
            cfg.ch_url = v;
        }
        if let Ok(v) = std::env::var("CH_TABLE") {
            cfg.ch_table = v;
        }
        if let Ok(v) = std::env::var("WEB_DIR") {
            cfg.web_dir = v;
        }
        if let Ok(v) = std::env::var("SCAN_LOG_DIR") {
            cfg.scan_log_dir = std::path::PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("JWT_SECRET") {
            cfg.jwt_secret = v;
        }
        Ok(cfg)
    }
}

fn expect_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| {
        panic!(
            "missing required config: set {name} env var or create dust-serve.toml with this field"
        )
    })
}
