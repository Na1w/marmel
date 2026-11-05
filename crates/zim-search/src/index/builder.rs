use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use tantivy::doc;

use crate::archive::ZimArchive;
use crate::embed::Embedder;
use crate::error::{Result, ZimSearchError};
use crate::extract::{chunk_text, clean_html};
use crate::index::tantivy_engine::TantivyEngine;
use crate::index::vector::{DiskVectorMeta, VectorChunkRef};
use crate::index::IndexMode;

/// Builder configuration for creating disk-backed Tantivy and Vector indexes.
pub struct IndexBuilder {
    pub mode: IndexMode,
    pub target_chunk_words: usize,
    pub overlap_words: usize,
    pub batch_size: usize,
    pub max_chunks_per_article: Option<usize>,
    pub semantic_lead_only: bool,
    pub max_semantic_chunks: Option<usize>,
    pub show_progress: bool,
}

impl Default for IndexBuilder {
    fn default() -> Self {
        Self {
            mode: IndexMode::Hybrid,
            target_chunk_words: 240,
            overlap_words: 35,
            batch_size: 256,
            max_chunks_per_article: None,
            semantic_lead_only: false,
            max_semantic_chunks: None,
            show_progress: true,
        }
    }
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_mode(mut self, mode: IndexMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_chunk_size(mut self, target_words: usize, overlap_words: usize) -> Self {
        self.target_chunk_words = target_words;
        self.overlap_words = overlap_words;
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    pub fn with_max_chunks_per_article(mut self, max: Option<usize>) -> Self {
        self.max_chunks_per_article = max;
        self
    }

    pub fn with_semantic_lead_only(mut self, lead_only: bool) -> Self {
        self.semantic_lead_only = lead_only;
        self
    }

    pub fn with_max_semantic_chunks(mut self, max: Option<usize>) -> Self {
        self.max_semantic_chunks = max;
        self
    }

    pub fn with_progress(mut self, show_progress: bool) -> Self {
        self.show_progress = show_progress;
        self
    }

    /// Stage 1: Build a disk-backed Tantivy index in `output_dir` in streaming batches.
    /// Memory consumption stays strictly bounded by Tantivy's 50MB buffer (< 150MB total RAM).
    pub fn build_tantivy_stage(&self, archive: &ZimArchive, output_dir: &Path) -> Result<usize> {
        let (_index, mut writer, fields) = TantivyEngine::create_or_overwrite(output_dir)?;
        let groups = archive.collect_cluster_groups()?;
        let total_articles: usize = groups.iter().map(|g| g.targets.len()).sum();

        let progress_pb = if self.show_progress {
            let pb = ProgressBar::new(total_articles as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} articles ({eta}) {msg}")
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("#>-"),
            );
            pb.set_message("Streaming full text to disk-backed Tantivy index...");
            Some(pb)
        } else {
            None
        };

        let target_words = self.target_chunk_words;
        let overlap_words = self.overlap_words;
        let mut current_chunk_id = 0u32;
        let mut total_chunks = 0usize;

        for cluster_batch in groups.chunks(128) {
            let batch_article_count: usize = cluster_batch.iter().map(|g| g.targets.len()).sum();
            let batch_docs: Vec<(u32, String, String, String)> = cluster_batch
                .par_iter()
                .flat_map(|group| {
                    let mut docs = Vec::new();
                    if let Ok(cluster) = archive.raw().get_cluster(group.cluster_idx) {
                        if let Ok(guard) = cluster.read() {
                            for target in &group.targets {
                                if let Ok(blob) = guard.blob(target.blob_idx) {
                                    let text = String::from_utf8_lossy(blob);
                                    let cleaned = clean_html(&text);
                                    if cleaned.split_whitespace().count() >= 15 {
                                        docs.push((
                                            target.index,
                                            target.url.clone(),
                                            target.title.clone(),
                                            cleaned,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    docs
                })
                .collect();

            if let Some(ref pb) = progress_pb {
                pb.inc(batch_article_count as u64);
            }

            for (doc_id, url, title, cleaned_text) in batch_docs {
                let doc_chunks = chunk_text(
                    doc_id,
                    &url,
                    &title,
                    &cleaned_text,
                    target_words,
                    overlap_words,
                    self.max_chunks_per_article,
                    &mut current_chunk_id,
                );
                for chunk in &doc_chunks {
                    writer
                        .add_document(doc!(
                            fields.chunk_id => chunk.id as u64,
                            fields.doc_id => chunk.doc_id as u64,
                            fields.url => chunk.article_url.as_str(),
                            fields.title => chunk.title.as_str(),
                            fields.chunk_index => chunk.chunk_index as u64,
                            fields.body => chunk.text.as_str(),
                        ))
                        .map_err(|e| ZimSearchError::Other(e.to_string()))?;
                }
                total_chunks += doc_chunks.len();
            }
        }

        if let Some(ref pb) = progress_pb {
            pb.set_message("Flushing and committing Tantivy segments to disk...");
        }

        writer
            .commit()
            .map_err(|e| ZimSearchError::Other(e.to_string()))?;

        if let Some(ref pb) = progress_pb {
            pb.finish_with_message(format!(
                "Tantivy disk index complete: {} chunks across {} articles",
                total_chunks, total_articles
            ));
        }

        Ok(total_chunks)
    }

    /// Stage 2: Populate dense semantic vector embeddings onto a disk-backed Tantivy index directory.
    /// Streams articles in cluster order, computes embeddings in low-memory batches,
    /// directly appends vectors to `vectors.bin` and refs to `vectors_refs.bin`, and supports
    /// safe crash-recovery and resuming from the last checkpoint.
    pub fn populate_tantivy_semantic_stage(
        &self,
        archive: &ZimArchive,
        tantivy_dir: &Path,
        embedder: &dyn Embedder,
    ) -> Result<usize> {
        let vectors_path = tantivy_dir.join("vectors.bin");
        let refs_path = tantivy_dir.join("vectors_refs.bin");
        let meta_path = tantivy_dir.join("vectors.meta.json");

        let float_dim = embedder.dimension();
        let bytes_per_vec = float_dim * std::mem::size_of::<f32>();
        let bytes_per_ref = std::mem::size_of::<VectorChunkRef>();

        // Check if resuming from an existing checkpoint
        let mut existing_count = 0usize;
        if vectors_path.exists() && refs_path.exists() {
            let vec_bytes = std::fs::metadata(&vectors_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let ref_bytes = std::fs::metadata(&refs_path).map(|m| m.len()).unwrap_or(0);
            let count_from_vecs = (vec_bytes / bytes_per_vec as u64) as usize;
            let count_from_refs = (ref_bytes / bytes_per_ref as u64) as usize;
            existing_count = count_from_vecs.min(count_from_refs);

            if existing_count > 0 {
                let valid_vec_bytes = (existing_count * bytes_per_vec) as u64;
                let valid_ref_bytes = (existing_count * bytes_per_ref) as u64;
                let f_vec = OpenOptions::new().write(true).open(&vectors_path)?;
                f_vec.set_len(valid_vec_bytes)?;
                let f_ref = OpenOptions::new().write(true).open(&refs_path)?;
                f_ref.set_len(valid_ref_bytes)?;

                eprintln!(
                    "[IndexBuilder] Resuming Tantivy semantic vectors from chunk {}...",
                    existing_count
                );
            }
        }

        let mut vectors_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&vectors_path)?;

        let mut refs_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&refs_path)?;

        let groups = archive.collect_cluster_groups()?;
        let total_articles: usize = groups.iter().map(|g| g.targets.len()).sum();

        let progress_pb = if self.show_progress {
            let pb = ProgressBar::new(total_articles as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.magenta/blue}] {pos}/{len} articles ({eta}) {msg}")
                    .unwrap_or_else(|_| ProgressStyle::default_bar())
                    .progress_chars("#>-"),
            );
            pb.set_message(format!(
                "Generating semantic embeddings ({}, chunked)...",
                embedder.model_name()
            ));
            Some(pb)
        } else {
            None
        };

        let target_words = self.target_chunk_words;
        let overlap_words = self.overlap_words;
        let mut current_chunk_id = 0u32;
        let mut total_embedded = existing_count;
        let mut seen_chunk_idx = 0usize;

        let mut batch_texts: Vec<String> = Vec::with_capacity(self.batch_size);
        let mut batch_refs: Vec<VectorChunkRef> = Vec::with_capacity(self.batch_size);

        'outer: for cluster_batch in groups.chunks(64) {
            let batch_article_count: usize = cluster_batch.iter().map(|g| g.targets.len()).sum();
            let batch_docs: Vec<(u32, String, String, String)> = cluster_batch
                .par_iter()
                .flat_map(|group| {
                    let mut docs = Vec::new();
                    if let Ok(cluster) = archive.raw().get_cluster(group.cluster_idx) {
                        if let Ok(guard) = cluster.read() {
                            for target in &group.targets {
                                if let Ok(blob) = guard.blob(target.blob_idx) {
                                    let text = String::from_utf8_lossy(blob);
                                    let cleaned = clean_html(&text);
                                    if cleaned.split_whitespace().count() >= 15 {
                                        docs.push((
                                            target.index,
                                            target.url.clone(),
                                            target.title.clone(),
                                            cleaned,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    docs
                })
                .collect();

            if let Some(ref pb) = progress_pb {
                pb.inc(batch_article_count as u64);
            }

            for (doc_id, url, title, cleaned_text) in batch_docs {
                let doc_chunks = chunk_text(
                    doc_id,
                    &url,
                    &title,
                    &cleaned_text,
                    target_words,
                    overlap_words,
                    self.max_chunks_per_article,
                    &mut current_chunk_id,
                );

                for chunk in doc_chunks {
                    if self.semantic_lead_only && chunk.chunk_index != 0 {
                        continue;
                    }

                    if let Some(max_sem) = self.max_semantic_chunks {
                        if total_embedded + batch_texts.len() >= max_sem {
                            break 'outer;
                        }
                    }

                    seen_chunk_idx += 1;
                    if seen_chunk_idx <= existing_count {
                        continue;
                    }

                    batch_texts.push(chunk.text);
                    batch_refs.push(VectorChunkRef {
                        chunk_id: chunk.id,
                        doc_id: chunk.doc_id,
                    });

                    if batch_texts.len() >= self.batch_size {
                        let text_refs: Vec<&str> = batch_texts.iter().map(|s| s.as_str()).collect();
                        let vectors = embedder.embed_batch(&text_refs)?;

                        for vec in &vectors {
                            for val in vec {
                                vectors_file.write_all(&val.to_le_bytes())?;
                            }
                        }
                        for r in &batch_refs {
                            let slice = unsafe {
                                std::slice::from_raw_parts(
                                    r as *const VectorChunkRef as *const u8,
                                    std::mem::size_of::<VectorChunkRef>(),
                                )
                            };
                            refs_file.write_all(slice)?;
                        }
                        vectors_file.flush()?;
                        refs_file.flush()?;

                        total_embedded += batch_texts.len();
                        batch_texts.clear();
                        batch_refs.clear();
                    }
                }
            }
        }

        // Flush any remaining items in buffer
        if !batch_texts.is_empty() {
            let text_refs: Vec<&str> = batch_texts.iter().map(|s| s.as_str()).collect();
            let vectors = embedder.embed_batch(&text_refs)?;

            for vec in &vectors {
                for val in vec {
                    vectors_file.write_all(&val.to_le_bytes())?;
                }
            }
            for r in &batch_refs {
                let slice = unsafe {
                    std::slice::from_raw_parts(
                        r as *const VectorChunkRef as *const u8,
                        std::mem::size_of::<VectorChunkRef>(),
                    )
                };
                refs_file.write_all(slice)?;
            }
            vectors_file.flush()?;
            refs_file.flush()?;

            total_embedded += batch_texts.len();
        }

        // Write meta json
        let meta = DiskVectorMeta {
            model: embedder.model_name().to_string(),
            dim: embedder.dimension(),
            count: total_embedded,
            lead_only: self.semantic_lead_only,
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| ZimSearchError::Other(e.to_string()))?;
        std::fs::write(&meta_path, meta_json)?;

        if let Some(ref pb) = progress_pb {
            pb.finish_with_message(format!(
                "Tantivy semantic vectors complete: {} vectors saved to disk",
                total_embedded
            ));
        }

        Ok(total_embedded)
    }
}
