//! Writes notes to CouchDB in Self-hosted LiveSync's format, so they reach
//! every device just like an edit made in Obsidian. The mirror and the search
//! index catch up through the `_changes` feed, as for any other edit.
//!
//! A note is a metadata document (`type: "plain"`) listing its chunks
//! (`type: "leaf"`, ids prefixed with `h:`). Chunks are written first: a
//! device that sees the metadata before its chunks waits for them. Chunk ids
//! are content hashes, so rewriting a chunk that already exists is a no-op.
//! The metadata carries the `_rev` it was based on, so CouchDB rejects the
//! write if a device changed the note in the meantime.
//!
//! Document ids follow LiveSync's `path2id` (livesync-commonlib, Oct 2026):
//! the path, lowercased unless the vault handles file names as case-sensitive,
//! with `/` prepended when it starts with `_` (CouchDB reserves those ids).
//! That setting, and whether the vault is encrypted, come from the milestone
//! document the devices keep in the database.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::couch::Couch;
use crate::livesync::{chunk_data, classify, DataType, Entry};
use crate::state::State;

const MILESTONE_ID: &str = "_local/obsydian_livesync_milestone";
/// Chunks are cut at a line break below this many bytes.
const CHUNK_SIZE: usize = 50 * 1024;
/// Larger notes are refused: a model has no business writing them in one go.
const MAX_NOTE_SIZE: usize = 2 * 1024 * 1024;
/// Not allowed in a file or folder name: they break sync on Windows or
/// Android, or Obsidian's links.
const FORBIDDEN_CHARS: &str = "\\:*?\"<>|#^[]";

/// What to do to a note.
pub enum Change {
    /// A new note with this content. Fails if the note exists.
    Create(String),
    /// Adds text at the end of an existing note, on a new line.
    Append(String),
    /// Replaces text that occurs exactly once in an existing note.
    Replace { old: String, new: String },
}

/// Result of a write.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Written {
    /// Path of the note in the vault.
    pub path: String,
    /// `true` when the note was created, `false` when it was edited.
    pub created: bool,
    /// `false` when the edit left the note unchanged, so nothing was written.
    pub changed: bool,
    /// Size of the note after the write, in bytes.
    pub size: usize,
}

pub struct Writer {
    couch: Couch,
    state: Arc<State>,
    /// One write at a time, so two edits of the same note don't race for its `_rev`.
    lock: Mutex<()>,
}

/// The note as it is in the database.
struct Current {
    /// The document, for its `_rev`. Also set for a deleted note, whose id is reused.
    doc: Option<Value>,
    /// Path and text, when the note exists.
    note: Option<(String, String)>,
}

#[derive(Debug, Default, PartialEq)]
struct Settings {
    case_sensitive: bool,
    encrypted: bool,
}

impl Writer {
    pub fn new(couch: Couch, state: Arc<State>) -> Self {
        Self {
            couch,
            state,
            lock: Mutex::new(()),
        }
    }

    pub async fn write(&self, path: &str, change: Change) -> Result<Written> {
        let path = check_path(path)?;
        let _one_at_a_time = self.lock.lock().await;

        let settings = Settings::from_milestone(self.couch.get_local(MILESTONE_ID).await?.as_ref());
        if settings.encrypted {
            bail!("the vault is end-to-end encrypted, so notes cannot be written");
        }
        // A note the mirror already has keeps its id, whatever the case setting.
        let id = match self.state.id_of_path(&path)? {
            Some(id) => id,
            None => path2id(&path, settings.case_sensitive),
        };
        let current = self.current(&id).await?;

        let (stored_path, text, created) = match (change, current.note) {
            (Change::Create(text), None) => (path, text, true),
            (Change::Create(_), Some((existing, _))) => {
                bail!("{existing} already exists; read it and use edit_note to change it")
            }
            (_, None) => bail!("note not found: {path}. Use create_note for a new note"),
            (Change::Append(add), Some((existing, old))) => (existing, append(&old, &add), false),
            (Change::Replace { old: find, new }, Some((existing, old))) => {
                let text = replace(&old, &find, &new)?;
                if text == old {
                    return Ok(Written {
                        path: display(&existing),
                        created: false,
                        changed: false,
                        size: old.len(),
                    });
                }
                (existing, text, false)
            }
        };
        if text.len() > MAX_NOTE_SIZE {
            bail!(
                "the note would have {} bytes; the limit is {MAX_NOTE_SIZE}",
                text.len()
            );
        }

        let mut children = Vec::new();
        let mut leaves = Vec::new();
        let mut seen = HashSet::new();
        for piece in split(&text) {
            let chunk = chunk_id(piece);
            if seen.insert(chunk.clone()) {
                leaves.push(json!({ "_id": chunk, "type": "leaf", "data": piece }));
            }
            children.push(chunk);
        }
        if !leaves.is_empty() {
            for result in self.couch.bulk_docs(&leaves).await? {
                // A conflict means the chunk exists, with the same content.
                match result.get("error").and_then(Value::as_str) {
                    None | Some("conflict") => {}
                    Some(e) => bail!("CouchDB refused a chunk: {e}"),
                }
            }
        }

        let now = now_ms();
        let ctime = current
            .doc
            .as_ref()
            .and_then(|d| d.get("ctime"))
            .and_then(Value::as_i64)
            .filter(|_| !created)
            .unwrap_or(now);
        let mut meta = json!({
            "_id": id,
            "path": stored_path,
            "children": children,
            "ctime": ctime,
            "mtime": now,
            "size": text.len(),
            "type": "plain",
            "eden": {},
        });
        if let Some(rev) = current.doc.as_ref().and_then(|d| d.get("_rev")) {
            meta["_rev"] = rev.clone();
        }
        let results = self.couch.bulk_docs(&[meta]).await?;
        match results
            .first()
            .and_then(|r| r.get("error"))
            .and_then(Value::as_str)
        {
            None => {}
            Some("conflict") => bail!(
                "{} changed while it was being saved, probably on a device; read it again and retry",
                display(&stored_path)
            ),
            Some(e) => bail!("CouchDB refused the note: {e}"),
        }
        Ok(Written {
            path: display(&stored_path),
            created,
            changed: true,
            size: text.len(),
        })
    }

    async fn current(&self, id: &str) -> Result<Current> {
        let Some(doc) = self.couch.get_docs(&[id.to_string()]).await?.remove(id) else {
            return Ok(Current {
                doc: None,
                note: None,
            });
        };
        let file = match classify(id, Some(&doc)) {
            Entry::File(f) => f,
            Entry::Deleted { .. } => {
                return Ok(Current {
                    doc: Some(doc),
                    note: None,
                })
            }
            Entry::Encrypted { .. } => {
                bail!("the vault is end-to-end encrypted, so notes cannot be written")
            }
            Entry::Ignored => bail!("{id} is not a note LiveSync syncs"),
        };
        if file.data_type != DataType::Text {
            bail!("{} is not a text file", file.path);
        }
        let ids = file.external_chunks();
        let chunks = self
            .couch
            .get_docs(&ids)
            .await?
            .iter()
            .filter_map(|(k, v)| chunk_data(v).map(|d| (k.clone(), d.to_string())))
            .collect();
        let bytes = file.assemble(&chunks).map_err(|e| {
            anyhow::anyhow!("could not read {} from the database: {e:?}", file.path)
        })?;
        let text = String::from_utf8(bytes)
            .with_context(|| format!("{} is not valid UTF-8", file.path))?;
        // The stored path, as LiveSync wrote it, so an edit never renames the file.
        let raw_path = doc
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(&file.path)
            .to_string();
        Ok(Current {
            doc: Some(doc),
            note: Some((raw_path, text)),
        })
    }
}

impl Settings {
    /// Every device records its settings in the milestone; any device with
    /// case-sensitive names or encryption makes the vault so.
    fn from_milestone(doc: Option<&Value>) -> Self {
        let nodes = doc
            .and_then(|d| d.get("tweak_values"))
            .and_then(Value::as_object);
        let any = |key: &str| {
            nodes.is_some_and(|n| {
                n.values()
                    .any(|v| v.get(key).and_then(Value::as_bool) == Some(true))
            })
        };
        Self {
            case_sensitive: any("handleFilenameCaseSensitive"),
            encrypted: any("encrypt"),
        }
    }
}

/// Validates a note path given by a client and normalizes it to `a/b/c.md`.
pub fn check_path(raw: &str) -> Result<String> {
    let path = raw.trim().trim_start_matches('/');
    if !path.to_lowercase().ends_with(".md") || path.len() <= 3 {
        bail!("only Markdown notes can be written: the path must end in .md (got {raw:?})");
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            bail!("invalid path {raw:?}");
        }
        if part.starts_with('.') {
            bail!("hidden files and folders cannot be written ({raw:?})");
        }
        if part.ends_with(' ') || part.ends_with('.') {
            bail!("names cannot end with a space or a dot ({raw:?})");
        }
        if let Some(c) = part
            .chars()
            .find(|c| c.is_control() || FORBIDDEN_CHARS.contains(*c))
        {
            bail!("{c:?} is not allowed in note or folder names ({raw:?})");
        }
    }
    Ok(path.to_string())
}

fn path2id(path: &str, case_sensitive: bool) -> String {
    let id = if case_sensitive {
        path.to_string()
    } else {
        path.to_lowercase()
    };
    if id.starts_with('_') {
        format!("/{id}")
    } else {
        id
    }
}

/// The path without LiveSync's leading `/`, as the mirror shows it.
fn display(path: &str) -> String {
    path.trim_start_matches('/').to_string()
}

fn append(text: &str, add: &str) -> String {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(add);
    out
}

fn replace(text: &str, old: &str, new: &str) -> Result<String> {
    if old.is_empty() {
        bail!("old_text must not be empty");
    }
    // Notes written on Windows may use CRLF, which the model doesn't see.
    let crlf = (
        old.replace("\r\n", "\n").replace('\n', "\r\n"),
        new.replace("\r\n", "\n").replace('\n', "\r\n"),
    );
    for (find, with) in [(old, new), (crlf.0.as_str(), crlf.1.as_str())] {
        match text.matches(find).count() {
            0 => continue,
            1 => return Ok(text.replacen(find, with, 1)),
            n => bail!("old_text appears {n} times in the note; include more of the surrounding text so it matches once"),
        }
    }
    bail!("old_text was not found in the note; read the note again and copy the text exactly")
}

/// Cuts the text into chunks of at most `CHUNK_SIZE` bytes, preferably after a
/// line break. Concatenated, they give back the text.
fn split(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.len() > CHUNK_SIZE {
        let mut cut = CHUNK_SIZE;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(nl) = rest[..cut].rfind('\n') {
            cut = nl + 1;
        }
        pieces.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() {
        pieces.push(rest);
    }
    pieces
}

/// A content hash. LiveSync's own ids (xxhash, base36) are shorter, so the two never collide.
fn chunk_id(piece: &str) -> String {
    let digest = Sha256::digest(piece.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("h:{hex}")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// An in-memory CouchDB database with the endpoints the writer uses, for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::{json, Value};

    #[derive(Default)]
    pub struct FakeDb {
        pub docs: Mutex<HashMap<String, Value>>,
        pub milestone: Mutex<Option<Value>>,
    }

    impl FakeDb {
        pub fn doc(&self, id: &str) -> Option<Value> {
            self.docs.lock().unwrap().get(id).cloned()
        }

        /// Stores a document as CouchDB would: only on top of its current `_rev`.
        pub fn put(&self, mut doc: Value) -> Value {
            let id = doc["_id"].as_str().unwrap().to_string();
            let mut docs = self.docs.lock().unwrap();
            let current = docs.get(&id).and_then(|d| d.get("_rev")).cloned();
            if current != doc.get("_rev").cloned() {
                return json!({ "id": id, "error": "conflict" });
            }
            let n = current
                .and_then(|r| r.as_str()?.split('-').next()?.parse::<u64>().ok())
                .unwrap_or(0)
                + 1;
            let rev = format!("{n}-fake");
            doc["_rev"] = json!(rev);
            docs.insert(id.clone(), doc);
            json!({ "ok": true, "id": id, "rev": rev })
        }
    }

    /// Serves the database at the returned URL.
    pub async fn spawn() -> (String, Arc<FakeDb>) {
        let db = Arc::new(FakeDb::default());
        let app = Router::new()
            .route("/db/_all_docs", post(all_docs))
            .route("/db/_bulk_docs", post(bulk_docs))
            .route("/db/_local/obsydian_livesync_milestone", get(milestone))
            .with_state(db.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/db", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, db)
    }

    async fn all_docs(State(db): State<Arc<FakeDb>>, Json(body): Json<Value>) -> Json<Value> {
        let rows: Vec<Value> = body["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| match db.doc(key.as_str().unwrap()) {
                Some(doc) => {
                    json!({ "key": key, "id": key, "value": { "rev": doc["_rev"] }, "doc": doc })
                }
                None => json!({ "key": key, "error": "not_found" }),
            })
            .collect();
        Json(json!({ "rows": rows }))
    }

    async fn bulk_docs(State(db): State<Arc<FakeDb>>, Json(body): Json<Value>) -> Json<Value> {
        let results: Vec<Value> = body["docs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|doc| db.put(doc.clone()))
            .collect();
        Json(Value::Array(results))
    }

    async fn milestone(State(db): State<Arc<FakeDb>>) -> Response {
        match db.milestone.lock().unwrap().clone() {
            Some(doc) => Json(doc).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::fake::{self, FakeDb};
    use super::*;

    async fn writer() -> (Writer, Arc<FakeDb>, Arc<State>, tempfile::TempDir) {
        let (url, db) = fake::spawn().await;
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(State::open(&dir.path().join("state.db")).unwrap());
        let couch = Couch::connect(&url, "user", "password").unwrap();
        (Writer::new(couch, state.clone()), db, state, dir)
    }

    /// The note's text as a device would assemble it.
    fn text_of(db: &FakeDb, id: &str) -> String {
        let doc = db.doc(id).unwrap_or_else(|| panic!("{id} was not written"));
        let Entry::File(file) = classify(id, Some(&doc)) else {
            panic!("{id} is not a file: {doc}");
        };
        let chunks: HashMap<String, String> = file
            .external_chunks()
            .into_iter()
            .map(|c| {
                let leaf = db.doc(&c).unwrap();
                assert_eq!(leaf["type"], "leaf");
                (c, chunk_data(&leaf).unwrap().to_string())
            })
            .collect();
        String::from_utf8(file.assemble(&chunks).unwrap()).unwrap()
    }

    fn milestone(db: &FakeDb, tweaks: Value) {
        *db.milestone.lock().unwrap() = Some(json!({
            "_id": MILESTONE_ID,
            "type": "milestoneinfo",
            "tweak_values": { "node-a": tweaks },
        }));
    }

    #[tokio::test]
    async fn creates_and_edits_in_livesync_format() {
        let (w, db, _state, _dir) = writer().await;

        let created = w
            .write("Ideas/Cache.md", Change::Create("# Cache\n".into()))
            .await
            .unwrap();
        assert!(created.created && created.changed);
        assert_eq!(created.path, "Ideas/Cache.md");
        // LiveSync's default: ids are lowercase paths; the path keeps its case.
        let doc = db.doc("ideas/cache.md").unwrap();
        assert_eq!(doc["path"], "Ideas/Cache.md");
        assert_eq!(doc["type"], "plain");
        assert_eq!(doc["size"], 8);
        assert_eq!(doc["eden"], json!({}));
        let ctime = doc["ctime"].as_i64().unwrap();
        assert_eq!(doc["mtime"].as_i64().unwrap(), ctime);
        assert_eq!(text_of(&db, "ideas/cache.md"), "# Cache\n");

        let err = w
            .write("ideas/cache.md", Change::Create("again".into()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        w.write("Ideas/Cache.md", Change::Append("More.".into()))
            .await
            .unwrap();
        assert_eq!(text_of(&db, "ideas/cache.md"), "# Cache\nMore.");
        let edited = w
            .write(
                "Ideas/Cache.md",
                Change::Replace {
                    old: "More.".into(),
                    new: "Less.".into(),
                },
            )
            .await
            .unwrap();
        assert!(!edited.created && edited.changed);
        assert_eq!(text_of(&db, "ideas/cache.md"), "# Cache\nLess.");
        let doc = db.doc("ideas/cache.md").unwrap();
        assert_eq!(doc["_rev"], "3-fake");
        assert_eq!(doc["ctime"].as_i64().unwrap(), ctime);

        let same = w
            .write(
                "Ideas/Cache.md",
                Change::Replace {
                    old: "Less.".into(),
                    new: "Less.".into(),
                },
            )
            .await
            .unwrap();
        assert!(!same.changed);
        assert_eq!(db.doc("ideas/cache.md").unwrap()["_rev"], "3-fake");

        let err = w
            .write("Ideas/Other.md", Change::Append("x".into()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn edits_start_from_what_devices_wrote() {
        let (w, db, _state, _dir) = writer().await;
        w.write("a.md", Change::Create("one\n".into()))
            .await
            .unwrap();
        // A device edits the note: new chunk, new revision.
        let mut doc = db.doc("a.md").unwrap();
        db.put(json!({ "_id": "h:device", "type": "leaf", "data": "one\ntwo\n" }));
        doc["children"] = json!(["h:device"]);
        db.put(doc);

        w.write("a.md", Change::Append("three\n".into()))
            .await
            .unwrap();
        assert_eq!(text_of(&db, "a.md"), "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn replace_needs_one_exact_match() {
        let (w, db, _state, _dir) = writer().await;
        w.write("a.md", Change::Create("x\r\ny y\r\n".into()))
            .await
            .unwrap();
        let replace = |old: &str, new: &str| Change::Replace {
            old: old.into(),
            new: new.into(),
        };

        let err = w.write("a.md", replace("y", "z")).await.unwrap_err();
        assert!(err.to_string().contains("appears 2 times"), "{err}");
        let err = w.write("a.md", replace("nope", "z")).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        // The model sends LF; the note uses CRLF.
        w.write("a.md", replace("x\ny y", "a\nb")).await.unwrap();
        assert_eq!(text_of(&db, "a.md"), "a\r\nb\r\n");
    }

    #[tokio::test]
    async fn ids_follow_the_vault_settings() {
        let (w, db, state, _dir) = writer().await;
        w.write("_drafts/A.md", Change::Create("a".into()))
            .await
            .unwrap();
        // CouchDB reserves ids starting with `_`.
        assert_eq!(db.doc("/_drafts/a.md").unwrap()["path"], "_drafts/A.md");

        milestone(&db, json!({ "handleFilenameCaseSensitive": true }));
        w.write("Notes/B.md", Change::Create("b".into()))
            .await
            .unwrap();
        assert!(db.doc("Notes/B.md").is_some());

        // A note the mirror knows keeps its id and path, whatever case the client uses.
        state.upsert_file("Notes/B.md", "Notes/B.md", None).unwrap();
        let w2 = w
            .write("notes/b.md", Change::Append("!".into()))
            .await
            .unwrap();
        assert_eq!(w2.path, "Notes/B.md");
        assert_eq!(text_of(&db, "Notes/B.md"), "b\n!");
        assert!(db.doc("notes/b.md").is_none());

        milestone(&db, json!({ "encrypt": true }));
        let err = w
            .write("c.md", Change::Create("c".into()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("encrypted"), "{err}");
    }

    #[tokio::test]
    async fn a_deleted_note_can_be_created_again() {
        let (w, db, _state, _dir) = writer().await;
        db.put(json!({
            "_id": "gone.md", "path": "gone.md", "type": "plain",
            "deleted": true, "children": [], "ctime": 1, "mtime": 1, "size": 0
        }));
        let created = w
            .write("gone.md", Change::Create("back".into()))
            .await
            .unwrap();
        assert!(created.created);
        let doc = db.doc("gone.md").unwrap();
        assert_eq!(doc["_rev"], "2-fake");
        assert!(doc.get("deleted").is_none());
        assert_ne!(doc["ctime"], 1);
        assert_eq!(text_of(&db, "gone.md"), "back");
    }

    #[test]
    fn paths_are_checked() {
        assert_eq!(check_path(" /Ideas/Café.md ").unwrap(), "Ideas/Café.md");
        assert_eq!(check_path("a.MD").unwrap(), "a.MD");
        for bad in [
            "a.txt",
            ".md",
            "../a.md",
            "a//b.md",
            "./a.md",
            ".obsidian/a.md",
            "a/.trash/b.md",
            "a /b.md",
            "a./b.md",
            "a:b.md",
            "a#b.md",
            "[a].md",
            "a\nb.md",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn chunks_concatenate_back() {
        let lines: String = (0..4000)
            .map(|i| format!("line {i} with some text\n"))
            .collect();
        let pieces = split(&lines);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| p.len() <= CHUNK_SIZE));
        assert!(pieces[..pieces.len() - 1].iter().all(|p| p.ends_with('\n')));
        assert_eq!(pieces.concat(), lines);

        // No line breaks, multibyte characters: still cut on a char boundary.
        let long = "é".repeat(CHUNK_SIZE);
        assert_eq!(split(&long).concat(), long);
        assert!(split("").is_empty());
        assert_ne!(chunk_id("a"), chunk_id("b"));
        assert!(chunk_id("a").starts_with("h:") && !chunk_id("a").starts_with("h:+"));
    }
}
