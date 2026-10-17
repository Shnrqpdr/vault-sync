//! Main loop: consumes CouchDB's `_changes` feed and applies it to the mirror on disk.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::backup::BackupStatus;
use crate::config::Config;
use crate::couch::{seq_to_string, Couch};
use crate::livesync::{chunk_data, classify, AssembleError, Entry};
use crate::mirror::{self, MirrorLock};
use crate::state::State;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub last_seq: String,
    pub files: i64,
    pub pending: usize,
    pub written_total: u64,
    pub deleted_total: u64,
    /// The vault has E2EE enabled: the mirror cannot read its content.
    pub encrypted_vault: bool,
    pub last_error: Option<String>,
    /// Notes in the search index.
    pub notes_indexed: usize,
    /// Git snapshots; `None` when backups are disabled.
    pub backup: Option<BackupStatus>,
}

pub type SharedStatus = Arc<RwLock<Status>>;

pub struct Syncer {
    cfg: Config,
    couch: Couch,
    state: Arc<State>,
    status: SharedStatus,
    lock: MirrorLock,
    /// Paths changed in the mirror, for the search index.
    changed: UnboundedSender<String>,
}

#[derive(Default)]
struct BatchStats {
    written: u64,
    deleted: u64,
}

impl Syncer {
    pub fn new(
        cfg: Config,
        couch: Couch,
        state: Arc<State>,
        status: SharedStatus,
        lock: MirrorLock,
        changed: UnboundedSender<String>,
    ) -> Self {
        Self {
            cfg,
            couch,
            state,
            status,
            lock,
            changed,
        }
    }

    pub async fn run(self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.step().await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "sync cycle failed; retrying in {backoff:?}");
                    self.status.write().await.last_error = Some(format!("{e:#}"));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    /// One cycle: fetch changes (+ pending notes), apply everything, and only then move the checkpoint.
    async fn step(&self) -> Result<()> {
        let since = self.state.since()?;
        let resp = self
            .couch
            .changes(&since, self.cfg.poll_timeout, self.cfg.batch_limit)
            .await?;

        let mut docs: Vec<(String, Option<Value>)> = Vec::with_capacity(resp.results.len());
        let mut seen = HashSet::new();
        for change in resp.results {
            if seen.insert(change.id.clone()) {
                docs.push((change.id, change.doc));
            }
        }

        // Notes that arrived before their chunks are retried every cycle.
        let pending: Vec<String> = self
            .state
            .pending_ids()?
            .into_iter()
            .filter(|id| !seen.contains(id))
            .collect();
        if !pending.is_empty() {
            let fetched = self.couch.get_docs(&pending).await?;
            for id in pending {
                let doc = fetched.get(&id).cloned();
                docs.push((id, doc));
            }
        }

        let entries: Vec<Entry> = docs
            .iter()
            .map(|(id, doc)| classify(id, doc.as_ref()))
            .collect();

        let needed: Vec<String> = entries
            .iter()
            .filter_map(|e| match e {
                Entry::File(f) => Some(f.external_chunks()),
                _ => None,
            })
            .flatten()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let chunks: HashMap<String, String> = if needed.is_empty() {
            HashMap::new()
        } else {
            self.couch
                .get_docs(&needed)
                .await?
                .iter()
                .filter_map(|(k, v)| chunk_data(v).map(|d| (k.clone(), d.to_string())))
                .collect()
        };

        let mut stats = BatchStats::default();
        let mut saw_encrypted = false;
        {
            let _mirror = self.lock.lock().await;
            for entry in entries {
                if matches!(entry, Entry::Encrypted { .. }) {
                    saw_encrypted = true;
                }
                self.apply(entry, &chunks, &since, &mut stats).await?;
            }
        }

        let last_seq = seq_to_string(&resp.last_seq);
        self.state.set_since(&last_seq)?;

        if stats.written + stats.deleted > 0 {
            info!(
                written = stats.written,
                deleted = stats.deleted,
                "batch applied"
            );
        }

        let mut st = self.status.write().await;
        if saw_encrypted && !st.encrypted_vault {
            warn!("vault is end-to-end encrypted: the mirror cannot read the notes");
        }
        st.encrypted_vault |= saw_encrypted;
        st.last_seq = last_seq;
        st.files = self.state.file_count()?;
        st.pending = self.state.pending_ids()?.len();
        st.written_total += stats.written;
        st.deleted_total += stats.deleted;
        st.last_error = None;
        Ok(())
    }

    async fn apply(
        &self,
        entry: Entry,
        chunks: &HashMap<String, String>,
        since: &str,
        stats: &mut BatchStats,
    ) -> Result<()> {
        let root = self.cfg.vault_dir.as_path();
        match entry {
            Entry::File(f) => match f.assemble(chunks) {
                Ok(bytes) => {
                    // Same id, new path: remove the old file.
                    if let Some(old) = self.state.path_of(&f.id)? {
                        if old != f.path {
                            mirror::remove_file(root, &old).await?;
                            self.notify(old);
                        }
                    }
                    mirror::write_file(root, &f.path, &bytes, f.mtime).await?;
                    self.notify(f.path.clone());
                    self.state.upsert_file(&f.id, &f.path, f.mtime)?;
                    self.state.remove_pending(&f.id)?;
                    stats.written += 1;
                    debug!(path = %f.path, bytes = bytes.len(), "note written");
                }
                Err(AssembleError::MissingChunks(missing)) => {
                    debug!(path = %f.path, missing = missing.len(), "chunks have not arrived yet");
                    self.state.add_pending(&f.id, since)?;
                }
                Err(AssembleError::Undecodable(why)) => {
                    warn!(path = %f.path, reason = %why, "note skipped");
                    self.state.remove_pending(&f.id)?;
                }
            },
            Entry::Deleted { id, path } => {
                let path = match path {
                    Some(p) => Some(p),
                    None => self.state.path_of(&id)?,
                };
                if let Some(p) = path {
                    mirror::remove_file(root, &p).await?;
                    stats.deleted += 1;
                    debug!(path = %p, "note removed");
                    self.notify(p);
                }
                self.state.remove_file(&id)?;
                self.state.remove_pending(&id)?;
            }
            Entry::Encrypted { .. } | Entry::Ignored => {}
        }
        Ok(())
    }

    fn notify(&self, path: String) {
        // Only fails if the indexer stopped; syncing goes on regardless.
        let _ = self.changed.send(path);
    }
}
