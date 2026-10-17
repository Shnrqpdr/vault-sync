//! Periodic Git snapshots of the mirror, with an optional push to a private remote.
//!
//! The repository lives outside the mirror (separate `git_dir`, work tree = the
//! mirror), so the mirror keeps holding only notes. It shells out to `git`,
//! which already handles SSH, HTTPS with tokens and gc without extra dependencies.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::BackupConfig;
use crate::mirror::MirrorLock;
use crate::sync::SharedStatus;

const AUTHOR_NAME: &str = "vault-core";
const AUTHOR_EMAIL: &str = "vault-core@localhost";
const LOCAL_TIMEOUT: Duration = Duration::from_secs(120);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(180);
/// How many paths to list in the commit message body.
const MAX_LISTED: usize = 100;

#[derive(Debug, Clone, Default, Serialize)]
pub struct BackupStatus {
    pub remote: bool,
    pub last_commit: Option<String>,
    /// Unix epoch (seconds).
    pub last_commit_at: Option<u64>,
    pub last_push_at: Option<u64>,
    /// There are local commits that have not reached the remote yet.
    pub unpushed: bool,
    pub last_error: Option<String>,
}

pub struct Snapshotter {
    git: Git,
    interval: Duration,
    has_remote: bool,
    branch: String,
    lock: MirrorLock,
    status: SharedStatus,
    /// The repository already has a base compatible with the remote (see `ensure_base`).
    base_ready: bool,
    unpushed: bool,
}

#[derive(Debug)]
struct Commit {
    hash: String,
    summary: String,
}

impl Snapshotter {
    pub async fn new(
        cfg: BackupConfig,
        work_tree: PathBuf,
        lock: MirrorLock,
        status: SharedStatus,
    ) -> Result<Self> {
        let ssh_command = match &cfg.ssh_key {
            Some(key) if key.exists() => Some(ssh_command(key, &cfg.git_dir)),
            Some(key) => {
                info!(key = %key.display(), "backup SSH key not found; continuing without it");
                None
            }
            None => None,
        };
        let git = Git {
            git_dir: cfg.git_dir,
            work_tree,
            ssh_command,
        };
        git.init(&cfg.branch, cfg.remote.as_deref()).await?;

        let has_remote = cfg.remote.is_some();
        status.write().await.backup = Some(BackupStatus {
            remote: has_remote,
            ..Default::default()
        });
        Ok(Self {
            git,
            interval: cfg.interval,
            has_remote,
            branch: cfg.branch,
            lock,
            status,
            base_ready: false,
            // A previous run may have left commits that were never pushed.
            unpushed: has_remote,
        })
    }

    pub async fn run(mut self) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let result = self.cycle().await;
            let unpushed = self.unpushed;
            self.update(|b| {
                b.unpushed = unpushed;
                b.last_error = match result {
                    Ok(()) => None,
                    Err(e) => {
                        let msg = redact(&format!("{e:#}"));
                        warn!(error = %msg, "Git backup failed; retrying next cycle");
                        Some(msg)
                    }
                };
            })
            .await;
        }
    }

    async fn cycle(&mut self) -> Result<()> {
        self.ensure_base().await?;
        if let Some(commit) = self.snapshot().await? {
            info!(commit = %commit.hash, summary = %commit.summary, "snapshot created");
            self.unpushed = self.has_remote;
            self.update(|b| {
                b.last_commit = Some(commit.hash);
                b.last_commit_at = Some(now_unix());
            })
            .await;
        }
        if self.has_remote && self.unpushed {
            self.push().await?;
            self.unpushed = false;
            self.update(|b| b.last_push_at = Some(now_unix())).await;
        }
        Ok(())
    }

    /// In a fresh repository whose remote already has history (e.g. the state
    /// volume was lost), adopts the remote history before the first commit, so
    /// the next snapshot is a fast-forward instead of a new root the push would
    /// reject. The work tree is not touched.
    async fn ensure_base(&mut self) -> Result<()> {
        if self.base_ready {
            return Ok(());
        }
        if !self.has_remote || self.git.has_head().await? {
            self.base_ready = true;
            return Ok(());
        }
        let refname = format!("refs/heads/{}", self.branch);
        let out = self
            .git
            .run(
                &["ls-remote", "--exit-code", "origin", &refname],
                NETWORK_TIMEOUT,
            )
            .await?;
        match out.status.code() {
            Some(0) => {
                self.git
                    .check(&["fetch", "--quiet", "origin", &refname], NETWORK_TIMEOUT)
                    .await?;
                self.git
                    .check(&["reset", "--quiet", "FETCH_HEAD"], LOCAL_TIMEOUT)
                    .await?;
                info!(branch = %self.branch, "adopted the remote history");
            }
            // The branch does not exist on the remote: the first local commit becomes the root.
            Some(2) => {}
            _ => bail!("git ls-remote failed: {}", stderr(&out)),
        }
        self.base_ready = true;
        Ok(())
    }

    /// Commits the current state of the mirror, if anything changed since the last snapshot.
    async fn snapshot(&self) -> Result<Option<Commit>> {
        {
            let _mirror = self.lock.lock().await;
            self.git.check(&["add", "--all"], LOCAL_TIMEOUT).await?;
        }
        let diff = self
            .git
            .check(
                &["diff", "--cached", "--no-renames", "--name-status", "-z"],
                LOCAL_TIMEOUT,
            )
            .await?;
        let changes = parse_name_status(&diff);
        if changes.is_empty() {
            return Ok(None);
        }
        let message = commit_message(&changes);
        self.git
            .check(
                &["commit", "--quiet", "--no-verify", "-m", &message],
                LOCAL_TIMEOUT,
            )
            .await?;
        let hash = self
            .git
            .check(&["rev-parse", "--short", "HEAD"], LOCAL_TIMEOUT)
            .await?;
        Ok(Some(Commit {
            hash: String::from_utf8_lossy(&hash).trim().to_string(),
            summary: message.lines().next().unwrap_or_default().to_string(),
        }))
    }

    async fn push(&self) -> Result<()> {
        if !self.git.has_head().await? {
            return Ok(()); // mirror still empty, nothing to push
        }
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        self.git
            .check(&["push", "--quiet", "origin", &refspec], NETWORK_TIMEOUT)
            .await?;
        Ok(())
    }

    async fn update(&self, f: impl FnOnce(&mut BackupStatus)) {
        if let Some(b) = self.status.write().await.backup.as_mut() {
            f(b);
        }
    }
}

struct Git {
    git_dir: PathBuf,
    work_tree: PathBuf,
    ssh_command: Option<String>,
}

impl Git {
    async fn init(&self, branch: &str, remote: Option<&str>) -> Result<()> {
        if !self.git_dir.join("HEAD").exists() {
            if let Some(parent) = self.git_dir.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            self.check(
                &["init", "--quiet", "--initial-branch", branch],
                LOCAL_TIMEOUT,
            )
            .await?;
            info!(git_dir = %self.git_dir.display(), "backup repository created");
        }
        // Only vault-core uses this repository: an index.lock at startup comes from
        // an interrupted git (restart, OOM) and would block every later commit.
        if tokio::fs::remove_file(self.git_dir.join("index.lock"))
            .await
            .is_ok()
        {
            warn!("removed a stale index.lock from the backup repository");
        }
        for (key, value) in [
            ("user.name", AUTHOR_NAME),
            ("user.email", AUTHOR_EMAIL),
            ("commit.gpgsign", "false"),
        ] {
            self.check(&["config", key, value], LOCAL_TIMEOUT).await?;
        }
        let info_dir = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info_dir).await?;
        // Temporary files from the mirror's atomic writes.
        tokio::fs::write(info_dir.join("exclude"), ".*.vault-core.tmp\n").await?;

        // Re-apply the remote on every start so .env changes take effect.
        self.run(&["remote", "remove", "origin"], LOCAL_TIMEOUT)
            .await?;
        if let Some(url) = remote {
            self.check(&["remote", "add", "origin", url], LOCAL_TIMEOUT)
                .await?;
        }
        Ok(())
    }

    async fn run(&self, args: &[&str], limit: Duration) -> Result<Output> {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(&self.work_tree)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(ssh) = &self.ssh_command {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        tokio::time::timeout(limit, cmd.output())
            .await
            .with_context(|| format!("git {} exceeded {limit:?}", args[0]))?
            .context("could not run git")
    }

    /// Like `run`, but fails if git exits with an error. Returns stdout.
    async fn check(&self, args: &[&str], limit: Duration) -> Result<Vec<u8>> {
        let out = self.run(args, limit).await?;
        if !out.status.success() {
            bail!("git {} failed: {}", args[0], stderr(&out));
        }
        Ok(out.stdout)
    }

    async fn has_head(&self) -> Result<bool> {
        let out = self
            .run(&["rev-parse", "--verify", "--quiet", "HEAD"], LOCAL_TIMEOUT)
            .await?;
        Ok(out.status.success())
    }
}

fn ssh_command(key: &Path, git_dir: &Path) -> String {
    // known_hosts in the state volume: trust the host on first connection and
    // refuse if its key changes later.
    format!(
        "ssh -i '{}' -o IdentitiesOnly=yes -o BatchMode=yes \
         -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile='{}'",
        key.display(),
        git_dir.join("known_hosts").display()
    )
}

fn stderr(out: &Output) -> String {
    redact(String::from_utf8_lossy(&out.stderr).trim())
}

/// Hides credentials embedded in URLs (`https://user:token@host/...`).
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c == '\'' || c == '"' || c.is_whitespace())
            .unwrap_or(tail.len());
        rest = match tail[..end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                &tail[at..]
            }
            None => tail,
        };
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Added,
    Modified,
    Deleted,
}

/// Parses `git diff --name-status -z --no-renames`: `status\0path\0` pairs.
fn parse_name_status(raw: &[u8]) -> Vec<(Kind, String)> {
    let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let kind = match status.first() {
            Some(b'A') => Kind::Added,
            Some(b'D') => Kind::Deleted,
            _ => Kind::Modified,
        };
        out.push((kind, String::from_utf8_lossy(path).into_owned()));
    }
    out
}

fn commit_message(changes: &[(Kind, String)]) -> String {
    let parts: Vec<String> = [
        (Kind::Added, "added"),
        (Kind::Modified, "modified"),
        (Kind::Deleted, "deleted"),
    ]
    .into_iter()
    .filter_map(|(kind, word)| {
        let n = changes.iter().filter(|(k, _)| *k == kind).count();
        (n > 0).then(|| format!("{n} {word}"))
    })
    .collect();

    let mut msg = format!("snapshot: {}\n\n", parts.join(", "));
    for (kind, path) in changes.iter().take(MAX_LISTED) {
        let letter = match kind {
            Kind::Added => 'A',
            Kind::Modified => 'M',
            Kind::Deleted => 'D',
        };
        let _ = writeln!(msg, "{letter} {path}");
    }
    if changes.len() > MAX_LISTED {
        let _ = writeln!(msg, "… and {} more", changes.len() - MAX_LISTED);
    }
    msg
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::RwLock;

    use super::*;
    use crate::sync::Status;

    fn git_out(git_dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_DIR", git_dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    async fn snapshotter(git_dir: &Path, vault: &Path, remote: Option<&Path>) -> Snapshotter {
        let cfg = BackupConfig {
            interval: Duration::from_secs(60),
            git_dir: git_dir.to_path_buf(),
            remote: remote.map(|p| p.display().to_string()),
            branch: "main".into(),
            ssh_key: None,
        };
        let status: SharedStatus = Arc::new(RwLock::new(Status::default()));
        Snapshotter::new(cfg, vault.to_path_buf(), MirrorLock::default(), status)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn commits_only_when_the_mirror_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (git_dir, vault) = (dir.path().join("vault.git"), dir.path().join("vault"));
        std::fs::create_dir_all(vault.join("notes")).unwrap();
        std::fs::write(vault.join("notes/a.md"), "# A").unwrap();
        std::fs::write(vault.join(".a.md.vault-core.tmp"), "junk").unwrap();

        let mut snap = snapshotter(&git_dir, &vault, None).await;
        snap.cycle().await.unwrap();
        assert!(!vault.join(".git").exists());
        assert_eq!(
            git_out(&git_dir, &["log", "--format=%s"]),
            "snapshot: 1 added\n"
        );
        assert_eq!(
            git_out(&git_dir, &["ls-tree", "-r", "--name-only", "HEAD"]),
            "notes/a.md\n"
        );

        snap.cycle().await.unwrap();
        assert_eq!(git_out(&git_dir, &["rev-list", "--count", "HEAD"]), "1\n");

        std::fs::write(vault.join("notes/a.md"), "# A edited").unwrap();
        std::fs::write(vault.join("b.md"), "b").unwrap();
        snap.cycle().await.unwrap();
        std::fs::remove_file(vault.join("b.md")).unwrap();
        snap.cycle().await.unwrap();
        assert_eq!(
            git_out(&git_dir, &["log", "--format=%s"]),
            "snapshot: 1 deleted\nsnapshot: 1 added, 1 modified\nsnapshot: 1 added\n"
        );
        let status = snap.status.read().await.backup.clone().unwrap();
        assert!(status.last_commit.is_some());
        assert!(!status.remote);
        assert!(!snap.unpushed, "nothing to push without a remote");
    }

    #[tokio::test]
    async fn pushes_and_adopts_remote_history_after_losing_state() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let vault = dir.path().join("vault");
        std::fs::create_dir_all(&vault).unwrap();
        git_out(&remote, &["init", "--quiet", "--bare"]);

        std::fs::write(vault.join("a.md"), "a").unwrap();
        let mut snap = snapshotter(&dir.path().join("state1.git"), &vault, Some(&remote)).await;
        snap.cycle().await.unwrap();
        assert!(!snap.unpushed);
        assert_eq!(git_out(&remote, &["rev-list", "--count", "main"]), "1\n");

        // State volume lost: new repository, same mirror, same remote.
        std::fs::write(vault.join("b.md"), "b").unwrap();
        let mut snap = snapshotter(&dir.path().join("state2.git"), &vault, Some(&remote)).await;
        snap.cycle().await.unwrap();
        assert_eq!(
            git_out(&remote, &["log", "--format=%s", "main"]),
            "snapshot: 1 added\nsnapshot: 1 added\n"
        );
        assert_eq!(
            git_out(&remote, &["ls-tree", "-r", "--name-only", "main"]),
            "a.md\nb.md\n"
        );
    }

    #[test]
    fn message_lists_changes() {
        let raw = b"A\0new.md\0M\0folder/old.md\0D\0junk.png\0A\0other.md\0";
        let changes = parse_name_status(raw);
        assert_eq!(changes.len(), 4);
        assert_eq!(
            commit_message(&changes),
            "snapshot: 2 added, 1 modified, 1 deleted\n\n\
             A new.md\nM folder/old.md\nD junk.png\nA other.md\n"
        );
        assert!(parse_name_status(b"").is_empty());
    }

    #[test]
    fn hides_credentials() {
        assert_eq!(
            redact("fatal: unable to access 'https://x-access-token:ghp_abc@github.com/u/r.git/'"),
            "fatal: unable to access 'https://***@github.com/u/r.git/'"
        );
        assert_eq!(
            redact("https://github.com/u/r and git@github.com:u/r"),
            "https://github.com/u/r and git@github.com:u/r"
        );
    }
}
