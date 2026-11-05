use serde::{Deserialize, Serialize};

pub mod builder;
pub mod tantivy_engine;
pub mod vector;

pub use builder::IndexBuilder;
pub use tantivy_engine::TantivyEngine;
pub use vector::{DiskVectorIndex, DiskVectorMeta, VectorChunkRef, VectorIndex};

/// Index operational mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexMode {
    /// Pure BM25 keyword inverted index.
    BM25Only,
    /// Pure dense semantic vector index.
    SemanticOnly,
    /// Both BM25 inverted index and dense semantic vector index.
    Hybrid,
}

impl std::str::FromStr for IndexMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "bm25" | "bm25only" | "keyword" | "lexical" => Ok(IndexMode::BM25Only),
            "semantic" | "semanticonly" | "vector" => Ok(IndexMode::SemanticOnly),
            "hybrid" | "both" => Ok(IndexMode::Hybrid),
            other => Err(format!(
                "Invalid index mode: '{}'. Expected 'bm25', 'semantic', or 'hybrid'",
                other
            )),
        }
    }
}
