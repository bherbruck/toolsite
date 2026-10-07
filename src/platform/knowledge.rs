//! `search` and `fetch`: the two tools a connector that reads knowledge
//! expects, in the shape OpenAI documents for ChatGPT. Outside Developer
//! Mode, ChatGPT shows a connector only these two, so without them a site
//! connects and then has nothing to offer.
//!
//! `search(query)` names the apps and pages the caller may open, plus the
//! guide. `fetch(id)` returns one of them as readable text. Both answer with
//! `structured_content` and the same JSON as a text block, which is what the
//! shape asks for. Nothing here decides visibility: the host hands in the
//! slugs its caller may see, decided by the same rules as everywhere else.

use crate::{
    config::Config,
    content::store::{page_path, page_title, page_url, read_meta, read_notes, relative_time},
};
use serde::Serialize;

/// The most text one `fetch` hands back. A page is read for its words, not
/// its bundle.
const MAX_TEXT_BYTES: usize = 200 * 1024;
const MAX_RESULTS: usize = 20;
pub const GUIDE_ID: &str = "guide";
const GUIDE_TITLE: &str = "How toolsite works";
/// A query that mentions the platform itself also finds the guide.
const GUIDE_WORDS: [&str; 10] = [
    "toolsite", "publish", "handler", "schema", "access", "blob", "repository", "guide", "deploy", "mcp",
];

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchHit {
    /// What `fetch` takes: the page's slug, or `guide`.
    pub id: String,
    pub title: String,
    /// The page's public address, for a citation.
    pub url: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SearchOutput {
    pub results: Vec<SearchHit>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FetchOutput {
    pub id: String,
    pub title: String,
    /// The page's visible text, then the app's notes when there are any.
    pub text: String,
    pub url: String,
    /// Slug, access, when it changed, whether it has a handler, declared views.
    pub metadata: serde_json::Value,
}

fn guide_url(config: &Config) -> String {
    let base = config.base_url.as_deref().unwrap_or(&config.local_base);
    format!("{base}/guide")
}

/// Ranks `slugs` against `query`. Lower is better: a title or slug that
/// starts with the query, then one that contains it, then notes that do.
async fn score(config: &Config, slug: &str, needle: &str) -> Option<(u8, String)> {
    let title = match page_path(config, slug).await {
        Some(path) => page_title(&path).await,
        None => None,
    }
    .unwrap_or_else(|| slug.to_string());
    let lower_title = title.to_lowercase();
    let lower_slug = slug.to_lowercase();
    if needle.is_empty() || lower_title.starts_with(needle) || lower_slug.starts_with(needle) {
        return Some((0, title));
    }
    if lower_title.contains(needle) || lower_slug.contains(needle) {
        return Some((1, title));
    }
    let app = slug.split('/').next().unwrap_or(slug);
    if let Some(notes) = read_notes(config, app).await
        && notes.to_lowercase().contains(needle)
    {
        return Some((2, title));
    }
    None
}

/// The results for `query` among `visible`, the slugs the caller may open.
pub async fn search(config: &Config, query: &str, visible: &[String]) -> SearchOutput {
    let needle = query.trim().to_lowercase();
    let mut ranked: Vec<(u8, String, String)> = Vec::new();
    for slug in visible {
        if let Some((rank, title)) = score(config, slug, &needle).await {
            ranked.push((rank, slug.clone(), title));
        }
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut results: Vec<SearchHit> = ranked
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, slug, title)| SearchHit {
            url: page_url(config, &slug),
            id: slug,
            title,
        })
        .collect();
    let about_the_platform = needle
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| GUIDE_WORDS.iter().any(|g| word.starts_with(g)));
    if about_the_platform && results.len() < MAX_RESULTS {
        results.push(SearchHit {
            id: GUIDE_ID.to_string(),
            title: GUIDE_TITLE.to_string(),
            url: guide_url(config),
        });
    }
    SearchOutput { results }
}

/// The guide, as a document.
pub fn fetch_guide(config: &Config) -> FetchOutput {
    FetchOutput {
        id: GUIDE_ID.to_string(),
        title: GUIDE_TITLE.to_string(),
        text: crate::platform::scaffold::GUIDE.to_string(),
        url: guide_url(config),
        metadata: serde_json::json!({ "kind": "guide" }),
    }
}

/// One page the caller may open, as text. `None` when there is no page at
/// the slug; the caller has already decided the slug may be seen.
/// `manages` says whether the caller manages the app, which is what it
/// takes to see how its resident instance runs and why it last failed.
pub async fn fetch_page(config: &Config, slug: &str, manages: bool) -> Option<FetchOutput> {
    let path = page_path(config, slug).await?;
    let html = tokio::fs::read_to_string(&path).await.ok()?;
    let title = page_title(&path).await.unwrap_or_else(|| slug.to_string());
    let app = slug.split('/').next().unwrap_or(slug).to_string();
    let mut text = visible_text(&html);
    if let Some(notes) = read_notes(config, &app).await
        && !notes.trim().is_empty()
    {
        text.push_str("\n\nNotes kept with the app:\n");
        text.push_str(notes.trim());
    }
    if text.len() > MAX_TEXT_BYTES {
        let mut cut = MAX_TEXT_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    let meta = read_meta(config, &app).await;
    let modified = tokio::fs::metadata(&path)
        .await
        .ok()
        .and_then(|m| m.modified().ok())
        .map(relative_time);
    let mut views: Vec<String> = meta.queryable.clone();
    views.extend(meta.policies.iter().map(|p| p.view.clone()));
    Some(FetchOutput {
        id: slug.to_string(),
        title,
        text,
        url: page_url(config, slug),
        metadata: serde_json::json!({
            "slug": slug,
            "app": app,
            "access": crate::content::store::effective_gate(config, &app, "/").await.gate,
            "updated": modified,
            "has_handler": config.data_dir.join(&app).join("handler.wasm").is_file(),
            "views": views,
            "open_connections": config.connections.open(&app),
            "resident": meta.resident.filter(|_| manages).map(|_| config.residents.status(&app).unwrap_or_default()),
        }),
    })
}

/// The words of a page: scripts and styles dropped, tags removed, a few
/// entities decoded, whitespace collapsed. Enough to read, not a parser.
pub fn visible_text(html: &str) -> String {
    let lower = html.to_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip whole script and style elements, comments, and any tag.
            let rest = &lower[i..];
            let skip_to = if rest.starts_with("<script") {
                rest.find("</script>").map(|end| i + end + "</script>".len())
            } else if rest.starts_with("<style") {
                rest.find("</style>").map(|end| i + end + "</style>".len())
            } else if rest.starts_with("<!--") {
                rest.find("-->").map(|end| i + end + 3)
            } else {
                rest.find('>').map(|end| i + end + 1)
            };
            match skip_to {
                Some(end) => {
                    // Block-level boundaries become line breaks so headings
                    // and paragraphs do not run together.
                    if rest.starts_with("</p")
                        || rest.starts_with("</div")
                        || rest.starts_with("</h")
                        || rest.starts_with("</li")
                        || rest.starts_with("</tr")
                        || rest.starts_with("<br")
                    {
                        out.push('\n');
                    } else {
                        out.push(' ');
                    }
                    i = end;
                }
                None => break,
            }
        } else {
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    // Collapse runs of whitespace, keeping single line breaks.
    let mut text = String::with_capacity(decoded.len());
    let mut last_space = true;
    let mut last_newline = false;
    for ch in decoded.chars() {
        if ch == '\n' {
            if !last_newline {
                while text.ends_with(' ') {
                    text.pop();
                }
                text.push('\n');
            }
            last_newline = true;
            last_space = true;
        } else if ch.is_whitespace() {
            if !last_space {
                text.push(' ');
            }
            last_space = true;
        } else {
            text.push(ch);
            last_space = false;
            last_newline = false;
        }
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_of_a_page_is_its_words_and_not_its_markup() {
        let html = "<!doctype html><html><head><title>T</title><style>p{color:red}</style>\
                    <script>alert('x')</script></head><body><h1>Hello &amp; welcome</h1>\
                    <p>One   two</p><p>Three</p><!-- hidden --></body></html>";
        let text = visible_text(html);
        assert_eq!(text, "T\nHello & welcome\nOne two\nThree");
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn an_unclosed_tag_ends_the_text_rather_than_leaking_markup() {
        assert_eq!(visible_text("<p>fine</p><div class=\"open"), "fine");
    }
}
