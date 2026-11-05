use thiserror::Error;

/// Core error types for the zim-search library.
#[derive(Error, Debug)]
pub enum ZimSearchError {
    #[error("ZIM archive error: {0}")]
    Zim(#[from] zim::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Article not found: {0}")]
    ArticleNotFound(String),

    #[error("Invalid index file format or version mismatch: {0}")]
    InvalidIndex(String),

    #[error("Search error: {0}")]
    Search(String),

    #[error("Generic error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ZimSearchError>;
