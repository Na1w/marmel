use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Vector storage and cosine similarity search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndex {
    /// Dimension of each embedding vector (e.g. 384).
    pub dim: usize,
    /// Total number of stored vectors.
    pub count: usize,
    /// Explicit chunk IDs corresponding to each vector (enables semantic indexing on subsets).
    #[serde(default)]
    pub chunk_ids: Vec<u32>,
    /// Contiguous buffer storing all normalized embedding vectors sequentially.
    pub data: Vec<f32>,
}

impl VectorIndex {
    /// Construct a new vector index directly from a contiguous flat buffer of floats and optional chunk IDs.
    pub fn from_flat(data: Vec<f32>, dim: usize, chunk_ids: Vec<u32>) -> Self {
        let count = data.len().checked_div(dim).unwrap_or(0);
        Self {
            dim,
            count,
            chunk_ids,
            data,
        }
    }

    /// Construct a new vector index from a list of vectors.
    /// Expects vectors to be L2-normalized.
    pub fn new(vectors: Vec<Vec<f32>>, dim: usize) -> Self {
        let count = vectors.len();
        let chunk_ids: Vec<u32> = (0..count as u32).collect();
        let mut data = Vec::with_capacity(count * dim);
        for mut vec in vectors {
            if vec.len() != dim {
                vec.resize(dim, 0.0);
            }
            data.extend_from_slice(&vec);
        }

        Self {
            dim,
            count,
            chunk_ids,
            data,
        }
    }

    /// Retrieve a slice to the vector for chunk `idx`.
    pub fn get_vector(&self, idx: usize) -> Option<&[f32]> {
        if idx >= self.count {
            return None;
        }
        let start = idx * self.dim;
        let end = start + self.dim;
        Some(&self.data[start..end])
    }

    /// Perform parallel cosine similarity search using Rayon.
    /// Returns sorted (chunk_id, similarity_score) descending.
    pub fn search(&self, query_vec: &[f32], limit: usize) -> Vec<(u32, f32)> {
        if self.count == 0 || query_vec.len() != self.dim {
            return Vec::new();
        }

        // Parallel compute dot product against each document vector
        let scores: Vec<(u32, f32)> = self
            .data
            .par_chunks_exact(self.dim)
            .enumerate()
            .map(|(idx, doc_vec)| {
                // SIMD-friendly dot product
                let dot_product: f32 = query_vec
                    .iter()
                    .zip(doc_vec.iter())
                    .map(|(q, d)| q * d)
                    .sum();
                let chunk_id = if idx < self.chunk_ids.len() {
                    self.chunk_ids[idx]
                } else {
                    idx as u32
                };
                (chunk_id, dot_product)
            })
            .collect();

        // Extract top-K scores
        let mut sorted = scores;
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(limit);
        sorted
    }
}

/// Metadata stored in `vectors.meta.json` inside a Tantivy directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskVectorMeta {
    pub model: String,
    pub dim: usize,
    pub count: usize,
    pub lead_only: bool,
}

/// A compact 8-byte reference linking an embedding vector to its chunk ID and ZIM entry doc ID.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VectorChunkRef {
    pub chunk_id: u32,
    pub doc_id: u32,
}

/// Memory-mapped zero-copy vector storage and cosine similarity search engine.
pub struct DiskVectorIndex {
    _data_file: std::fs::File,
    mmap_data: memmap2::Mmap,
    _refs_file: Option<std::fs::File>,
    mmap_refs: Option<memmap2::Mmap>,
    pub meta: DiskVectorMeta,
}

impl DiskVectorIndex {
    /// Attempt to open a memory-mapped vector index from a directory.
    /// Returns Ok(None) if vectors have not been generated yet.
    pub fn open(dir: &std::path::Path) -> crate::error::Result<Option<Self>> {
        let meta_path = dir.join("vectors.meta.json");
        let data_path = dir.join("vectors.bin");
        if !meta_path.exists() || !data_path.exists() {
            return Ok(None);
        }

        let meta_str = std::fs::read_to_string(&meta_path)?;
        let meta: DiskVectorMeta = serde_json::from_str(&meta_str).map_err(|e| {
            crate::error::ZimSearchError::Other(format!("Failed to parse vectors.meta.json: {}", e))
        })?;

        let data_file = std::fs::File::open(&data_path)?;
        let mmap_data = unsafe { memmap2::Mmap::map(&data_file)? };

        let refs_path = dir.join("vectors_refs.bin");
        let (refs_file, mmap_refs) = if refs_path.exists() {
            let f = std::fs::File::open(&refs_path)?;
            let m = unsafe { memmap2::Mmap::map(&f)? };
            (Some(f), Some(m))
        } else {
            (None, None)
        };

        Ok(Some(Self {
            _data_file: data_file,
            mmap_data,
            _refs_file: refs_file,
            mmap_refs,
            meta,
        }))
    }

    /// Perform parallel cosine similarity search directly against memory-mapped vectors.
    /// Returns: Vec<(chunk_id, doc_id, cosine_similarity_score)>
    pub fn search(&self, query_vec: &[f32], limit: usize) -> Vec<(u32, u32, f32)> {
        if self.meta.count == 0 || query_vec.len() != self.meta.dim {
            return Vec::new();
        }

        let total_floats = self.meta.count * self.meta.dim;
        let expected_bytes = total_floats * std::mem::size_of::<f32>();
        if self.mmap_data.len() < expected_bytes {
            return Vec::new();
        }

        let floats: &[f32] = unsafe {
            std::slice::from_raw_parts(self.mmap_data.as_ptr() as *const f32, total_floats)
        };

        let chunk_refs: Option<&[VectorChunkRef]> = self.mmap_refs.as_ref().and_then(|m| {
            if m.len() >= self.meta.count * std::mem::size_of::<VectorChunkRef>() {
                Some(unsafe {
                    std::slice::from_raw_parts(m.as_ptr() as *const VectorChunkRef, self.meta.count)
                })
            } else {
                None
            }
        });

        let scores: Vec<(u32, u32, f32)> = floats
            .par_chunks_exact(self.meta.dim)
            .enumerate()
            .map(|(idx, doc_vec)| {
                let dot_product: f32 = query_vec
                    .iter()
                    .zip(doc_vec.iter())
                    .map(|(q, d)| q * d)
                    .sum();
                let (chunk_id, doc_id) = if let Some(refs) = chunk_refs {
                    (refs[idx].chunk_id, refs[idx].doc_id)
                } else {
                    (idx as u32, idx as u32)
                };
                (chunk_id, doc_id, dot_product)
            })
            .collect();

        let mut sorted = scores;
        sorted.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        sorted.truncate(limit);
        sorted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_search() {
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.0, 1.0, 0.0];
        let v3 = vec![
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
        ];

        let index = VectorIndex::new(vec![v1, v2, v3], 3);
        let query = vec![1.0, 0.0, 0.0];

        let results = index.search(&query, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 0); // exact match
        assert!((results[0].1 - 1.0).abs() < 1e-4);
        assert_eq!(results[1].0, 2); // partial match
    }

    #[test]
    fn test_vector_search_subset() {
        // Vectors corresponding to non-consecutive chunk IDs (e.g. only lead chunks [10, 42, 99])
        let flat_data = vec![
            1.0,
            0.0,
            0.0, // chunk 10
            0.0,
            1.0,
            0.0, // chunk 42
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0, // chunk 99
        ];
        let chunk_ids = vec![10, 42, 99];
        let index = VectorIndex::from_flat(flat_data, 3, chunk_ids);

        let query = vec![0.0, 1.0, 0.0];
        let results = index.search(&query, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, 42); // chunk 42 matched perfectly
        assert!((results[0].1 - 1.0).abs() < 1e-4);
        assert_eq!(results[1].0, 99); // chunk 99 matched partially
    }
}
