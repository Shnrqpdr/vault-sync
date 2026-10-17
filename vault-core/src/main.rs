mod api;
mod backup;
mod catalog;
mod config;
mod couch;
mod livesync;
mod mcp;
mod mirror;
mod notes;
mod oauth;
mod sse;
mod state;
mod sync;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::backup::Snapshotter;
use crate::catalog::Catalog;
use crate::config::Config;
use crate::couch::Couch;
use crate::mirror::MirrorLock;
use crate::state::State;
use crate::sync::{SharedStatus, Status, Syncer};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("health") {
        return health_check().await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("vault_core=info")),
        )
        .init();

    let cfg = Config::from_env()?;
    let couch = Couch::new(&cfg)?;
    let state = Arc::new(State::open(&cfg.state_path)?);
    tokio::fs::create_dir_all(&cfg.vault_dir)
        .await
        .with_context(|| format!("could not create {}", cfg.vault_dir.display()))?;

    let status: SharedStatus = Arc::new(RwLock::new(Status::default()));
    let lock = MirrorLock::default();

    // Search only reads the mirror on disk, so it works even while CouchDB is down.
    let catalog = Arc::new(Catalog::new(cfg.vault_dir.clone())?);
    let (changed_tx, changed_rx) = mpsc::unbounded_channel();
    let index_task = tokio::spawn(catalog::run_indexer(
        catalog.clone(),
        changed_rx,
        status.clone(),
    ));

    let backup_task = match cfg.backup.clone() {
        Some(backup) => {
            let snap =
                Snapshotter::new(backup, cfg.vault_dir.clone(), lock.clone(), status.clone())
                    .await
                    .context("could not set up Git backups")?;
            Some(tokio::spawn(snap.run()))
        }
        None => {
            info!("Git backups disabled (BACKUP_INTERVAL_SECS=0)");
            None
        }
    };

    let syncer = Syncer::new(
        cfg.clone(),
        couch.clone(),
        state.clone(),
        status.clone(),
        lock,
        changed_tx,
    );
    let sync_state = state.clone();
    let sync_task = tokio::spawn(async move {
        // Wait for CouchDB (and the database) to become available.
        while let Err(e) = couch.ping_db().await {
            warn!(error = %format!("{e:#}"), "waiting for CouchDB");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        let since = sync_state.since().unwrap_or_default();
        info!(since = %since, "connected to CouchDB; starting sync");
        syncer.run().await;
    });

    let mcp_task = match &cfg.mcp {
        Some(mcp_cfg) => {
            let app = mcp::router(mcp_cfg, catalog.clone(), state.clone())?;
            let listener = tokio::net::TcpListener::bind(&mcp_cfg.addr)
                .await
                .with_context(|| format!("could not listen on {}", mcp_cfg.addr))?;
            info!(
                addr = %mcp_cfg.addr,
                url = %format!("{}/mcp", mcp_cfg.public_url),
                token = mcp_cfg.token.is_some(),
                oauth = mcp_cfg.password.is_some(),
                "MCP server listening"
            );
            Some(tokio::spawn(mcp::serve(listener, app, shutdown_signal())))
        }
        None => {
            info!("MCP server disabled (set MCP_TOKEN and/or MCP_PASSWORD)");
            None
        }
    };

    let app = api::router(AppState { status, catalog });
    let listener = tokio::net::TcpListener::bind(&cfg.http_addr)
        .await
        .with_context(|| format!("could not listen on {}", cfg.http_addr))?;
    info!(addr = %cfg.http_addr, db = %cfg.couch_db, vault = %cfg.vault_dir.display(), "HTTP API listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    sync_task.abort();
    index_task.abort();
    if let Some(task) = mcp_task {
        task.abort();
    }
    if let Some(task) = backup_task {
        task.abort();
    }
    info!("shut down");
    Ok(())
}

/// `vault-core health`: the Docker health check. Queries the local API, which
/// may be listening only on 127.0.0.1 inside the container.
async fn health_check() -> Result<()> {
    let addr = std::env::var("HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let port = addr.rsplit(':').next().unwrap_or("8080");
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("local API did not answer")?
        .error_for_status()
        .context("local API answered with an error")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
