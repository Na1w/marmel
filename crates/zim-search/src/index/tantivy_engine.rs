use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, Value, STORED, STRING, TEXT};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

use crate::error::{Result, ZimSearchError};

/// Schema fields for the Tantivy disk-backed search engine.
#[derive(Clone, Copy)]
pub struct TantivySchemaFields {
    pub chunk_id: Field,
    pub doc_id: Field,
    pub url: Field,
    pub title: Field,
    pub chunk_index: Field,
    pub body: Field,
}

use crate::index::vector::DiskVectorIndex;

pub struct TantivyEngine {
    index: Index,
    reader: IndexReader,
    fields: TantivySchemaFields,
    vector_index: Option<DiskVectorIndex>,
}

impl TantivyEngine {
    /// Create schema: body is indexed for BM25 search but NOT stored on disk,
    /// keeping disk usage small and eliminating duplicate text storage.
    pub fn build_schema() -> (Schema, TantivySchemaFields) {
        let mut builder = Schema::builder();
        let chunk_id = builder.add_u64_field("chunk_id", STORED);
        let doc_id = builder.add_u64_field("doc_id", STORED);
        let url = builder.add_text_field("url", STRING | STORED);
        let title = builder.add_text_field("title", TEXT | STORED);
        let chunk_index = builder.add_u64_field("chunk_index", STORED);
        let body = builder.add_text_field("body", TEXT);
        let schema = builder.build();
        let fields = TantivySchemaFields {
            chunk_id,
            doc_id,
            url,
            title,
            chunk_index,
            body,
        };
        (schema, fields)
    }

    /// Open an existing Tantivy index directory, and lazily load disk vector index if present.
    pub fn open(dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(dir).map_err(|e| ZimSearchError::Other(e.to_string()))?;
        let schema = index.schema();
        let fields = TantivySchemaFields {
            chunk_id: schema
                .get_field("chunk_id")
                .map_err(|e| ZimSearchError::Other(e.to_string()))?,
            doc_id: schema
                .get_field("doc_id")
                .map_err(|e| ZimSearchError::Other(e.to_string()))?,
            url: schema
                .get_field("url")
                .map_err(|e| ZimSearchError::Other(e.to_string()))?,
            title: schema
                .get_field("title")
                .map_err(|e| ZimSearchError::Other(e.to_string()))?,
            chunk_index: schema
                .get_field("chunk_index")
                .map_err(|e| ZimSearchError::Other(e.to_string()))?,
            body: schema
                .get_field("body")
                .map_err(|e| ZimSearchError::Other(e.to_string()))?,
        };
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| ZimSearchError::Other(e.to_string()))?;

        let vector_index = DiskVectorIndex::open(dir).ok().flatten();

        Ok(Self {
            index,
            reader,
            fields,
            vector_index,
        })
    }

    /// Check if disk vector embeddings are available for this Tantivy index.
    pub fn has_vectors(&self) -> bool {
        self.vector_index.is_some()
    }

    /// Access the underlying disk vector index if present.
    pub fn vector_index(&self) -> Option<&DiskVectorIndex> {
        self.vector_index.as_ref()
    }

    /// Run cosine similarity search against stored dense vectors.
    /// Returns: Vec<(chunk_id, doc_id, cosine_score)>
    pub fn vector_search(&self, query_vec: &[f32], limit: usize) -> Vec<(u32, u32, f32)> {
        if let Some(ref vi) = self.vector_index {
            vi.search(query_vec, limit)
        } else {
            Vec::new()
        }
    }

    /// Create or overwrite a Tantivy index in a directory.
    pub fn create_or_overwrite(dir: &Path) -> Result<(Index, IndexWriter, TantivySchemaFields)> {
        if !dir.exists() {
            std::fs::create_dir_all(dir)?;
        }
        let (schema, fields) = Self::build_schema();
        let index =
            Index::create_in_dir(dir, schema).map_err(|e| ZimSearchError::Other(e.to_string()))?;
        // 50 MB memory budget for the writer - flushes segments to disk automatically!
        let writer = index
            .writer(50_000_000)
            .map_err(|e| ZimSearchError::Other(e.to_string()))?;
        Ok((index, writer, fields))
    }

    /// Search the Tantivy index with BM25 keyword scoring.
    /// Returns: Vec<(chunk_id, doc_id, url, title, chunk_index, bm25_score)>
    #[allow(clippy::type_complexity)]
    pub fn search(
        &self,
        query_str: &str,
        limit: usize,
    ) -> Result<Vec<(u32, u32, String, String, u32, f32)>> {
        let searcher = self.reader.searcher();
        let query_parser =
            QueryParser::for_index(&self.index, vec![self.fields.title, self.fields.body]);
        let query = query_parser
            .parse_query(query_str)
            .map_err(|e| ZimSearchError::Search(e.to_string()))?;

        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|e| ZimSearchError::Search(e.to_string()))?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher
                .doc(doc_address)
                .map_err(|e| ZimSearchError::Search(e.to_string()))?;

            let chunk_id = retrieved_doc
                .get_first(self.fields.chunk_id)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let doc_id = retrieved_doc
                .get_first(self.fields.doc_id)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let url = retrieved_doc
                .get_first(self.fields.url)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let title = retrieved_doc
                .get_first(self.fields.title)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let chunk_index = retrieved_doc
                .get_first(self.fields.chunk_index)
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            results.push((chunk_id, doc_id, url, title, chunk_index, score));
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::doc;

    #[test]
    fn test_tantivy_engine_roundtrip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (_index, mut writer, fields) =
            TantivyEngine::create_or_overwrite(temp_dir.path()).unwrap();

        writer.add_document(doc!(
            fields.chunk_id => 1u64,
            fields.doc_id => 10u64,
            fields.url => "article1.html",
            fields.title => "Rust Programming Language",
            fields.chunk_index => 0u64,
            fields.body => "Rust is a multi-paradigm, general-purpose programming language that emphasizes performance, type safety, and concurrency.",
        )).unwrap();

        writer.add_document(doc!(
            fields.chunk_id => 2u64,
            fields.doc_id => 11u64,
            fields.url => "article2.html",
            fields.title => "Python Language",
            fields.chunk_index => 0u64,
            fields.body => "Python is a high-level, general-purpose programming language emphasizing code readability.",
        )).unwrap();

        writer.commit().unwrap();

        let engine = TantivyEngine::open(temp_dir.path()).unwrap();
        let results = engine.search("concurrency", 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
        assert_eq!(results[0].2, "article1.html");
        assert_eq!(results[0].3, "Rust Programming Language");

        let results_python = engine.search("readability", 5).unwrap();
        assert_eq!(results_python.len(), 1);
        assert_eq!(results_python[0].0, 2);
    }
}
