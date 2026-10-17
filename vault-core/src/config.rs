use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// Configuration read from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub couch_url: String,
    pub couch_db: String,
    pub couch_user: String,
    pub couch_password: String,
    pub vault_dir: PathBuf,
    pub state_path: PathBuf,
    pub http_addr: String,
    /// How long CouchDB holds a longpoll request open without changes.
    pub poll_timeout: Duration,
    /// How many changes to request per `_changes` batch.
    pub batch_limit: u32,
    /// Git snapshots of the mirror; `None` when disabled.
    pub backup: Option<BackupConfig>,
    /// Public MCP server; `None` when disabled.
    pub mcp: Option<McpConfig>,
}

/// MCP server (with OAuth) exposed to the internet, separate from the internal API.
#[derive(Clone)]
pub struct McpConfig {
    pub addr: String,
    /// Public URL without a trailing slash (e.g. `https://vault.example.com`).
    pub public_url: String,
    /// Static token for clients that can send headers (Claude Code).
    pub token: Option<String>,
    /// Password for the OAuth authorization page (claude.ai, desktop and mobile apps).
    pub password: Option<String>,
}

// Manual Debug: `Config` is `Debug` and secrets must never reach the logs.
impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            .field("addr", &self.addr)
            .field("public_url", &self.public_url)
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Credentials this short are not allowed on the internet.
const MIN_SECRET_LEN: usize = 16;

/// Periodic Git snapshots of the mirror.
#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub interval: Duration,
    /// Repository directory, outside the mirror (the work tree is `vault_dir`).
    pub git_dir: PathBuf,
    /// Remote to push to; without it the history stays local.
    pub remote: Option<String>,
    pub branch: String,
    /// SSH deploy key. Ignored if the file does not exist.
    pub ssh_key: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            couch_url: env_or("COUCH_URL", "http://localhost:5984")
                .trim_end_matches('/')
                .to_string(),
            couch_db: env_or("COUCH_DB", "obsidian"),
            couch_user: required("COUCH_USER")?,
            couch_password: required("COUCH_PASSWORD")?,
            vault_dir: env_or("VAULT_DIR", "./data/vault").into(),
            state_path: env_or("STATE_PATH", "./data/state/vault-core.db").into(),
            http_addr: env_or("HTTP_ADDR", "127.0.0.1:8080"),
            poll_timeout: Duration::from_millis(
                env_or("POLL_TIMEOUT_MS", "30000")
                    .parse()
                    .context("invalid POLL_TIMEOUT_MS")?,
            ),
            batch_limit: env_or("BATCH_LIMIT", "500")
                .parse()
                .context("invalid BATCH_LIMIT")?,
            backup: BackupConfig::from_env()?,
            mcp: McpConfig::from_env()?,
        })
    }
}

impl BackupConfig {
    fn from_env() -> Result<Option<Self>> {
        let secs: u64 = env_or("BACKUP_INTERVAL_SECS", "600")
            .parse()
            .context("invalid BACKUP_INTERVAL_SECS")?;
        if secs == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            interval: Duration::from_secs(secs),
            git_dir: env_or("BACKUP_GIT_DIR", "./data/state/vault.git").into(),
            remote: optional("BACKUP_REMOTE"),
            branch: env_or("BACKUP_BRANCH", "main"),
            ssh_key: optional("BACKUP_SSH_KEY").map(PathBuf::from),
        }))
    }
}

impl McpConfig {
    fn from_env() -> Result<Option<Self>> {
        let token = optional("MCP_TOKEN");
        let password = optional("MCP_PASSWORD");
        if token.is_none() && password.is_none() {
            return Ok(None);
        }
        for (key, value) in [("MCP_TOKEN", &token), ("MCP_PASSWORD", &password)] {
            if value
                .as_ref()
                .is_some_and(|v| v.chars().count() < MIN_SECRET_LEN)
            {
                bail!("{key} must be at least {MIN_SECRET_LEN} characters long");
            }
        }
        let public_url = required("MCP_PUBLIC_URL")?
            .trim_end_matches('/')
            .to_string();
        if !public_url.starts_with("https://") && !public_url.starts_with("http://localhost") {
            bail!("MCP_PUBLIC_URL must be https:// (required by OAuth)");
        }
        Ok(Some(Self {
            addr: env_or("MCP_ADDR", "127.0.0.1:8081"),
            public_url,
            token,
            password,
        }))
    }
}

fn env_or(key: &str, default: &str) -> String {
    optional(key).unwrap_or_else(|| default.to_string())
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn required(key: &str) -> Result<String> {
    optional(key).with_context(|| format!("environment variable {key} is required"))
}
