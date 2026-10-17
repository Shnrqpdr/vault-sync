//! Catalog of the mirror: a full-text index (tantivy, in memory) plus note
//! metadata (tags, frontmatter, links) for search, listing and backlinks.
//!
//! The mirror on disk is the source of truth: the catalog is rebuilt on every
//! start and then updated from the paths the syncer reports. There is no
//! separate checkpoint that could drift from the mirror.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    Facet, FacetOptions, Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions,
    Value as _, STORED, STRING,
};
use tantivy::snippet::{Snippet, SnippetGenerator};
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer,
    TextAnalyzer,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{info, warn};
use walkdir::WalkDir;

use crate::mirror;
use crate::notes;
use crate::sync::SharedStatus;

const TOKENIZER: &str = "pt";
const WRITER_MEMORY: usize = 50_000_000;
const SNIPPET_CHARS: usize = 240;

pub struct Catalog {
    vault_dir: PathBuf,
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: Fields,
    store: RwLock<Store>,
}

#[derive(Clone, Copy)]
struct Fields {
    path: Field,
    title: Field,
    body: Field,
    tags: Field,
    folder: Field,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct NoteMeta {
    pub path: String,
    pub title: String,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    /// Last modification in ms since the Unix epoch (LiveSync's `mtime`).
    pub mtime: Option<i64>,
    pub size: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct NoteDetail {
    #[serde(flatten)]
    pub meta: NoteMeta,
    pub frontmatter: Value,
    pub links: Vec<Link>,
    pub backlinks: Vec<String>,
    pub content: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct Link {
    pub target: String,
    /// The file the link points to; `None` when it is broken.
    pub path: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub tags: Vec<String>,
    pub score: f32,
    /// Text snippet with the matched terms wrapped in `**`.
    pub snippet: String,
}

#[derive(Debug, PartialEq, Serialize, JsonSchema)]
pub struct TagCount {
    pub tag: String,
    pub count: usize,
}

/// Filters shared by search and listing. Both include nested levels: the
/// tag `project` matches `project/vault`, the folder `a` matches `a/b/c.md`.
#[derive(Debug, Default)]
pub struct Filter {
    pub tag: Option<String>,
    pub folder: Option<String>,
}

struct Note {
    meta: NoteMeta,
    frontmatter: Value,
    links: Vec<String>,
}

#[derive(Default)]
struct Store {
    notes: BTreeMap<String, Note>,
    /// Every file (notes and attachments) by lowercase path.
    by_lower: HashMap<String, String>,
    /// Paths by lowercase file name, to resolve `[[Name]]`.
    by_name: HashMap<String, BTreeSet<String>>,
}

impl Catalog {
    pub fn new(vault_dir: PathBuf) -> Result<Self> {
        let mut schema = Schema::builder();
        let text = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let fields = Fields {
            path: schema.add_text_field("path", STRING | STORED),
            title: schema.add_text_field("title", text.clone()),
            body: schema.add_text_field("body", text.set_stored()),
            tags: schema.add_facet_field("tags", FacetOptions::default()),
            folder: schema.add_facet_field("folder", FacetOptions::default()),
        };
        let index = Index::create_in_ram(schema.build());
        // Lowercase, accent-free, Portuguese stemming: "Reuniões" matches "reuniao".
        index.tokenizers().register(
            TOKENIZER,
            TextAnalyzer::builder(SimpleTokenizer::default())
                .filter(RemoveLongFilter::limit(40))
                .filter(LowerCaser)
                // Accents go before stemming, or "não" and "nao" become different terms.
                .filter(AsciiFoldingFilter)
                .filter(Stemmer::new(Language::Portuguese))
                .build(),
        );
        let writer = index
            .writer_with_num_threads(1, WRITER_MEMORY)
            .context("could not create the search index")?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            vault_dir,
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
            store: RwLock::new(Store::default()),
        })
    }

    pub fn count(&self) -> usize {
        read(&self.store).notes.len()
    }

    /// Re-reads the whole mirror. Returns the number of notes.
    pub fn rebuild(&self) -> Result<usize> {
        let mut paths = Vec::new();
        // Sorted, so document ids (and anything that depends on them) are the
        // same on every machine instead of following the filesystem's order.
        let walker = WalkDir::new(&self.vault_dir)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'));
        for entry in walker {
            match entry {
                Ok(e) if e.file_type().is_file() => {
                    if let Ok(rel) = e.path().strip_prefix(&self.vault_dir) {
                        paths.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "error walking the mirror"),
            }
        }
        let mut writer = lock(&self.writer);
        writer.delete_all_documents()?;
        *write(&self.store) = Store::default();
        self.apply(&mut writer, &paths)?;
        Ok(self.count())
    }

    /// Updates the given paths to match what is on disk now.
    pub fn refresh(&self, paths: &[String]) -> Result<()> {
        let mut writer = lock(&self.writer);
        self.apply(&mut writer, paths)
    }

    fn apply(&self, writer: &mut IndexWriter, paths: &[String]) -> Result<()> {
        for path in paths {
            if let Err(e) = self.apply_one(writer, path) {
                warn!(path = %path, error = %format!("{e:#}"), "could not index");
            }
        }
        writer.commit().context("index commit")?;
        self.reader.reload().context("index reload")?;
        Ok(())
    }

    fn apply_one(&self, writer: &IndexWriter, path: &str) -> Result<()> {
        writer.delete_term(Term::from_field_text(self.fields.path, path));
        let full = mirror::safe_join(&self.vault_dir, path)?;
        let meta = match std::fs::metadata(&full) {
            Ok(m) if m.is_file() => m,
            _ => {
                write(&self.store).remove(path);
                return Ok(());
            }
        };
        if !path.to_lowercase().ends_with(".md") {
            write(&self.store).add_file(path); // attachment: only matters for link resolution
            return Ok(());
        }

        let bytes = std::fs::read(&full).with_context(|| format!("read {}", full.display()))?;
        let parsed = notes::parse(path, &String::from_utf8_lossy(&bytes));
        let note = Note {
            meta: NoteMeta {
                path: path.to_string(),
                title: parsed.title,
                tags: parsed.tags,
                aliases: parsed.aliases,
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64),
                size: meta.len(),
            },
            frontmatter: parsed.frontmatter,
            links: parsed.links,
        };

        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.path, path);
        // Aliases count as title: searching "roadmap" finds the note with that alias.
        let mut title = note.meta.title.clone();
        for alias in &note.meta.aliases {
            title.push('\n');
            title.push_str(alias);
        }
        doc.add_text(self.fields.title, &title);
        doc.add_text(self.fields.body, &parsed.body);
        for tag in &note.meta.tags {
            if let Some(f) = facet(tag) {
                doc.add_facet(self.fields.tags, f);
            }
        }
        if let Some(f) = path.rsplit_once('/').and_then(|(dir, _)| facet(dir)) {
            doc.add_facet(self.fields.folder, f);
        }
        writer.add_document(doc)?;

        let mut store = write(&self.store);
        store.add_file(path);
        store.notes.insert(path.to_string(), note);
        Ok(())
    }

    pub fn search(&self, q: &str, filter: &Filter, limit: usize) -> Result<Vec<SearchHit>> {
        let mut parser =
            QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
        parser.set_conjunction_by_default();
        parser.set_field_boost(self.fields.title, 2.0);
        // Lenient: unbalanced quotes, `C++` and the like become a search, not an error.
        let (text_query, _) = parser.parse_query_lenient(q);
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];
        let tag = filter.tag.as_deref().map(normalize_tag);
        for (field, value) in [
            (self.fields.tags, tag.as_deref()),
            (self.fields.folder, filter.folder.as_deref()),
        ] {
            if let Some(f) = value.and_then(facet) {
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_facet(field, &f),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
        }
        let query = BooleanQuery::new(clauses);

        let searcher = self.reader.searcher();
        let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        let mut snippets = SnippetGenerator::create(&searcher, &query, self.fields.body)?;
        snippets.set_max_num_chars(SNIPPET_CHARS);

        let store = read(&self.store);
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let Some(note) = doc
                .get_first(self.fields.path)
                .and_then(|v| v.as_str())
                .and_then(|p| store.notes.get(p))
            else {
                continue;
            };
            let mut snippet = highlight(&snippets.snippet_from_doc(&doc));
            if snippet.is_empty() {
                // Only the title matched: show the start of the note.
                let body = doc.get_first(self.fields.body).and_then(|v| v.as_str());
                snippet = preview(body.unwrap_or_default());
            }
            hits.push(SearchHit {
                path: note.meta.path.clone(),
                title: note.meta.title.clone(),
                tags: note.meta.tags.clone(),
                score,
                snippet,
            });
        }
        Ok(hits)
    }

    /// Most recent notes first.
    pub fn list(&self, filter: &Filter, limit: usize) -> Vec<NoteMeta> {
        let tag = filter.tag.as_deref().map(normalize_tag);
        let folder = filter
            .folder
            .as_deref()
            .map(|f| f.trim_matches('/'))
            .filter(|f| !f.is_empty())
            .map(|f| format!("{f}/"));
        let store = read(&self.store);
        let mut out: Vec<NoteMeta> = store
            .notes
            .values()
            .map(|n| &n.meta)
            .filter(|m| {
                tag.as_deref()
                    .is_none_or(|t| m.tags.iter().any(|x| tag_matches(x, t)))
            })
            .filter(|m| folder.as_deref().is_none_or(|f| m.path.starts_with(f)))
            .cloned()
            .collect();
        out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
        out.truncate(limit);
        out
    }

    /// Tags by number of notes.
    pub fn tags(&self) -> Vec<TagCount> {
        let store = read(&self.store);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for note in store.notes.values() {
            for tag in &note.meta.tags {
                *counts.entry(tag).or_default() += 1;
            }
        }
        let mut out: Vec<TagCount> = counts
            .into_iter()
            .map(|(tag, count)| TagCount {
                tag: tag.to_string(),
                count,
            })
            .collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.tag.cmp(&b.tag)));
        out
    }

    /// A note by exact path or as a `[[link]]`: without `.md`, without the
    /// folder and case-insensitive.
    pub fn note(&self, path: &str) -> Result<Option<NoteDetail>> {
        let store = read(&self.store);
        let Some(note) = store
            .resolve("", path)
            .and_then(|real| store.notes.get(&real))
        else {
            return Ok(None);
        };
        let real = note.meta.path.clone();
        let links = note
            .links
            .iter()
            .map(|target| Link {
                target: target.clone(),
                path: store.resolve(&real, target),
            })
            .collect();
        let backlinks = store.backlinks(&real);
        let meta = note.meta.clone();
        let frontmatter = note.frontmatter.clone();
        drop(store);

        let full = mirror::safe_join(&self.vault_dir, &real)?;
        let content = match std::fs::read(&full) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", full.display())),
        };
        Ok(Some(NoteDetail {
            meta,
            frontmatter,
            links,
            backlinks,
            content,
        }))
    }
}

impl Store {
    fn add_file(&mut self, path: &str) {
        self.by_lower.insert(path.to_lowercase(), path.to_string());
        self.by_name
            .entry(file_name(path).to_lowercase())
            .or_default()
            .insert(path.to_string());
    }

    fn remove(&mut self, path: &str) {
        self.notes.remove(path);
        let lower = path.to_lowercase();
        if self.by_lower.get(&lower).is_some_and(|p| p == path) {
            self.by_lower.remove(&lower);
        }
        let name = file_name(&lower).to_string();
        if let Some(set) = self.by_name.get_mut(&name) {
            set.remove(path);
            if set.is_empty() {
                self.by_name.remove(&name);
            }
        }
    }

    /// Resolves a link target like Obsidian does: path relative to the note,
    /// path from the vault root and, finally, file name (the shallowest wins).
    /// Case-insensitive; `.md` is optional.
    fn resolve(&self, from: &str, target: &str) -> Option<String> {
        let mut candidates = vec![target.to_string()];
        if !target.to_lowercase().ends_with(".md") {
            candidates.push(format!("{target}.md"));
        }
        let dir = from.rsplit_once('/').map_or("", |(d, _)| d);
        for c in &candidates {
            let relative = (!dir.is_empty())
                .then(|| normalize_path(&format!("{dir}/{c}")))
                .flatten();
            for p in relative.into_iter().chain(normalize_path(c)) {
                if let Some(found) = self.by_lower.get(&p.to_lowercase()) {
                    return Some(found.clone());
                }
            }
        }
        for c in &candidates {
            let Some(lower) = normalize_path(c).map(|c| c.to_lowercase()) else {
                continue;
            };
            let suffix = format!("/{lower}");
            let found = self.by_name.get(file_name(&lower)).and_then(|paths| {
                paths
                    .iter()
                    .filter(|p| {
                        let p = p.to_lowercase();
                        p == lower || p.ends_with(&suffix)
                    })
                    .min_by_key(|p| (p.matches('/').count(), p.len()))
            });
            if let Some(found) = found {
                return Some(found.clone());
            }
        }
        None
    }

    fn backlinks(&self, target: &str) -> Vec<String> {
        self.notes
            .values()
            .filter(|n| n.meta.path != target)
            .filter(|n| {
                n.links
                    .iter()
                    .any(|l| self.resolve(&n.meta.path, l).as_deref() == Some(target))
            })
            .map(|n| n.meta.path.clone())
            .collect()
    }
}

/// Builds the catalog from the mirror, then applies the paths the syncer
/// sends, in batches.
pub async fn run_indexer(
    catalog: Arc<Catalog>,
    mut changed: UnboundedReceiver<String>,
    status: SharedStatus,
) {
    let c = catalog.clone();
    match tokio::task::spawn_blocking(move || c.rebuild()).await {
        Ok(Ok(n)) => info!(notes = n, "search index ready"),
        Ok(Err(e)) => warn!(error = %format!("{e:#}"), "failed to build the search index"),
        Err(e) => warn!(error = %e, "indexing aborted"),
    }
    status.write().await.notes_indexed = catalog.count();

    while let Some(first) = changed.recv().await {
        let mut paths = vec![first];
        while let Ok(p) = changed.try_recv() {
            paths.push(p);
        }
        paths.sort();
        paths.dedup();
        let c = catalog.clone();
        match tokio::task::spawn_blocking(move || c.refresh(&paths)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %format!("{e:#}"), "failed to update the index"),
            Err(e) => warn!(error = %e, "indexing aborted"),
        }
        status.write().await.notes_indexed = catalog.count();
    }
}

fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches('#').to_lowercase()
}

fn tag_matches(tag: &str, wanted: &str) -> bool {
    tag == wanted
        || tag
            .strip_prefix(wanted)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `a/b/c` becomes the facet `/a/b/c`. Facets index their ancestors, so
/// filtering by `/a` also matches `/a/b/c`.
fn facet(path: &str) -> Option<Facet> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| Facet::from_path(parts))
}

/// Resolves `.` and `..`. `None` if it escapes the vault root.
fn normalize_path(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn highlight(snippet: &Snippet) -> String {
    let fragment = snippet.fragment();
    let mut out = String::with_capacity(fragment.len() + 8);
    let mut last = 0;
    for range in snippet.highlighted() {
        out.push_str(&fragment[last..range.start]);
        out.push_str("**");
        out.push_str(&fragment[range.clone()]);
        out.push_str("**");
        last = range.end;
    }
    out.push_str(&fragment[last..]);
    collapse_whitespace(&out)
}

fn preview(body: &str) -> String {
    collapse_whitespace(&body.chars().take(SNIPPET_CHARS).collect::<String>())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(files: &[(&str, &str)]) -> (tempfile::TempDir, Catalog) {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        let catalog = Catalog::new(dir.path().to_path_buf()).unwrap();
        catalog.rebuild().unwrap();
        (dir, catalog)
    }

    fn paths(hits: &[SearchHit]) -> Vec<&str> {
        hits.iter().map(|h| h.path.as_str()).collect()
    }

    fn filter(tag: Option<&str>, folder: Option<&str>) -> Filter {
        Filter {
            tag: tag.map(str::to_string),
            folder: folder.map(str::to_string),
        }
    }

    #[test]
    fn accent_insensitive_stemmed_search_with_filters() {
        let (_dir, cat) = vault(&[
            (
                "Trabalho/Reunião.md",
                "---\ntags: [projeto/vault]\n---\nA reunião não terminou. Muitas notas.",
            ),
            (
                "Pessoal/Diário.md",
                "Hoje escrevi uma nota sobre a reunião. #pessoal",
            ),
            ("Pessoal/.oculto.md", "reunião escondida"),
            ("img.png", "binário"),
        ]);
        assert_eq!(cat.count(), 2);

        let all = cat.search("reuniao", &Filter::default(), 10).unwrap();
        assert_eq!(all.len(), 2);
        // The title matches too, so the "Reunião" note comes first.
        assert_eq!(all[0].path, "Trabalho/Reunião.md");
        assert!(all[0].snippet.contains("**reunião**"), "{}", all[0].snippet);

        let hits = cat.search("nao nota", &Filter::default(), 10).unwrap();
        assert_eq!(paths(&hits), vec!["Trabalho/Reunião.md"]);

        let by_tag = cat.search("reuniao", &filter(Some("#Projeto"), None), 10);
        assert_eq!(paths(&by_tag.unwrap()), vec!["Trabalho/Reunião.md"]);
        let by_folder = cat.search("reuniao", &filter(None, Some("Pessoal")), 10);
        assert_eq!(paths(&by_folder.unwrap()), vec!["Pessoal/Diário.md"]);
        assert!(cat
            .search("reuniao", &filter(Some("proj"), None), 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn links_backlinks_and_refresh() {
        let (dir, cat) = vault(&[
            (
                "a/Origem.md",
                "[[Destino]] [[sub/Outra|x]] [rel](../b/Rel.md) ![[foto.png]] [[Nada]]",
            ),
            ("a/sub/Outra.md", "volta para [[origem]]"),
            ("b/Destino.md", "# Destino\n"),
            ("b/Rel.md", "rel"),
            ("fotos/foto.png", "png"),
        ]);
        let note = cat.note("a/Origem.md").unwrap().unwrap();
        let resolved: Vec<_> = note.links.iter().map(|l| l.path.as_deref()).collect();
        assert_eq!(
            resolved,
            vec![
                Some("b/Destino.md"),
                Some("a/sub/Outra.md"),
                Some("fotos/foto.png"),
                None,
                // Markdown links come after the wikilinks on the same line.
                Some("b/Rel.md"),
            ]
        );
        assert_eq!(note.backlinks, vec!["a/sub/Outra.md"]);
        assert!(note.content.starts_with("[[Destino]]"));

        // Lookup as a link: no folder, no .md, any case.
        let dest = cat.note("destino").unwrap().unwrap();
        assert_eq!(dest.meta.path, "b/Destino.md");
        assert_eq!(dest.backlinks, vec!["a/Origem.md"]);

        std::fs::remove_file(dir.path().join("b/Destino.md")).unwrap();
        std::fs::write(dir.path().join("Nada.md"), "agora existe #nova").unwrap();
        cat.refresh(&["b/Destino.md".into(), "Nada.md".into()])
            .unwrap();
        assert!(cat.note("destino").unwrap().is_none());
        // Only the note that mentions "[[Destino]]" in its text is left.
        let hits = cat.search("destino", &Filter::default(), 10).unwrap();
        assert_eq!(paths(&hits), vec!["a/Origem.md"]);
        let note = cat.note("a/Origem.md").unwrap().unwrap();
        assert_eq!(note.links[0].path, None);
        assert_eq!(note.links[3].path.as_deref(), Some("Nada.md"));
        assert_eq!(
            cat.tags(),
            vec![TagCount {
                tag: "nova".into(),
                count: 1
            }]
        );
    }

    #[test]
    fn lists_by_recency_tag_and_folder() {
        let (dir, cat) = vault(&[
            ("x/velha.md", "#projeto/a"),
            ("x/nova.md", "#projeto"),
            ("y/outra.md", "#projetox"),
        ]);
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        std::fs::File::options()
            .write(true)
            .open(dir.path().join("x/velha.md"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        cat.refresh(&["x/velha.md".into()]).unwrap();

        let list = cat.list(&filter(Some("projeto"), None), 10);
        let got: Vec<_> = list.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(got, vec!["x/nova.md", "x/velha.md"]);
        assert_eq!(list[1].mtime, Some(1_000_000));
        assert_eq!(cat.list(&filter(None, Some("/y/")), 10).len(), 1);
        assert_eq!(cat.list(&Filter::default(), 1).len(), 1);
    }

    #[test]
    fn query_syntax() {
        let (_dir, cat) = vault(&[
            ("a.md", "# Café\nplano de café da manhã"),
            ("b.md", "# Manhã\nda manhã o plano de café"),
        ]);
        let search = |q| paths(&cat.search(q, &Filter::default(), 10).unwrap()).join(",");
        assert_eq!(search("\"cafe da manha\""), "a.md");
        assert_eq!(search("plano -\"da manha o\""), "a.md");
        assert_eq!(search("title:manha"), "b.md");
        // Unbalanced quotes are not an error.
        assert_eq!(
            cat.search("\"plano", &Filter::default(), 10).unwrap().len(),
            2
        );
    }

    #[test]
    fn path_resolution() {
        assert_eq!(normalize_path("a/./b/../c.md").as_deref(), Some("a/c.md"));
        assert_eq!(normalize_path("../fora.md"), None);
        assert!(tag_matches("a/b", "a"));
        assert!(!tag_matches("ab", "a"));
    }
}
