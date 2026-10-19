//! Minimal client for the CouchDB HTTP API.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;

#[derive(Clone)]
pub struct Couch {
    http: reqwest::Client,
    db_url: String,
    user: String,
    password: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangesResponse {
    pub results: Vec<Change>,
    pub last_seq: Value,
}

#[derive(Debug, Deserialize)]
pub struct Change {
    pub id: String,
    pub doc: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AllDocsResponse {
    rows: Vec<AllDocsRow>,
}

#[derive(Debug, Deserialize)]
struct AllDocsRow {
    key: Option<String>,
    doc: Option<Value>,
}

impl Couch {
    pub fn new(cfg: &Config) -> Result<Self> {
        Self::connect(
            &format!("{}/{}", cfg.couch_url, cfg.couch_db),
            &cfg.couch_user,
            &cfg.couch_password,
        )
    }

    pub fn connect(db_url: &str, user: &str, password: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build the HTTP client")?;
        Ok(Self {
            http,
            db_url: db_url.to_string(),
            user: user.to_string(),
            password: password.to_string(),
        })
    }

    /// Checks that the database exists and the credentials work.
    pub async fn ping_db(&self) -> Result<()> {
        let resp = self
            .http
            .get(&self.db_url)
            .basic_auth(&self.user, Some(&self.password))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("CouchDB is unreachable")?;
        match resp.status().as_u16() {
            200 => Ok(()),
            401 | 403 => bail!("CouchDB rejected the credentials"),
            404 => bail!("database {} does not exist", self.db_url),
            s => bail!("CouchDB answered {s}"),
        }
    }

    /// Long-polls the changes feed, with the documents included.
    pub async fn changes(
        &self,
        since: &str,
        timeout: Duration,
        limit: u32,
    ) -> Result<ChangesResponse> {
        let url = format!("{}/_changes", self.db_url);
        let timeout_ms = timeout.as_millis().to_string();
        let limit = limit.to_string();
        let resp = self
            .http
            .get(url)
            .basic_auth(&self.user, Some(&self.password))
            .query(&[
                ("feed", "longpoll"),
                ("include_docs", "true"),
                ("since", since),
                ("timeout", timeout_ms.as_str()),
                ("limit", limit.as_str()),
            ])
            // Headroom over the longpoll timeout so the response is not cut short.
            .timeout(timeout + Duration::from_secs(30))
            .send()
            .await
            .context("_changes request failed")?;
        // A device's "Rebuild everything" in LiveSync deletes and recreates the
        // database: missing for a moment, then admin-only until couchdb-guard
        // restores the permissions. Say so instead of a bare status code.
        match resp.status().as_u16() {
            403 => bail!(
                "CouchDB denied access to the database (403). If a device just ran LiveSync's \
                 \"Rebuild everything\", couchdb-guard restores the permissions shortly"
            ),
            404 => bail!("the database does not exist (404); a device may be rebuilding it"),
            _ => {}
        }
        resp.error_for_status()
            .context("_changes returned an error")?
            .json()
            .await
            .context("invalid _changes response")
    }

    /// Fetches several documents by id. Missing or deleted ids are left out of the map.
    pub async fn get_docs(&self, ids: &[String]) -> Result<HashMap<String, Value>> {
        let mut out = HashMap::with_capacity(ids.len());
        for batch in ids.chunks(200) {
            let url = format!("{}/_all_docs", self.db_url);
            let resp: AllDocsResponse = self
                .http
                .post(url)
                .basic_auth(&self.user, Some(&self.password))
                .query(&[("include_docs", "true")])
                .json(&json!({ "keys": batch }))
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .context("_all_docs request failed")?
                .error_for_status()
                .context("_all_docs returned an error")?
                .json()
                .await
                .context("invalid _all_docs response")?;
            for row in resp.rows {
                if let (Some(key), Some(doc)) = (row.key, row.doc) {
                    if !doc.is_null() {
                        out.insert(key, doc);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Reads a `_local/...` document, which `_all_docs` does not return.
    pub async fn get_local(&self, id: &str) -> Result<Option<Value>> {
        let resp = self
            .http
            .get(format!("{}/{id}", self.db_url))
            .basic_auth(&self.user, Some(&self.password))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .with_context(|| format!("request for {id} failed"))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        let doc = resp
            .error_for_status()
            .with_context(|| format!("{id} returned an error"))?
            .json()
            .await
            .with_context(|| format!("invalid {id} response"))?;
        Ok(Some(doc))
    }

    /// Writes documents in one request. CouchDB answers per document: each
    /// result has `ok` and `rev`, or `error` (`"conflict"` on a stale `_rev`).
    pub async fn bulk_docs(&self, docs: &[Value]) -> Result<Vec<Value>> {
        self.http
            .post(format!("{}/_bulk_docs", self.db_url))
            .basic_auth(&self.user, Some(&self.password))
            .json(&json!({ "docs": docs }))
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("_bulk_docs request failed")?
            .error_for_status()
            .context("_bulk_docs returned an error")?
            .json()
            .await
            .context("invalid _bulk_docs response")
    }
}

/// CouchDB 2+ uses an opaque string `seq`; older versions used a number.
pub fn seq_to_string(seq: &Value) -> String {
    match seq {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
