//! The vault mirror on disk: atomic writes and safe removals.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// Held by the syncer while it applies a batch and by the Git snapshot while
/// it reads the mirror, so a snapshot never captures half a batch.
pub type MirrorLock = Arc<tokio::sync::Mutex<()>>;

/// Joins `rel` to `root`, accepting only normal components (no `..`, no root).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    let mut out = root.to_path_buf();
    let mut any = false;
    for comp in rel_path.components() {
        match comp {
            Component::Normal(c) => {
                out.push(c);
                any = true;
            }
            Component::CurDir => {}
            _ => bail!("rejected path: {rel:?}"),
        }
    }
    if !any {
        bail!("empty path");
    }
    Ok(out)
}

/// Writes atomically. `mtime` (ms since the Unix epoch, as LiveSync stores it)
/// becomes the file's modification time, so the mirror reflects the original note.
pub async fn write_file(root: &Path, rel: &str, bytes: &[u8], mtime: Option<i64>) -> Result<()> {
    let target = safe_join(root, rel)?;
    if let Some(dir) = target.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("mkdir {}", dir.display()))?;
    }
    // Skip identical content (avoids pointless commits and events).
    if let Ok(existing) = tokio::fs::read(&target).await {
        if existing == bytes {
            return Ok(());
        }
    }
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!(".{file_name}.vault-core.tmp"));
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    if let Some(ms) = mtime.and_then(|m| u64::try_from(m).ok()).filter(|m| *m > 0) {
        let file = tokio::fs::File::options().write(true).open(&tmp).await?;
        file.into_std()
            .await
            .set_modified(UNIX_EPOCH + Duration::from_millis(ms))
            .with_context(|| format!("set mtime of {}", tmp.display()))?;
    }
    tokio::fs::rename(&tmp, &target)
        .await
        .with_context(|| format!("rename to {}", target.display()))?;
    Ok(())
}

pub async fn remove_file(root: &Path, rel: &str) -> Result<()> {
    let target = safe_join(root, rel)?;
    match tokio::fs::remove_file(&target).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("rm {}", target.display())),
    }
    // Remove directories left empty, without leaving the root.
    let mut dir = target.parent().map(Path::to_path_buf);
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        if tokio::fs::remove_dir(&d).await.is_err() {
            break; // not empty (or already gone)
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_blocks_escapes() {
        let root = Path::new("/vault");
        assert_eq!(
            safe_join(root, "a/b.md").unwrap(),
            PathBuf::from("/vault/a/b.md")
        );
        assert_eq!(
            safe_join(root, "./a.md").unwrap(),
            PathBuf::from("/vault/a.md")
        );
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "a/../../x").is_err());
        assert!(safe_join(root, "/etc/passwd").is_err());
        assert!(safe_join(root, "").is_err());
    }

    #[tokio::test]
    async fn writes_and_removes_pruning_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "x/y/nota.md", b"oi", Some(1_700_000_000_000))
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(root.join("x/y/nota.md")).await.unwrap(),
            b"oi"
        );
        let modified = std::fs::metadata(root.join("x/y/nota.md"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            modified,
            UNIX_EPOCH + Duration::from_millis(1_700_000_000_000)
        );
        write_file(root, "x/y/nota.md", b"oi de novo", None)
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(root.join("x/y/nota.md")).await.unwrap(),
            b"oi de novo"
        );
        remove_file(root, "x/y/nota.md").await.unwrap();
        assert!(!root.join("x").exists());
        assert!(root.exists());
        remove_file(root, "nao/existe.md").await.unwrap();
    }
}
