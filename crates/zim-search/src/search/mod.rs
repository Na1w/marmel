use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::archive::ZimArchive;
use crate::embed::{Embedder, SEMANTIC_SUPPORTED};
use crate::error::Result;
use crate::extract::{clean_html, extract_snippet};
use crate::index::tantivy_engine::TantivyEngine;

/// Operational search mode for queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    BM25,
    Semantic,
    Hybrid,
}

impl std::str::FromStr for SearchMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "bm25" | "keyword" | "lexical" => Ok(SearchMode::BM25),
            "semantic" | "vector" => Ok(SearchMode::Semantic),
            "hybrid" | "both" => Ok(SearchMode::Hybrid),
            other => Err(format!(
                "Invalid search mode '{}'. Allowed: bm25, semantic, hybrid",
                other
            )),
        }
    }
}

/// A ranked search result item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub rank: usize,
    pub chunk_id: u32,
    #[serde(default)]
    pub doc_id: u32,
    pub article_url: String,
    pub title: String,
    pub score: f32,
    pub snippet: String,
    pub full_text: String,
}

/// One-line notice emitted when an explicitly requested vector mode has to degrade to lexical
/// search. Dense vectors are opt-in at index time, so most indexes will not have any and a
/// `semantic`/`hybrid` request legitimately falls back to BM25.
///
/// The reason is stated honestly, and the possible reasons are distinguished:
/// * this build **does** have dense-vector support and the index simply carries no vectors
///   (re-index with the semantic opt-in flag), or
/// * this binary was compiled **without** the `semantic` Cargo feature, so it owns no built-in
///   embedder at all and could not query vectors even if they existed — only rebuilding with
///   `cargo build --features semantic` changes that. That wording describes the *binary's* normal
///   capability and stays useful, but it is suppressed when `embedder_available` is `true`: a
///   caller-injected ONNX-free embedder (`ArchiveManager::set_embedder`, typically `MockEmbedder`)
///   works in every configuration, so there the missing piece is the index, not the build.
///
/// This stays a notice either way: query-time vector unavailability is never a hard error.
///
/// The rebuild command inside the ONNX-free reason is spliced at runtime from the single source of
/// truth [`crate::embed::SEMANTIC_REBUILD_COMMAND`] (audit A11) rather than re-typed here; the
/// rendered line is byte-for-byte what it always was.
fn vector_mode_fallback_notice(requested: &str, embedder_available: bool) {
    eprintln!(
        "[SearchEngine] '{}' requested but no usable dense vectors are available ({}); falling back to BM25 lexical search.",
        requested, vector_mode_fallback_reason(embedder_available)
    );
}

/// The reason clause of [`vector_mode_fallback_notice`], split out only so a test can assert that
/// the emitted wording still quotes [`crate::embed::SEMANTIC_REBUILD_COMMAND`] (drift guard, audit
/// A11). Same predicate, same arm contents, same `(requested, embedder_available)` semantics as
/// before: the rebuild wording appears exactly when the binary owns no embedder at all
/// (`SEMANTIC_SUPPORTED == false` **and** no injected embedder), and the notice itself is still
/// fired solely from the vector-mode fallback paths.
fn vector_mode_fallback_reason(embedder_available: bool) -> String {
    if SEMANTIC_SUPPORTED || embedder_available {
        "semantic vectors are opt-in at index time".to_string()
    } else {
        format!(
            "this binary was built without the 'semantic' feature — rebuild with \
             `{}` — and dense vectors are also opt-in at index time",
            crate::embed::SEMANTIC_REBUILD_COMMAND
        )
    }
}

/// Search engine orchestrating lexical, semantic, and hybrid retrieval over disk-backed Tantivy.
pub struct SearchEngine {
    tantivy: Arc<TantivyEngine>,
    embedder: Option<Arc<dyn Embedder>>,
    archive: Option<Arc<ZimArchive>>,
}

impl SearchEngine {
    pub fn new(
        tantivy: Arc<TantivyEngine>,
        embedder: Option<Arc<dyn Embedder>>,
        archive: Option<Arc<ZimArchive>>,
    ) -> Self {
        Self {
            tantivy,
            embedder,
            archive,
        }
    }

    /// Alias for backwards compatibility.
    pub fn new_tantivy(
        tantivy: Arc<TantivyEngine>,
        embedder: Option<Arc<dyn Embedder>>,
        archive: Option<Arc<ZimArchive>>,
    ) -> Self {
        Self::new(tantivy, embedder, archive)
    }

    pub fn with_archive(mut self, archive: Arc<ZimArchive>) -> Self {
        self.archive = Some(archive);
        self
    }

    /// Whether the already-loaded index carries dense semantic vectors.
    ///
    /// Side-effect free capability probe: it only reports what is already loaded in memory, it never
    /// touches disk and never constructs an embedder. Dense vectors are **opt-in at index time**, so
    /// `false` is the normal state for most library indexes. Note that `true` only means the vector
    /// *files* exist — in a build without the `semantic` Cargo feature they cannot be queried,
    /// because no embedder exists to turn the query into a vector.
    pub fn has_vectors(&self) -> bool {
        self.tantivy.has_vectors()
    }

    /// Whether the dense vectors of this index can actually be queried by this engine: the index
    /// must carry vectors **and** an embedder must be loaded (built-in default or injected via
    /// [`crate::manager::ArchiveManager::set_embedder`]).
    pub fn vectors_usable(&self) -> bool {
        self.embedder.is_some() && self.tantivy.has_vectors()
    }

    /// Auto-detect the best search mode for the indices and embedder already loaded.
    ///
    /// Contract — auto-detection is decided by **capability**, never by the `semantic` Cargo
    /// feature flag:
    /// - [`SearchMode::Hybrid`] exactly when [`SearchEngine::vectors_usable`] holds, i.e. the index
    ///   carries dense vectors **and** an embedder is loaded (built-in default or injected via
    ///   [`crate::manager::ArchiveManager::set_embedder`]);
    /// - [`SearchMode::BM25`] otherwise — the default state of any index built without the semantic
    ///   opt-in flag, and of any engine that owns no embedder.
    ///
    /// Why the feature flag is deliberately *not* part of the predicate: a plain build without the
    /// `semantic` Cargo feature still ends up at BM25, because
    /// [`crate::manager::ArchiveManager::get_or_init_embedder`] fails there (no ONNX stack ⇒ the
    /// actionable "rebuild with `cargo build --features semantic`" error) and
    /// [`crate::manager::ArchiveManager::get_search_engine`] therefore hands this engine
    /// `embedder = None`. Leftover `vectors.meta.json` files from an older semantic build stay
    /// unusable in such a binary (no embedder ⇒ no query vector), exactly as before. What the flag
    /// must *not* do is veto Hybrid for an engine that does hold a working embedder: an injected
    /// ONNX-free embedder (`set_embedder`, typically `MockEmbedder`) is available in every build
    /// configuration, and [`crate::manager::ArchiveManager::ensure_all_indexed`] legitimately writes
    /// vectors with it, so refusing Hybrid there would strand vectors this engine can query.
    ///
    /// Callers that only need to advertise build capabilities (CLI hints, web UI defaults) should use
    /// [`SearchEngine::has_vectors`] or `crate::manager::ArchiveManager::semantic_supported`
    /// instead of inferring them from this mode.
    pub fn default_mode(&self) -> SearchMode {
        if self.vectors_usable() {
            SearchMode::Hybrid
        } else {
            SearchMode::BM25
        }
    }

    /// Perform a search using the specified or auto-detected mode.
    pub fn search(
        &self,
        query: &str,
        mode: Option<SearchMode>,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let active_mode = mode.unwrap_or_else(|| self.default_mode());
        match active_mode {
            SearchMode::BM25 => self.search_bm25(query, limit),
            SearchMode::Semantic => self.search_semantic(query, limit),
            SearchMode::Hybrid => self.search_hybrid(query, limit, 0.75),
        }
    }

    /// Pure lexical search via BM25.
    pub fn search_bm25(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let hits = self.tantivy.search(query, limit)?;
        let mut results = Vec::with_capacity(hits.len());
        for (rank, (chunk_id, doc_id, url, title, _chunk_idx, score)) in
            hits.into_iter().enumerate()
        {
            let (snippet, full_text) = if let Some(ref arch) = self.archive {
                if let Ok((_mime, bytes)) = arch.read_entry_bytes_by_index(doc_id) {
                    let text = String::from_utf8_lossy(&bytes);
                    let cleaned = clean_html(&text);
                    let snip = extract_snippet(&cleaned, query, 240);
                    (snip, cleaned)
                } else if let Ok(Some((_mime, bytes))) = arch.read_by_url(&url) {
                    let text = String::from_utf8_lossy(&bytes);
                    let cleaned = clean_html(&text);
                    let snip = extract_snippet(&cleaned, query, 240);
                    (snip, cleaned)
                } else {
                    (title.clone(), String::new())
                }
            } else {
                (title.clone(), String::new())
            };

            results.push(SearchResult {
                rank: rank + 1,
                chunk_id,
                doc_id,
                article_url: url,
                title,
                score,
                snippet,
                full_text,
            });
        }
        Ok(results)
    }

    /// Pure semantic search via dense vector cosine similarity.
    ///
    /// Query-time contract: this never fails because vectors are missing. If the index has no dense
    /// vectors, or no embedder is loaded to encode the query (which is the normal state of a binary
    /// built without the `semantic` feature), a one-line notice is emitted and the query is answered
    /// with BM25 instead.
    pub fn search_semantic(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        if !self.vectors_usable() {
            vector_mode_fallback_notice("semantic", self.embedder.is_some());
            return self.search_bm25(query, limit);
        }

        let emb = self.embedder.as_ref().unwrap();
        let query_vec = emb.embed_query(query)?;
        let hits = self.tantivy.vector_search(&query_vec, limit);
        let mut results = Vec::with_capacity(hits.len());
        for (rank, (chunk_id, doc_id, score)) in hits.into_iter().enumerate() {
            let (url, title) = if let Some(ref arch) = self.archive {
                arch.get_entry_meta_by_index(doc_id)
                    .unwrap_or_else(|_| (String::new(), format!("Doc #{}", doc_id)))
            } else {
                (String::new(), format!("Doc #{}", doc_id))
            };
            let (snippet, full_text) = if let Some(ref arch) = self.archive {
                if let Ok((_mime, bytes)) = arch.read_entry_bytes_by_index(doc_id) {
                    let text = String::from_utf8_lossy(&bytes);
                    let cleaned = clean_html(&text);
                    let snip = extract_snippet(&cleaned, query, 240);
                    (snip, cleaned)
                } else {
                    (title.clone(), String::new())
                }
            } else {
                (title.clone(), String::new())
            };

            results.push(SearchResult {
                rank: rank + 1,
                chunk_id,
                doc_id,
                article_url: url,
                title,
                score,
                snippet,
                full_text,
            });
        }
        Ok(results)
    }

    /// Hybrid search combining BM25 and Semantic Vector search using normalized score fusion.
    ///
    /// Falls back to pure BM25 (with the same one-line notice as the semantic path) when the index
    /// has no dense vectors or no embedder is available — vectors are opt-in at index time, and a
    /// binary built without the `semantic` feature has no embedder at all.
    pub fn search_hybrid(
        &self,
        query: &str,
        limit: usize,
        weight: f32,
    ) -> Result<Vec<SearchResult>> {
        if !self.tantivy.has_vectors() || self.embedder.is_none() {
            vector_mode_fallback_notice("hybrid", self.embedder.is_some());
            return self.search_bm25(query, limit);
        }

        let emb = self.embedder.as_ref().unwrap();
        let candidate_pool_size = (limit * 4).max(50);

        let bm25_hits = self.tantivy.search(query, candidate_pool_size)?;
        let query_vec = emb.embed_query(query)?;
        let vector_hits = self.tantivy.vector_search(&query_vec, candidate_pool_size);

        let vector_weight = weight.clamp(0.0, 1.0);
        let bm25_weight = 1.0 - vector_weight;

        let max_bm25 = bm25_hits.first().map(|h| h.5).unwrap_or(1.0).max(1e-4);

        let mut candidate_map: HashMap<u32, (u32, String, String, f32)> = HashMap::new();

        for (chunk_id, doc_id, score) in vector_hits {
            let norm_score = score.max(0.0) * vector_weight;
            let (url, title) = if let Some(ref arch) = self.archive {
                arch.get_entry_meta_by_index(doc_id)
                    .unwrap_or_else(|_| (String::new(), format!("Doc #{}", doc_id)))
            } else {
                (String::new(), format!("Doc #{}", doc_id))
            };
            candidate_map.insert(chunk_id, (doc_id, url, title, norm_score));
        }

        for (chunk_id, doc_id, url, title, _chunk_idx, score) in bm25_hits {
            let norm_score = (score / max_bm25).clamp(0.0, 1.0) * bm25_weight;
            candidate_map
                .entry(chunk_id)
                .and_modify(|entry| entry.3 += norm_score)
                .or_insert((doc_id, url, title, norm_score));
        }

        let mut fused: Vec<(u32, u32, String, String, f32)> = candidate_map
            .into_iter()
            .map(|(chunk_id, (doc_id, url, title, score))| (chunk_id, doc_id, url, title, score))
            .collect();

        fused.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
        fused.truncate(limit);

        let mut results = Vec::with_capacity(fused.len());
        for (rank, (chunk_id, doc_id, url, title, score)) in fused.into_iter().enumerate() {
            let (snippet, full_text) = if let Some(ref arch) = self.archive {
                if let Ok((_mime, bytes)) = arch.read_entry_bytes_by_index(doc_id) {
                    let text = String::from_utf8_lossy(&bytes);
                    let cleaned = clean_html(&text);
                    (extract_snippet(&cleaned, query, 240), cleaned)
                } else {
                    (title.clone(), String::new())
                }
            } else {
                (title.clone(), String::new())
            };

            results.push(SearchResult {
                rank: rank + 1,
                chunk_id,
                doc_id,
                article_url: url,
                title,
                score,
                snippet,
                full_text,
            });
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::MockEmbedder;
    use crate::index::tantivy_engine::TantivyEngine;
    use tantivy::doc;

    /// Create a one-document BM25 (Tantivy) index on disk and return its directory.
    fn indexed_dir() -> tempfile::TempDir {
        let temp_dir = tempfile::tempdir().unwrap();
        let (_index, mut writer, fields) =
            TantivyEngine::create_or_overwrite(temp_dir.path()).unwrap();
        writer
            .add_document(doc!(
                fields.chunk_id => 1u64,
                fields.doc_id => 10u64,
                fields.url => "article1.html",
                fields.title => "Rust Programming Language",
                fields.chunk_index => 0u64,
                fields.body => "Rust is a multi-paradigm programming language emphasizing performance and concurrency.",
            ))
            .unwrap();
        writer.commit().unwrap();
        temp_dir
    }

    /// Open a search engine over an existing index directory, with an optional embedder.
    fn engine_over(dir: &std::path::Path, embedder: Option<Arc<dyn Embedder>>) -> SearchEngine {
        SearchEngine::new(Arc::new(TantivyEngine::open(dir).unwrap()), embedder, None)
    }

    /// Build a BM25-only (dense-vector-less) search engine over a throwaway disk index.
    /// The `TempDir` is returned so the disk-backed index outlives the engine.
    fn lexical_only_engine() -> (SearchEngine, tempfile::TempDir) {
        let temp_dir = indexed_dir();
        (engine_over(temp_dir.path(), None), temp_dir)
    }

    /// Simulate an index that carries dense vectors — e.g. `vectors.meta.json` left over from an
    /// older semantic build — written through the ONNX-free vector file format only.
    fn write_stale_vectors(dir: &std::path::Path) {
        std::fs::write(
            dir.join("vectors.meta.json"),
            r#"{"model":"MockHashEmbedder","dim":4,"count":1,"lead_only":false}"#,
        )
        .unwrap();
        let mut bytes = Vec::new();
        for value in [1.0f32, 0.0, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        std::fs::write(dir.join("vectors.bin"), bytes).unwrap();
    }

    #[test]
    fn vectors_are_absent_unless_indexed_with_opt_in() {
        let (engine, _tmp) = lexical_only_engine();
        assert!(!engine.has_vectors());
        assert_eq!(engine.default_mode(), SearchMode::BM25);
    }

    #[test]
    fn explicit_vector_modes_degrade_to_bm25_without_error() {
        let (engine, _tmp) = lexical_only_engine();
        let lexical = engine.search_bm25("concurrency", 5).unwrap();
        assert_eq!(
            lexical.len(),
            1,
            "sanity: lexical search finds the document"
        );

        for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
            let results = engine.search("concurrency", Some(mode), 5).unwrap();
            assert_eq!(results.len(), lexical.len());
            assert_eq!(results[0].chunk_id, lexical[0].chunk_id);
            assert_eq!(results[0].doc_id, lexical[0].doc_id);
        }

        // Boundary: no matches still succeeds instead of erroring out.
        let empty = engine
            .search("definitely-not-present-token", Some(SearchMode::Hybrid), 5)
            .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn auto_detected_mode_searches_lexically() {
        let (engine, _tmp) = lexical_only_engine();
        let auto = engine.search("concurrency", None, 5).unwrap();
        assert_eq!(auto.len(), 1);
    }

    // ---------------------------------------------------------------------------------------
    // Feature-aware capability tests

    /// Feature-off build: `vectors.meta.json` from an older semantic build is reported by the pure
    /// capability probe, but it is **not usable** (no embedder exists), so auto-detection stays BM25.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn stale_vector_files_stay_bm25_without_the_feature() {
        let dir = indexed_dir();
        write_stale_vectors(dir.path());

        let engine = engine_over(dir.path(), None);
        assert!(
            engine.has_vectors(),
            "the probe still reports what is on disk"
        );
        assert!(
            !engine.vectors_usable(),
            "a binary without the 'semantic' feature owns no embedder, so vectors are unusable"
        );
        assert_eq!(engine.default_mode(), SearchMode::BM25);
    }

    /// Feature-off build: an explicitly requested vector mode over a vector-bearing index must
    /// degrade to BM25 gracefully — query time never hard-errors.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn explicit_vector_modes_stay_graceful_on_stale_vectors_without_the_feature() {
        let dir = indexed_dir();
        write_stale_vectors(dir.path());
        let engine = engine_over(dir.path(), None);

        let lexical = engine.search_bm25("concurrency", 5).unwrap();
        assert_eq!(
            lexical.len(),
            1,
            "sanity: lexical search finds the document"
        );
        for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
            let results = engine.search("concurrency", Some(mode), 5).unwrap();
            assert_eq!(
                results.len(),
                lexical.len(),
                "{mode:?} must answer with BM25 results"
            );
            assert_eq!(results[0].chunk_id, lexical[0].chunk_id);
        }
    }

    /// Feature-off build, **no embedder**: auto-detection must not advertise Hybrid. What decides it
    /// is the missing embedder, not the feature flag — in a plain feature-off build
    /// `ArchiveManager::get_or_init_embedder` fails, so the manager hands the engine `None` and
    /// leftover `vectors.meta.json` files stay unusable. The injection seam stays available to callers
    /// who ask for a vector mode explicitly, and with an injected embedder auto-detection does switch
    /// to Hybrid (see `injected_embedder_with_vector_files_autodetects_hybrid_in_both_builds`).
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn auto_mode_stays_bm25_without_an_embedder_without_the_feature() {
        let dir = indexed_dir();
        write_stale_vectors(dir.path());
        let engine = engine_over(dir.path(), None);
        assert!(
            !engine.vectors_usable(),
            "no embedder ⇒ no query vector ⇒ vectors unusable"
        );
        assert_eq!(engine.default_mode(), SearchMode::BM25);

        // The explicit seam still works: hybrid returns results, never an error.
        let seam_dir = indexed_dir();
        write_stale_vectors(seam_dir.path());
        let injected = engine_over(seam_dir.path(), Some(Arc::new(MockEmbedder::new(4))));
        let results = injected
            .search("concurrency", Some(SearchMode::Hybrid), 5)
            .unwrap();
        assert!(!results.is_empty());
    }

    /// **Both** build configurations: an injected ONNX-free embedder plus vector files on disk is a
    /// usable vector setup, so auto-detection answers Hybrid whether or not the `semantic` feature is
    /// compiled in — the decision is capability-based, matching
    /// `ArchiveManager::ensure_all_indexed`, which legitimately writes vectors through an injected
    /// embedder in either build. The control half (same kind of index, no embedder) stays BM25.
    /// `MockEmbedder` keeps this offline — `FastEmbedder` is never constructed by a test, so no model
    /// download.
    #[test]
    fn injected_embedder_with_vector_files_autodetects_hybrid_in_both_builds() {
        let dir = indexed_dir();
        write_stale_vectors(dir.path());
        let engine = engine_over(dir.path(), Some(Arc::new(MockEmbedder::new(4))));
        assert!(
            engine.has_vectors(),
            "the probe reports the vector files on disk"
        );
        assert!(
            engine.vectors_usable(),
            "embedder + vector files ⇒ the vectors are queryable in any configuration"
        );
        assert_eq!(engine.default_mode(), SearchMode::Hybrid);

        // Control: a vector-bearing index with no embedder at all stays BM25 in both configurations.
        let control_dir = indexed_dir();
        write_stale_vectors(control_dir.path());
        let without_embedder = engine_over(control_dir.path(), None);
        assert!(without_embedder.has_vectors());
        assert!(!without_embedder.vectors_usable());
        assert_eq!(without_embedder.default_mode(), SearchMode::BM25);
    }

    /// Feature-on build: vectors + embedder still auto-detect Hybrid (no regression). `MockEmbedder`
    /// keeps this offline — `FastEmbedder` is never constructed by a test, so no model download.
    #[cfg(feature = "semantic")]
    #[test]
    fn vector_bearing_index_with_embedder_still_autodetects_hybrid() {
        let dir = indexed_dir();
        write_stale_vectors(dir.path());
        let engine = engine_over(dir.path(), Some(Arc::new(MockEmbedder::new(4))));
        assert!(engine.has_vectors());
        assert!(engine.vectors_usable());
        assert_eq!(engine.default_mode(), SearchMode::Hybrid);
    }

    /// Audit A11 drift guard: the search-time fallback notice must quote the single-source rebuild
    /// command ([`crate::embed::SEMANTIC_REBUILD_COMMAND`]) instead of a hand-retyped copy of it.
    /// That wording exists only in the ONNX-free arm (`SEMANTIC_SUPPORTED == false` **and** no
    /// injected embedder), so the gate matches the capability tests above. Boundary half: when an
    /// embedder *is* loaded the notice must keep blaming the index, never the build.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn fallback_notice_without_the_feature_quotes_the_canonical_rebuild_command() {
        let reason = vector_mode_fallback_reason(false);
        assert!(
            reason.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "the fallback notice must quote the canonical command: {reason}"
        );
        assert!(
            reason.contains("opt-in at index time"),
            "the notice must still name the index-time opt-in: {reason}"
        );

        let with_embedder = vector_mode_fallback_reason(true);
        assert!(
            !with_embedder.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "an injected embedder means the index lacks vectors, not the build: {with_embedder}"
        );
        assert!(
            with_embedder.contains("opt-in at index time"),
            "the capability arm wording is unchanged: {with_embedder}"
        );
    }
}
