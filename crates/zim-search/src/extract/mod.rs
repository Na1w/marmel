use serde::{Deserialize, Serialize};

/// Represents an individual indexed passage or chunk from an article.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunk {
    /// Global sequential chunk ID across the index.
    pub id: u32,
    /// Identifier of the parent article.
    pub doc_id: u32,
    /// Canonical article URL inside the ZIM archive.
    pub article_url: String,
    /// Article title.
    pub title: String,
    /// Chunk index within the document (0-based).
    pub chunk_index: u32,
    /// Cleaned plain text content of this chunk.
    pub text: String,
}

/// Convert HTML content to cleaned, normalized plain text with noise removal.
/// Uses standard semantic HTML5 tags (<main>, <article>, <footer>) and MediaWiki
/// content container IDs to strip navigation and boilerplate in a language-agnostic way.
pub fn clean_html(raw_html: &str) -> String {
    // Fast path: if there are no HTML tags, avoid running nanohtml2text parser
    if !raw_html.contains('<') {
        let mut clean_lines = Vec::new();
        let mut prev_empty = false;
        for line in raw_html.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if !prev_empty {
                    clean_lines.push("");
                    prev_empty = true;
                }
            } else {
                clean_lines.push(trimmed);
                prev_empty = false;
            }
        }
        return clean_lines.join("\n");
    }

    // 1. Isolate the core document content container if standard semantic tags are present
    let content_slice = if let Some(pos) = raw_html.find("<div id=\"mw-content-text\"") {
        &raw_html[pos..]
    } else if let Some(pos) = raw_html.find("<div id=\"bodyContent\"") {
        &raw_html[pos..]
    } else if let Some(pos) = raw_html.find("<div id=\"content\"") {
        &raw_html[pos..]
    } else if let Some(pos) = raw_html.find("<main") {
        &raw_html[pos..]
    } else if let Some(pos) = raw_html.find("<article") {
        &raw_html[pos..]
    } else {
        raw_html
    };

    // Cut off footer / navigation tail if present
    let trimmed_html = if let Some(pos) = content_slice.find("<footer") {
        &content_slice[..pos]
    } else if let Some(pos) = content_slice.find("<div id=\"footer") {
        &content_slice[..pos]
    } else if let Some(pos) = content_slice.find("<div id=\"mw-navigation\"") {
        &content_slice[..pos]
    } else {
        content_slice
    };

    // 2. Convert HTML tags to readable text
    let converted = nanohtml2text::html2text(trimmed_html);

    // 3. Normalize whitespace, remove repeated blank lines
    let mut clean_lines = Vec::new();
    let mut prev_empty = false;

    for line in converted.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_empty {
                clean_lines.push("");
                prev_empty = true;
            }
        } else {
            clean_lines.push(trimmed);
            prev_empty = false;
        }
    }

    clean_lines.join("\n")
}

/// Chunks text into overlapping windows for optimal vector embedding and retrieval.
/// Default: target around 120 words with 25 words overlap.
#[allow(clippy::too_many_arguments)]
pub fn chunk_text(
    doc_id: u32,
    url: &str,
    title: &str,
    text: &str,
    target_words: usize,
    overlap_words: usize,
    max_chunks_per_doc: Option<usize>,
    start_id: &mut u32,
) -> Vec<DocumentChunk> {
    if let Some(0) = max_chunks_per_doc {
        return Vec::new();
    }

    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }

    // If text is short, make a single chunk
    if words.len() <= target_words {
        let chunk = DocumentChunk {
            id: *start_id,
            doc_id,
            article_url: url.to_string(),
            title: title.to_string(),
            chunk_index: 0,
            text: words.join(" "),
        };
        *start_id += 1;
        return vec![chunk];
    }

    let mut chunks = Vec::new();
    let mut start_idx = 0;
    let mut chunk_idx = 0;
    let step = target_words.saturating_sub(overlap_words).max(1);

    while start_idx < words.len() {
        let end_idx = (start_idx + target_words).min(words.len());
        let chunk_words = &words[start_idx..end_idx];

        // Format chunk text, prefixing with title for semantic context
        let content = chunk_words.join(" ");

        chunks.push(DocumentChunk {
            id: *start_id,
            doc_id,
            article_url: url.to_string(),
            title: title.to_string(),
            chunk_index: chunk_idx,
            text: content,
        });

        *start_id += 1;
        chunk_idx += 1;

        if let Some(max_chunks) = max_chunks_per_doc {
            if chunk_idx >= max_chunks as u32 {
                break;
            }
        }

        if end_idx == words.len() {
            break;
        }
        start_idx += step;
    }

    chunks
}

/// Extracts a concise highlight snippet centered around matching query terms.
pub fn extract_snippet(text: &str, query: &str, max_chars: usize) -> String {
    let query_terms: Vec<String> = query
        .split_whitespace()
        .map(|s| {
            s.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|s| !s.is_empty())
        .collect();

    let text_lower = text.to_lowercase();

    // Find the first occurrence of any query term
    let mut best_pos = None;
    for term in &query_terms {
        if let Some(pos) = text_lower.find(term) {
            best_pos = Some(match best_pos {
                Some(prev) => std::cmp::min(prev, pos),
                None => pos,
            });
        }
    }

    let start_pos = best_pos.unwrap_or(0);
    let half_window = max_chars / 2;

    let safe_floor = |idx: usize| -> usize {
        let mut i = idx.min(text.len());
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };

    let safe_ceil = |idx: usize| -> usize {
        let mut i = idx.min(text.len());
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
        i
    };

    let raw_start = safe_floor(start_pos.saturating_sub(half_window));
    let window_start = if raw_start > 0 {
        text[raw_start..]
            .find(' ')
            .map(|p| safe_floor(raw_start + p + 1))
            .unwrap_or(raw_start)
    } else {
        0
    };

    let target_end = safe_ceil(window_start.saturating_add(max_chars));
    let window_end = if target_end < text.len() {
        let remaining = &text[window_start..target_end];
        remaining
            .rfind(' ')
            .map(|p| safe_floor(window_start + p))
            .unwrap_or(target_end)
    } else {
        text.len()
    };

    let snippet_slice = text[window_start..window_end].trim();
    let prefix = if window_start > 0 { "..." } else { "" };
    let suffix = if window_end < text.len() { "..." } else { "" };

    format!("{}{}{}", prefix, snippet_slice, suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_html() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b>!</p></body></html>";
        let cleaned = clean_html(html);
        assert!(cleaned.contains("Title"));
        assert!(cleaned.contains("Hello world!"));
    }

    #[test]
    fn test_chunk_text() {
        let text = "one two three four five six seven eight nine ten";
        let mut id = 0;
        let chunks = chunk_text(1, "test.html", "Test", text, 5, 2, None, &mut id);
        assert!(!chunks.is_empty());
        assert_eq!(chunks[0].title, "Test");

        let mut id2 = 0;
        let capped_chunks = chunk_text(1, "test.html", "Test", text, 5, 2, Some(1), &mut id2);
        assert_eq!(capped_chunks.len(), 1);
    }
}
