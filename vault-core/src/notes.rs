//! Parses an Obsidian-style Markdown note: YAML frontmatter, title, tags,
//! aliases and links.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use percent_encoding::percent_decode_str;
use regex::Regex;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedNote {
    pub title: String,
    /// YAML object at the top of the note, or `Null`.
    pub frontmatter: Value,
    /// Lowercase, without `#`, sorted and deduplicated.
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    /// Link targets, without `#heading`, `|alias` or percent-encoding.
    pub links: Vec<String>,
    /// Text without the frontmatter.
    pub body: String,
}

static INLINE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)#([\p{L}\p{N}_/\-]+)").unwrap());
static WIKI_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\[\]\n]+?)\]\]").unwrap());
static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[^\]\n]*\]\(\s*(<[^>\n]+>|[^)\s]+)").unwrap());
static URL_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.\-]*:").unwrap());

pub fn parse(path: &str, text: &str) -> ParsedNote {
    let (yaml, body) = split_frontmatter(text);
    let frontmatter = yaml
        .and_then(|y| serde_yaml_ng::from_str::<Value>(y).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null);

    let mut tags: BTreeSet<String> = list_field(&frontmatter, &["tags", "tag"])
        .iter()
        .filter_map(|t| clean_tag(t))
        .collect();
    let aliases = list_field(&frontmatter, &["aliases", "alias"]);
    let mut links = Vec::new();
    let mut h1 = None;

    let mut fence: Option<&str> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
            fence = Some(marker);
            continue;
        }
        let line = strip_inline_code(line);
        if h1.is_none() {
            if let Some(h) = line.trim_start().strip_prefix("# ") {
                h1 = Some(h.trim().to_string());
            }
        }
        for cap in INLINE_TAG.captures_iter(&line) {
            tags.extend(clean_tag(&cap[1]));
        }
        for cap in WIKI_LINK.captures_iter(&line) {
            let inner = cap[1].replace("\\|", "|");
            let target = inner.split('|').next().unwrap_or_default();
            push_link(&mut links, target.split('#').next().unwrap_or_default());
        }
        for cap in MD_LINK.captures_iter(&line) {
            let raw = cap[1].trim_start_matches('<').trim_end_matches('>');
            if URL_SCHEME.is_match(raw) {
                continue; // http:, mailto:, obsidian: ...
            }
            let raw = raw.split(['#', '?']).next().unwrap_or_default();
            push_link(&mut links, &percent_decode_str(raw).decode_utf8_lossy());
        }
    }

    let title = frontmatter
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(h1)
        .unwrap_or_else(|| file_stem(path).to_string());

    ParsedNote {
        title,
        frontmatter,
        tags: tags.into_iter().collect(),
        aliases,
        links,
        body: body.to_string(),
    }
}

/// Splits the leading `---` block from the rest of the note.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// A frontmatter field that may be a list or a comma-separated string.
fn list_field(frontmatter: &Value, keys: &[&str]) -> Vec<String> {
    let Some(value) = keys.iter().find_map(|k| frontmatter.get(*k)) else {
        return Vec::new();
    };
    let items: Vec<String> = match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(s) => s.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Normalizes a tag. As in Obsidian, all-digit tags are not tags.
fn clean_tag(raw: &str) -> Option<String> {
    let tag = raw.trim().trim_start_matches('#').trim_matches('/');
    if tag.is_empty() || tag.chars().all(|c| c.is_ascii_digit() || c == '/') {
        return None;
    }
    if tag.contains(char::is_whitespace) {
        return None;
    }
    Some(tag.to_lowercase())
}

fn push_link(links: &mut Vec<String>, target: &str) {
    let target = target.trim();
    if !target.is_empty() && !links.iter().any(|l| l == target) {
        links.push(target.to_string());
    }
}

/// Replaces `inline code` with spaces: there are no tags or links inside it.
fn strip_inline_code(line: &str) -> String {
    if !line.contains('`') {
        return line.to_string();
    }
    line.split('`')
        .enumerate()
        .map(|(i, part)| if i % 2 == 0 { part } else { " " })
        .collect()
}

fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".md").unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frontmatter_title_tags_and_aliases() {
        let text = "---\ntitle: Planning\ntags: [Project/Vault, '#idea']\naliases: plan, roadmap\n---\n# Another title\nText with #todo and #2024 and #Project/Vault.\n";
        let n = parse("notes/plan.md", text);
        assert_eq!(n.title, "Planning");
        assert_eq!(n.tags, vec!["idea", "project/vault", "todo"]);
        assert_eq!(n.aliases, vec!["plan", "roadmap"]);
        assert_eq!(n.frontmatter["title"], json!("Planning"));
        assert!(n.body.starts_with("# Another title"));
    }

    #[test]
    fn title_falls_back_to_h1_then_file_name() {
        assert_eq!(parse("a/b.md", "text\n# Cabeçalho\n").title, "Cabeçalho");
        assert_eq!(parse("a/Reunião.md", "no title").title, "Reunião");
        // Invalid frontmatter does not break the note.
        let n = parse("x.md", "---\nkey: [unclosed\n---\nbody #ok");
        assert_eq!(n.frontmatter, Value::Null);
        assert_eq!(n.tags, vec!["ok"]);
    }

    #[test]
    fn wiki_markdown_and_embed_links() {
        let text = "See [[Note A]], [[folder/Note B#Heading|alias]], ![[img.png]], \
                    [[Table\\|alias]], [[#only heading]],\n\
                    [rel](../other%20note.md#x), [site](https://example.com), \
                    [mail](mailto:a@b.c), [[Note A]] again and [sp](<with space.md>).";
        let n = parse("x.md", text);
        assert_eq!(
            n.links,
            vec![
                "Note A",
                "folder/Note B",
                "img.png",
                "Table",
                "../other note.md",
                "with space.md"
            ]
        );
    }

    #[test]
    fn ignores_code_and_headings() {
        let text = "## Not a tag\n```\n#no [[NoLink]]\n```\nUse `#nothing [[x]]` but #yes\n~~~\n#also_no\n~~~\n";
        let n = parse("x.md", text);
        assert_eq!(n.tags, vec!["yes"]);
        assert!(n.links.is_empty());
    }
}
