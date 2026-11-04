use std::sync::Arc;

use anyhow::{anyhow, Result};
use axum::{
    extract::State,
    http::Request,
    middleware::Next,
    response::Response,
    routing::post,
    Json, Router,
};
use bcrypt::{hash as bcrypt_hash, verify, DEFAULT_COST};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ch_client::ClickHouse;
use crate::AppState;

#[derive(Clone)]
pub struct AuthService {
    jwt_secret: String,
    /// Read client — used to look up users and verify passwords.
    reader: Arc<ClickHouse>,
    /// Write client — used to create the users table and insert the admin user.
    /// Reuses the dust scan write account, which has INSERT privileges.
    writer: Arc<ClickHouse>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String, // username
    exp: usize,  // expiry timestamp
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub username: String,
}

impl AuthService {
    pub fn new(
        config: Arc<crate::config::Config>,
        reader: Arc<ClickHouse>,
    ) -> Result<Self> {
        let writer = Arc::new(ClickHouse::new(
            config.ch_url.clone(),
            config.ch_write_user.clone(),
            config.ch_write_password.clone(),
            config.ch_table.clone(),
        ));
        let svc = Self {
            jwt_secret: config.jwt_secret.clone(),
            reader,
            writer,
        };
        // Best-effort schema/admin bootstrap. ClickHouse may not be up yet
        // (it can start after the web server); in that case we log and defer.
        // Per-request handlers will surface CH errors when they actually run.
        if let Err(e) = svc.ensure_schema().and_then(|()| svc.ensure_admin(&config.admin_user, &config.admin_password)) {
            eprintln!("warning: auth bootstrap failed (CH unreachable?): {e}");
            eprintln!("         login will fail until ClickHouse is available");
        }
        Ok(svc)
    }

    fn ensure_schema(&self) -> Result<()> {
        let sql = "CREATE TABLE IF NOT EXISTS dust_users (
            username String,
            pass_hash String,
            created_at DateTime DEFAULT now()
        ) ENGINE = ReplacingMergeTree() ORDER BY username";
        self.writer.query_scalar(sql)?;
        Ok(())
    }

    fn ensure_admin(&self, user: &str, password: &str) -> Result<()> {
        let count = self
            .reader
            .query_scalar("SELECT count() FROM dust_users")?
            .parse::<u64>()
            .unwrap_or(0);
        if count > 0 {
            return Ok(());
        }
        let hashed = bcrypt_hash(password, DEFAULT_COST)
            .map_err(|e| anyhow!("bcrypt hash: {e}"))?;
        // Insert via JSONEachRow so the hashed string is safely escaped.
        let row = serde_json::to_string(&json!({
            "username": user,
            "pass_hash": hashed,
        }))?;
        self.writer.insert_json_each_row("dust_users", &row)?;
        Ok(())
    }

    pub fn verify_credentials(&self, username: &str, password: &str) -> Result<bool> {
        // Use a parameterized query via {name:Value} to avoid injection.
        let sql = "SELECT pass_hash FROM dust_users WHERE username = {user:String} LIMIT 1";
        let stored = self.reader.query_scalar_param(sql, &[("user", username)])?;
        if stored.is_empty() {
            return Ok(false);
        }
        verify(password, &stored).map_err(|e| anyhow!("bcrypt verify: {e}"))
    }

    pub fn issue_token(&self, username: &str) -> Result<String> {
        let exp = (Utc::now() + Duration::hours(24)).timestamp() as usize;
        let claims = Claims {
            sub: username.to_string(),
            exp,
        };
        encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.jwt_secret.as_bytes()),
        )
        .map_err(|e| anyhow!("jwt encode: {e}"))
    }

    pub fn validate_token(&self, token: &str) -> Result<String> {
        let data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.jwt_secret.as_bytes()),
            &Validation::default(),
        )
        .map_err(|e| anyhow!("jwt decode: {e}"))?;
        Ok(data.claims.sub)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/login", post(login))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, (axum::http::StatusCode, String)> {
    let ok = state
        .auth
        .verify_credentials(&req.username, &req.password)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !ok {
        return Err((axum::http::StatusCode::UNAUTHORIZED, "invalid credentials".into()));
    }
    let token = state
        .auth
        .issue_token(&req.username)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(LoginResponse {
        token,
        username: req.username,
    }))
}

/// Axum middleware: require a valid Bearer token.
pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, axum::http::StatusCode> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
    let token = header
        .strip_prefix("Bearer ")
        .ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
    let user = state
        .auth
        .validate_token(token)
        .map_err(|_| axum::http::StatusCode::UNAUTHORIZED)?;
    req.extensions_mut().insert(AuthedUser(user));
    Ok(next.run(req).await)
}

#[derive(Clone)]
pub struct AuthedUser(pub String);
