use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::archive::ZimArchive;
use crate::embed::{default_embedder, Embedder};
use crate::error::{Result, ZimSearchError};
use crate::extract::{clean_html, extract_snippet};
use crate::index::builder::IndexBuilder;
use crate::index::tantivy_engine::TantivyEngine;
use crate::search::{SearchEngine, SearchMode, SearchResult};

/// Compile-time capability flag, imported **privately** for use inside this module (and its tests).
/// It says whether *this binary* was compiled with dense-vector (ONNX) support at all. The public
/// surface is kept to exactly two paths — `crate::SEMANTIC_SUPPORTED` (crate root) and
/// `crate::embed::SEMANTIC_SUPPORTED` — so this module deliberately does **not** re-export it;
/// manager callers use [`ArchiveManager::semantic_supported`] instead.
use crate::embed::SEMANTIC_SUPPORTED;

/// Which archives receive dense semantic vectors. Opt-in: default is no vectors.
///
/// Semantic (vector) indexing never runs unless the caller explicitly asks for it:
/// [`SemanticScope::None`] (the default) means BM25 only, [`SemanticScope::All`] embeds every
/// indexed archive, and [`SemanticScope::Only`] embeds just the listed archives. Identifiers are
/// matched against an archive's exact filename, its file stem, or its archive `name`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SemanticScope {
    /// No vectors at all (default when the user does not opt in).
    #[default]
    None,
    /// Vectors for every indexed archive.
    All,
    /// Vectors only for these archive identifiers (filename, stem, or archive name).
    Only(Vec<String>),
}

/// Knobs for the semantic pass (replaces the unreachable `IndexBuilder::new()` defaults).
///
/// Every field maps straight onto an [`IndexBuilder`] setter, so callers can bound the cost of the
/// opt-in embedding pass (`lead_only`, `max_chunks`, `batch_size`) without touching the builder.
#[derive(Debug, Clone, Default)]
pub struct SemanticOptions {
    /// Embed only the lead chunk of each article instead of every chunk.
    pub lead_only: bool,
    /// Hard cap on the number of vectors emitted per archive.
    pub max_chunks: Option<usize>,
    /// Number of passages sent to the embedder per batch.
    pub batch_size: Option<usize>,
}

impl SemanticScope {
    /// True when the scope asks for any semantic vectors at all.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, SemanticScope::None)
    }
}

/// Case-sensitive identifier equivalence used for semantic scoping: two tokens refer to the same
/// archive when they match after an optional trailing `.zim` is stripped, which covers the exact
/// filename, the file stem, and the archive `name` forms accepted by `resolve_archive_path` and
/// reported by `list_archives`.
fn same_archive_token(identifier: &str, selector: &str) -> bool {
    let a = identifier.trim().trim_end_matches(".zim");
    let b = selector.trim().trim_end_matches(".zim");
    !a.is_empty() && !b.is_empty() && a == b
}

/// Actionable explanation for "this binary cannot build dense vectors because the ONNX inference
/// stack was never compiled into it", used by index-time errors in a feature-off build.
///
/// The wording is not duplicated by hand: it is the exact error produced by the single source of
/// truth, [`crate::embed::default_embedder`], which is guaranteed to fail in a build without the
/// `semantic` Cargo feature. A compile-time constant (`SEMANTIC_SUPPORTED`) decides which side of
/// that line the binary sits on — no runtime switch can add ONNX support afterwards.
fn unsupported_dense_vectors_error() -> ZimSearchError {
    match default_embedder() {
        // Only reachable with `--features semantic`, where this helper is never called.
        Ok(_) => ZimSearchError::Embedding(
            "dense vector embedding is unavailable in this build".to_string(),
        ),
        Err(err) => err,
    }
}

/// Information about an archive managed in a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedArchiveInfo {
    pub name: String,
    pub filename: String,
    pub zim_path: PathBuf,
    pub index_path: PathBuf,
    pub title: String,
    pub size_bytes: u64,
    pub total_entries: u32,
    pub is_indexed: bool,
    pub index_mode: Option<String>,
    pub chunk_count: Option<usize>,
}

/// A search hit aggregated across multiple archives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiSearchResult {
    pub archive: String,
    pub archive_title: String,
    #[serde(flatten)]
    pub result: SearchResult,
}

/// Paginated search results wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedSearchResults {
    pub total_matches: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub results: Vec<MultiSearchResult>,
}

/// Paginated article plain text or HTML content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedArticleContent {
    pub archive: String,
    pub url: String,
    pub title: String,
    pub total_chars: usize,
    pub offset_chars: usize,
    pub returned_chars: usize,
    pub has_more: bool,
    pub content: String,
}

/// Paginated list of directory entries/articles inside an archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedArticleList {
    pub archive: String,
    pub total_entries: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub entries: Vec<crate::archive::EntrySummary>,
}

/// Manages multiple ZIM archives in a directory and their corresponding Tantivy indexes.
pub struct ArchiveManager {
    zim_dir: PathBuf,
    index_dir: PathBuf,
    embedder: Arc<Mutex<Option<Arc<dyn Embedder>>>>,
    cached_archives: Mutex<HashMap<String, (SystemTime, Arc<ZimArchive>)>>,
    cached_engines: Mutex<HashMap<String, (SystemTime, Arc<SearchEngine>)>>,
    cached_archive_info: Mutex<HashMap<String, (SystemTime, SystemTime, ManagedArchiveInfo)>>,
}

impl ArchiveManager {
    /// Create a new manager with paths to the ZIM archive directory and the index directory.
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(zim_dir: P1, index_dir: P2) -> Result<Self> {
        let zim_dir = zim_dir.as_ref().to_path_buf();
        let index_dir = index_dir.as_ref().to_path_buf();

        if !index_dir.exists() {
            fs::create_dir_all(&index_dir)?;
        }

        Ok(Self {
            zim_dir,
            index_dir,
            embedder: Arc::new(Mutex::new(None)),
            cached_archives: Mutex::new(HashMap::new()),
            cached_engines: Mutex::new(HashMap::new()),
            cached_archive_info: Mutex::new(HashMap::new()),
        })
    }

    pub fn zim_dir(&self) -> &Path {
        &self.zim_dir
    }

    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// Inject a custom embedder (e.g. `MockEmbedder`) so the semantic pass and semantic search never
    /// require the default ONNX model download.
    ///
    /// The injected instance is returned by [`ArchiveManager::get_or_init_embedder`] and therefore
    /// used by every opt-in semantic indexing call and by semantic search on indexes that already
    /// carry vectors.
    ///
    /// This seam works in **both** build configurations: the whole dense-vector data plane
    /// (`IndexBuilder` → `DiskVectorIndex` → `TantivyEngine` → `SearchEngine`) only ever sees
    /// `Arc<dyn Embedder>`, which is ONNX-free. Injecting an embedder is therefore the documented
    /// way to exercise vector indexing/search in a binary built **without** the `semantic` feature —
    /// it does not, and cannot, add ONNX inference to such a binary.
    pub fn set_embedder(&self, embedder: Arc<dyn Embedder>) {
        *self.embedder.lock().unwrap() = Some(embedder);
    }

    /// Whether **this binary** was compiled with dense-vector (ONNX) support, i.e. whether the
    /// `semantic` Cargo feature was enabled at build time.
    ///
    /// This is a **compile-time** property, not a runtime knob: `false` means no ONNX inference code
    /// exists in the process, so the built-in embedder cannot be constructed and only
    /// BM25/Tantivy lexical search is available (plus any embedder a caller injects through
    /// [`ArchiveManager::set_embedder`]). No runtime option can turn it on — the binary has to be
    /// rebuilt with `cargo build --features semantic`.
    pub fn semantic_supported(&self) -> bool {
        SEMANTIC_SUPPORTED
    }

    /// Retrieve or lazily initialize the shared dense-vector embedder.
    ///
    /// Returns the embedder previously injected via [`ArchiveManager::set_embedder`] when present;
    /// otherwise delegates to the feature-aware factory [`crate::embed::default_embedder`]:
    /// * with `--features semantic` → the ONNX-backed `FastEmbedder` (first call may download
    ///   model weights),
    /// * without it → `Err(ZimSearchError::Embedding(..))` naming the missing feature and the exact
    ///   rebuild command. There is deliberately no silent no-op embedder: callers that can live
    ///   without vectors (search, `/api/search`) swallow the error, callers that need vectors
    ///   (indexing) propagate it.
    pub fn get_or_init_embedder(&self) -> Result<Arc<dyn Embedder>> {
        let mut guard = self.embedder.lock().unwrap();
        if let Some(ref emb) = *guard {
            return Ok(Arc::clone(emb));
        }

        let embedder: Arc<dyn Embedder> = default_embedder()?;
        *guard = Some(Arc::clone(&embedder));
        Ok(embedder)
    }

    /// Return expected index path for a given ZIM file stem.
    pub fn index_path_for(&self, filename_or_stem: &str) -> PathBuf {
        self.tantivy_path_for(filename_or_stem)
    }

    /// Return expected Tantivy index directory for a given ZIM file stem.
    pub fn tantivy_path_for(&self, filename_or_stem: &str) -> PathBuf {
        let clean_stem = filename_or_stem.trim_end_matches(".zim");
        self.index_dir.join(format!("{}.tantivy", clean_stem))
    }

    /// Scan `zim_dir` and list all `.zim` files and their status.
    pub fn list_archives(&self) -> Result<Vec<ManagedArchiveInfo>> {
        let mut archives = Vec::new();

        if !self.zim_dir.exists() {
            return Ok(archives);
        }

        for entry in fs::read_dir(&self.zim_dir)? {
            let entry = entry?;
            let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
            let path = entry.path();
            if is_file && path.extension().and_then(|s| s.to_str()) == Some("zim") {
                let filename = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let tantivy_path = self.tantivy_path_for(&filename);

                let entry_meta = entry.metadata().ok();
                let zim_mtime = entry_meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let (is_indexed, index_mtime) = match fs::metadata(tantivy_path.join("meta.json")) {
                    Ok(m) => (true, m.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
                    Err(_) => (false, SystemTime::UNIX_EPOCH),
                };

                // Fast in-memory cache lookup
                {
                    let guard = self.cached_archive_info.lock().unwrap();
                    if let Some((cached_zmtime, cached_imtime, info)) = guard.get(&filename) {
                        if *cached_zmtime == zim_mtime && *cached_imtime == index_mtime {
                            archives.push(info.clone());
                            continue;
                        }
                    }
                }

                let size_bytes = entry_meta.map(|m| m.len()).unwrap_or(0);
                let mut title = stem.replace(['_', '-'], " ");
                let mut total_entries = 0;
                let mut index_mode = None;
                let chunk_count = None;

                if is_indexed {
                    if tantivy_path.join("vectors.meta.json").exists() {
                        index_mode = Some("Hybrid".to_string());
                    } else {
                        index_mode = Some("BM25".to_string());
                    }
                } else {
                    let guard = self.cached_archives.lock().unwrap();
                    let key = path.to_string_lossy();
                    if let Some((_, arch)) = guard.get(key.as_ref()) {
                        let m = arch.metadata();
                        title = m.title;
                        total_entries = m.total_entries;
                    }
                }

                let info = ManagedArchiveInfo {
                    name: stem,
                    filename: filename.clone(),
                    zim_path: path,
                    index_path: tantivy_path,
                    title,
                    size_bytes,
                    total_entries,
                    is_indexed,
                    index_mode,
                    chunk_count,
                };

                self.cached_archive_info
                    .lock()
                    .unwrap()
                    .insert(filename, (zim_mtime, index_mtime, info.clone()));
                archives.push(info);
            }
        }

        archives.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(archives)
    }

    /// Build a disk-backed Tantivy full-text index for an archive.
    pub fn build_tantivy_for_archive(&self, identifier: &str, force: bool) -> Result<usize> {
        let path = self.resolve_archive_path(identifier)?;
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let tantivy_dir = self.tantivy_path_for(filename);
        let archive = self.get_archive(identifier)?;

        if !force && tantivy_dir.join("meta.json").exists() {
            return Ok(0);
        }

        if force && tantivy_dir.exists() {
            let _ = std::fs::remove_dir_all(&tantivy_dir);
        }

        let builder = IndexBuilder::new();
        let total_chunks = builder.build_tantivy_stage(&archive, &tantivy_dir)?;

        let key = path.to_string_lossy().to_string();
        self.cached_engines.lock().unwrap().remove(&key);

        Ok(total_chunks)
    }

    /// Populate disk-backed dense semantic vectors onto an existing Tantivy index for one archive.
    ///
    /// Opt-in only: the caller must decide that this archive gets vectors (see
    /// [`ArchiveManager::semantic_in_scope`]). Marker semantics are unchanged — `vectors.meta.json`
    /// means "done" (early `Ok(0)` unless `force`), `vectors.bin` / `vectors_refs.bin` are the resume
    /// checkpoint, and `force` deletes all three before re-running. `opts` is forwarded to the
    /// [`IndexBuilder`] instead of relying on its unreachable `IndexBuilder::new()` defaults.
    ///
    /// Embedder availability (index time = strict): the embedder comes from
    /// [`ArchiveManager::get_or_init_embedder`] — an injected instance (available in **both** build
    /// configurations) or the built-in default. In a build **without** the `semantic` Cargo feature
    /// the built-in default does not exist, so this returns the actionable
    /// `ZimSearchError::Embedding("… rebuild with `cargo build --features semantic` …")` rather than
    /// quietly leaving a BM25-only index behind.
    pub fn populate_tantivy_semantic_for_archive(
        &self,
        identifier: &str,
        force: bool,
        opts: &SemanticOptions,
    ) -> Result<usize> {
        let path = self.resolve_archive_path(identifier)?;
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let tantivy_dir = self.tantivy_path_for(filename);
        let archive = self.get_archive(identifier)?;

        if !force && tantivy_dir.join("vectors.meta.json").exists() {
            return Ok(0);
        }

        if force {
            let _ = std::fs::remove_file(tantivy_dir.join("vectors.bin"));
            let _ = std::fs::remove_file(tantivy_dir.join("vectors_refs.bin"));
            let _ = std::fs::remove_file(tantivy_dir.join("vectors.meta.json"));
        }

        let embedder = match self.get_or_init_embedder() {
            Ok(embedder) => embedder,
            // Index-time contract: the caller explicitly asked for dense vectors on this archive, so
            // a missing embedder is a hard, actionable error — never a silent "0 vectors written".
            // In a binary built without the `semantic` Cargo feature (and without an injected
            // embedder) this is always the "rebuild with `cargo build --features semantic`" error.
            Err(err) if !SEMANTIC_SUPPORTED => {
                eprintln!(
                    "[ArchiveManager] [Vectors] Cannot embed '{}': {}",
                    filename, err
                );
                return Err(unsupported_dense_vectors_error());
            }
            // Feature-on build: an ONNX failure (e.g. model weights unreachable while offline) is
            // propagated verbatim.
            Err(err) => return Err(err),
        };
        let mut builder = IndexBuilder::new().with_semantic_lead_only(opts.lead_only);
        if let Some(max_chunks) = opts.max_chunks {
            builder = builder.with_max_semantic_chunks(Some(max_chunks));
        }
        if let Some(batch_size) = opts.batch_size {
            builder = builder.with_batch_size(batch_size);
        }
        let count =
            builder.populate_tantivy_semantic_stage(&archive, &tantivy_dir, embedder.as_ref())?;

        let key = path.to_string_lossy().to_string();
        self.cached_engines.lock().unwrap().remove(&key);

        Ok(count)
    }

    /// Decide, purely and without touching the filesystem, whether an archive should receive dense
    /// semantic vectors under `scope`.
    ///
    /// Opt-in contract: always `false` for [`SemanticScope::None`], always `true` for
    /// [`SemanticScope::All`], and a case-sensitive membership test for [`SemanticScope::Only`], where
    /// each listed identifier may be an exact filename, a file stem, or an archive `name`.
    pub fn semantic_in_scope(&self, identifier: &str, scope: &SemanticScope) -> bool {
        match scope {
            SemanticScope::None => false,
            SemanticScope::All => true,
            SemanticScope::Only(selectors) => selectors
                .iter()
                .any(|selector| same_archive_token(identifier, selector)),
        }
    }

    /// Build and save the index of **exactly one** archive, addressed by identifier.
    ///
    /// `identifier` is resolved by [`ArchiveManager::resolve_archive_path`] — exact `.zim` filename,
    /// file stem, archive `name`, or title (case-insensitive there) — and no other archive in
    /// `zim_dir` is touched. Both stages are non-destructive (`force == false`): the BM25 stage is
    /// skipped when `<stem>.tantivy/meta.json` already exists and the semantic stage is skipped when
    /// `<stem>.tantivy/vectors.meta.json` already exists, so repeated calls are cheap and never
    /// re-embed.
    ///
    /// Returned count = BM25 chunks written by the full-text stage (`0` when that stage was skipped
    /// because the index already exists), **not** the number of vectors emitted by the semantic stage
    /// (use [`ArchiveManager::populate_tantivy_semantic_for_archive`] when the vector count matters).
    ///
    /// # Opt-in semantics
    /// * `semantic: false` → BM25/Tantivy only. The embedder is never constructed, so no model weights
    ///   are ever downloaded. This matches the crate-wide default: dense vectors are always opt-in.
    /// * `semantic: true` → "generate dense vectors for **THIS** archive". It is the library equivalent
    ///   of the CLI flag `--semantic-archive <name>` (one named source), and `opts` carries the cost
    ///   knobs the CLI derives from its flags (`lead_only`, `max_chunks`, `batch_size`).
    ///
    /// # Relationship to the per-source scoping rules
    /// `SemanticScope` / [`ArchiveManager::semantic_in_scope`] are deliberately **not** consulted here:
    /// passing `semantic: true` *is* the per-archive opt-in decision, so there is nothing left to scope.
    /// The batch entry point [`ArchiveManager::ensure_all_indexed`] is where scope rules apply — it runs
    /// the BM25 stage for every archive and then embeds exactly those archives `semantic_in_scope`
    /// selects. The equivalences are: `SemanticScope::All` ≈ `semantic: true` for every archive,
    /// `SemanticScope::Only(vec![id])` ≈ `semantic: true` for that one archive (identifiers are matched
    /// against filename, file stem, or archive name), and `SemanticScope::None` ≈ `semantic: false`.
    /// Library-wide callers should use `ensure_all_indexed`; "this archive, vectors please" callers use
    /// this function. The MCP `index_archive` tool follows the same single-archive shape.
    ///
    /// # Failure contract when `semantic: true`
    /// Never a silent BM25 downgrade:
    /// * Binary built **without** the `semantic` Cargo feature ([`crate::embed::SEMANTIC_SUPPORTED`] ==
    ///   `false`) and no injected embedder → **hard error, raised before any index work**: the actionable
    ///   [`crate::embed::default_embedder`] wording, i.e. `ZimSearchError::Embedding` with a message that
    ///   names the missing feature and the exact rebuild command
    ///   (`cargo build --features semantic`). Failing before the BM25 stage avoids burning a potentially
    ///   expensive full-text build on a run that can never embed, and keeps the message byte-identical to
    ///   the one `ensure_all_indexed` and
    ///   [`ArchiveManager::populate_tantivy_semantic_for_archive`] surface.
    /// * Binary built **with** the feature → an embedder failure (e.g. model weights unreachable while
    ///   offline) is propagated verbatim.
    /// * Documented exception, in either build: an embedder injected through
    ///   [`ArchiveManager::set_embedder`] *is* a working, ONNX-free embedder, so `semantic: true` runs
    ///   normally and no download happens.
    pub fn build_index(
        &self,
        identifier: &str,
        semantic: bool,
        opts: &SemanticOptions,
    ) -> Result<usize> {
        // Capability pre-flight, only reachable in a binary compiled without the `semantic` feature.
        // An explicit `semantic: true` can never succeed there unless the caller injected an ONNX-free
        // embedder, so report the missing feature up front instead of after a full BM25 build.
        if semantic && !SEMANTIC_SUPPORTED && self.get_or_init_embedder().is_err() {
            return Err(unsupported_dense_vectors_error());
        }

        let chunks = self.build_tantivy_for_archive(identifier, false)?;
        if semantic {
            self.populate_tantivy_semantic_for_archive(identifier, false, opts)?;
        }
        Ok(chunks)
    }

    /// Ensure all ZIM archives found in zim_dir have an index in two phases:
    /// Phase 1: Build Tantivy BM25 index across ALL archives first (always).
    /// Phase 2: Populate semantic vectors only when the caller opts in via `scope`, and only for the
    /// archives it selects — [`SemanticScope::None`] means no vectors at all,
    /// [`SemanticScope::All`] embeds every archive, [`SemanticScope::Only`] embeds the listed ones.
    /// Returns the number of archives that gained a new BM25 index in Phase 1.
    ///
    /// Index-time contract for a binary built **without** the `semantic` Cargo feature
    /// ([`crate::embed::SEMANTIC_SUPPORTED`] == `false`): an enabled scope is a **hard error**, never a silent
    /// skip. Phase 1 still runs to completion (lexical indexes stay usable), one stderr error line
    /// names the missing feature, and the actionable [`crate::embed::default_embedder`] error is
    /// returned so callers can tell the user how to rebuild. A caller-injected ONNX-free embedder
    /// via [`ArchiveManager::set_embedder`] is the documented way to keep Phase 2 functional in such
    /// a build.
    pub fn ensure_all_indexed(
        &self,
        scope: SemanticScope,
        opts: &SemanticOptions,
    ) -> Result<usize> {
        let archives = self.list_archives()?;
        let mut count = 0;

        // Phase 1: Build Tantivy disk index across all unindexed archives
        eprintln!(
            "[ArchiveManager] Checking indexes for {} archives...",
            archives.len()
        );
        for arch in &archives {
            if !arch.is_indexed {
                eprintln!(
                    "[ArchiveManager] [Tantivy] Full-text indexing '{}' ({})...",
                    arch.filename, arch.title
                );
                let t0 = std::time::Instant::now();
                let chunks = self.build_tantivy_for_archive(&arch.filename, false)?;
                eprintln!(
                    "[ArchiveManager] [Tantivy] Indexed '{}': {} chunks in {:.2}s",
                    arch.filename,
                    chunks,
                    t0.elapsed().as_secs_f32()
                );
                count += 1;
            }
        }

        // Phase 2: Populate semantic embeddings only when the caller opted in for this archive set
        if !scope.is_enabled() {
            eprintln!(
                "[ArchiveManager] [Phase 2/2] Skipped semantic vectors: dense vectors are opt-in (SemanticScope::None)."
            );
            return Ok(count);
        }

        // Compile-time capability gate for Phase 2. A binary built without the `semantic` Cargo
        // feature carries no ONNX inference stack whatsoever, so the built-in embedder simply does
        // not exist in it. Phase 1 above has already completed, so the honest behaviour is one
        // stderr error line plus the actionable rebuild error — not a silent "vectors done" skip.
        // (The CLI layer pre-empts this message, but the library must not lie to its other callers.)
        //
        // Documented exception: a caller-injected ONNX-free embedder (`set_embedder`, typically
        // `MockEmbedder`) *is* a working embedder, so Phase 2 legitimately runs in either build.
        if !SEMANTIC_SUPPORTED {
            match self.get_or_init_embedder() {
                Ok(embedder) => eprintln!(
                    "[ArchiveManager] [Phase 2/2] Notice: this binary was built without the 'semantic' feature; dense vectors will be produced by the injected embedder '{}' instead.",
                    embedder.model_name()
                ),
                Err(err) => {
                    eprintln!("[ArchiveManager] [Phase 2/2] ERROR: semantic vectors were requested, but {err}");
                    eprintln!(
                        "[ArchiveManager] [Phase 1/2] BM25/Tantivy indexing finished for {} archive(s) before this failure; Phase 2 (dense vectors) did not run.",
                        count
                    );
                    return Err(unsupported_dense_vectors_error());
                }
            }
        }

        if let SemanticScope::Only(selectors) = &scope {
            eprintln!(
                "[ArchiveManager] [Vectors] Semantic vectors limited to {} selected archive(s): {}",
                selectors.len(),
                selectors.join(", ")
            );
            for selector in selectors {
                let matched = archives.iter().any(|arch| {
                    same_archive_token(&arch.filename, selector)
                        || same_archive_token(&arch.name, selector)
                });
                if !matched {
                    eprintln!(
                        "[ArchiveManager] [Vectors] Notice: '{}' matched no archive in '{}'",
                        selector,
                        self.zim_dir.display()
                    );
                }
            }
        }

        eprintln!("[ArchiveManager] [Phase 2/2] Populating semantic vector embeddings for in-scope archives...");
        for arch in &archives {
            if !self.semantic_in_scope(&arch.filename, &scope) {
                continue;
            }
            let tantivy_path = self.tantivy_path_for(&arch.filename);
            if tantivy_path.join("meta.json").exists()
                && !tantivy_path.join("vectors.meta.json").exists()
            {
                eprintln!(
                    "[ArchiveManager] [Vectors] Generating embeddings for '{}' (Tantivy)...",
                    arch.filename
                );
                let t0 = std::time::Instant::now();
                self.populate_tantivy_semantic_for_archive(&arch.filename, false, opts)?;
                eprintln!(
                    "[ArchiveManager] [Vectors] Completed '{}' in {:.2}s",
                    arch.filename,
                    t0.elapsed().as_secs_f32()
                );
            }
        }

        Ok(count)
    }

    /// Resolve an archive identifier (can be exact filename, stem, or title) to a specific `.zim` path.
    pub fn resolve_archive_path(&self, identifier: &str) -> Result<PathBuf> {
        let direct = self.zim_dir.join(identifier);
        if direct.exists() && direct.is_file() {
            return Ok(direct);
        }

        let with_ext = self.zim_dir.join(format!("{}.zim", identifier));
        if with_ext.exists() && with_ext.is_file() {
            return Ok(with_ext);
        }

        // Search among available archives by title or stem
        for arch in self.list_archives()? {
            if arch.filename.eq_ignore_ascii_case(identifier)
                || arch.name.eq_ignore_ascii_case(identifier)
                || arch.title.eq_ignore_ascii_case(identifier)
            {
                return Ok(arch.zim_path);
            }
        }

        Err(ZimSearchError::Other(format!(
            "ZIM archive '{}' not found in directory '{}'",
            identifier,
            self.zim_dir.display()
        )))
    }

    /// Retrieve or lazily load a ZimArchive instance. Automatically reloads if file was modified on disk.
    pub fn get_archive(&self, identifier: &str) -> Result<Arc<ZimArchive>> {
        let path = self.resolve_archive_path(identifier)?;
        let key = path.to_string_lossy().to_string();

        let current_mtime = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        {
            let guard = self.cached_archives.lock().unwrap();
            if let Some((cached_mtime, arch)) = guard.get(&key) {
                if *cached_mtime >= current_mtime {
                    return Ok(Arc::clone(arch));
                }
            }
        }

        let archive = Arc::new(ZimArchive::open(&path)?);
        self.cached_archives
            .lock()
            .unwrap()
            .insert(key, (current_mtime, Arc::clone(&archive)));
        Ok(archive)
    }

    /// Fast path to retrieve or load a search engine when archive info and paths are already resolved.
    fn get_search_engine_for_indexed_archive(
        &self,
        arch: &ManagedArchiveInfo,
    ) -> Result<Arc<SearchEngine>> {
        let key = arch.zim_path.to_string_lossy().to_string();
        let tantivy_path = &arch.index_path;

        {
            let guard = self.cached_engines.lock().unwrap();
            if let Some((_, engine)) = guard.get(&key) {
                return Ok(Arc::clone(engine));
            }
        }

        let current_mtime = fs::metadata(tantivy_path.join("meta.json"))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let tantivy = TantivyEngine::open(tantivy_path)?;
        let embedder_opt = if tantivy.has_vectors() {
            self.get_or_init_embedder().ok()
        } else {
            None
        };

        let engine = Arc::new(SearchEngine::new(Arc::new(tantivy), embedder_opt, None));

        self.cached_engines
            .lock()
            .unwrap()
            .insert(key, (current_mtime, Arc::clone(&engine)));
        Ok(engine)
    }

    /// Retrieve or load a search engine for a given archive. Automatically reloads if index was modified on disk.
    pub fn get_search_engine(
        &self,
        identifier: &str,
        auto_index_if_missing: bool,
    ) -> Result<Arc<SearchEngine>> {
        let path = self.resolve_archive_path(identifier)?;
        let key = path.to_string_lossy().to_string();
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let tantivy_path = self.tantivy_path_for(filename);
        let meta_path = tantivy_path.join("meta.json");

        let meta_stat = fs::metadata(&meta_path).ok();
        let is_indexed = meta_stat.is_some();
        let current_mtime = meta_stat
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        if is_indexed {
            let guard = self.cached_engines.lock().unwrap();
            if let Some((cached_mtime, engine)) = guard.get(&key) {
                if *cached_mtime >= current_mtime {
                    return Ok(Arc::clone(engine));
                }
            }
        }

        if !is_indexed {
            if auto_index_if_missing {
                eprintln!(
                    "[ArchiveManager] Index missing for '{}'. Building disk-backed Tantivy full-text index...",
                    filename
                );
                self.build_tantivy_for_archive(identifier, false)?;
            } else {
                return Err(ZimSearchError::Other(format!(
                    "Archive '{}' is not indexed. Run `zim-search index` or pass auto_index.",
                    filename
                )));
            }
        }

        let tantivy = TantivyEngine::open(&tantivy_path)?;
        // Query-time contract (graceful, unlike index time): a stale `vectors.meta.json` from an
        // older semantic build must never break searching. If no embedder can be obtained — the
        // normal outcome in a binary built without the `semantic` feature — the engine is built
        // without one and `SearchEngine` degrades to BM25 with a one-line notice.
        let embedder_opt = if tantivy.has_vectors() {
            self.get_or_init_embedder().ok()
        } else {
            None
        };

        let engine = Arc::new(SearchEngine::new(Arc::new(tantivy), embedder_opt, None));

        self.cached_engines
            .lock()
            .unwrap()
            .insert(key, (current_mtime, Arc::clone(&engine)));
        Ok(engine)
    }

    /// Search across a specific archive or across ALL managed archives with offset and limit pagination.
    pub fn search(
        &self,
        identifier: Option<&str>,
        query: &str,
        mode: Option<SearchMode>,
        offset: usize,
        limit: usize,
        auto_index: bool,
    ) -> Result<PaginatedSearchResults> {
        let t_total = std::time::Instant::now();
        let fetch_limit = offset.saturating_add(limit);
        let mut aggregated = Vec::new();
        let mode_label = match mode {
            Some(SearchMode::BM25) => "BM25 (lexical)",
            Some(SearchMode::Semantic) => "Semantic (vector)",
            Some(SearchMode::Hybrid) => "Hybrid (BM25 + vector)",
            None => "Auto (BM25 default)",
        };

        if let Some(target) = identifier {
            eprintln!(
                "[Search] Query: \"{}\" | Target: '{}' | Mode: {}",
                query, target, mode_label
            );
            let t_query = std::time::Instant::now();
            let engine = self.get_search_engine(target, auto_index)?;
            let results = engine.search(query, mode, fetch_limit)?;
            eprintln!(
                "[Search] Index query on '{}' finished in {:.2}ms ({} candidate hit(s))",
                target,
                t_query.elapsed().as_secs_f64() * 1000.0,
                results.len()
            );

            let fallback_title = target.trim_end_matches(".zim").replace(['_', '-'], " ");
            for r in results {
                aggregated.push(MultiSearchResult {
                    archive: target.to_string(),
                    archive_title: fallback_title.clone(),
                    result: r,
                });
            }
        } else {
            let archives = self.list_archives()?;
            let mut indexed_archives: Vec<ManagedArchiveInfo> = archives
                .into_iter()
                .filter(|arch| arch.is_indexed)
                .collect();
            // Schedule largest archives first to avoid tail-latency stragglers in parallel execution
            indexed_archives.sort_by_key(|a| std::cmp::Reverse(a.size_bytes));

            let search_threads = std::env::var("RAYON_NUM_THREADS")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or_else(|| {
                    let cpus = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4);
                    std::cmp::min(cpus, 8)
                });

            let indexed_count = indexed_archives.len();
            eprintln!(
                "[Search] Query: \"{}\" | Target: All Archives ({} indexed) | Mode: {} | Threads: {}",
                query,
                indexed_count,
                mode_label,
                search_threads
            );

            let t_query = std::time::Instant::now();
            let query_archive_fn = |arch: ManagedArchiveInfo| -> Option<Vec<MultiSearchResult>> {
                let engine = self.get_search_engine_for_indexed_archive(&arch).ok()?;
                let results = engine.search(query, mode, fetch_limit).ok()?;
                if results.is_empty() {
                    return None;
                }
                Some(
                    results
                        .into_iter()
                        .map(|r| MultiSearchResult {
                            archive: arch.filename.clone(),
                            archive_title: arch.title.clone(),
                            result: r,
                        })
                        .collect(),
                )
            };

            let results_nested: Vec<Vec<MultiSearchResult>> = if let Ok(pool) =
                rayon::ThreadPoolBuilder::new()
                    .num_threads(search_threads)
                    .build()
            {
                pool.install(|| {
                    indexed_archives
                        .into_par_iter()
                        .filter_map(query_archive_fn)
                        .collect()
                })
            } else {
                indexed_archives
                    .into_par_iter()
                    .filter_map(query_archive_fn)
                    .collect()
            };

            let matching_archives = results_nested.len();
            for sublist in results_nested {
                aggregated.extend(sublist);
            }
            eprintln!(
                "[Search] Queried {} archive(s) in {:.2}ms: found {} candidate hit(s) across {} matching archive(s)",
                indexed_count,
                t_query.elapsed().as_secs_f64() * 1000.0,
                aggregated.len(),
                matching_archives
            );
        }

        // Sort by score descending across all archives
        aggregated.sort_by(|a, b| {
            b.result
                .score
                .partial_cmp(&a.result.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total_matches = aggregated.len();
        let mut page_results: Vec<MultiSearchResult> = aggregated
            .into_iter()
            .skip(offset)
            .take(limit)
            .enumerate()
            .map(|(i, mut item)| {
                item.result.rank = offset + i + 1;
                item
            })
            .collect();

        // Hydrate metadata and snippets for the final winning results on-demand
        if !page_results.is_empty() {
            let t_hydrate = std::time::Instant::now();
            let mut touched_archives = std::collections::HashSet::new();
            for item in &mut page_results {
                if let Ok(arch) = self.get_archive(&item.archive) {
                    touched_archives.insert(item.archive.clone());
                    if identifier.is_some() {
                        let meta_title = arch.metadata().title;
                        if !meta_title.is_empty() {
                            item.archive_title = meta_title;
                        }
                    }
                    if item.result.title.starts_with("Doc #") || item.result.article_url.is_empty()
                    {
                        if let Ok((u, t)) = arch.get_entry_meta_by_index(item.result.doc_id) {
                            item.result.article_url = u;
                            item.result.title = t;
                        }
                    }
                    if item.result.snippet == item.result.title
                        || item.result.snippet.is_empty()
                        || item.result.snippet.starts_with("Doc #")
                    {
                        if let Ok((_mime, bytes)) =
                            arch.read_entry_bytes_by_index(item.result.doc_id)
                        {
                            let text = String::from_utf8_lossy(&bytes);
                            let cleaned = clean_html(&text);
                            item.result.snippet = extract_snippet(&cleaned, query, 240);
                            item.result.full_text = cleaned;
                        } else if let Ok(Some((_mime, bytes))) =
                            arch.read_by_url(&item.result.article_url)
                        {
                            let text = String::from_utf8_lossy(&bytes);
                            let cleaned = clean_html(&text);
                            item.result.snippet = extract_snippet(&cleaned, query, 240);
                            item.result.full_text = cleaned;
                        }
                    }
                }
            }
            eprintln!(
                "[Search] Hydrated snippets for top {} result(s) from {} archive(s) in {:.2}ms (total: {:.2}ms)",
                page_results.len(),
                touched_archives.len(),
                t_hydrate.elapsed().as_secs_f64() * 1000.0,
                t_total.elapsed().as_secs_f64() * 1000.0
            );
        } else {
            eprintln!(
                "[Search] No matches found (total: {:.2}ms)",
                t_total.elapsed().as_secs_f64() * 1000.0
            );
        }

        let has_more = offset.saturating_add(limit) < total_matches;

        Ok(PaginatedSearchResults {
            total_matches,
            offset,
            limit,
            has_more,
            results: page_results,
        })
    }

    /// Read an article with character-based pagination to avoid overflowing LLM context.
    pub fn read_article_paginated(
        &self,
        identifier: &str,
        url: &str,
        raw: bool,
        offset_chars: usize,
        limit_chars: usize,
    ) -> Result<PaginatedArticleContent> {
        let archive = self.get_archive(identifier)?;
        let (mime, data) = archive.read_by_url(url)?.ok_or_else(|| {
            ZimSearchError::ArticleNotFound(format!("Article '{}' not found in archive", url))
        })?;

        let full_text = if raw || !mime.starts_with("text/html") {
            String::from_utf8_lossy(&data).to_string()
        } else {
            let html_str = String::from_utf8_lossy(&data);
            crate::clean_html(&html_str)
        };

        let total_chars = full_text.chars().count();
        let content: String = full_text
            .chars()
            .skip(offset_chars)
            .take(limit_chars)
            .collect();
        let returned_chars = content.chars().count();
        let has_more = offset_chars.saturating_add(returned_chars) < total_chars;

        let title = archive
            .list_entries(Some(url), 1)
            .ok()
            .and_then(|list| list.into_iter().next().map(|e| e.title))
            .unwrap_or_else(|| url.to_string());

        Ok(PaginatedArticleContent {
            archive: identifier.to_string(),
            url: url.to_string(),
            title,
            total_chars,
            offset_chars,
            returned_chars,
            has_more,
            content,
        })
    }

    /// List entries/articles inside an archive with offset and limit pagination.
    pub fn list_articles_paginated(
        &self,
        identifier: &str,
        filter: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<PaginatedArticleList> {
        let archive = self.get_archive(identifier)?;
        let max_fetch = offset.saturating_add(limit).max(500);
        let all_entries = archive.list_entries(filter, max_fetch)?;
        let total_entries = all_entries.len();

        let page_entries = all_entries.into_iter().skip(offset).take(limit).collect();
        let has_more = offset.saturating_add(limit) < total_entries;

        Ok(PaginatedArticleList {
            archive: identifier.to_string(),
            total_entries,
            offset,
            limit,
            has_more,
            entries: page_entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::MockEmbedder;

    fn manager() -> ArchiveManager {
        let dir = tempfile::tempdir().unwrap();
        ArchiveManager::new(dir.path().join("zims"), dir.path().join("indices")).unwrap()
    }

    #[test]
    fn semantic_scope_defaults_to_none() {
        assert_eq!(SemanticScope::default(), SemanticScope::None);
        assert!(!SemanticScope::default().is_enabled());
        assert!(SemanticScope::All.is_enabled());
        assert!(SemanticScope::Only(vec!["a.zim".to_string()]).is_enabled());
    }

    #[test]
    fn semantic_in_scope_none_is_never_in_scope() {
        let mgr = manager();
        assert!(!mgr.semantic_in_scope("wikipedia_en_all.zim", &SemanticScope::None));
        assert!(!mgr.semantic_in_scope("anything", &SemanticScope::None));
    }

    #[test]
    fn semantic_in_scope_all_is_always_in_scope() {
        let mgr = manager();
        assert!(mgr.semantic_in_scope("wikipedia_en_all.zim", &SemanticScope::All));
        assert!(mgr.semantic_in_scope("", &SemanticScope::All));
    }

    #[test]
    fn semantic_in_scope_only_matches_filename_stem_or_name() {
        let mgr = manager();
        let scope = SemanticScope::Only(vec!["wikipedia_en_all.zim".to_string()]);
        // exact filename, file stem, and archive name forms all resolve to the same archive
        assert!(mgr.semantic_in_scope("wikipedia_en_all.zim", &scope));
        assert!(mgr.semantic_in_scope("wikipedia_en_all", &scope));
        assert!(!mgr.semantic_in_scope("other_archive.zim", &scope));
    }

    #[test]
    fn semantic_in_scope_only_is_case_sensitive() {
        let mgr = manager();
        let scope = SemanticScope::Only(vec!["Wiki_en.zim".to_string()]);
        assert!(mgr.semantic_in_scope("Wiki_en.zim", &scope));
        assert!(!mgr.semantic_in_scope("wiki_en.zim", &scope));
    }

    #[test]
    fn semantic_in_scope_only_handles_empty_and_blank_selectors() {
        let mgr = manager();
        assert!(!mgr.semantic_in_scope("wikipedia_en_all.zim", &SemanticScope::Only(vec![])));
        let blank = SemanticScope::Only(vec![String::new(), "   ".to_string(), ".zim".to_string()]);
        assert!(!mgr.semantic_in_scope("wikipedia_en_all.zim", &blank));
    }

    #[test]
    fn semantic_options_default_is_off_and_unbounded() {
        let opts = SemanticOptions::default();
        assert!(!opts.lead_only);
        assert_eq!(opts.max_chunks, None);
        assert_eq!(opts.batch_size, None);
    }

    /// The `set_embedder` seam is the ONNX-free escape hatch and must work in **both** build
    /// configurations: an injected embedder always wins over the built-in default, so no model
    /// download ever happens in a test.
    #[test]
    fn set_embedder_is_returned_by_get_or_init_embedder() {
        let mgr = manager();
        mgr.set_embedder(Arc::new(MockEmbedder::new(32)));
        let emb = mgr.get_or_init_embedder().unwrap();
        assert_eq!(emb.model_name(), "MockHashEmbedder");
        assert_eq!(emb.dimension(), 32);
    }

    /// Compile-time capability report: `semantic_supported()` mirrors the Cargo feature and the
    /// free `SEMANTIC_SUPPORTED` constant, never a runtime state.
    #[test]
    fn semantic_supported_mirrors_the_compiled_feature() {
        let mgr = manager();
        assert_eq!(mgr.semantic_supported(), SEMANTIC_SUPPORTED);
        assert_eq!(mgr.semantic_supported(), cfg!(feature = "semantic"));
    }

    /// Feature-on build: every scope succeeds on an empty library and no embedder is ever built
    /// (Phase 2 iterates zero archives, so no ONNX model is downloaded by the test).
    #[cfg(feature = "semantic")]
    #[test]
    fn ensure_all_indexed_on_empty_dir_is_ok_for_every_scope() {
        let mgr = manager();
        let opts = SemanticOptions::default();
        assert!(mgr.semantic_supported());
        assert_eq!(
            mgr.ensure_all_indexed(SemanticScope::None, &opts).unwrap(),
            0
        );
        assert_eq!(
            mgr.ensure_all_indexed(SemanticScope::All, &opts).unwrap(),
            0
        );
        // unknown selector must only emit a notice, never an error
        let only = SemanticScope::Only(vec!["does_not_exist.zim".to_string()]);
        assert_eq!(mgr.ensure_all_indexed(only, &opts).unwrap(), 0);
    }

    /// Feature-off build: this binary carries no ONNX support at all.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn semantic_supported_is_false_without_the_feature() {
        const { assert!(!SEMANTIC_SUPPORTED) };
        assert!(!manager().semantic_supported());
    }

    /// Feature-off build: the default embedder factory must fail through the manager with an
    /// actionable error, instead of silently handing out a no-op embedder.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn get_or_init_embedder_reports_actionable_error_without_the_feature() {
        let mgr = manager();
        let msg = match mgr.get_or_init_embedder() {
            Ok(emb) => panic!(
                "get_or_init_embedder() must not succeed without the 'semantic' feature (got {})",
                emb.model_name()
            ),
            Err(ZimSearchError::Embedding(msg)) => msg,
            Err(other) => panic!("expected ZimSearchError::Embedding, got {other:?}"),
        };
        assert!(
            msg.contains("without the 'semantic' feature"),
            "error must name the missing feature: {msg}"
        );
        assert!(
            msg.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "error must name the rebuild command: {msg}"
        );
    }

    /// Feature-off build: asking for dense vectors is a hard, actionable error — the library must
    /// not pretend an embedding pass happened.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn ensure_all_indexed_reports_actionable_error_for_enabled_scope_without_the_feature() {
        let mgr = manager();
        let opts = SemanticOptions::default();

        for scope in [
            SemanticScope::All,
            SemanticScope::Only(vec!["does_not_exist.zim".to_string()]),
        ] {
            let err = match mgr.ensure_all_indexed(scope.clone(), &opts) {
                Ok(n) => panic!("{scope:?} must fail in a feature-off build, got Ok({n})"),
                Err(err) => err,
            };
            let msg = match err {
                ZimSearchError::Embedding(msg) => msg,
                other => panic!("expected ZimSearchError::Embedding, got {other:?}"),
            };
            assert!(
                msg.contains("without the 'semantic' feature")
                    && msg.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
                "error must be actionable: {msg}"
            );
            // No lie: the failed embedding pass must not leave vector markers behind.
            assert!(!mgr
                .index_path_for("does_not_exist")
                .join("vectors.meta.json")
                .exists());
        }

        // BM25-only runs are untouched by the gate: Phase 2 is skipped by design, successfully.
        assert_eq!(
            mgr.ensure_all_indexed(SemanticScope::None, &opts).unwrap(),
            0
        );
    }

    /// Feature-off build: the injected-embedder seam keeps Phase 2 usable (the dense-vector data
    /// plane is ONNX-free), so the capability gate must not fire for it.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn ensure_all_indexed_accepts_injected_embedder_without_the_feature() {
        let mgr = manager();
        mgr.set_embedder(Arc::new(MockEmbedder::new(64)));
        assert_eq!(
            mgr.ensure_all_indexed(SemanticScope::All, &SemanticOptions::default())
                .unwrap(),
            0,
            "empty library + injected embedder: Phase 2 runs and embeds nothing"
        );
    }

    /// Feature-off build: the manager must surface the exact same wording as the single source of
    /// truth (`crate::embed::default_embedder`) — guards against message drift.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn manager_and_factory_share_one_actionable_message() {
        let factory_msg = match default_embedder() {
            Ok(_) => panic!("default_embedder() must fail without the 'semantic' feature"),
            Err(err) => err.to_string(),
        };
        let manager_msg = match manager().get_or_init_embedder() {
            Ok(_) => panic!("get_or_init_embedder() must fail without the 'semantic' feature"),
            Err(err) => err.to_string(),
        };
        let index_msg = match manager().populate_tantivy_semantic_for_archive(
            "does_not_exist",
            false,
            &SemanticOptions::default(),
        ) {
            // Missing archive is checked first, so only compare when the embedder was reached.
            Ok(n) => panic!("populate must not succeed without the feature, got Ok({n})"),
            Err(err) => err.to_string(),
        };
        assert_eq!(factory_msg, manager_msg);
        assert!(
            factory_msg.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "the shared wording must quote the canonical command: {factory_msg}"
        );
        // `populate_tantivy_semantic_for_archive` fails earlier (unknown archive) — that is fine,
        // the point is it never silently returns Ok.
        assert!(index_msg.contains("does_not_exist"));
    }

    /// Audit A11 drift guard: the manager's feature-off hard error is not re-worded by hand — it is
    /// the factory error, and therefore must keep carrying the canonical rebuild command verbatim.
    /// A reword that drops the command has to fail here, not in the field.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn manager_hard_error_quotes_the_canonical_rebuild_command() {
        let msg = unsupported_dense_vectors_error().to_string();
        assert!(
            msg.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "hard error must quote the canonical command: {msg}"
        );
        assert!(
            msg.contains("without the 'semantic' feature"),
            "hard error must name the missing feature: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // `build_index` — the specified single-archive public entry point
    // ------------------------------------------------------------------

    /// Identifier guaranteed to be absent from the private temp libraries used below, so any failure
    /// observed is the ordinary archive-lookup error rather than something incidental.
    const MISSING_ARCHIVE: &str = "no_such_archive_in_this_library";

    /// Absolute archive-library directory the fixture scan tries first — the same location the CLI
    /// falls back to (`src/main.rs::DEFAULT_ZIM_DIR`). The CWD-relative `kiwix_dir` symlink that
    /// used to sit next to the crate is gone, so scanning that name would only ever find nothing.
    const ZIM_DIR_DEFAULT: &str = "/mnt/extradisk/kiwix";

    /// Locate an optional `.zim` fixture the same way the integration harness does
    /// (`tests/semantic_opt_in.rs`): `$ZIM_TEST_FILE` first, then the first `*.zim` inside
    /// the archive-library directory (`/mnt/extradisk/kiwix`), then the crate directory.
    /// Never downloads anything.
    fn locate_fixture() -> Option<PathBuf> {
        if let Some(raw) = std::env::var_os("ZIM_TEST_FILE") {
            let candidate = PathBuf::from(&raw);
            if candidate.is_file() {
                return Some(candidate);
            }
            eprintln!(
                "[manager tests] ZIM_TEST_FILE='{}' is not a readable file; falling back to a directory scan.",
                candidate.display()
            );
        }
        first_zim_in(Path::new(ZIM_DIR_DEFAULT)).or_else(|| first_zim_in(Path::new(".")))
    }

    /// First `*.zim` file directly inside `dir`, chosen deterministically (sorted).
    fn first_zim_in(dir: &Path) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
            .ok()?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("zim")
            })
            .collect();
        candidates.sort();
        candidates.into_iter().next()
    }

    /// Fixture gate: the fixture path, or `None` after printing a skip notice. A missing fixture is
    /// never a failure and never triggers a download.
    fn fixture_or_skip(test: &str) -> Option<PathBuf> {
        match locate_fixture() {
            Some(path) => {
                eprintln!(
                    "[manager tests] {test}: using fixture '{}'.",
                    path.display()
                );
                Some(path)
            }
            None => {
                eprintln!(
                    "SKIP {test}: no .zim fixture reachable (set ZIM_TEST_FILE=/path/to/archive.zim, \
                     or place a *.zim in /mnt/extradisk/kiwix) — fixture-gated assertions skipped; \
                     nothing was downloaded and the suite is not failed."
                );
                None
            }
        }
    }

    /// Stage a real fixture as `staged_name` inside a private library directory and hand back a
    /// manager over it. Hard links are tried first (Kiwix archives are multi-gigabyte), a copy is the
    /// fallback. The caller must keep the returned `TempDir` alive.
    fn staged_manager(fixture: &Path, staged_name: &str) -> (tempfile::TempDir, ArchiveManager) {
        let lib = tempfile::tempdir().expect("temp library dir");
        let zim_dir = lib.path().join("zims");
        fs::create_dir_all(&zim_dir).expect("create staged library dir");
        let target = zim_dir.join(staged_name);
        if fs::hard_link(fixture, &target).is_err() {
            fs::copy(fixture, &target).unwrap_or_else(|err| {
                panic!(
                    "cannot stage fixture '{}' as {}: {err}",
                    fixture.display(),
                    target.display()
                )
            });
        }
        let mgr = ArchiveManager::new(&zim_dir, lib.path().join("indices"))
            .expect("manager over staged library");
        (lib, mgr)
    }

    /// `semantic: false` ⇒ BM25/Tantivy only, never an error and never a vector write.
    ///
    /// Runs in **both** build configurations. The first half needs no fixture: an absent archive must
    /// fail with the plain lookup error and *not* with an embedding error, which is what proves the
    /// vector pass was never entered. The fixture-gated half then asserts the documented on-disk
    /// markers and the idempotent second call.
    #[test]
    fn build_index_with_semantic_false_is_bm25_only_and_never_touches_the_vector_pass() {
        let mgr = manager();
        let opts = SemanticOptions::default();

        let msg = match mgr.build_index(MISSING_ARCHIVE, false, &opts) {
            Ok(n) => panic!("a missing archive must not index successfully (got Ok({n}))"),
            Err(ZimSearchError::Embedding(msg)) => panic!(
                "semantic: false must never surface an embedding error (BM25-only path): {msg}"
            ),
            Err(other) => other.to_string(),
        };
        assert!(
            msg.contains(MISSING_ARCHIVE),
            "the failure must be the archive-lookup error: {msg}"
        );
        assert!(
            !mgr.index_path_for(MISSING_ARCHIVE)
                .join("vectors.meta.json")
                .exists(),
            "semantic: false must never leave vector markers behind"
        );

        let fixture = match fixture_or_skip(
            "build_index_with_semantic_false_is_bm25_only_and_never_touches_the_vector_pass",
        ) {
            Some(path) => path,
            None => return,
        };
        let (_lib, mgr) = staged_manager(&fixture, "bm25_only.zim");
        let stem = "bm25_only";
        let index_dir = mgr.index_path_for(stem);

        let chunks = mgr
            .build_index(stem, false, &opts)
            .expect("the BM25 stage must succeed for a real archive");
        assert!(chunks > 0, "the BM25 stage must emit chunks, got {chunks}");
        assert!(
            index_dir.join("meta.json").exists(),
            "the BM25 marker must exist after semantic: false"
        );
        assert!(
            !index_dir.join("vectors.meta.json").exists()
                && !index_dir.join("vectors.bin").exists(),
            "semantic: false must not emit any vector artefact"
        );
        let listed = mgr.list_archives().expect("listing the staged library");
        let info = listed
            .iter()
            .find(|arch| arch.name == stem)
            .expect("staged archive is listed");
        assert!(info.is_indexed);
        assert_eq!(
            info.index_mode.as_deref(),
            Some("BM25"),
            "a semantic: false index must be reported as BM25-only"
        );

        // Idempotent + still non-embedding: the second call skips the finished BM25 stage.
        assert_eq!(
            mgr.build_index(stem, false, &opts).unwrap(),
            0,
            "a repeat call must skip the existing BM25 stage"
        );
        assert!(!index_dir.join("vectors.meta.json").exists());
    }

    /// Feature-ON build: `semantic: true` means "embed THIS archive" and must actually reach the
    /// vector pass — through the injected `MockEmbedder`, so no ONNX model is ever downloaded by a test.
    #[cfg(feature = "semantic")]
    #[test]
    fn build_index_semantic_true_in_a_feature_on_build_attempts_vectors_without_downloading() {
        let mgr = manager();
        mgr.set_embedder(Arc::new(MockEmbedder::new(64)));
        assert!(mgr.semantic_supported());
        let opts = SemanticOptions {
            lead_only: true,
            max_chunks: Some(8),
            batch_size: Some(4),
        };

        // No fixture needed: with the feature compiled in, the capability gate must NOT fire — the
        // only available failure is the ordinary archive-lookup error.
        match mgr.build_index(MISSING_ARCHIVE, true, &opts) {
            Ok(n) => panic!("a missing archive must not index successfully (got Ok({n}))"),
            Err(ZimSearchError::Embedding(msg)) => {
                panic!("a feature-on build must never hit the 'missing feature' gate: {msg}")
            }
            Err(other) => assert!(
                other.to_string().contains(MISSING_ARCHIVE),
                "expected the archive-lookup error, got {other:?}"
            ),
        }

        let fixture = match fixture_or_skip(
            "build_index_semantic_true_in_a_feature_on_build_attempts_vectors_without_downloading",
        ) {
            Some(path) => path,
            None => return,
        };
        let (_lib, mgr) = staged_manager(&fixture, "vector_target.zim");
        mgr.set_embedder(Arc::new(MockEmbedder::new(64)));
        let stem = "vector_target";
        let index_dir = mgr.index_path_for(stem);

        let chunks = mgr
            .build_index(stem, true, &opts)
            .expect("semantic: true must index this archive in a feature-on build");
        assert!(chunks > 0, "the BM25 stage must emit chunks, got {chunks}");
        assert!(index_dir.join("meta.json").exists());
        assert!(
            index_dir.join("vectors.meta.json").exists(),
            "semantic: true must generate dense vectors for exactly this archive"
        );
        let listed = mgr.list_archives().expect("listing the staged library");
        let info = listed
            .iter()
            .find(|arch| arch.name == stem)
            .expect("staged archive is listed");
        assert_eq!(
            info.index_mode.as_deref(),
            Some("Hybrid"),
            "vectors must flip the reported index mode to Hybrid"
        );
    }

    /// Feature-OFF build: `semantic: true` is a hard, actionable error — never a silent BM25
    /// downgrade — and it is raised before any index work happens.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn build_index_semantic_true_without_the_feature_fails_fast_with_the_rebuild_hint() {
        let mgr = manager();
        assert!(!mgr.semantic_supported());
        let opts = SemanticOptions::default();

        let err = match mgr.build_index(MISSING_ARCHIVE, true, &opts) {
            Ok(n) => panic!("semantic: true must fail in a feature-off build, got Ok({n})"),
            Err(err) => err,
        };
        let msg = match err {
            ZimSearchError::Embedding(msg) => msg,
            other => panic!("expected ZimSearchError::Embedding, got {other:?}"),
        };
        assert!(
            msg.contains("without the 'semantic' feature")
                && msg.contains(crate::embed::SEMANTIC_REBUILD_COMMAND),
            "error must name the missing feature and the rebuild command: {msg}"
        );

        // One source of truth for the wording (`crate::embed::default_embedder`).
        let factory_msg = match default_embedder() {
            Ok(_) => panic!("default_embedder() must not succeed without the 'semantic' feature"),
            Err(ZimSearchError::Embedding(msg)) => msg,
            Err(err) => err.to_string(),
        };
        assert_eq!(
            msg, factory_msg,
            "build_index must surface the factory error verbatim"
        );

        // Fail fast: nothing was written, so no BM25 build was burned before reporting the gate.
        let index_dir = mgr.index_path_for(MISSING_ARCHIVE);
        assert!(
            !index_dir.exists(),
            "the capability gate must fire before any index directory is created"
        );
        assert!(!index_dir.join("meta.json").exists());
        assert!(!index_dir.join("vectors.meta.json").exists());

        // And the BM25-only opt-out on the same manager still works, in the same build.
        assert!(
            matches!(
                mgr.build_index(MISSING_ARCHIVE, false, &opts),
                Err(ZimSearchError::Other(_))
            ),
            "semantic: false must stay a plain lookup failure, never a capability error"
        );
    }

    /// Feature-OFF build: the `set_embedder` seam keeps `semantic: true` runnable, so the capability
    /// pre-flight must not fire for an injected ONNX-free embedder.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn build_index_semantic_true_accepts_an_injected_embedder_without_the_feature() {
        let mgr = manager();
        mgr.set_embedder(Arc::new(MockEmbedder::new(64)));
        let opts = SemanticOptions::default();

        match mgr.build_index(MISSING_ARCHIVE, true, &opts) {
            Ok(n) => panic!("a missing archive must not index successfully (got Ok({n}))"),
            Err(ZimSearchError::Embedding(msg)) => {
                panic!("the capability gate must not fire for an injected embedder: {msg}")
            }
            Err(other) => assert!(
                other.to_string().contains(MISSING_ARCHIVE),
                "expected the archive-lookup error, got {other:?}"
            ),
        }

        let fixture = match fixture_or_skip(
            "build_index_semantic_true_accepts_an_injected_embedder_without_the_feature",
        ) {
            Some(path) => path,
            None => return,
        };
        let (_lib, mgr) = staged_manager(&fixture, "injected_target.zim");
        mgr.set_embedder(Arc::new(MockEmbedder::new(64)));
        let stem = "injected_target";

        mgr.build_index(stem, true, &opts)
            .expect("an injected embedder must drive the semantic pass in a feature-off build");
        assert!(
            mgr.index_path_for(stem).join("vectors.meta.json").exists(),
            "the ONNX-free data plane must still write vectors in a feature-off build"
        );
    }
}
