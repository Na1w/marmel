use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zim::{MimeType, Namespace, Zim};

use crate::error::{Result, ZimSearchError};

/// Metadata extracted from a ZIM archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveMetadata {
    pub title: String,
    pub creator: String,
    pub date: String,
    pub description: String,
    pub language: String,
    pub tags: String,
    pub total_entries: u32,
    pub has_illustration: bool,
}

/// Lightweight summary of an entry inside the ZIM archive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntrySummary {
    pub index: u32,
    pub namespace: char,
    pub url: String,
    pub title: String,
    pub mime_type: String,
    pub is_redirect: bool,
}

/// A parsed HTML/Text article ready for indexing.
#[derive(Debug, Clone)]
pub struct ArticleDocument {
    pub index: u32,
    pub url: String,
    pub title: String,
    pub mime_type: String,
    pub raw_content: Vec<u8>,
}

/// A target blob inside a cluster for an indexable article.
#[derive(Debug, Clone)]
pub struct ClusterBlobTarget {
    pub index: u32,
    pub url: String,
    pub title: String,
    pub mime_type: String,
    pub blob_idx: u32,
}

/// A group of target article blobs that belong to the same cluster.
#[derive(Debug, Clone)]
pub struct ClusterGroup {
    pub cluster_idx: u32,
    pub targets: Vec<ClusterBlobTarget>,
}

/// High-level wrapper over a ZIM archive providing convenient querying and extraction.
#[derive(Clone)]
pub struct ZimArchive {
    path: PathBuf,
    zim: Arc<Zim>,
}

impl ZimArchive {
    /// Opens a ZIM archive from disk using memory mapping.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let p = path.as_ref().to_path_buf();
        let zim = Zim::new(&p)?;
        Ok(Self {
            path: p,
            zim: Arc::new(zim),
        })
    }

    /// Archive path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Raw underlying Zim reference.
    pub fn raw(&self) -> &Zim {
        &self.zim
    }

    /// Read metadata key value as string if present.
    pub fn get_metadata_string(&self, key: &str) -> Option<String> {
        self.zim
            .metadata(key)
            .ok()
            .flatten()
            .and_then(|c| c.to_vec().ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
    }

    /// Read illustration image bytes if available.
    pub fn get_illustration(&self) -> Option<Vec<u8>> {
        self.zim
            .metadata("Illustration_48x48@1")
            .ok()
            .flatten()
            .and_then(|c| c.to_vec().ok())
    }

    /// Read high-level metadata of the archive.
    pub fn metadata(&self) -> ArchiveMetadata {
        let title = self.get_metadata_string("Title").unwrap_or_else(|| {
            self.path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled ZIM")
                .to_string()
        });
        let creator = self.get_metadata_string("Creator").unwrap_or_default();
        let date = self.get_metadata_string("Date").unwrap_or_default();
        let description = self.get_metadata_string("Description").unwrap_or_default();
        let language = self
            .get_metadata_string("Language")
            .unwrap_or_else(|| "unknown".to_string());
        let tags = self.get_metadata_string("Tags").unwrap_or_default();
        let has_illustration = self.get_illustration().is_some();

        ArchiveMetadata {
            title,
            creator,
            date,
            description,
            language,
            tags,
            total_entries: self.zim.header.article_count,
            has_illustration,
        }
    }

    /// Total number of directory entries in the archive.
    pub fn total_entries(&self) -> u32 {
        self.zim.header.article_count
    }

    /// Retrieve URL and Title for a specific entry index.
    pub fn get_entry_meta_by_index(&self, idx: u32) -> Result<(String, String)> {
        let entry = self.zim.get_by_url_index(idx)?;
        let resolved = self.zim.resolve(entry)?;
        let title = if resolved.title.is_empty() {
            resolved.url.clone()
        } else {
            resolved.title
        };
        Ok((resolved.url, title))
    }

    /// Retrieve content bytes for a specific directory entry index.
    pub fn read_entry_bytes_by_index(&self, idx: u32) -> Result<(String, Vec<u8>)> {
        let entry = self.zim.get_by_url_index(idx)?;
        let resolved = self.zim.resolve(entry)?;
        let mime = match &resolved.mime_type {
            MimeType::Type(m) => m.clone(),
            _ => "application/octet-stream".to_string(),
        };

        if let Some(content) = self.zim.entry_content(&resolved)? {
            let bytes = content.to_vec()?;
            Ok((mime, bytes))
        } else {
            Err(ZimSearchError::ArticleNotFound(format!(
                "No content in entry index {}",
                idx
            )))
        }
    }

    /// Read raw content bytes and mime type for a given URL / path.
    pub fn read_by_url(&self, url: &str) -> Result<Option<(String, Vec<u8>)>> {
        let clean_url = url.trim_start_matches('/');

        // 1. Try direct find in UserContent (ZIM 6.x)
        if let Ok(Some(idx)) = self.zim.find_by_path(Namespace::UserContent, clean_url) {
            let (mime, bytes) = self.read_entry_bytes_by_index(idx)?;
            return Ok(Some((mime, bytes)));
        }

        // 2. Try Articles namespace (ZIM 5.x)
        if let Ok(Some(idx)) = self.zim.find_by_path(Namespace::Articles, clean_url) {
            let (mime, bytes) = self.read_entry_bytes_by_index(idx)?;
            return Ok(Some((mime, bytes)));
        }

        // 3. Try stripping leading namespace character if present (e.g. "C/...", "A/...")
        if let Some((ns_prefix, rest)) = clean_url.split_once('/') {
            let ns = match ns_prefix {
                "C" | "c" => Some(Namespace::UserContent),
                "A" | "a" => Some(Namespace::Articles),
                "I" | "i" => Some(Namespace::ImagesFile),
                "M" | "m" => Some(Namespace::Metadata),
                "W" | "w" => Some(Namespace::CategoriesArticle),
                _ => None,
            };
            if let Some(ns) = ns {
                if let Ok(Some(idx)) = self.zim.find_by_path(ns, rest) {
                    let (mime, bytes) = self.read_entry_bytes_by_index(idx)?;
                    return Ok(Some((mime, bytes)));
                }
            }
        }

        Ok(None)
    }

    /// Read the main article of the archive if configured.
    pub fn main_page(&self) -> Result<Option<ArticleDocument>> {
        if let Some(entry) = self.zim.main_page()? {
            let resolved = self.zim.resolve(entry)?;
            let mime = match &resolved.mime_type {
                MimeType::Type(m) => m.clone(),
                _ => "text/html".to_string(),
            };
            if let Some(content) = self.zim.entry_content(&resolved)? {
                return Ok(Some(ArticleDocument {
                    index: 0,
                    url: resolved.url,
                    title: resolved.title,
                    mime_type: mime,
                    raw_content: content.to_vec()?,
                }));
            }
        }
        Ok(None)
    }

    /// Fast directory scan grouping all indexable articles by cluster ID.
    /// This allows sequential disk access and eliminates redundant cluster decompressions.
    pub fn collect_cluster_groups(&self) -> Result<Vec<ClusterGroup>> {
        use std::collections::HashMap;
        use zim::Target;

        let mut map: HashMap<u32, Vec<ClusterBlobTarget>> = HashMap::new();
        let total = self.zim.header.article_count;

        for i in 0..total {
            let entry = match self.zim.get_by_url_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entry.namespace != Namespace::UserContent
                && entry.namespace != Namespace::Articles
                && entry.namespace != Namespace::CategoriesArticle
            {
                continue;
            }

            let mime_str = match &entry.mime_type {
                MimeType::Type(m) => m.clone(),
                _ => continue,
            };

            if !mime_str.starts_with("text/html")
                && !mime_str.starts_with("text/plain")
                && !mime_str.starts_with("application/xhtml")
            {
                continue;
            }

            if entry.url.ends_with(".js")
                || entry.url.ends_with(".css")
                || entry.url.contains("_cover")
            {
                continue;
            }

            let resolved = match self.zim.resolve(entry) {
                Ok(r) => r,
                Err(_) => continue,
            };

            if let Some(Target::Cluster(cluster_idx, blob_idx)) = resolved.target {
                let title = if resolved.title.is_empty() {
                    resolved.url.clone()
                } else {
                    resolved.title.clone()
                };

                map.entry(cluster_idx).or_default().push(ClusterBlobTarget {
                    index: i,
                    url: resolved.url,
                    title,
                    mime_type: mime_str,
                    blob_idx,
                });
            }
        }

        let mut groups: Vec<ClusterGroup> = map
            .into_iter()
            .map(|(cluster_idx, targets)| ClusterGroup {
                cluster_idx,
                targets,
            })
            .collect();
        groups.sort_by_key(|g| g.cluster_idx);
        Ok(groups)
    }

    /// Stream indexable articles in small batches by cluster order.
    pub fn stream_indexable_batches<F>(&self, batch_size: usize, mut callback: F) -> Result<usize>
    where
        F: FnMut(Vec<ArticleDocument>) -> Result<()>,
    {
        let groups = self.collect_cluster_groups()?;
        let mut batch = Vec::with_capacity(batch_size);
        let mut total_processed = 0usize;

        for group in groups {
            if let Ok(cluster) = self.zim.get_cluster(group.cluster_idx) {
                if let Ok(guard) = cluster.read() {
                    for target in group.targets {
                        if let Ok(blob) = guard.blob(target.blob_idx) {
                            batch.push(ArticleDocument {
                                index: target.index,
                                url: target.url,
                                title: target.title,
                                mime_type: target.mime_type,
                                raw_content: blob.to_vec(),
                            });

                            if batch.len() >= batch_size {
                                total_processed += batch.len();
                                callback(std::mem::replace(
                                    &mut batch,
                                    Vec::with_capacity(batch_size),
                                ))?;
                            }
                        }
                    }
                }
            }
        }

        if !batch.is_empty() {
            total_processed += batch.len();
            callback(batch)?;
        }

        Ok(total_processed)
    }

    /// Collect all textual and HTML articles eligible for search indexing.
    pub fn collect_indexable_articles(&self) -> Result<Vec<ArticleDocument>> {
        let mut articles = Vec::new();
        self.stream_indexable_batches(500, |batch| {
            articles.extend(batch);
            Ok(())
        })?;
        Ok(articles)
    }

    /// List entry summaries with optional limit and query filter.
    pub fn list_entries(&self, filter: Option<&str>, limit: usize) -> Result<Vec<EntrySummary>> {
        let mut list = Vec::new();
        let filter_lower = filter.map(|f| f.to_lowercase());

        for (idx, entry_res) in self.zim.iterate_by_urls().enumerate() {
            let entry = match entry_res {
                Ok(e) => e,
                Err(_) => continue,
            };

            let mime = match &entry.mime_type {
                MimeType::Type(m) => m.clone(),
                MimeType::Redirect => "redirect".to_string(),
                MimeType::LinkTarget => "linktarget".to_string(),
                MimeType::DeletedEntry => "deleted".to_string(),
            };

            let matches = if let Some(ref query) = filter_lower {
                entry.url.to_lowercase().contains(query)
                    || entry.title.to_lowercase().contains(query)
            } else {
                true
            };

            if matches {
                list.push(EntrySummary {
                    index: idx as u32,
                    namespace: entry.namespace.as_byte() as char,
                    url: entry.url,
                    title: entry.title,
                    mime_type: mime,
                    is_redirect: matches!(entry.mime_type, MimeType::Redirect),
                });

                if list.len() >= limit {
                    break;
                }
            }
        }

        Ok(list)
    }
}
