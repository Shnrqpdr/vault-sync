//! Interprets the documents Self-hosted LiveSync stores in CouchDB.
//!
//! Format (checked against vrtmrz/obsidian-livesync + livesync-commonlib, Oct 2026):
//! - File metadata: `type` = "plain" (text) or "newnote" (binary), with
//!   `path`, `children` (chunk ids), `mtime`, `ctime`, `size`, `deleted`
//!   (soft delete) and `eden` (inlined chunks, optional).
//! - Legacy: `type` = "notes" or missing, with the content inline in `data`.
//! - Chunks: `type` = "leaf", id prefixed with `h:`, content in `data`.
//!   Text: plain concatenation. Binary: each chunk is independent base64.
//! - E2EE enabled: `path` starts with `/\:` and chunk ids with `h:+`.
//! - Paths prefixed with `i:`, `ix:` or `ps:` are hidden files,
//!   customization sync and plugins, not notes.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::Value;

const ENCRYPTED_PATH_PREFIX: &str = "/\\:";
const ENCRYPTED_CHUNK_PREFIX: &str = "h:+";
const CHUNK_PREFIX: &str = "h:";
const NON_NOTE_PATH_PREFIXES: [&str; 3] = ["i:", "ix:", "ps:"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Text,
    Binary,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// Content assembled from `children` (+ `eden`).
    Chunked {
        children: Vec<String>,
        eden: HashMap<String, String>,
    },
    /// Legacy format with the content inline.
    Inline(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    pub id: String,
    pub path: String,
    pub data_type: DataType,
    pub body: Body,
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A vault file that exists.
    File(FileEntry),
    /// A deleted file. `path` is only present on LiveSync soft deletes;
    /// CouchDB tombstones (`_deleted`) carry just the id.
    Deleted { id: String, path: Option<String> },
    /// End-to-end encrypted vault: unreadable without the passphrase.
    Encrypted { id: String },
    /// Chunks, internal documents, design docs, hidden files, etc.
    Ignored,
}

pub fn classify(id: &str, doc: Option<&Value>) -> Entry {
    if id.starts_with("_design/") || id.starts_with("_local/") {
        return Entry::Ignored;
    }
    if id.starts_with(CHUNK_PREFIX) {
        return Entry::Ignored;
    }
    let Some(doc) = doc else {
        return Entry::Deleted {
            id: id.to_string(),
            path: None,
        };
    };
    let tombstone = doc
        .get("_deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let soft_deleted = doc.get("deleted").and_then(Value::as_bool).unwrap_or(false);
    let path = doc.get("path").and_then(Value::as_str);

    if tombstone {
        return Entry::Deleted {
            id: id.to_string(),
            path: path.and_then(note_path).map(str::to_string),
        };
    }

    let doc_type = doc.get("type").and_then(Value::as_str);
    let data_type = match doc_type {
        Some("plain") => DataType::Text,
        Some("newnote") => DataType::Binary,
        Some("notes") | None if path.is_some() => DataType::Text,
        _ => return Entry::Ignored,
    };

    let Some(raw_path) = path else {
        return Entry::Ignored;
    };
    if raw_path.starts_with(ENCRYPTED_PATH_PREFIX) {
        return Entry::Encrypted { id: id.to_string() };
    }
    let Some(path) = note_path(raw_path) else {
        return Entry::Ignored;
    };

    if soft_deleted {
        return Entry::Deleted {
            id: id.to_string(),
            path: Some(path.to_string()),
        };
    }

    let body = match doc_type {
        Some("plain") | Some("newnote") => {
            let children: Vec<String> = doc
                .get("children")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if children
                .iter()
                .any(|c| c.starts_with(ENCRYPTED_CHUNK_PREFIX))
            {
                return Entry::Encrypted { id: id.to_string() };
            }
            let eden = doc
                .get("eden")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| {
                            v.get("data")
                                .and_then(Value::as_str)
                                .map(|d| (k.clone(), d.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Body::Chunked { children, eden }
        }
        _ => {
            let data = match doc.get("data") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect(),
                _ => String::new(),
            };
            Body::Inline(data)
        }
    };

    Entry::File(FileEntry {
        id: id.to_string(),
        path: path.to_string(),
        data_type,
        body,
        mtime: doc.get("mtime").and_then(Value::as_i64),
    })
}

/// Returns the note path, or `None` for LiveSync's special paths.
fn note_path(raw: &str) -> Option<&str> {
    if NON_NOTE_PATH_PREFIXES.iter().any(|p| raw.starts_with(p)) {
        return None;
    }
    let p = raw.trim_start_matches('/');
    (!p.is_empty()).then_some(p)
}

#[derive(Debug, PartialEq)]
pub enum AssembleError {
    /// Chunks have not reached the server yet (or were compacted away).
    MissingChunks(Vec<String>),
    /// Legacy binary encoding (`%`) or invalid base64.
    Undecodable(String),
}

impl FileEntry {
    /// Ids of the chunks that must be fetched from the database (those not in `eden`).
    pub fn external_chunks(&self) -> Vec<String> {
        match &self.body {
            Body::Chunked { children, eden } => children
                .iter()
                .filter(|c| !eden.contains_key(*c))
                .cloned()
                .collect(),
            Body::Inline(_) => Vec::new(),
        }
    }

    /// Assembles the file content from its chunks.
    pub fn assemble(&self, chunks: &HashMap<String, String>) -> Result<Vec<u8>, AssembleError> {
        let parts: Vec<&str> = match &self.body {
            Body::Inline(data) => vec![data.as_str()],
            Body::Chunked { children, eden } => {
                let missing: Vec<String> = children
                    .iter()
                    .filter(|c| !eden.contains_key(*c) && !chunks.contains_key(*c))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    return Err(AssembleError::MissingChunks(missing));
                }
                children
                    .iter()
                    .map(|c| {
                        eden.get(c)
                            .or_else(|| chunks.get(c))
                            .map(String::as_str)
                            .unwrap_or("")
                    })
                    .collect()
            }
        };

        match self.data_type {
            DataType::Text => Ok(parts.concat().into_bytes()),
            DataType::Binary => {
                if parts.first().is_some_and(|p| p.starts_with('%')) {
                    return Err(AssembleError::Undecodable(
                        "legacy binary encoding (%) is not supported".into(),
                    ));
                }
                let mut out = Vec::new();
                for part in parts {
                    let bytes = B64
                        .decode(part)
                        .map_err(|e| AssembleError::Undecodable(format!("invalid base64: {e}")))?;
                    out.extend_from_slice(&bytes);
                }
                Ok(out)
            }
        }
    }
}

/// Extracts the content of a chunk document (`type: "leaf"`).
pub fn chunk_data(doc: &Value) -> Option<&str> {
    doc.get("data").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(e: Entry) -> FileEntry {
        match e {
            Entry::File(f) => f,
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn text_from_chunks_and_eden() {
        let doc = json!({
            "_id": "notas/a.md", "path": "notas/a.md", "type": "plain",
            "children": ["h:1", "h:2", "h:3"],
            "eden": { "h:2": { "data": "meio ", "epoch": 1 } },
            "mtime": 1700000000000i64
        });
        let f = file(classify("notas/a.md", Some(&doc)));
        assert_eq!(f.external_chunks(), vec!["h:1", "h:3"]);
        let chunks = HashMap::from([
            ("h:1".to_string(), "início ".to_string()),
            ("h:3".to_string(), "fim".to_string()),
        ]);
        assert_eq!(f.assemble(&chunks).unwrap(), "início meio fim".as_bytes());
    }

    #[test]
    fn binary_is_base64_per_chunk() {
        let doc = json!({ "path": "img.png", "type": "newnote", "children": ["h:a", "h:b"] });
        let f = file(classify("img.png", Some(&doc)));
        let chunks = HashMap::from([
            ("h:a".to_string(), B64.encode([1u8, 2, 3, 4])),
            ("h:b".to_string(), B64.encode([5u8])),
        ]);
        assert_eq!(f.assemble(&chunks).unwrap(), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn missing_chunks() {
        let doc = json!({ "path": "x.md", "type": "plain", "children": ["h:1", "h:2"] });
        let f = file(classify("x.md", Some(&doc)));
        let chunks = HashMap::from([("h:1".to_string(), "a".to_string())]);
        assert_eq!(
            f.assemble(&chunks),
            Err(AssembleError::MissingChunks(vec!["h:2".into()]))
        );
    }

    #[test]
    fn legacy_inline_content() {
        let doc = json!({ "path": "old.md", "type": "notes", "data": "olá" });
        let f = file(classify("old.md", Some(&doc)));
        assert_eq!(f.assemble(&HashMap::new()).unwrap(), "olá".as_bytes());
    }

    #[test]
    fn soft_delete_and_tombstone() {
        let soft = json!({ "path": "a.md", "type": "plain", "deleted": true, "children": [] });
        assert_eq!(
            classify("a.md", Some(&soft)),
            Entry::Deleted {
                id: "a.md".into(),
                path: Some("a.md".into())
            }
        );
        let tomb = json!({ "_id": "f:abc", "_deleted": true });
        assert_eq!(
            classify("f:abc", Some(&tomb)),
            Entry::Deleted {
                id: "f:abc".into(),
                path: None
            }
        );
    }

    #[test]
    fn ignores_special_docs_and_detects_e2ee() {
        assert_eq!(
            classify("h:xyz", Some(&json!({ "type": "leaf", "data": "x" }))),
            Entry::Ignored
        );
        assert_eq!(classify("_design/x", Some(&json!({}))), Entry::Ignored);
        assert_eq!(
            classify("obsydian_livesync_version", Some(&json!({ "version": 12 }))),
            Entry::Ignored
        );
        let hidden = json!({ "path": "i:.obsidian/app.json", "type": "plain", "children": [] });
        assert_eq!(
            classify("i:.obsidian/app.json", Some(&hidden)),
            Entry::Ignored
        );
        let enc = json!({ "path": "/\\:abcdef", "type": "plain", "children": ["h:+1"] });
        assert_eq!(
            classify("f:1", Some(&enc)),
            Entry::Encrypted { id: "f:1".into() }
        );
    }
}
