pub mod archive;
pub mod embed;
pub mod error;
pub mod extract;
pub mod index;
pub mod manager;
pub mod mcp;
pub mod search;
pub mod server;

// Re-exports of key types
pub use archive::{ArchiveMetadata, ArticleDocument, EntrySummary, ZimArchive};
/// Feature-agnostic dense-vector factory: works with `--features semantic`, returns an
/// actionable `ZimSearchError::Embedding` in the default (ONNX-free) build.
pub use embed::default_embedder;
/// ONNX-backed dense embedder — only part of the public API with `--features semantic`.
#[cfg(feature = "semantic")]
pub use embed::FastEmbedder;
pub use embed::{Embedder, MockEmbedder, SEMANTIC_SUPPORTED};
/// The canonical rebuild command and the canonical refusal sentence quoted by every "this build has
/// no dense-vector support" message (CLI, library error, MCP server). Single-sourced so a reword
/// cannot silently drop the instruction — the module drift tests fail if a site stops quoting them.
pub use embed::{SEMANTIC_FEATURE_UNAVAILABLE, SEMANTIC_REBUILD_COMMAND};
pub use error::{Result, ZimSearchError};
pub use extract::{chunk_text, clean_html, extract_snippet, DocumentChunk};
pub use index::{
    DiskVectorIndex, DiskVectorMeta, IndexBuilder, IndexMode, TantivyEngine, VectorChunkRef,
    VectorIndex,
};
pub use manager::{
    ArchiveManager, ManagedArchiveInfo, MultiSearchResult, PaginatedArticleContent,
    PaginatedArticleList, PaginatedSearchResults, SemanticOptions, SemanticScope,
};
pub use mcp::run_mcp_server;
pub use search::{SearchEngine, SearchMode, SearchResult};
pub use server::run_server;
