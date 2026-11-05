//! Pluggable embedding backends for dense-vector (semantic) search.
//!
//! Semantic support is a **two-level opt-in**:
//!
//! 1. **Compile time (Cargo feature `semantic`)** — decides whether the ONNX inference stack
//!    (`fastembed`, which carries `ort`/`ort-sys` and the ONNX Runtime) is compiled into this
//!    crate at all. It is **off by default**: a default build is BM25/Tantivy lexical only,
//!    links no ONNX code and never downloads a model. [`SEMANTIC_SUPPORTED`] reports which
//!    side of that line the binary was built on.
//! 2. **Runtime (per-run / per-source scope)** — `SemanticScope` + `SemanticOptions`
//!    (see `crate::manager`) decide, for a given indexing run or search, whether dense
//!    vectors are generated or consulted even when the feature *is* compiled in.
//!
//! Always available, independent of the feature: the ONNX-free [`Embedder`] trait, the
//! offline [`MockEmbedder`] (pure djb2 word hashing), [`normalize_vectors`], and the whole
//! dense-vector *data plane* in `crate::index`, which only ever sees
//! `Arc<dyn Embedder>` / `&dyn Embedder`.
//!
//! Requires `--features semantic`: [`FastEmbedder`] (local ONNX model; downloads weights on
//! first use). Feature-agnostic call sites should use [`default_embedder`], which yields a
//! working embedder in a feature-on build and an actionable error in a feature-off build —
//! never a silent no-op embedder.

#[cfg(feature = "semantic")]
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
#[cfg(feature = "semantic")]
use std::sync::Mutex;

use crate::error::{Result, ZimSearchError};

/// Pluggable interface for generating dense semantic vector embeddings.
pub trait Embedder: Send + Sync {
    /// Dimension of the generated vector embeddings.
    fn dimension(&self) -> usize;

    /// Name or identifier of the underlying model.
    fn model_name(&self) -> &str;

    /// Generate embeddings for a batch of text passages.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Generate an embedding vector for a single query string.
    fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[query])?;
        results
            .into_iter()
            .next()
            .ok_or_else(|| ZimSearchError::Embedding("No embedding returned for query".to_string()))
    }
}

/// Normalize vectors to unit length for fast dot-product cosine similarity
// `normalize_vectors` is ONNX-free and stays part of the ungated surface so the dense-vector
// data plane keeps its normalisation helper; in a `semantic`-off build its only current
// caller (`FastEmbedder::embed_batch`) is compiled out, hence the conditional allow.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn normalize_vectors(vectors: Vec<Vec<f32>>) -> Vec<Vec<f32>> {
    vectors
        .into_iter()
        .map(|mut vec| {
            let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 1e-6 {
                for v in vec.iter_mut() {
                    *v /= norm;
                }
            }
            vec
        })
        .collect()
}

/// Local ONNX-based dense embedding model using fastembed (default: 8-bit quantized AllMiniLML6V2Q).
/// Uses a multi-instance worker pool to saturate available CPU cores concurrently.
///
/// Only compiled when the `semantic` Cargo feature is enabled — this type embeds ONNX
/// concrete types (`fastembed::TextEmbedding` in its pool, `fastembed::EmbeddingModel` in
/// [`FastEmbedder::new_with_model`]) and pulls in the ONNX Runtime at build/link time.
#[cfg(feature = "semantic")]
pub struct FastEmbedder {
    pool: Vec<Mutex<TextEmbedding>>,
    name: String,
    dim: usize,
}

#[cfg(feature = "semantic")]
impl FastEmbedder {
    /// Initialize with default quantized AllMiniLML6V2Q (384-dim, fast & accurate).
    pub fn new_default() -> Result<Self> {
        Self::new_with_model(EmbeddingModel::AllMiniLML6V2Q, "AllMiniLML6V2Q")
    }

    /// Initialize with a specific model.
    pub fn new_with_model(model_type: EmbeddingModel, name: &str) -> Result<Self> {
        let available_cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        let intra_threads = Some(4.min(available_cpus));
        let mut options = TextInitOptions::new(model_type);
        options.intra_threads = intra_threads;
        options.show_download_progress = false;

        let mut text_embedding = TextEmbedding::try_new(options).map_err(|e| {
            ZimSearchError::Embedding(format!("Failed to initialize embedder: {}", e))
        })?;

        let probe = text_embedding
            .embed(vec!["probe"], None)
            .map_err(|e| ZimSearchError::Embedding(format!("Probe failed: {}", e)))?;
        let dim = probe.first().map(|v| v.len()).unwrap_or(384);

        Ok(Self {
            pool: vec![Mutex::new(text_embedding)],
            name: name.to_string(),
            dim,
        })
    }
}

#[cfg(feature = "semantic")]
impl Embedder for FastEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        &self.name
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let mut guard = self.pool[0]
            .lock()
            .map_err(|_| ZimSearchError::Embedding("Embedder mutex poisoned".to_string()))?;

        let doc_vec: Vec<&str> = texts.to_vec();
        let vectors = guard.embed(doc_vec, None).map_err(|e| {
            ZimSearchError::Embedding(format!("Embedding generation failed: {}", e))
        })?;

        Ok(normalize_vectors(vectors))
    }
}

/// Lightweight offline pseudo-embedder based on character n-gram hashing.
/// Great for offline unit tests and instant mock benchmarking.
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self::new(64)
    }
}

impl Embedder for MockEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        "MockHashEmbedder"
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            let mut vec = vec![0.0f32; self.dim];
            for word in text.split_whitespace() {
                let hash = word
                    .bytes()
                    .fold(5381u64, |h, b| (h << 5).wrapping_add(h) + (b as u64));
                let idx = (hash as usize) % self.dim;
                vec[idx] += 1.0;
            }
            let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 1e-6 {
                for v in vec.iter_mut() {
                    *v /= norm;
                }
            }
            results.push(vec);
        }
        Ok(results)
    }
}

/// True when this binary was built with dense-vector (ONNX) support, i.e. when the Cargo
/// feature `semantic` was enabled at compile time.
///
/// Contract: `false` guarantees that no ONNX code is present in this binary at all
/// (BM25/Tantivy lexical search only). Call sites must consult it — or the error from
/// [`default_embedder`] — and degrade to lexical search rather than fail the whole command.
pub const SEMANTIC_SUPPORTED: bool = cfg!(feature = "semantic");

/// The ONE canonical rebuild command every "this build has no dense-vector support" message quotes
/// (audit A11). The wording of those messages stays hand-written where a `const` cannot be composed
/// (Rust has no way to splice one `&str` `const` into another, and `format!` is not `const`), but the
/// command itself is single-sourced here: the drift tests in `embed`, `manager`, `mcp` and the
/// `zim-search` binary assert that each hand-maintained site still contains this exact substring, so
/// rewording a message without the command fails a test instead of silently diverging.
pub const SEMANTIC_REBUILD_COMMAND: &str = "cargo build --features semantic";

/// The canonical refusal sentence for a dense-vector request this binary can never honour. It is the
/// one sentence shared verbatim by the CLI fail-fast (`semantic_request_supported` in `src/main.rs`)
/// and the MCP protocol refusal (`SEMANTIC_OPT_IN_UNSUPPORTED` in `src/mcp/mod.rs`), which must agree
/// (audit A1). The two surfaces keep their own literals (a `const` cannot be assembled from this one)
/// and are pinned to it by drift tests.
pub const SEMANTIC_FEATURE_UNAVAILABLE: &str =
    "Semantic vector indexing is not available in this build: recompile with `cargo build --features semantic`.";

/// Construct the default dense-vector embedder, or an actionable error in a feature-off build.
/// Never returns a silent no-op embedder.
///
/// Contract:
/// * with `--features semantic`: returns [`FastEmbedder`] (local ONNX AllMiniLML6V2Q; the
///   first call may download model weights and can fail offline — the failure is reported as
///   [`ZimSearchError::Embedding`]),
/// * without it: returns [`ZimSearchError::Embedding`] naming the missing feature and the
///   exact rebuild command, so callers keep BM25 as a fallback instead of silently returning
///   zero-length or all-zero vectors.
pub fn default_embedder() -> Result<std::sync::Arc<dyn Embedder>> {
    #[cfg(feature = "semantic")]
    {
        FastEmbedder::new_default().map(|e| std::sync::Arc::new(e) as std::sync::Arc<dyn Embedder>)
    }
    #[cfg(not(feature = "semantic"))]
    {
        // Built at runtime so the command is quoted from the single source of truth
        // ([`SEMANTIC_REBUILD_COMMAND`]) instead of being re-typed here (audit A11). The rendered
        // sentence is byte-for-byte what it always was.
        Err(ZimSearchError::Embedding(format!(
            "this binary was built without the 'semantic' feature; rebuild with `{}` to enable dense vector embeddings",
            SEMANTIC_REBUILD_COMMAND
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_embedder() {
        let embedder = MockEmbedder::default();
        let texts = vec!["PowerPC architecture", "Nintendo Wii hardware"];
        let vecs = embedder.embed_batch(&texts).unwrap();
        assert_eq!(vecs.len(), 2);
        assert_eq!(vecs[0].len(), 64);
    }

    /// Feature-off build (the default): the binary carries no ONNX support.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn semantic_supported_is_false_without_the_feature() {
        const { assert!(!SEMANTIC_SUPPORTED) };
    }

    /// Feature-off build: the default-embedder factory must fail with an actionable message
    /// instead of returning a silent no-op embedder.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn default_embedder_reports_actionable_error_without_the_feature() {
        let msg = match default_embedder() {
            Ok(_) => panic!("default_embedder() must not succeed without the 'semantic' feature"),
            Err(ZimSearchError::Embedding(msg)) => msg,
            Err(other) => panic!("expected ZimSearchError::Embedding, got {other:?}"),
        };
        assert!(
            msg.contains("without the 'semantic' feature"),
            "error must name the missing feature: {msg}"
        );
        assert!(
            msg.contains(SEMANTIC_REBUILD_COMMAND),
            "error must name the rebuild command: {msg}"
        );
    }

    /// Audit A11 drift guard, compiled in **both** build configurations: the two anchor strings are
    /// the only place the rebuild command and the refusal sentence are written out, so they must stay
    /// consistent with each other. Every other surface quotes them (directly at runtime, or via a
    /// drift test on its own hand-maintained literal).
    #[test]
    fn semantic_anchors_stay_consistent_with_each_other() {
        assert!(
            SEMANTIC_FEATURE_UNAVAILABLE.contains(SEMANTIC_REBUILD_COMMAND),
            "the canonical refusal sentence must quote the canonical command: {SEMANTIC_FEATURE_UNAVAILABLE}"
        );
        assert!(
            !SEMANTIC_FEATURE_UNAVAILABLE.contains('\n'),
            "the sentence is embedded inside one-line messages by the CLI and the MCP server: {SEMANTIC_FEATURE_UNAVAILABLE}"
        );
        assert!(
            SEMANTIC_REBUILD_COMMAND.contains("--features semantic"),
            "the command must stay the cargo invocation that turns the feature on"
        );
    }

    /// Audit A11: the factory message must be assembled around the anchor command (not re-typed next
    /// to it), so a reword that paraphrases or drops the cargo invocation is caught here.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn default_embedder_error_is_built_from_the_anchor_command() {
        let msg = match default_embedder() {
            Ok(_) => panic!("default_embedder() must not succeed without the 'semantic' feature"),
            Err(ZimSearchError::Embedding(msg)) => msg,
            Err(other) => panic!("expected ZimSearchError::Embedding, got {other:?}"),
        };
        assert!(
            msg.contains(&format!(
                "rebuild with `{}` to enable dense vector embeddings",
                SEMANTIC_REBUILD_COMMAND
            )),
            "the message must interpolate the anchor verbatim: {msg}"
        );
    }

    /// Feature-on build: only the compile-time constant is asserted here — constructing
    /// `FastEmbedder` in a test would download ONNX model weights, which unit tests must not do.
    #[cfg(feature = "semantic")]
    #[test]
    fn semantic_supported_is_true_with_the_feature() {
        assert!(SEMANTIC_SUPPORTED);
    }
}
