use anyhow::{anyhow, Result};
use reqwest::blocking::{Client, Response};
use serde_json::Value;

/// Minimal ClickHouse HTTP client.
///
/// ClickHouse's native HTTP interface accepts a SQL query as a POST body.
/// We use `{name:Type}` parameter placeholders (ClickHouse server-side binding)
/// wherever user input is interpolated, so SQL injection is not possible.
pub struct ClickHouse {
    url: String,
    user: String,
    password: String,
    table: String,
    client: Client,
}

impl ClickHouse {
    pub fn new(url: String, user: String, password: String, table: String) -> Self {
        Self {
            url,
            user,
            password,
            table,
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("build reqwest client"),
        }
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Execute a query and return rows as a JSON array of objects (JSONEachRow).
    pub fn query_json(&self, sql: &str) -> Result<Vec<Value>> {
        let body = self.run_query(sql, "JSONEachRow")?;
        let mut rows = Vec::new();
        for line in body.lines() {
            if line.is_empty() {
                continue;
            }
            rows.push(serde_json::from_str(line)?);
        }
        Ok(rows)
    }

    /// Execute a parameterized query, returning rows as JSON objects.
    /// `params` is a slice of (name, value) tuples; each is sent as a
    /// `param_{name}=value` query-string parameter, and the SQL must reference
    /// them as `{name:Type}` — but we let the caller pick the type, so values
    /// are passed untyped and the SQL should use `{name:String}` etc.
    pub fn query_json_param(
        &self,
        sql: &str,
        params: &[(&str, &str)],
    ) -> Result<Vec<Value>> {
        let body = self.run_query_param(sql, "JSONEachRow", params)?;
        let mut rows = Vec::new();
        for line in body.lines() {
            if line.is_empty() {
                continue;
            }
            rows.push(serde_json::from_str(line)?);
        }
        Ok(rows)
    }

    /// Execute a query and return the raw scalar (first column of first row).
    pub fn query_scalar(&self, sql: &str) -> Result<String> {
        let body = self.run_query(sql, "TabSeparated")?;
        Ok(body.trim().to_string())
    }

    /// Parameterized scalar query.
    pub fn query_scalar_param(
        &self,
        sql: &str,
        params: &[(&str, &str)],
    ) -> Result<String> {
        let body = self.run_query_param(sql, "TabSeparated", params)?;
        Ok(body.trim().to_string())
    }

    /// Insert rows provided as a JSONEachRow body into the given table.
    pub fn insert_json_each_row(&self, table: &str, json_each_row_body: &str) -> Result<()> {
        let url = format!(
            "{}/?user={}&password={}&query={}",
            self.url,
            self.user,
            self.password,
            urlencoding::encode(&format!("INSERT INTO {table} FORMAT JSONEachRow")),
        );
        let resp = self
            .client
            .post(&url)
            .body(json_each_row_body.to_string())
            .send()
            .map_err(|e| anyhow!("CH insert request failed: {e}"))?;
        check_response(resp)?;
        Ok(())
    }

    fn run_query(&self, sql: &str, format: &str) -> Result<String> {
        let url = format!(
            "{}/?user={}&password={}&max_execution_time=300",
            self.url, self.user, self.password
        );
        let full = format!("{sql}\nFORMAT {format}");
        let resp = self
            .client
            .post(&url)
            .body(full)
            .send()
            .map_err(|e| anyhow!("ClickHouse request failed: {e}"))?;
        check_response(resp)
    }

    fn run_query_param(
        &self,
        sql: &str,
        format: &str,
        params: &[(&str, &str)],
    ) -> Result<String> {
        let mut url = format!(
            "{}/?user={}&password={}&max_execution_time=300",
            self.url, self.user, self.password
        );
        for (name, value) in params {
            url.push_str(&format!(
                "&param_{name}={}",
                urlencoding::encode(value)
            ));
        }
        let full = format!("{sql}\nFORMAT {format}");
        let resp = self
            .client
            .post(&url)
            .body(full)
            .send()
            .map_err(|e| anyhow!("ClickHouse request failed: {e}"))?;
        check_response(resp)
    }
}

fn check_response(resp: Response) -> Result<String> {
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| anyhow!("reading CH response: {e}"))?;
    if !status.is_success() {
        return Err(anyhow!("ClickHouse error ({}): {}", status, text));
    }
    Ok(text)
}
