use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tracing_subscriber::EnvFilter;

// `default_embedder` / `SEMANTIC_SUPPORTED` instead of `FastEmbedder`: the CLI must compile and run
// in the ONNX-free default build, where `FastEmbedder` does not exist at all.
use zim_search::{
    clean_html, default_embedder, ArchiveManager, Embedder, IndexBuilder, IndexMode, MockEmbedder,
    SearchEngine, SearchMode, SemanticOptions, SemanticScope, TantivyEngine, ZimArchive,
    SEMANTIC_FEATURE_UNAVAILABLE, SEMANTIC_REBUILD_COMMAND, SEMANTIC_SUPPORTED,
};

// Build honesty for `--semantic` / `--semantic-archive`: each flag is declared exactly ONCE with
// feature-neutral help, because a `#[cfg]`-twin field inside a `Subcommand` variant puts duplicate
// field tokens into `clap_derive`'s input (audit N1). A binary compiled without the `semantic`
// feature instead states the limitation where it can be checked at runtime: one note per `index` run
// (`note_semantic_feature_absent`), the fail-fast `semantic_request_supported` guard, and
// `VECTOR_BUILD_HINT` on `search`.

/// Tail appended to the `search` BM25-fallback notices in an ONNX-free build so the hint names the
/// rebuild needed before dense vectors can exist at all. Empty when the feature is compiled in,
/// which keeps a feature-on build's `search` output byte-identical to before.
const VECTOR_BUILD_HINT: &str = if SEMANTIC_SUPPORTED {
    ""
} else {
    " This binary has no dense-vector support (compiled without the 'semantic' feature): rebuild \
     with `cargo build --features semantic` before vectors can be generated or searched."
};

/// Default archive-library location, used only when a command was given no explicit path (`.`) and
/// the current directory holds no `*.zim`. It is an absolute path on the data volume, replacing the
/// CWD-relative `kiwix_dir` symlink the crate used to sit next to, so no working directory or
/// symlink is needed any more.
const DEFAULT_ZIM_DIR: &str = "/mnt/extradisk/kiwix";

/// Default index location (`<stem>.tantivy/` folders) whenever `--index` / `--index-dir` is absent.
/// Absolute on purpose: it replaces the CWD-relative `indices` / `indicies` / `.indexes` lookup
/// chain, which only resolved if the process happened to be started in the data directory.
const DEFAULT_INDEX_DIR: &str = "/mnt/extradisk/kiwixidx";

#[derive(Parser, Debug)]
#[command(
    name = "zim-search",
    author = "Fredrik",
    version = "0.1.0",
    about = "ZIM archive reader with vector and BM25 search"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Inspect metadata and statistics of a ZIM archive or directory of archives
    Info {
        /// Path to a .zim file or directory of .zim archives (default: current directory or /mnt/extradisk/kiwix)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Explicit kiwix / zim directory override
        #[arg(long, visible_alias = "kiwix-dir", alias = "zim-dir")]
        kiwix_dir: Option<PathBuf>,

        /// Directory where indexes are located
        #[arg(long, visible_alias = "index", alias = "indices-dir")]
        index_dir: Option<PathBuf>,
    },

    /// List directory entries and articles in an archive or directory of archives
    List {
        /// Path to a .zim file or directory of .zim archives (default: current directory or /mnt/extradisk/kiwix)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Explicit kiwix / zim directory override
        #[arg(long, visible_alias = "kiwix-dir", alias = "zim-dir")]
        kiwix_dir: Option<PathBuf>,

        /// Directory where indexes are located
        #[arg(long, visible_alias = "index", alias = "indices-dir")]
        index_dir: Option<PathBuf>,

        /// Substring filter for URLs or titles
        #[arg(short, long)]
        filter: Option<String>,

        /// Maximum entries to display
        #[arg(short, long, default_value = "30")]
        limit: usize,
    },

    /// Read and display an article from an archive
    Read {
        /// Article URL or relative path
        url: String,

        /// Specific archive name if library contains multiple archives
        #[arg(short, long)]
        archive: Option<String>,

        /// Path to the .zim file or directory containing the archive (default: current directory or /mnt/extradisk/kiwix)
        #[arg(
            short,
            long,
            visible_alias = "kiwix-dir",
            alias = "zim-dir",
            default_value = "."
        )]
        path: PathBuf,

        /// Output raw HTML / bytes instead of cleaned plain text
        #[arg(short, long)]
        raw: bool,
    },

    /// Index a ZIM file or an entire directory of ZIM archives
    Index {
        /// Path to a specific .zim file or directory of .zim archives (default: current directory or /mnt/extradisk/kiwix)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Explicit kiwix / zim directory override
        #[arg(long, visible_alias = "kiwix-dir", alias = "zim-dir")]
        kiwix_dir: Option<PathBuf>,

        /// Output index file path (only used when indexing a single file)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Directory to store index files
        #[arg(long, visible_alias = "index", alias = "indices-dir")]
        index_dir: Option<PathBuf>,

        /// Force re-indexing even if an index already exists
        #[arg(short, long)]
        force: bool,

        /// Indexing mode: 'semantic', 'hybrid', or 'bm25'. Omitting --mode behaves like 'bm25': no dense vectors are generated unless --semantic or --semantic-archive is passed
        #[arg(short, long)]
        mode: Option<String>,

        /// Opt in: generate dense semantic vector embeddings for every archive in scope (off by default)
        #[arg(long)]
        semantic: bool,

        /// Opt in and limit dense semantic vectors to the named archive: archive name, file stem, or .zim filename (repeatable)
        #[arg(long, visible_alias = "semantic-source")]
        semantic_archive: Vec<String>,

        /// Target words per chunk passage
        #[arg(long, default_value = "240")]
        chunk_words: usize,

        /// Overlap words between chunks
        #[arg(long, default_value = "35")]
        overlap_words: usize,

        /// Maximum chunks to extract per article/book (e.g. 1 or 2 keeps memory < 200MB on massive book archives)
        #[arg(long, visible_alias = "max-chunks")]
        max_chunks_per_article: Option<usize>,

        /// Only generate semantic dense vector embeddings for the lead passage of each article (chunk_index == 0)
        #[arg(long)]
        semantic_lead_only: bool,

        /// Maximum total semantic vector embeddings to generate
        #[arg(long)]
        max_semantic_chunks: Option<usize>,

        /// Batch size for neural embeddings
        #[arg(long, default_value = "256")]
        batch_size: usize,

        /// Indexing engine backend: 'tantivy' (disk-backed streaming, memory < 150MB) or 'unified' (in-memory bincode)
        #[arg(long, default_value = "tantivy")]
        engine: String,

        /// Use fast mock embedder (offline testing) instead of ONNX model
        #[arg(long)]
        mock_embedder: bool,
    },

    /// Query indexed ZIM archives
    Search {
        /// Search query (keywords or natural language question)
        query: String,

        /// Path to a .zim file or directory of archives (default: current directory or /mnt/extradisk/kiwix)
        #[arg(short, long, default_value = ".")]
        path: PathBuf,

        /// Explicit kiwix / zim directory override
        #[arg(long, visible_alias = "kiwix-dir", alias = "zim-dir")]
        kiwix_dir: Option<PathBuf>,

        /// Prebuilt index file or index directory
        #[arg(short, long)]
        index: Option<PathBuf>,

        /// Directory where indexes are located
        #[arg(long, visible_alias = "indices-dir")]
        index_dir: Option<PathBuf>,

        /// Specific archive name to search if path is a directory
        #[arg(short, long)]
        archive: Option<String>,

        /// Search mode override: 'semantic', 'hybrid', or 'bm25'
        #[arg(short, long)]
        mode: Option<String>,

        /// Number of top results to return
        #[arg(short, long, default_value = "5")]
        top: usize,

        /// Pagination offset
        #[arg(long, default_value = "0")]
        offset: usize,

        /// If index file is missing, automatically build an in-memory index on the fly
        #[arg(long, default_value_t = true)]
        auto_index: bool,
    },

    /// Launch interactive web interface and article viewer for the entire library
    Serve {
        /// Path to directory of archives or a specific .zim file (default: current directory or /mnt/extradisk/kiwix)
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Explicit kiwix / zim directory override
        #[arg(long, visible_alias = "kiwix-dir", alias = "zim-dir")]
        kiwix_dir: Option<PathBuf>,

        /// Directory where indexes are located
        #[arg(long, visible_alias = "index", alias = "indices-dir")]
        index_dir: Option<PathBuf>,

        /// Host address to bind
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port to listen on
        #[arg(short, long, default_value = "8080")]
        port: u16,

        /// Automatically build index if missing
        #[arg(long, default_value_t = true)]
        auto_index: bool,
    },

    /// Run as a Model Context Protocol (MCP) server over standard I/O (stdio) for LLMs
    Mcp {
        /// Directory containing .zim archive files (default: current directory or /mnt/extradisk/kiwix)
        #[arg(
            long,
            visible_alias = "kiwix-dir",
            alias = "kiwix_dir",
            default_value = "."
        )]
        zim_dir: PathBuf,

        /// Directory storing per-archive Tantivy index folders `<stem>.tantivy/` holding meta.json (BM25) and optional vectors.meta.json (semantic vectors) (default: /mnt/extradisk/kiwixidx)
        #[arg(
            long,
            visible_alias = "index",
            alias = "indices-dir",
            alias = "indices"
        )]
        index_dir: Option<PathBuf>,

        /// Automatically build BM25 lexical indexes on startup for all archives; semantic vectors stay opt-in per archive through the 'index_archive' MCP tool with "semantic": true
        #[arg(long, default_value_t = false)]
        auto_index: bool,
    },
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(suffix) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(suffix);
        }
    }
    p.to_path_buf()
}

fn resolve_paths(
    specified_path: &Path,
    specified_index_dir: Option<PathBuf>,
) -> (PathBuf, PathBuf) {
    let expanded_path = expand_tilde(specified_path);
    let zim_dir = if expanded_path == Path::new(".") {
        let has_local_zim = std::fs::read_dir(".")
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("zim"))
            })
            .unwrap_or(false);

        if !has_local_zim && Path::new(DEFAULT_ZIM_DIR).exists() {
            PathBuf::from(DEFAULT_ZIM_DIR)
        } else {
            expanded_path
        }
    } else {
        expanded_path
    };

    let index_dir = if let Some(dir) = specified_index_dir {
        expand_tilde(&dir)
    } else {
        PathBuf::from(DEFAULT_INDEX_DIR)
    };

    (zim_dir, index_dir)
}

fn default_index_path(zim_path: &Path, index_dir: Option<&Path>) -> PathBuf {
    let stem = zim_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("index");
    if let Some(dir) = index_dir {
        return dir.join(format!("{}.tantivy", stem));
    }

    if Path::new(DEFAULT_INDEX_DIR).is_dir() {
        return Path::new(DEFAULT_INDEX_DIR).join(format!("{}.tantivy", stem));
    }

    let parent = zim_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{}.tantivy", stem))
}

/// Resolves the user-facing index flags into a semantic-vector scope.
///
/// Precedence:
/// 1. `--semantic-archive` (repeatable) wins over `--semantic` and over an explicit mode, and yields
///    [`SemanticScope::Only`].
/// 2. `--semantic` yields [`SemanticScope::All`].
/// 3. An explicit `--mode hybrid|semantic` implies [`SemanticScope::All`] (naming the mode is itself
///    an opt-in).
/// 4. An absent `--mode` (or `--mode bm25`) with no semantic flags implies [`SemanticScope::None`],
///    i.e. BM25-only with no dense vectors — the product default.
///
/// A malformed mode string is always a hard error, even when the semantic flags alone would decide
/// the scope. Pure and free-standing: it never touches the filesystem or a `Cli` instance, so the
/// mapping stays trivially unit-testable.
fn resolve_semantic_scope(
    mode: Option<&str>,
    semantic: bool,
    semantic_archive: &[String],
) -> anyhow::Result<SemanticScope> {
    let parsed_mode = match mode {
        Some(raw) => Some(raw.parse::<IndexMode>().map_err(|e| anyhow::anyhow!(e))?),
        None => None,
    };

    if !semantic_archive.is_empty() {
        return Ok(SemanticScope::Only(semantic_archive.to_vec()));
    }
    if semantic {
        return Ok(SemanticScope::All);
    }

    Ok(match parsed_mode {
        None | Some(IndexMode::BM25Only) => SemanticScope::None,
        Some(IndexMode::Hybrid) | Some(IndexMode::SemanticOnly) => SemanticScope::All,
    })
}

/// Refuse an explicit dense-vector opt-in that this binary cannot possibly honour.
///
/// `scope_enabled` is the resolved opt-in decision (i.e. [`SemanticScope::is_enabled`]: `true` for
/// `--semantic`, any `--semantic-archive` selector, or an explicit `--mode hybrid|semantic`). The
/// ONNX-free default build (no `semantic` Cargo feature) contains no embedding model whatsoever, so
/// such a request is rejected up front with the exact rebuild command — it must never be answered
/// with a silent BM25 fallback, because the user explicitly asked for vectors.
///
/// `mock_embedder` (`--mock-embedder`) is the documented offline escape hatch: [`MockEmbedder`] is
/// compiled in every configuration, so an opt-in that injects it is still a genuine (if synthetic)
/// vector run and is therefore not rejected.
///
/// Pure — no filesystem, no ZIM access, no embedder construction — so the fail-fast decision stays
/// unit-testable in both feature configurations without a fixture.
fn semantic_request_supported(scope_enabled: bool, mock_embedder: bool) -> anyhow::Result<()> {
    if scope_enabled && !SEMANTIC_SUPPORTED && !mock_embedder {
        // Assembled at runtime from the canonical sentence and the canonical command (audit A11), so
        // the CLI fail-fast, the library and the MCP refusal stay one sentence rather than three
        // hand-kept copies. The rendered text is byte-for-byte what it always was.
        anyhow::bail!(
            "{}\n  This binary was compiled without the 'semantic' Cargo feature and therefore \
             contains no embedding model. Choose one: rebuild with `{}`, drop --semantic / \
             --semantic-archive / --mode hybrid|semantic for a BM25-only run, or pass \
             --mock-embedder for an offline run that generates synthetic vectors.",
            SEMANTIC_FEATURE_UNAVAILABLE,
            SEMANTIC_REBUILD_COMMAND,
        );
    }
    Ok(())
}

/// Say out loud when an ONNX-free build still runs the vector pass because `--mock-embedder`
/// injected the always-compiled offline embedder, so no output ever implies real ONNX embeddings.
fn note_mock_embedder_in_onnx_free_build(mock_embedder: bool) {
    if mock_embedder && !SEMANTIC_SUPPORTED {
        eprintln!(
            "[zim-search] Notice: this binary was compiled without the 'semantic' feature, so the \
             vector pass uses the offline built-in mock embedder (--mock-embedder) and no ONNX model. \
             Rebuild with `{}` for real dense embeddings.",
            SEMANTIC_REBUILD_COMMAND
        );
    }
}

/// The one build-honesty line an ONNX-free `index` run prints up front, or `None` when this binary
/// really does carry dense-vector support.
///
/// Kept separate from the printer, and free of any I/O, so the wording and the applicability rule
/// are unit-testable without capturing stderr. It exists because the clap help is feature-neutral:
/// each flag is declared exactly once (audit N1), so the help text cannot name the rebuild.
fn semantic_feature_absent_note() -> Option<&'static str> {
    if SEMANTIC_SUPPORTED {
        None
    } else {
        Some(
            "Note: this binary was built without the 'semantic' feature; dense vectors are unavailable \
             (rebuild with `cargo build --features semantic`).",
        )
    }
}

/// Print [`semantic_feature_absent_note`] at the start of the `index` handler, at most once per run.
///
/// The one-shot guard makes the "never twice per run" contract structural — no caller can print the
/// line twice — and the `bool` return (was the line printed?) is what the bin tests assert.
fn note_semantic_feature_absent() -> bool {
    static PRINTED: AtomicBool = AtomicBool::new(false);

    let Some(note) = semantic_feature_absent_note() else {
        return false;
    };
    if PRINTED.swap(true, Ordering::Relaxed) {
        return false;
    }
    eprintln!("{note}");
    true
}

/// Human-readable label for a resolved [`SemanticScope`], used in the indexing summary output.
fn describe_semantic_scope(scope: &SemanticScope) -> String {
    match scope {
        SemanticScope::None => "off (BM25 only)".to_string(),
        SemanticScope::All => "all archives".to_string(),
        SemanticScope::Only(selectors) => {
            format!(
                "{} selected archive(s): {}",
                selectors.len(),
                selectors.join(", ")
            )
        }
    }
}

/// Interpret the `index_mode` marker reported by [`ArchiveManager::list_archives`] (`"Hybrid"` or
/// `"BM25"` today, written by whichever build indexed the archive), answering whether that index
/// claims dense semantic vectors.
///
/// Case-insensitive and accepting both vector markers, deliberately mirroring the web UI's probe in
/// `crate::server::index_mode_has_vectors`: the CLI and the web UI must not reach opposite
/// conclusions about the same library (audit A3). A lowercase `hybrid`/`semantic` marker — written by
/// other tooling or an older build — therefore counts here too. Pure: no filesystem, no engine.
fn index_mode_has_vectors(index_mode: Option<&str>) -> bool {
    match index_mode {
        Some(mode) => {
            let mode = mode.to_ascii_lowercase();
            mode == "hybrid" || mode == "semantic"
        }
        None => false,
    }
}

/// Whether [`SearchEngine`] will print its own BM25-fallback notice for this run, so the CLI must not
/// repeat it (audit A4: one user-facing fallback line per run).
///
/// The engine speaks up exactly when a vector mode is actually attempted and the vectors cannot be
/// queried: an explicit `--mode semantic|hybrid` with no usable vectors (see
/// `SearchEngine::search_semantic` / `search_hybrid`). An absent mode auto-detects, which resolves to
/// BM25 in that same situation and prints nothing, and an explicit `--mode bm25` never asks for
/// vectors at all — in both cases the CLI has to say it itself. Pure, no I/O.
fn engine_announces_bm25_fallback(search_mode: Option<SearchMode>, vectors_usable: bool) -> bool {
    !vectors_usable
        && matches!(
            search_mode,
            Some(SearchMode::Semantic) | Some(SearchMode::Hybrid)
        )
}

/// Opt-in gate for the single-file indexing path, where no [`zim_search::ArchiveManager`] exists.
/// Mirrors `ArchiveManager::semantic_in_scope`: identifiers may be an exact `.zim` filename, a file
/// stem, or an archive name, compared after stripping a trailing `.zim`.
fn semantic_scope_covers_file(scope: &SemanticScope, filename: &str, stem: &str) -> bool {
    match scope {
        SemanticScope::None => false,
        SemanticScope::All => true,
        SemanticScope::Only(selectors) => selectors.iter().any(|selector| {
            let selector = selector.trim().trim_end_matches(".zim");
            !selector.is_empty()
                && (filename.trim().trim_end_matches(".zim") == selector
                    || stem.trim().trim_end_matches(".zim") == selector)
        }),
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Mcp {
            zim_dir,
            index_dir,
            auto_index,
        } => {
            let (actual_zim_dir, actual_index_dir) = resolve_paths(&zim_dir, index_dir);
            let manager = ArchiveManager::new(&actual_zim_dir, &actual_index_dir)?;
            zim_search::run_mcp_server(manager, auto_index)?;
        }

        Commands::Info {
            path,
            kiwix_dir,
            index_dir,
        } => {
            let target_path = kiwix_dir.unwrap_or(path);
            let (actual_path, actual_index_dir) = resolve_paths(&target_path, index_dir);
            if actual_path.is_dir() {
                let manager = ArchiveManager::new(&actual_path, &actual_index_dir)?;
                let archives = manager.list_archives()?;

                let total_size: u64 = archives.iter().map(|a| a.size_bytes).sum();
                let total_entries: u32 = archives.iter().map(|a| a.total_entries).sum();
                let indexed_count = archives.iter().filter(|a| a.is_indexed).count();

                println!("============================================================");
                println!("  ZIM Library Overview");
                println!("============================================================");
                println!("  Directory:       {}", actual_path.display());
                println!("  Index Directory: {}", actual_index_dir.display());
                println!("  Total Archives:  {}", archives.len());
                println!("  Indexed:         {}/{}", indexed_count, archives.len());
                println!("  Total Entries:   {}", total_entries);
                println!(
                    "  Total Disk Size: {:.2} MB",
                    total_size as f64 / 1_048_576.0
                );
                println!("------------------------------------------------------------");
                println!(
                    "{:<20} {:<30} {:<10} {:<10} FILE",
                    "NAME", "TITLE", "ENTRIES", "INDEXED"
                );
                println!("{}", "-".repeat(95));
                for a in archives {
                    let title_trunc = if a.title.len() > 28 {
                        format!("{}...", &a.title[..25])
                    } else {
                        a.title
                    };
                    let indexed_str = if a.is_indexed {
                        a.index_mode.unwrap_or_else(|| "Yes".to_string())
                    } else {
                        "No".to_string()
                    };
                    println!(
                        "{:<20} {:<30} {:<10} {:<10} {}",
                        a.name, title_trunc, a.total_entries, indexed_str, a.filename
                    );
                }
                println!("============================================================");
                return Ok(());
            }

            let archive = ZimArchive::open(&actual_path)?;
            let meta = archive.metadata();

            println!("============================================================");
            println!("  ZIM Archive Information");
            println!("============================================================");
            println!("  File:            {}", actual_path.display());
            println!("  Title:           {}", meta.title);
            println!("  Date:            {}", meta.date);
            println!("  Language:        {}", meta.language);
            println!("  Creator:         {}", meta.creator);
            println!("  Description:     {}", meta.description);
            println!("  Tags:            {}", meta.tags);
            println!("  Total Entries:   {}", meta.total_entries);
            println!(
                "  Illustration:    {}",
                if meta.has_illustration {
                    "Yes (48x48)"
                } else {
                    "No"
                }
            );
            println!("============================================================");
        }

        Commands::List {
            path,
            kiwix_dir,
            index_dir,
            filter,
            limit,
        } => {
            let target_path = kiwix_dir.unwrap_or(path);
            let (actual_path, actual_index_dir) = resolve_paths(&target_path, index_dir);
            if actual_path.is_dir() {
                let manager = ArchiveManager::new(&actual_path, &actual_index_dir)?;
                let archives = manager.list_archives()?;
                println!(
                    "Found {} ZIM archives in directory '{}':",
                    archives.len(),
                    actual_path.display()
                );
                println!(
                    "{:<25} {:<35} {:<10} {:<12} FILE",
                    "NAME", "TITLE", "ENTRIES", "INDEXED"
                );
                println!("{}", "-".repeat(105));
                for a in archives {
                    let title_trunc = if a.title.len() > 33 {
                        format!("{}...", &a.title[..30])
                    } else {
                        a.title
                    };
                    let indexed_str = if a.is_indexed {
                        a.index_mode.unwrap_or_else(|| "Yes".to_string())
                    } else {
                        "No".to_string()
                    };
                    println!(
                        "{:<25} {:<35} {:<10} {:<12} {}",
                        a.name, title_trunc, a.total_entries, indexed_str, a.filename
                    );
                }
                return Ok(());
            }

            let archive = ZimArchive::open(&actual_path)?;
            let entries = archive.list_entries(filter.as_deref(), limit)?;

            println!(
                "Found {} entries in {} (showing up to {}):",
                entries.len(),
                actual_path.display(),
                limit
            );
            println!(
                "{:<6} {:<4} {:<30} {:<35} URL",
                "INDEX", "NS", "MIME", "TITLE"
            );
            println!("{}", "-".repeat(105));

            for e in entries {
                let title_trunc = if e.title.len() > 33 {
                    format!("{}...", &e.title[..30])
                } else {
                    e.title
                };
                let mime_trunc = if e.mime_type.len() > 28 {
                    format!("{}...", &e.mime_type[..25])
                } else {
                    e.mime_type
                };

                println!(
                    "{:<6} {:<4} {:<30} {:<35} {}",
                    e.index, e.namespace, mime_trunc, title_trunc, e.url
                );
            }
        }

        Commands::Read {
            path,
            url,
            archive,
            raw,
        } => {
            let (actual_path, actual_index_dir) = resolve_paths(&path, None);
            let target_archive = if actual_path.is_dir() {
                let manager = ArchiveManager::new(&actual_path, &actual_index_dir)?;
                if let Some(ref a) = archive {
                    manager.get_archive(a)?
                } else {
                    let archives = manager.list_archives()?;
                    let first = archives
                        .first()
                        .ok_or_else(|| anyhow::anyhow!("No ZIM archives found in directory"))?;
                    manager.get_archive(&first.name)?
                }
            } else {
                Arc::new(ZimArchive::open(&actual_path)?)
            };

            match target_archive.read_by_url(&url)? {
                Some((mime, data)) => {
                    if raw {
                        std::io::Write::write_all(&mut std::io::stdout(), &data)?;
                    } else if mime.starts_with("text/html") {
                        let html_str = String::from_utf8_lossy(&data);
                        let clean = clean_html(&html_str);
                        println!("{}", clean);
                    } else {
                        let text = String::from_utf8_lossy(&data);
                        println!("{}", text);
                    }
                }
                None => {
                    eprintln!("Error: Entry '{}' not found", url);
                    std::process::exit(1);
                }
            }
        }

        Commands::Index {
            path,
            kiwix_dir,
            output,
            index_dir,
            force,
            mode,
            semantic,
            semantic_archive,
            chunk_words,
            overlap_words,
            max_chunks_per_article,
            semantic_lead_only,
            max_semantic_chunks,
            batch_size,
            engine: _,
            mock_embedder,
        } => {
            // Build honesty for a feature-neutral clap help: an ONNX-free build says up front, once
            // per run, that dense vectors are unavailable here. The fail-fast guard below and
            // `VECTOR_BUILD_HINT` on `search` carry the same message where it is actionable.
            note_semantic_feature_absent();

            let target_path = kiwix_dir.unwrap_or(path);
            let (actual_path, actual_index_dir) = resolve_paths(&target_path, index_dir);

            // Dense semantic vectors are strictly opt-in: one scope decision drives Phase 2 in BOTH
            // branches (directory batch + single file). Absent --semantic / --semantic-archive and
            // without an explicit --mode hybrid|semantic, nothing gets embedded.
            let scope = resolve_semantic_scope(mode.as_deref(), semantic, &semantic_archive)?;
            // Fail fast BEFORE any indexing work: an explicit vector opt-in in an ONNX-free build is
            // an error, never a silent BM25 fallback. This single guard runs ahead of the
            // directory/single-file branch split, so BOTH indexing paths are covered.
            semantic_request_supported(scope.is_enabled(), mock_embedder)?;
            let semantic_opts = SemanticOptions {
                lead_only: semantic_lead_only,
                max_chunks: max_semantic_chunks,
                batch_size: Some(batch_size),
            };

            // Directory / Library batch indexing mode
            if actual_path.is_dir() {
                let manager = ArchiveManager::new(&actual_path, &actual_index_dir)?;
                let archives = manager.list_archives()?;

                println!("Scanning directory: {}", actual_path.display());
                println!("Index directory:   {}", actual_index_dir.display());
                println!("Found {} ZIM archives", archives.len());

                println!("\n=== Phase 1/2: Building disk-backed Tantivy full-text index for all archives ===");
                let mut count = 0;
                for (i, a) in archives.iter().enumerate() {
                    let tantivy_path = manager.tantivy_path_for(&a.filename);
                    if !force && tantivy_path.join("meta.json").exists() {
                        println!(
                            "[{}/{}] Tantivy index exists: {}",
                            i + 1,
                            archives.len(),
                            a.name
                        );
                        continue;
                    }

                    println!(
                        "[{}/{}] Indexing full-text (Tantivy): {} ({})",
                        i + 1,
                        archives.len(),
                        a.name,
                        a.title
                    );
                    let t0 = Instant::now();
                    let chunks = manager.build_tantivy_for_archive(&a.name, force)?;
                    println!(
                        "[{}/{}] Indexed {}: {} chunks in {:.2}s",
                        i + 1,
                        archives.len(),
                        a.name,
                        chunks,
                        t0.elapsed().as_secs_f32()
                    );
                    count += 1;
                }
                println!("Phase 1/2 complete: {} archives processed.", count);

                if scope.is_enabled() {
                    // Honour --mock-embedder here too: inject it BEFORE the semantic pass so the
                    // manager never falls back to the ONNX model download (same 384-dim default the
                    // single-file path uses).
                    if mock_embedder {
                        manager.set_embedder(Arc::new(MockEmbedder::new(384)));
                    }
                    note_mock_embedder_in_onnx_free_build(mock_embedder);

                    println!("\n=== Phase 2/2: Generating semantic vector embeddings (opt-in) ===");
                    println!("Semantic scope: {}", describe_semantic_scope(&scope));
                    if let SemanticScope::Only(selectors) = &scope {
                        for selector in selectors {
                            let only_this = SemanticScope::Only(vec![selector.clone()]);
                            if !archives
                                .iter()
                                .any(|a| manager.semantic_in_scope(&a.name, &only_this))
                            {
                                eprintln!(
                                    "[zim-search] Notice: --semantic-archive '{}' matched no archive in '{}'",
                                    selector,
                                    actual_path.display()
                                );
                            }
                        }
                    }

                    let mut sem_generated = 0;
                    let mut sem_existing = 0;
                    let mut sem_in_scope = 0;
                    for (i, a) in archives.iter().enumerate() {
                        if !manager.semantic_in_scope(&a.name, &scope) {
                            continue;
                        }
                        sem_in_scope += 1;

                        let tantivy_path = manager.tantivy_path_for(&a.filename);
                        if !force && tantivy_path.join("vectors.meta.json").exists() {
                            println!("[{}/{}] Vectors exist: {}", i + 1, archives.len(), a.name);
                            sem_existing += 1;
                            continue;
                        }

                        println!(
                            "\n[{}/{}] Embedding: {} ({})",
                            i + 1,
                            archives.len(),
                            a.name,
                            a.title
                        );
                        let t0 = Instant::now();
                        manager.populate_tantivy_semantic_for_archive(
                            &a.name,
                            force,
                            &semantic_opts,
                        )?;
                        println!(
                            "[{}/{}] Vectors complete for {}: {:.2}s",
                            i + 1,
                            archives.len(),
                            a.name,
                            t0.elapsed().as_secs_f32()
                        );
                        sem_generated += 1;
                    }
                    println!(
                        "Phase 2/2 complete: {} archive(s) received semantic vectors, {} already had them \
                         ({} of {} archive(s) in scope: {}).",
                        sem_generated,
                        sem_existing,
                        sem_in_scope,
                        archives.len(),
                        describe_semantic_scope(&scope)
                    );
                } else {
                    println!(
                        "\nSemantic vectors skipped: pass --semantic (or --semantic-archive <name>) to generate them."
                    );
                }

                println!("\nBatch indexing finished.");
                return Ok(());
            }

            // Single file indexing mode
            let tantivy_dir =
                output.unwrap_or_else(|| default_index_path(&actual_path, Some(&actual_index_dir)));

            let has_bm25 = tantivy_dir.join("meta.json").exists();
            let has_vectors = tantivy_dir.join("vectors.meta.json").exists();

            let file_name = actual_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let file_stem = actual_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let vectors_for_this_file = semantic_scope_covers_file(&scope, &file_name, &file_stem);

            if !force && has_bm25 && (has_vectors || !vectors_for_this_file) {
                println!("Tantivy index already exists at: {}", tantivy_dir.display());
                println!("Use --force to rebuild from scratch.");
                if !has_vectors {
                    println!(
                        "Semantic vectors skipped: pass --semantic (or --semantic-archive <name>) to generate them."
                    );
                }
                return Ok(());
            }

            if force && tantivy_dir.exists() {
                let _ = std::fs::remove_dir_all(&tantivy_dir);
            }

            println!("Opening ZIM archive: {}", actual_path.display());
            let archive = ZimArchive::open(&actual_path)?;
            let meta = archive.metadata();
            println!("Archive title: {}", meta.title);

            // Semantic scoping knobs (--semantic-lead-only / --max-semantic-chunks / --batch-size)
            // are fed from the same SemanticOptions struct the batch path uses.
            let mut builder = IndexBuilder::new()
                .with_chunk_size(chunk_words, overlap_words)
                .with_semantic_lead_only(semantic_opts.lead_only)
                .with_max_chunks_per_article(max_chunks_per_article);
            if let Some(max_chunks) = semantic_opts.max_chunks {
                builder = builder.with_max_semantic_chunks(Some(max_chunks));
            }
            if let Some(semantic_batch_size) = semantic_opts.batch_size {
                builder = builder.with_batch_size(semantic_batch_size);
            }

            if !has_bm25 || force {
                println!(
                    "Building disk-backed Tantivy full-text index in: {}",
                    tantivy_dir.display()
                );
                let t0 = Instant::now();
                let chunks_count = builder.build_tantivy_stage(&archive, &tantivy_dir)?;
                println!(
                    "\n[Tantivy] Disk-backed full-text indexing complete in {:.2}s ({} chunks indexed across entire archive).",
                    t0.elapsed().as_secs_f32(),
                    chunks_count
                );
            } else {
                println!(
                    "[Tantivy] Existing full-text BM25 index found in: {}",
                    tantivy_dir.display()
                );
            }

            if vectors_for_this_file {
                println!(
                    "\n=== Phase 2/2: Generating semantic vector embeddings (disk-backed) ==="
                );
                note_mock_embedder_in_onnx_free_build(mock_embedder);
                let embedder: Arc<dyn Embedder> = if mock_embedder {
                    Arc::new(MockEmbedder::new(384))
                } else {
                    // Feature-agnostic factory: the ONNX model with `--features semantic`, an
                    // actionable "rebuild with --features semantic" error otherwise. It stays here as
                    // a second line of defence even though `semantic_request_supported` already
                    // refused such a run, so no code path ever constructs an embedder blindly.
                    default_embedder()?
                };
                let t1 = Instant::now();
                let vec_count = builder.populate_tantivy_semantic_stage(
                    &archive,
                    &tantivy_dir,
                    embedder.as_ref(),
                )?;
                println!(
                    "\n[Tantivy] Semantic vector embeddings complete in {:.2}s ({} vectors saved).",
                    t1.elapsed().as_secs_f32(),
                    vec_count
                );
            } else if scope.is_enabled() {
                println!(
                    "\n[Semantic] Vectors skipped for '{}': not selected by --semantic-archive (pass --semantic for every archive).",
                    file_name
                );
            } else {
                println!(
                    "\nSemantic vectors skipped: pass --semantic (or --semantic-archive <name>) to generate them."
                );
            }
        }

        Commands::Search {
            query,
            path,
            kiwix_dir,
            index,
            index_dir,
            archive,
            mode,
            top,
            offset,
            auto_index,
        } => {
            let search_mode = match mode.as_deref() {
                Some(m) => m.parse::<SearchMode>().ok(),
                None => Some(SearchMode::BM25),
            };
            let target_path = kiwix_dir.unwrap_or(path);
            let effective_index_dir =
                index_dir.or_else(|| index.as_ref().filter(|p| p.is_dir()).cloned());
            let (actual_path, actual_index_dir) = resolve_paths(&target_path, effective_index_dir);

            // At most ONE CLI-level BM25-fallback notice per run (audit A4): `SearchEngine` prints its
            // own notice whenever an explicitly requested vector mode has to degrade, so the CLI
            // defers to it instead of stacking a second, near-identical sentence.
            let mut cli_fallback_notice_emitted = false;

            if actual_path.is_dir() {
                let manager = ArchiveManager::new(&actual_path, &actual_index_dir)?;
                let mut effective_mode = search_mode;

                if !matches!(search_mode, Some(SearchMode::BM25)) {
                    let has_vectors = match archive.as_deref() {
                        Some(sel) => {
                            let tantivy_path = manager.tantivy_path_for(sel);
                            tantivy_path.join("vectors.meta.json").exists()
                        }
                        None => manager
                            .list_archives()
                            .map(|l| {
                                l.iter()
                                    .any(|a| index_mode_has_vectors(a.index_mode.as_deref()))
                            })
                            .unwrap_or(false),
                    };
                    if !has_vectors {
                        if !cli_fallback_notice_emitted {
                            eprintln!(
                                "[zim-search] No dense semantic vectors in scope: search falls back to BM25 (lexical) retrieval. \
                                 Index with `zim-search index --semantic` (or `--semantic-archive <name>`) to generate vectors.{}",
                                VECTOR_BUILD_HINT
                            );
                        }
                        effective_mode = Some(SearchMode::BM25);
                    } else if !SEMANTIC_SUPPORTED {
                        if !cli_fallback_notice_emitted {
                            eprintln!(
                                "[zim-search] Notice: an index here carries dense semantic vectors, but this binary was \
                                 compiled without the 'semantic' feature and cannot read them: retrieval runs BM25 \
                                 (lexical) only. Rebuild with `{}` to use those vectors.",
                                SEMANTIC_REBUILD_COMMAND
                            );
                        }
                        effective_mode = Some(SearchMode::BM25);
                    }
                }

                let t0 = Instant::now();
                let paginated = manager.search(
                    archive.as_deref(),
                    &query,
                    effective_mode,
                    offset,
                    top,
                    auto_index,
                )?;
                let elapsed = t0.elapsed();

                println!("============================================================");
                println!(
                    "  Query: '{}' (scope: {})",
                    query,
                    archive.as_deref().unwrap_or("All Archives")
                );
                println!(
                    "  Retrieved {} results (out of {} matches) in {:.2}ms",
                    paginated.results.len(),
                    paginated.total_matches,
                    elapsed.as_secs_f64() * 1000.0
                );
                println!("============================================================");

                for r in paginated.results {
                    println!(
                        "\n[{}] Score: {:.4} | {} ({})",
                        r.result.rank, r.result.score, r.result.title, r.archive_title
                    );
                    println!("    URL: {}", r.result.article_url);
                    println!("    Snippet: {}", r.result.snippet);
                }
                return Ok(());
            }

            let stem = actual_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("archive");
            let default_tantivy_dir = actual_index_dir.join(format!("{}.tantivy", stem));
            let index_path =
                index.unwrap_or_else(|| default_index_path(&actual_path, Some(&actual_index_dir)));
            let zim_archive = ZimArchive::open(&actual_path)?;

            let tantivy_dir = if index_path.is_dir() && index_path.join("meta.json").exists() {
                println!(
                    "Opening disk-backed Tantivy index from: {}",
                    index_path.display()
                );
                index_path
            } else if default_tantivy_dir.join("meta.json").exists() {
                println!(
                    "Opening disk-backed Tantivy index from: {}",
                    default_tantivy_dir.display()
                );
                default_tantivy_dir
            } else if auto_index {
                println!("Index not found. Building disk-backed Tantivy full-text index...");
                let builder = IndexBuilder::new();
                builder.build_tantivy_stage(&zim_archive, &default_tantivy_dir)?;
                default_tantivy_dir
            } else {
                eprintln!(
                    "Index not found at: {} or {}\nUse `zim-search index {}` to create it, or pass `--auto-index`.",
                    index_path.display(),
                    default_tantivy_dir.display(),
                    actual_path.display()
                );
                std::process::exit(1);
            };

            let tantivy = TantivyEngine::open(&tantivy_dir)?;
            let needs_embedder = matches!(
                search_mode,
                Some(SearchMode::Semantic) | Some(SearchMode::Hybrid)
            ) || (search_mode.is_none() && tantivy.has_vectors());

            let embedder: Option<Arc<dyn Embedder>> = if needs_embedder && tantivy.has_vectors() {
                // Graceful by design: an index can carry vectors this ONNX-free binary cannot read,
                // and search must degrade to BM25 rather than fail the command.
                match default_embedder() {
                    Ok(embedder) => Some(embedder),
                    Err(e) => {
                        // One user-facing fallback line (audit A4): the factory sentence and
                        // `VECTOR_BUILD_HINT` say the same thing, and an explicit
                        // `--mode semantic|hybrid` makes `SearchEngine` report the identical fallback
                        // itself, so the CLI stays quiet in that case.
                        if !engine_announces_bm25_fallback(search_mode, false)
                            && !cli_fallback_notice_emitted
                        {
                            let detail = if SEMANTIC_SUPPORTED {
                                format!(" Cause: {e}")
                            } else {
                                VECTOR_BUILD_HINT.to_string()
                            };
                            eprintln!(
                                "[zim-search] This index carries dense semantic vectors that this binary cannot read: \
                                 search falls back to BM25 (lexical) retrieval.{detail}"
                            );
                            cli_fallback_notice_emitted = true;
                        }
                        None
                    }
                }
            } else {
                None
            };

            // Same opt-in visibility for single-file indexes: name the fallback and the flag that
            // would have produced vectors, instead of degrading silently to BM25 — once per run, and
            // never on top of the engine's own notice (audit A4).
            if !tantivy.has_vectors()
                && !matches!(search_mode, Some(SearchMode::BM25))
                && !engine_announces_bm25_fallback(search_mode, false)
                && !cli_fallback_notice_emitted
            {
                eprintln!(
                    "[zim-search] This index has no dense semantic vectors: search falls back to BM25 (lexical) retrieval. \
                     Index with `zim-search index --semantic` (or `--semantic-archive <name>`) to generate vectors.{}",
                    VECTOR_BUILD_HINT
                );
            }

            let engine =
                SearchEngine::new(Arc::new(tantivy), embedder, Some(Arc::new(zim_archive)));

            eprintln!(
                "[Search] Query: \"{}\" | Target: '{}' | Mode: {:?}",
                query,
                actual_path.display(),
                search_mode.unwrap_or(SearchMode::BM25)
            );
            let t0 = Instant::now();
            let results = engine.search(&query, search_mode, offset + top)?;
            let elapsed = t0.elapsed();
            eprintln!(
                "[Search] Query finished in {:.2}ms ({} hit(s))",
                elapsed.as_secs_f64() * 1000.0,
                results.len()
            );

            let paged_results: Vec<_> = results.into_iter().skip(offset).take(top).collect();

            println!("============================================================");
            println!("  Query: '{}'", query);
            println!(
                "  Retrieved {} results in {:.2}ms",
                paged_results.len(),
                elapsed.as_secs_f64() * 1000.0
            );
            println!("============================================================");

            if paged_results.is_empty() {
                println!("No matching results found.");
            } else {
                for r in paged_results {
                    println!("\n[{}] Score: {:.4} | {}", r.rank, r.score, r.title);
                    println!("    URL: {}", r.article_url);
                    println!("    Snippet: {}", r.snippet);
                }
            }
        }

        Commands::Serve {
            path,
            kiwix_dir,
            index_dir,
            host,
            port,
            auto_index,
        } => {
            let target_path = kiwix_dir.unwrap_or(path);
            let (actual_path, actual_index_dir) = resolve_paths(&target_path, index_dir);
            let (zim_dir, actual_index_dir) = if actual_path.is_dir() {
                (actual_path, actual_index_dir)
            } else {
                let parent = actual_path.parent().unwrap_or(Path::new(".")).to_path_buf();
                (parent, actual_index_dir)
            };

            let manager = ArchiveManager::new(&zim_dir, &actual_index_dir)?;
            zim_search::run_server(manager, &host, port, auto_index)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_of(mode: Option<&str>, semantic: bool, archives: &[&str]) -> SemanticScope {
        let owned: Vec<String> = archives.iter().map(|s| s.to_string()).collect();
        resolve_semantic_scope(mode, semantic, &owned).expect("flag combination should be valid")
    }

    #[test]
    fn no_flags_means_no_semantic_vectors() {
        let scope = scope_of(None, false, &[]);
        assert_eq!(scope, SemanticScope::None);
        assert!(!scope.is_enabled());
    }

    #[test]
    fn mode_bm25_without_semantic_flags_means_no_semantic_vectors() {
        let scope = scope_of(Some("bm25"), false, &[]);
        assert_eq!(scope, SemanticScope::None);
        assert!(!scope.is_enabled());
    }

    #[test]
    fn semantic_flag_opts_in_for_every_archive() {
        let scope = scope_of(None, true, &[]);
        assert_eq!(scope, SemanticScope::All);
        assert!(scope.is_enabled());
    }

    #[test]
    fn semantic_archive_opts_in_for_named_archive_only() {
        let scope = scope_of(None, false, &["wikipedia"]);
        assert_eq!(scope, SemanticScope::Only(vec!["wikipedia".to_string()]));
        assert!(scope.is_enabled());
    }

    #[test]
    fn explicit_hybrid_mode_implies_all() {
        let scope = scope_of(Some("hybrid"), false, &[]);
        assert_eq!(scope, SemanticScope::All);
    }

    #[test]
    fn explicit_semantic_mode_implies_all() {
        let scope = scope_of(Some("semantic"), false, &[]);
        assert_eq!(scope, SemanticScope::All);
    }

    #[test]
    fn mode_bm25_combined_with_semantic_opts_in() {
        let scope = scope_of(Some("bm25"), true, &[]);
        assert_eq!(scope, SemanticScope::All);
    }

    #[test]
    fn semantic_archive_overrides_semantic_flag() {
        let scope = scope_of(Some("hybrid"), true, &["a.zim", "b"]);
        assert_eq!(
            scope,
            SemanticScope::Only(vec!["a.zim".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn empty_semantic_archive_list_does_not_narrow_the_scope() {
        let scope = scope_of(Some("bm25"), true, &[]);
        assert_eq!(scope, SemanticScope::All);
    }

    #[test]
    fn invalid_mode_string_is_rejected() {
        let owned: Vec<String> = Vec::new();
        let err = resolve_semantic_scope(Some("vector-hybrid"), false, &owned)
            .expect_err("unknown mode must be a hard error");
        assert!(err.to_string().contains("Invalid index mode"));
    }

    #[test]
    fn mode_aliases_are_accepted() {
        assert_eq!(scope_of(Some("keyword"), false, &[]), SemanticScope::None);
        assert_eq!(scope_of(Some("both"), false, &[]), SemanticScope::All);
        assert_eq!(scope_of(Some("VECTOR"), false, &[]), SemanticScope::All);
    }

    #[test]
    fn scope_gate_matches_name_stem_and_zim_filename() {
        let scope = SemanticScope::Only(vec!["wikipedia".to_string()]);
        assert!(semantic_scope_covers_file(
            &scope,
            "wikipedia.zim",
            "wikipedia"
        ));
        assert!(semantic_scope_covers_file(&scope, "wikipedia", "wikipedia"));
        assert!(!semantic_scope_covers_file(&scope, "other.zim", "other"));
        // Case-sensitive, mirroring `ArchiveManager::semantic_in_scope`.
        assert!(!semantic_scope_covers_file(
            &scope,
            "Wikipedia.zim",
            "Wikipedia"
        ));

        let dotted = SemanticScope::Only(vec!["wikipedia.zim".to_string()]);
        assert!(semantic_scope_covers_file(
            &dotted,
            "wikipedia.zim",
            "wikipedia"
        ));
        assert!(!semantic_scope_covers_file(&dotted, "", ""));
    }

    #[test]
    fn scope_gate_is_off_for_none_and_open_for_all() {
        assert!(!semantic_scope_covers_file(
            &SemanticScope::None,
            "wikipedia.zim",
            "wikipedia"
        ));
        assert!(semantic_scope_covers_file(
            &SemanticScope::All,
            "wikipedia.zim",
            "wikipedia"
        ));
    }

    // ---------------------------------------------------------------------------------
    // Build-honesty guard: an explicit semantic opt-in must be refused in an ONNX-free build
    // ---------------------------------------------------------------------------------

    #[cfg(not(feature = "semantic"))]
    #[test]
    fn explicit_opt_in_is_refused_in_an_onnx_free_build() {
        let err = semantic_request_supported(true, false).expect_err(
            "--semantic in a build without the 'semantic' feature must be a hard error",
        );
        let msg = err.to_string();
        // Pinned to the canonical sentence/command (audit A11) rather than to a re-typed copy of it:
        // the CLI, the library and the MCP protocol must keep refusing in the same words.
        assert!(
            msg.starts_with(SEMANTIC_FEATURE_UNAVAILABLE),
            "the refusal must open with the canonical sentence: {msg}"
        );
        assert!(msg.contains(SEMANTIC_REBUILD_COMMAND), "{msg}");
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn explicit_opt_in_is_accepted_when_vectors_are_compiled_in() {
        semantic_request_supported(true, false)
            .expect("a --features semantic build must accept an explicit opt-in");
    }

    #[test]
    fn no_opt_in_never_triggers_the_build_guard() {
        // BM25-only runs (the default) stay valid in every build configuration.
        semantic_request_supported(false, false)
            .expect("no opt-in must never be refused, whatever the build");
        semantic_request_supported(false, true)
            .expect("--mock-embedder without an opt-in must never be refused");
    }

    #[test]
    fn mock_embedder_keeps_an_explicit_opt_in_valid_in_every_build() {
        // MockEmbedder is compiled in with and without the feature, so an offline opt-in run stays
        // possible in the default build (the integration harness relies on this).
        semantic_request_supported(true, true)
            .expect("--mock-embedder must keep the opt-in working in both builds");
    }

    #[test]
    fn search_hint_names_the_rebuild_only_in_an_onnx_free_build() {
        if SEMANTIC_SUPPORTED {
            assert!(
                VECTOR_BUILD_HINT.is_empty(),
                "feature-on search output must stay unchanged"
            );
        } else {
            assert!(
                VECTOR_BUILD_HINT.contains("no dense-vector support"),
                "{VECTOR_BUILD_HINT}"
            );
            assert!(
                VECTOR_BUILD_HINT.contains(SEMANTIC_REBUILD_COMMAND),
                "the search hint must quote the canonical rebuild command: {VECTOR_BUILD_HINT}"
            );
        }
    }

    /// Audit A11 drift guard: the CLI's hand-maintained build-honesty wordings — the `search`
    /// `VECTOR_BUILD_HINT` const and the `index` note returned by `semantic_feature_absent_note()` —
    /// must keep quoting the canonical rebuild command owned by the library. They are `const` /
    /// `&'static str` literals that Rust cannot assemble out of another `const`, so the pin is this
    /// test: a reword that drops the instruction fails here instead of silently diverging from the
    /// library error and the MCP banner wording. The remaining CLI notices interpolate the anchor at
    /// runtime, so they cannot drift by construction.
    #[cfg(not(feature = "semantic"))]
    #[test]
    fn every_cli_build_honesty_wording_quotes_the_canonical_rebuild_command() {
        assert!(
            VECTOR_BUILD_HINT.contains(SEMANTIC_REBUILD_COMMAND),
            "VECTOR_BUILD_HINT drifted: {VECTOR_BUILD_HINT}"
        );

        let note = semantic_feature_absent_note().expect("an ONNX-free build must carry the note");
        assert!(
            note.contains(SEMANTIC_REBUILD_COMMAND),
            "semantic_feature_absent_note drifted: {note}"
        );

        let refusal = semantic_request_supported(true, false)
            .expect_err("an ONNX-free build must refuse an explicit vector opt-in")
            .to_string();
        assert!(
            refusal.contains(SEMANTIC_FEATURE_UNAVAILABLE),
            "the fail-fast must reuse the canonical sentence: {refusal}"
        );
        assert!(
            refusal.matches(SEMANTIC_REBUILD_COMMAND).count() >= 2,
            "the fail-fast lists the rebuild twice (recompile + choose-one): {refusal}"
        );
    }

    /// The `index` note exists only in an ONNX-free build, and says exactly one thing there.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_index_note_tracks_the_compiled_in_vector_support() {
        match semantic_feature_absent_note() {
            Some(note) => {
                assert!(
                    !SEMANTIC_SUPPORTED,
                    "a --features semantic build must have no note, got: {note}"
                );
                assert_eq!(note.lines().count(), 1, "one stderr line only:\n{note}");
                assert!(
                    note.starts_with("Note: this binary was built without the 'semantic' feature"),
                    "{note}"
                );
                assert!(
                    note.contains("rebuild with") && note.contains(SEMANTIC_REBUILD_COMMAND),
                    "the note must name the canonical rebuild command: {note}"
                );
            }
            None => assert!(SEMANTIC_SUPPORTED, "an ONNX-free build must carry the note"),
        }
    }

    // ---------------------------------------------------------------------------------
    // A3: the CLI and the web UI must agree on "does this library carry dense vectors?"
    // ---------------------------------------------------------------------------------

    #[test]
    fn both_vector_index_markers_count_case_insensitively() {
        for marker in [
            "hybrid", "Hybrid", "HYBRID", "semantic", "Semantic", "SEMANTIC",
        ] {
            assert!(
                index_mode_has_vectors(Some(marker)),
                "'{marker}' names a vector-carrying index"
            );
        }
    }

    #[test]
    fn lexical_or_absent_index_markers_mean_no_vectors() {
        assert!(!index_mode_has_vectors(Some("BM25")));
        assert!(!index_mode_has_vectors(Some("bm25")));
        assert!(
            !index_mode_has_vectors(None),
            "an unindexed or marker-less library must not be reported as vector-backed"
        );
    }

    // ---------------------------------------------------------------------------------
    // A4: one user-facing BM25-fallback notice per run, the engine's own notice included
    // ---------------------------------------------------------------------------------

    #[test]
    fn an_explicit_vector_mode_leaves_the_fallback_notice_to_the_engine() {
        for mode in [SearchMode::Semantic, SearchMode::Hybrid] {
            assert!(
                engine_announces_bm25_fallback(Some(mode), false),
                "{mode:?} without usable vectors makes `SearchEngine` print the fallback itself"
            );
        }
    }

    #[test]
    fn auto_detect_and_explicit_bm25_leave_the_fallback_notice_to_the_cli() {
        assert!(
            !engine_announces_bm25_fallback(None, false),
            "auto-detection resolves to BM25 without usable vectors and stays silent"
        );
        assert!(
            !engine_announces_bm25_fallback(Some(SearchMode::BM25), false),
            "an explicit lexical run never triggers a vector fallback notice"
        );
        assert!(
            !engine_announces_bm25_fallback(Some(SearchMode::Hybrid), true),
            "usable vectors mean no fallback and therefore no notice"
        );
    }

    /// The help text clap renders for `zim-search index --help`.
    ///
    /// clap answers `--help` with an error payload that carries the whole help block, so it can be
    /// captured in-process. Reading it from the real parser (instead of mirroring a constant) means
    /// these assertions check the exact strings of the build being compiled.
    fn index_help_text() -> String {
        Cli::try_parse_from(["zim-search", "index", "--help"])
            .expect_err("`--help` exits through an error that carries the rendered help")
            .to_string()
    }

    /// Collapse clap's line wrapping so assertions are about wording, not column width.
    fn flatten(help: &str) -> String {
        help.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[cfg(not(feature = "semantic"))]
    #[test]
    fn build_honesty_is_carried_by_runtime_behaviour_not_by_duplicated_help() {
        // Every flag is declared exactly once with feature-neutral help (audit N1), so the help text
        // no longer carries the build message at all — no duplicated strings, and no assertion about
        // how many times clap happens to render one.
        let help = flatten(&index_help_text());
        assert!(
            help.contains("--semantic"),
            "help must still list the flag:\n{help}"
        );
        assert!(
            help.contains("--semantic-archive") && help.contains("--semantic-source"),
            "help must still list the flag and its visible alias:\n{help}"
        );
        assert!(
            !help.contains("requires a build with --features semantic"),
            "the ONNX-free build must not rely on help wording for build honesty:\n{help}"
        );

        // Fail-fast helper: an explicit opt-in this binary cannot honour is a hard error that names
        // the exact rebuild command (behaviour, not help wording).
        let msg = semantic_request_supported(true, false)
            .expect_err("--semantic without the 'semantic' feature must be refused up front")
            .to_string();
        assert!(
            msg.starts_with(SEMANTIC_FEATURE_UNAVAILABLE),
            "the refusal must open with the canonical sentence: {msg}"
        );
        assert!(msg.contains(SEMANTIC_REBUILD_COMMAND), "{msg}");

        // Note helper: exactly one line naming the same rebuild, printed at most once per run.
        let note = semantic_feature_absent_note().expect("an ONNX-free build must carry the note");
        assert_eq!(
            note.lines().count(),
            1,
            "the note must be one line:\n{note}"
        );
        assert!(
            note.starts_with("Note: this binary was built without the 'semantic' feature"),
            "{note}"
        );
        assert!(note.contains("dense vectors are unavailable"), "{note}");
        assert!(
            note.contains("rebuild with") && note.contains(SEMANTIC_REBUILD_COMMAND),
            "the note must name the canonical rebuild command: {note}"
        );
        assert!(
            note_semantic_feature_absent(),
            "the first call must print the note"
        );
        assert!(
            !note_semantic_feature_absent(),
            "the note must never be printed twice per run"
        );
    }

    #[cfg(feature = "semantic")]
    #[test]
    fn index_help_promises_vectors_when_the_feature_is_compiled_in() {
        let help = flatten(&index_help_text());
        assert!(
            help.contains("--semantic"),
            "help must list the flag:\n{help}"
        );
        assert!(
            !help.contains("requires a build with --features semantic"),
            "a --features semantic build must not blame a missing feature:\n{help}"
        );
        assert!(
            help.contains("Opt in: generate dense semantic vector embeddings for every archive in scope (off by default)"),
            "--semantic help text must stay as it always was:\n{help}"
        );
    }

    // ------------------------------------------------------------------
    // `resolve_paths` — the fallbacks are absolute data-volume paths, never
    // CWD-relative names (`kiwix_dir`, `indices`, `indicies`, `.indexes`), so
    // the CLI resolves them the same way from any working directory.
    // ------------------------------------------------------------------

    #[test]
    fn index_dir_without_an_explicit_flag_resolves_to_the_absolute_default() {
        let (_, index_dir) = resolve_paths(Path::new("."), None);
        assert_eq!(index_dir, PathBuf::from(DEFAULT_INDEX_DIR));
    }

    #[test]
    fn explicit_index_dir_outranks_the_default() {
        let (_, index_dir) = resolve_paths(Path::new("."), Some(PathBuf::from("/srv/zim-indexes")));
        assert_eq!(index_dir, PathBuf::from("/srv/zim-indexes"));
    }

    #[test]
    fn explicit_archive_path_is_passed_through_unchanged() {
        let (zim_dir, _) = resolve_paths(Path::new("/data/library.zim"), None);
        assert_eq!(zim_dir, PathBuf::from("/data/library.zim"));
    }

    #[test]
    fn the_unspecified_archive_dir_never_falls_back_to_a_cwd_relative_name() {
        // With `.` and no local `*.zim` the only acceptable answers are the current directory and
        // the absolute default; a relative `kiwix_dir` here would depend on the process CWD.
        let (zim_dir, _) = resolve_paths(Path::new("."), None);
        // Compared against borrowed `&Path` constants instead of freshly allocated `PathBuf`s:
        // the comparison semantics are identical (`PathBuf: PartialEq<Path>` delegates to the
        // same `Path` comparison), but no owned value is built just to compare it.
        assert!(
            zim_dir == Path::new(".") || zim_dir == Path::new(DEFAULT_ZIM_DIR),
            "unexpected fallback archive dir: {}",
            zim_dir.display()
        );
    }

    #[test]
    fn tilde_expansion_is_still_applied_to_explicit_paths() {
        let Some(home) = std::env::var_os("HOME") else {
            panic!("HOME must be set to assert tilde expansion");
        };
        let (zim_dir, index_dir) =
            resolve_paths(Path::new("~/library.zim"), Some(PathBuf::from("~/indexes")));
        assert_eq!(zim_dir, PathBuf::from(&home).join("library.zim"));
        assert_eq!(index_dir, PathBuf::from(&home).join("indexes"));
    }
}
