//! Persistent state: feed checkpoint, id → path map, pending notes and OAuth data.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct State {
    conn: Mutex<Connection>,
}

impl State {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("could not open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS files (
                 id   TEXT PRIMARY KEY,
                 path TEXT NOT NULL,
                 mtime INTEGER
             );
             CREATE TABLE IF NOT EXISTS pending (id TEXT PRIMARY KEY, since TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS oauth_clients (
                 id            TEXT PRIMARY KEY,
                 name          TEXT NOT NULL,
                 redirect_uris TEXT NOT NULL,
                 created       INTEGER NOT NULL,
                 authorized    INTEGER NOT NULL DEFAULT 0,
                 secret_hash   TEXT
             );
             CREATE TABLE IF NOT EXISTS oauth_tokens (
                 hash      TEXT PRIMARY KEY,
                 kind      TEXT NOT NULL,
                 client_id TEXT NOT NULL,
                 expires   INTEGER NOT NULL
             );",
        )?;
        // Databases created before these columns existed get them added.
        for (column, definition) in [
            ("authorized", "INTEGER NOT NULL DEFAULT 0"),
            ("secret_hash", "TEXT"),
        ] {
            let exists = conn
                .prepare("SELECT 1 FROM pragma_table_info('oauth_clients') WHERE name = ?1")?
                .exists(params![column])?;
            if !exists {
                conn.execute_batch(&format!(
                    "ALTER TABLE oauth_clients ADD COLUMN {column} {definition}"
                ))?;
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn since(&self) -> Result<String> {
        Ok(self.get("since")?.unwrap_or_else(|| "0".to_string()))
    }

    pub fn set_since(&self, seq: &str) -> Result<()> {
        self.set("since", seq)
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT v FROM kv WHERE k = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO kv (k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn path_of(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT path FROM files WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Id of the document mirrored at `path`, ignoring case: LiveSync may have
    /// stored the file under another capitalization.
    pub fn id_of_path(&self, path: &str) -> Result<Option<String>> {
        let wanted = path.to_lowercase();
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, path FROM files")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, p) = row?;
            if p.to_lowercase() == wanted {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    pub fn upsert_file(&self, id: &str, path: &str, mtime: Option<i64>) -> Result<()> {
        self.conn().execute(
            "INSERT INTO files (id, path, mtime) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET path = excluded.path, mtime = excluded.mtime",
            params![id, path, mtime],
        )?;
        Ok(())
    }

    pub fn remove_file(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM files WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn file_count(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?)
    }

    pub fn add_pending(&self, id: &str, since: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO pending (id, since) VALUES (?1, ?2) ON CONFLICT(id) DO NOTHING",
            params![id, since],
        )?;
        Ok(())
    }

    pub fn remove_pending(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM pending WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn pending_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM pending ORDER BY since")?;
        let ids = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(ids)
    }
}

/// OAuth client registered dynamically (RFC 7591).
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthClient {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// Hash of the client secret, for confidential clients. Public clients
    /// (PKCE only, `token_endpoint_auth_method: none`) have none.
    pub secret_hash: Option<String>,
}

/// OAuth clients and tokens for the MCP server. Tokens are stored only as hashes.
impl State {
    pub fn add_client(&self, client: &OAuthClient, now: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO oauth_clients (id, name, redirect_uris, created, secret_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                client.id,
                client.name,
                serde_json::to_string(&client.redirect_uris)?,
                now,
                client.secret_hash,
            ],
        )?;
        Ok(())
    }

    pub fn client(&self, id: &str) -> Result<Option<OAuthClient>> {
        let row: Option<(String, String, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT name, redirect_uris, secret_hash FROM oauth_clients WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(match row {
            Some((name, uris, secret_hash)) => Some(OAuthClient {
                id: id.to_string(),
                name,
                redirect_uris: serde_json::from_str(&uris)?,
                secret_hash,
            }),
            None => None,
        })
    }

    pub fn client_count(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM oauth_clients", [], |r| r.get(0))?)
    }

    /// Marks the client as having completed an authorization at least once.
    pub fn mark_client_authorized(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE oauth_clients SET authorized = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Deletes clients registered before `cutoff` that never got a token.
    /// Registration is open by design, so this keeps junk registrations from
    /// piling up against the client limit. Returns how many were removed.
    pub fn prune_unauthorized_clients(&self, cutoff: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM oauth_clients WHERE authorized = 0 AND created < ?1",
            params![cutoff],
        )?)
    }

    pub fn add_token(&self, hash: &str, kind: &str, client_id: &str, expires: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO oauth_tokens (hash, kind, client_id, expires) VALUES (?1, ?2, ?3, ?4)",
            params![hash, kind, client_id, expires],
        )?;
        Ok(())
    }

    /// The client that owns the token, if it exists, has the given kind and has not expired.
    pub fn token_client(&self, hash: &str, kind: &str, now: i64) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT client_id FROM oauth_tokens WHERE hash = ?1 AND kind = ?2 AND expires > ?3",
                params![hash, kind, now],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Consumes the token (single use, like the rotating refresh token).
    pub fn take_token(&self, hash: &str, kind: &str, now: i64) -> Result<Option<String>> {
        let client = self.token_client(hash, kind, now)?;
        self.conn()
            .execute("DELETE FROM oauth_tokens WHERE hash = ?1", params![hash])?;
        Ok(client)
    }

    /// Revokes every OAuth token. Returns how many there were.
    pub fn clear_tokens(&self) -> Result<usize> {
        Ok(self.conn().execute("DELETE FROM oauth_tokens", [])?)
    }

    pub fn purge_expired_tokens(&self, now: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM oauth_tokens WHERE expires <= ?1", params![now])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_files_and_pending() {
        let dir = tempfile::tempdir().unwrap();
        let st = State::open(&dir.path().join("s.db")).unwrap();
        assert_eq!(st.since().unwrap(), "0");
        st.set_since("12-abc").unwrap();
        assert_eq!(st.since().unwrap(), "12-abc");

        st.upsert_file("a", "notas/a.md", Some(1)).unwrap();
        st.upsert_file("a", "notas/b.md", Some(2)).unwrap();
        assert_eq!(st.path_of("a").unwrap().as_deref(), Some("notas/b.md"));
        assert_eq!(st.file_count().unwrap(), 1);
        st.remove_file("a").unwrap();
        assert_eq!(st.path_of("a").unwrap(), None);

        st.add_pending("x", "1").unwrap();
        st.add_pending("x", "2").unwrap();
        assert_eq!(st.pending_ids().unwrap(), vec!["x"]);
        st.remove_pending("x").unwrap();
        assert!(st.pending_ids().unwrap().is_empty());
    }

    #[test]
    fn adds_missing_columns_to_old_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE oauth_clients (id TEXT PRIMARY KEY, name TEXT NOT NULL,
                 redirect_uris TEXT NOT NULL, created INTEGER NOT NULL);
                 INSERT INTO oauth_clients VALUES ('old', 'x', '[]', 0);",
            )
            .unwrap();
        let st = State::open(&path).unwrap();
        assert_eq!(st.client("old").unwrap().unwrap().secret_hash, None);
        assert_eq!(st.prune_unauthorized_clients(1).unwrap(), 1);
        // Opening again is a no-op.
        drop(st);
        State::open(&path).unwrap();
    }

    #[test]
    fn oauth_clients_and_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let st = State::open(&dir.path().join("s.db")).unwrap();
        let client = OAuthClient {
            id: "c1".into(),
            name: "Some client".into(),
            redirect_uris: vec!["https://client.example/cb".into()],
            secret_hash: Some("abc".into()),
        };
        st.add_client(&client, 0).unwrap();
        assert_eq!(st.client("c1").unwrap(), Some(client.clone()));
        assert_eq!(st.client("c2").unwrap(), None);

        // Only clients that never got a token, registered before the cutoff, go away.
        let junk = OAuthClient {
            id: "junk".into(),
            ..client.clone()
        };
        let fresh = OAuthClient {
            id: "fresh".into(),
            ..client.clone()
        };
        st.add_client(&junk, 10).unwrap();
        st.add_client(&fresh, 100).unwrap();
        st.mark_client_authorized("c1").unwrap();
        assert_eq!(st.prune_unauthorized_clients(50).unwrap(), 1);
        assert!(st.client("junk").unwrap().is_none());
        assert!(st.client("fresh").unwrap().is_some());
        assert!(st.client("c1").unwrap().is_some());

        st.add_token("h1", "refresh", "c1", 100).unwrap();
        assert_eq!(st.token_client("h1", "access", 50).unwrap(), None);
        assert_eq!(st.token_client("h1", "refresh", 100).unwrap(), None);
        assert_eq!(
            st.take_token("h1", "refresh", 50).unwrap().as_deref(),
            Some("c1")
        );
        assert_eq!(st.take_token("h1", "refresh", 50).unwrap(), None);
    }
}
