use std::sync::Arc;
use tiny_http::{Header, Response, Server, StatusCode};

use crate::embed::SEMANTIC_SUPPORTED;
use crate::error::Result;
use crate::manager::ArchiveManager;
use crate::search::SearchMode;

/// Runs the web server and article viewer across all managed archives.
pub fn run_server(manager: ArchiveManager, host: &str, port: u16, auto_index: bool) -> Result<()> {
    let addr = format!("{}:{}", host, port);
    let server = Server::http(&addr).map_err(|e| {
        crate::error::ZimSearchError::Other(format!("Failed to bind server to {}: {}", addr, e))
    })?;

    let manager = Arc::new(manager);

    println!("============================================================");
    println!("  ZIM Library Web Server");
    println!("  Listening at:    http://{}", addr);
    println!("  ZIM directory:   {}", manager.zim_dir().display());
    println!("  Index directory: {}", manager.index_dir().display());
    if let Ok(archives) = manager.list_archives() {
        println!("  Archives found:  {}", archives.len());
        for a in &archives {
            println!(
                "    - {} ({}, {})",
                a.name,
                a.title,
                if a.is_indexed {
                    "indexed"
                } else {
                    "not indexed"
                }
            );
        }
    }
    println!("============================================================");

    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let manager = Arc::clone(&manager);

        std::thread::spawn(move || {
            let (path, query) = if let Some(idx) = url.find('?') {
                (&url[..idx], &url[idx + 1..])
            } else {
                (url.as_str(), "")
            };

            if path == "/" || path == "/index.html" {
                let html = get_web_ui_html(library_supports_semantic(&manager));
                let response = Response::from_string(html).with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                        .unwrap(),
                );
                let _ = request.respond(response);
            } else if path == "/api/search" {
                handle_api_search(request, query, &manager, auto_index);
            } else if path == "/api/archives" {
                handle_api_archives(request, &manager);
            } else if path == "/api/info" {
                handle_api_info(request, &manager);
            } else if path == "/illustration" {
                handle_illustration(request, query, &manager);
            } else if path.starts_with("/raw/") || path.starts_with("/wiki/") {
                handle_article_or_asset(request, path, &manager);
            } else {
                let _ = request
                    .respond(Response::from_string("Not Found").with_status_code(StatusCode(404)));
            }
        });
    }

    Ok(())
}

fn handle_api_archives(request: tiny_http::Request, manager: &ArchiveManager) {
    match manager.list_archives() {
        Ok(archives) => {
            let json = serde_json::to_string(&archives).unwrap_or_else(|_| "[]".to_string());
            let response = Response::from_string(json).with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
            );
            let _ = request.respond(response);
        }
        Err(e) => {
            let err_json = format!(r#"{{"error":"{}"}}"#, e);
            let response = Response::from_string(err_json)
                .with_status_code(StatusCode(500))
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                );
            let _ = request.respond(response);
        }
    }
}

fn handle_api_search(
    request: tiny_http::Request,
    query_str: &str,
    manager: &ArchiveManager,
    auto_index: bool,
) {
    let mut q = String::new();
    let mut archive_target = None;
    let mut mode_str = String::new();
    let mut offset = 0usize;
    let mut limit = 10usize;

    for param in query_str.split('&') {
        if let Some((k, v)) = param.split_once('=') {
            let decoded_v = urlencoding::decode(v)
                .unwrap_or_else(|_| v.into())
                .to_string();
            match k {
                "q" => q = decoded_v,
                "archive" => {
                    if !decoded_v.trim().is_empty() {
                        archive_target = Some(decoded_v);
                    }
                }
                "mode" => mode_str = decoded_v,
                "offset" => {
                    if let Ok(o) = decoded_v.parse::<usize>() {
                        offset = o;
                    }
                }
                "limit" => {
                    if let Ok(l) = decoded_v.parse::<usize>() {
                        limit = l.clamp(1, 100);
                    }
                }
                _ => {}
            }
        }
    }

    if q.trim().is_empty() {
        let response =
            Response::from_string(r#"{"total_matches":0,"results":[],"has_more":false}"#)
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                );
        let _ = request.respond(response);
        return;
    }

    // Explicit but unavailable modes stay graceful: unparsable values degrade to the engine's
    // auto-detected mode, and a requested semantic/hybrid search on a vector-less index falls back
    // to BM25 inside `SearchEngine` (never a 500).
    let mode = mode_str.parse::<SearchMode>().ok();

    match manager.search(
        archive_target.as_deref(),
        &q,
        mode,
        offset,
        limit,
        auto_index,
    ) {
        Ok(results) => {
            let json = serde_json::to_string(&results).unwrap_or_else(|_| "{}".to_string());
            let response = Response::from_string(json).with_header(
                Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
            );
            let _ = request.respond(response);
        }
        Err(e) => {
            let err_json = format!(r#"{{"error":"{}"}}"#, e);
            let response = Response::from_string(err_json)
                .with_status_code(StatusCode(500))
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                );
            let _ = request.respond(response);
        }
    }
}

fn handle_api_info(request: tiny_http::Request, manager: &ArchiveManager) {
    let archives = manager.list_archives().unwrap_or_default();
    let total_archives = archives.len();
    let total_indexed = archives.iter().filter(|a| a.is_indexed).count();
    let total_entries: u32 = archives.iter().map(|a| a.total_entries).sum();

    let payload = serde_json::json!({
        "zim_dir": manager.zim_dir().display().to_string(),
        "index_dir": manager.index_dir().display().to_string(),
        "total_archives": total_archives,
        "total_indexed": total_indexed,
        "total_entries": total_entries,
        "archives": archives,
    });

    let json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let response = Response::from_string(json)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
    let _ = request.respond(response);
}

fn handle_illustration(request: tiny_http::Request, query_str: &str, manager: &ArchiveManager) {
    let mut archive_id = None;
    for param in query_str.split('&') {
        if let Some((k, v)) = param.split_once('=') {
            if k == "archive" {
                archive_id = Some(
                    urlencoding::decode(v)
                        .unwrap_or_else(|_| v.into())
                        .to_string(),
                );
            }
        }
    }

    let target_archive = if let Some(id) = archive_id {
        manager.get_archive(&id).ok()
    } else {
        manager
            .list_archives()
            .ok()
            .and_then(|list| list.first().and_then(|a| manager.get_archive(&a.name).ok()))
    };

    if let Some(arch) = target_archive {
        if let Some(img) = arch.get_illustration() {
            let response = Response::from_data(img)
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).unwrap());
            let _ = request.respond(response);
            return;
        }
    }

    let _ = request.respond(Response::empty(StatusCode(404)));
}

fn handle_article_or_asset(request: tiny_http::Request, path: &str, manager: &ArchiveManager) {
    let prefix = if path.starts_with("/raw/") {
        "/raw/"
    } else {
        "/wiki/"
    };
    let remainder = path.trim_start_matches(prefix);

    // Format is /wiki/<archive_name>/<article_path> OR /wiki/<article_path> (if 1 archive)
    let (archive_name, article_url) = if let Some((first, rest)) = remainder.split_once('/') {
        if manager.resolve_archive_path(first).is_ok() {
            (first, rest)
        } else {
            ("", remainder)
        }
    } else {
        ("", remainder)
    };

    let target_archive = if !archive_name.is_empty() {
        manager.get_archive(archive_name).ok()
    } else {
        // Fallback to first available archive
        manager
            .list_archives()
            .ok()
            .and_then(|list| list.first().and_then(|a| manager.get_archive(&a.name).ok()))
    };

    let Some(archive) = target_archive else {
        let _ = request
            .respond(Response::from_string("Archive not found").with_status_code(StatusCode(404)));
        return;
    };

    let decoded_url = urlencoding::decode(article_url).unwrap_or_else(|_| article_url.into());
    match archive.read_by_url(&decoded_url) {
        Ok(Some((mime, data))) => {
            let content_type = if mime.starts_with("text/") {
                format!("{mime}; charset=utf-8")
            } else {
                mime
            };
            let response = Response::from_data(data).with_header(
                Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap(),
            );
            let _ = request.respond(response);
        }
        _ => {
            let _ = request.respond(
                Response::from_string("Article or resource not found")
                    .with_status_code(StatusCode(404)),
            );
        }
    }
}

/// True when at least one indexed archive in the library carries dense semantic vectors **and** this
/// binary is able to query them.
///
/// Dense vectors are opt-in at index time, so `false` is the honest default for most libraries. The
/// on-disk answer comes from the per-archive `index_mode` marker already exposed by `/api/archives`
/// (`"Hybrid"` when a vector store exists, `"BM25"` when it does not), so rendering the page never
/// opens a Tantivy index nor constructs an embedder.
///
/// **Compile-time override:** in a binary built without the `semantic` Cargo feature
/// ([`ArchiveManager::semantic_supported`] == `false`, see `crate::embed::SEMANTIC_SUPPORTED`) the
/// result is always `false`, even when `vectors.meta.json` markers are lying around from an older
/// semantic build — such an index cannot be searched semantically by this binary, so the UI must not
/// advertise hybrid/semantic vector search.
fn library_supports_semantic(manager: &ArchiveManager) -> bool {
    if !manager.semantic_supported() {
        return false;
    }
    manager
        .list_archives()
        .unwrap_or_default()
        .iter()
        .any(|archive| index_mode_has_vectors(archive.index_mode.as_deref()))
}

/// Interpret the `index_mode` marker reported by [`ArchiveManager::list_archives`].
fn index_mode_has_vectors(index_mode: Option<&str>) -> bool {
    match index_mode {
        Some(mode) => {
            let mode = mode.to_ascii_lowercase();
            mode == "hybrid" || mode == "semantic"
        }
        None => false,
    }
}

/// Render the single-page web UI.
///
/// `semantic_available` decides which mode radio is preselected: `hybrid` only when some index
/// actually has dense vectors, otherwise `bm25`. An explicitly requested but unavailable mode is
/// still accepted by `/api/search`; it degrades to lexical search instead of failing.
///
/// The hint text names the *reason* vector search is unavailable, and distinguishes the two cases:
/// an index that was simply built without the semantic opt-in flag, versus a binary that was
/// compiled without the `semantic` Cargo feature and therefore has no vector search at all — the
/// rebuild command in that hint is spliced at runtime from
/// [`crate::embed::SEMANTIC_REBUILD_COMMAND`] (audit A11), not re-typed here. A feature-off build
/// never preselects hybrid, whatever the on-disk markers say.
fn get_web_ui_html(semantic_available: bool) -> String {
    let semantic_available = semantic_available && SEMANTIC_SUPPORTED;
    let (hybrid_checked, bm25_checked) = if semantic_available {
        (" checked", "")
    } else {
        ("", " checked")
    };
    let mode_hint = if !SEMANTIC_SUPPORTED {
        // Audit A11: the rebuild command is spliced at runtime from the single source of truth
        // (`crate::embed::SEMANTIC_REBUILD_COMMAND`) instead of being re-typed inside the literal.
        // A `&str` const cannot be spliced into another `&str` literal, so the arms are unified to
        // `String` and the composition happens here, at the point of use; the rendered hint is
        // byte-for-byte what it always was.
        format!(
            "Semantic / hybrid vector search is unavailable in this build: it needs a binary compiled with the 'semantic' feature (`{}`). It also needs an index built with the semantic opt-in flag — BM25 is preselected.",
            crate::embed::SEMANTIC_REBUILD_COMMAND
        )
    } else if semantic_available {
        "Semantic / hybrid need an index built with the semantic opt-in flag — at least one indexed archive here has dense vectors.".to_string()
    } else {
        "Semantic / hybrid need an index built with the semantic opt-in flag — these indexes are lexical-only, so BM25 is preselected.".to_string()
    };

    r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ZIM Library Search</title>
<style>
  :root {
    --bg: #0f172a;
    --card: #1e293b;
    --border: #334155;
    --text: #f8fafc;
    --muted: #94a3b8;
    --primary: #38bdf8;
    --primary-hover: #0284c7;
    --accent: #818cf8;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: system-ui, -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
    background-color: var(--bg);
    color: var(--text);
    padding: 2rem 1rem;
    line-height: 1.5;
  }
  .container { max-width: 960px; margin: 0 auto; }
  header { text-align: center; margin-bottom: 2rem; }
  h1 { font-size: 2.2rem; font-weight: 700; color: var(--primary); margin-bottom: 0.5rem; }
  .subtitle { color: var(--muted); font-size: 1rem; }
  .search-box {
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: 12px;
    padding: 1.5rem;
    margin-bottom: 2rem;
    box-shadow: 0 4px 20px rgba(0,0,0,0.3);
  }
  .input-group { display: flex; gap: 0.5rem; margin-bottom: 1rem; }
  input[type="text"] {
    flex: 1;
    background: #0b1120;
    border: 1px solid var(--border);
    color: var(--text);
    padding: 0.75rem 1rem;
    border-radius: 8px;
    font-size: 1.1rem;
    outline: none;
    transition: border-color 0.2s;
  }
  input[type="text"]:focus { border-color: var(--primary); }
  button {
    background: var(--primary);
    color: #0b1120;
    border: none;
    padding: 0.75rem 1.5rem;
    border-radius: 8px;
    font-weight: 600;
    font-size: 1rem;
    cursor: pointer;
    transition: background 0.2s;
  }
  button:hover { background: var(--primary-hover); }
  .controls { display: flex; justify-content: space-between; align-items: center; flex-wrap: wrap; gap: 1rem; color: var(--muted); font-size: 0.9rem; }
  .radio-group { display: flex; gap: 1rem; }
  .radio-group label { display: flex; align-items: center; gap: 0.3rem; cursor: pointer; }
  select {
    background: #0b1120;
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 6px 10px;
    outline: none;
  }
  .result-card {
    background: var(--card);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 1.25rem;
    margin-bottom: 1rem;
    transition: transform 0.15s, border-color 0.15s;
  }
  .result-card:hover { border-color: var(--primary); transform: translateY(-2px); }
  .result-title { font-size: 1.25rem; font-weight: 600; margin-bottom: 0.25rem; }
  .result-title a { color: var(--primary); text-decoration: none; }
  .result-title a:hover { text-decoration: underline; }
  .result-meta { font-size: 0.85rem; color: var(--muted); margin-bottom: 0.75rem; display: flex; gap: 0.75rem; align-items: center; }
  .result-snippet { font-size: 0.95rem; color: #cbd5e1; }
  .badge {
    display: inline-block;
    padding: 0.15rem 0.5rem;
    border-radius: 4px;
    background: #334155;
    color: #38bdf8;
    font-size: 0.8rem;
    font-family: monospace;
  }
  .archive-badge {
    background: #1e1b4b;
    color: #818cf8;
    border: 1px solid #3730a3;
  }
  #status { text-align: center; color: var(--muted); margin-top: 1.5rem; }
  .pagination-bar { display: flex; justify-content: center; gap: 1rem; margin-top: 1.5rem; }
</style>
</head>
<body>
<div class="container">
  <header>
    <h1>ZIM Library</h1>
    <div class="subtitle">Search &amp; Article Viewer</div>
  </header>
  <div class="search-box">
    <div class="input-group">
      <input type="text" id="query" placeholder="Search across all archives or ask a question..." autofocus>
      <button onclick="performSearch(0)">Search</button>
    </div>
    <div class="controls">
      <div>
        <label for="archive-select">Archive: </label>
        <select id="archive-select" onfocus="loadArchives()">
          <option value="">All Archives</option>
        </select>
      </div>
      <div class="radio-group">
        <label><input type="radio" name="mode" value="semantic"> Semantic</label>
        <label><input type="radio" name="mode" value="hybrid"__HYBRID_CHECKED__> Hybrid</label>
        <label><input type="radio" name="mode" value="bm25"__BM25_CHECKED__> BM25</label>
      </div>
      <div>
        <label for="limit">Limit: </label>
        <select id="limit">
          <option value="5">5</option>
          <option value="10" selected>10</option>
          <option value="20">20</option>
          <option value="50">50</option>
        </select>
      </div>
    </div>
    <div class="subtitle">__MODE_HINT__</div>
  </div>
  <div id="results"></div>
  <div id="status"></div>
  <div id="pagination" class="pagination-bar"></div>
</div>
<script>
let currentOffset = 0;
let currentQuery = '';

document.addEventListener('DOMContentLoaded', loadArchives);

document.getElementById('query').addEventListener('keydown', e => {
  if (e.key === 'Enter') performSearch(0);
});

async function loadArchives() {
  try {
    const res = await fetch('/api/archives');
    const archives = await res.json();
    const select = document.getElementById('archive-select');
    const currentVal = select.value;
    select.innerHTML = '<option value="">All Archives</option>';
    for (const a of archives) {
      const opt = document.createElement('option');
      opt.value = a.name;
      const status = a.is_indexed ? `indexed, ${a.chunk_count || a.total_entries} chunks` : 'not indexed';
      opt.textContent = `${a.title || a.name} (${status})`;
      select.appendChild(opt);
    }
    if (currentVal) select.value = currentVal;
  } catch (err) {
    console.error('Failed to load archives:', err);
  }
}

async function performSearch(offset = 0) {
  const q = document.getElementById('query').value.trim();
  if (!q) return;
  currentQuery = q;
  currentOffset = offset;

  const archive = document.getElementById('archive-select').value;
  const mode = document.querySelector('input[name="mode"]:checked').value;
  const limit = parseInt(document.getElementById('limit').value, 10);
  const statusDiv = document.getElementById('status');
  const resultsDiv = document.getElementById('results');
  const paginationDiv = document.getElementById('pagination');

  statusDiv.innerText = 'Searching...';
  if (offset === 0) resultsDiv.innerHTML = '';
  paginationDiv.innerHTML = '';

  const t0 = performance.now();
  try {
    let url = `/api/search?q=${encodeURIComponent(q)}&mode=${mode}&offset=${offset}&limit=${limit}`;
    if (archive) url += `&archive=${encodeURIComponent(archive)}`;

    const res = await fetch(url);
    const data = await res.json();
    const elapsed = Math.round(performance.now() - t0);

    if (data.error) {
      statusDiv.innerHTML = `<span style="color:#f87171">Error: ${data.error}</span>`;
      return;
    }

    const items = data.results || [];
    if (items.length === 0 && offset === 0) {
      statusDiv.innerText = `No results found in ${elapsed}ms.`;
      return;
    }

    statusDiv.innerText = `Showing results ${offset + 1}–${offset + items.length} of ${data.total_matches} matches (${elapsed}ms, ${mode.toUpperCase()} mode)`;

    resultsDiv.innerHTML = items.map(item => {
      const res = item.result || item;
      const archiveParam = encodeURIComponent(item.archive || '');
      const urlParam = encodeURIComponent(res.article_url || '');
      const articleLink = `/wiki/${archiveParam}/${urlParam}`;
      const scoreStr = typeof res.score === 'number' ? res.score.toFixed(4) : (res.score || '');

      return `
        <div class="result-card">
          <div class="result-title">
            <a href="${articleLink}" target="_blank">${res.title || res.article_url || 'Untitled'}</a>
          </div>
          <div class="result-meta">
            <span class="badge archive-badge">${item.archive_title || item.archive || ''}</span>
            <span class="badge">Score: ${scoreStr}</span>
            <span>${res.article_url || ''}</span>
          </div>
          <div class="result-snippet">${res.snippet || ''}</div>
        </div>
      `;
    }).join('');

    // Pagination buttons
    let paginationHtml = '';
    if (offset > 0) {
      paginationHtml += `<button onclick="performSearch(${Math.max(0, offset - limit)})">&larr; Previous</button>`;
    }
    if (data.has_more) {
      paginationHtml += `<button onclick="performSearch(${offset + limit})">Next &rarr;</button>`;
    }
    paginationDiv.innerHTML = paginationHtml;

  } catch (err) {
    statusDiv.innerHTML = `<span style="color:#f87171">Request failed: ${err}</span>`;
  }
}
</script>
</body>
</html>"#
        .replace("__HYBRID_CHECKED__", hybrid_checked)
        .replace("__BM25_CHECKED__", bm25_checked)
        .replace("__MODE_HINT__", &mode_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Library whose `index_mode` markers claim dense vectors, without needing a real ZIM file:
    /// `list_archives` only inspects the index markers (`meta.json` / `vectors.meta.json`).
    /// The `TempDir` is returned so the directories outlive the manager.
    fn manager_with_vector_markers() -> (ArchiveManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            ArchiveManager::new(dir.path().join("zims"), dir.path().join("indices")).unwrap();
        std::fs::create_dir_all(manager.zim_dir()).unwrap();
        std::fs::write(manager.zim_dir().join("first.zim"), b"placeholder").unwrap();

        let index_dir = manager.tantivy_path_for("first.zim");
        std::fs::create_dir_all(&index_dir).unwrap();
        std::fs::write(index_dir.join("meta.json"), b"{}").unwrap();
        std::fs::write(index_dir.join("vectors.meta.json"), b"{}").unwrap();

        (manager, dir)
    }

    #[test]
    fn index_mode_marker_recognises_vector_backed_indexes() {
        assert!(index_mode_has_vectors(Some("Hybrid")));
        assert!(index_mode_has_vectors(Some("hybrid")));
        assert!(index_mode_has_vectors(Some("Semantic")));
        assert!(!index_mode_has_vectors(Some("BM25")));
        assert!(!index_mode_has_vectors(Some("bm25")));
        assert!(!index_mode_has_vectors(None));
    }

    #[test]
    fn mode_radio_preselects_the_mode_that_actually_works() {
        let lexical = get_web_ui_html(false);
        assert!(lexical.contains(r#"<input type="radio" name="mode" value="bm25" checked>"#));
        assert!(lexical.contains(r#"<input type="radio" name="mode" value="hybrid">"#));
        assert!(lexical.contains(r#"<input type="radio" name="mode" value="semantic">"#));
        assert!(!lexical.contains(r#"value="semantic" checked"#));

        let vector_backed = get_web_ui_html(true);
        if SEMANTIC_SUPPORTED {
            assert!(vector_backed
                .contains(r#"<input type="radio" name="mode" value="hybrid" checked>"#));
            assert!(vector_backed.contains(r#"<input type="radio" name="mode" value="bm25">"#));
            assert!(!vector_backed.contains("cargo build --features semantic"));
        } else {
            // Defence in depth: even a `true` argument cannot preselect hybrid in a feature-off build.
            assert!(vector_backed.contains(r#"<input type="radio" name="mode" value="hybrid">"#));
            assert!(
                vector_backed.contains(r#"<input type="radio" name="mode" value="bm25" checked>"#)
            );
        }
        assert!(!vector_backed.contains(r#"value="semantic" checked"#));

        for html in [&lexical, &vector_backed] {
            assert!(!html.contains("__HYBRID_CHECKED__"));
            assert!(!html.contains("__BM25_CHECKED__"));
            assert!(!html.contains("__MODE_HINT__"));
            assert!(html.contains("semantic opt-in flag"));
        }
    }

    /// Feature-on build: on-disk vector markers do enable the hybrid preselection.
    #[cfg(feature = "semantic")]
    #[test]
    fn library_supports_semantic_follows_the_index_markers_with_the_feature() {
        let (manager, _tmp) = manager_with_vector_markers();
        assert!(manager.semantic_supported());
        assert!(library_supports_semantic(&manager));
        let html = get_web_ui_html(library_supports_semantic(&manager));
        assert!(html.contains(r#"<input type="radio" name="mode" value="hybrid" checked>"#));
    }

    /// Feature-off build: the UI must not advertise hybrid/semantic vector search, no matter what
    /// `vectors.meta.json` markers exist on disk, and the hint must name the build-time feature.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn library_supports_semantic_is_false_without_the_feature() {
        let (manager, _tmp) = manager_with_vector_markers();
        assert!(!manager.semantic_supported());
        // The archive really does carry a vector marker — the binary still cannot use it.
        let archives = manager.list_archives().unwrap();
        assert_eq!(archives.len(), 1);
        assert!(index_mode_has_vectors(archives[0].index_mode.as_deref()));
        assert!(
            !library_supports_semantic(&manager),
            "a feature-off binary must never advertise vector search"
        );

        let html = get_web_ui_html(library_supports_semantic(&manager));
        assert!(html.contains("cargo build --features semantic"));
        assert!(html.contains(r#"<input type="radio" name="mode" value="bm25" checked>"#));
        assert!(!html.contains(r#"value="hybrid" checked"#));
        assert!(!html.contains(r#"value="semantic" checked"#));
    }

    /// Audit A11 drift guard: the feature-off build-honesty hint rendered into the web UI must be
    /// composed from the single-source rebuild command
    /// [`crate::embed::SEMANTIC_REBUILD_COMMAND`], so rewording it without the command fails here
    /// instead of silently diverging from the CLI, MCP and `embed` surfaces. That hint text exists
    /// only in the `!SEMANTIC_SUPPORTED` arm, hence the same gate as the test above. No fixture and
    /// no network: `get_web_ui_html` is a pure string render.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn feature_off_web_ui_hint_quotes_the_canonical_rebuild_command() {
        let html = get_web_ui_html(false);
        assert!(
            html.contains("Semantic / hybrid vector search is unavailable in this build"),
            "the feature-off hint must still state why vector search is unavailable: {html}"
        );
        assert!(
            html.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "the web UI hint must quote the canonical command: {html}"
        );
        assert!(
            !html.contains("__MODE_HINT__"),
            "the placeholder must have been substituted: {html}"
        );
    }
}
