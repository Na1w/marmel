# zim-search

ZIM archive reader with vector and BM25 search.

## Overview

- Reads ZIM files (versions 5.x and 6.x) using statically compiled compression (Zstandard, LZMA).
- Dense vector search with local 8-bit quantized ONNX embeddings (`AllMiniLML6V2Q`, 384 dimensions) — available only in a build made with the `semantic` Cargo feature (see [Build features](#build-features)), and opt-in per archive at index time.
- Inverted index with Okapi BM25 and stopword filtering — always built, and the only index (and the only retrieval mode) in a default build.
- Hybrid retrieval combining normalized vector similarity and BM25 scores — in a `semantic` build, auto-detected when the index carries vectors.
- Model Context Protocol (MCP) server over standard I/O for LLMs.
- Multi-archive directory scanning with decoupled index directory support.
- Local HTTP server with search interface and article viewer.

---

## Building

The crate lives in the `wip/` workspace (`wip/Cargo.toml` → `[workspace] members = ["crates/zim-search"]`),
so every `cargo` command can either be run from the workspace root with `-p zim-search`, or bare from
this directory (`wip/crates/zim-search`) — cargo then scopes the bare command to this package.

```bash
cargo build --release -p zim-search          # from wip/
```

Binary output: `wip/target/release/zim-search` — a workspace member has no private `target/`; all
artifacts land in the shared `wip/target/`. Every `wip/target/release/zim-search` below is that one
binary, written from the repository layout (adjust the `wip/` prefix to your checkout root).

### Build features

The `semantic` Cargo feature (`Cargo.toml`: `default = []`, `semantic = ["dep:fastembed"]`, `fastembed = { version = "7.1.1", optional = true }`) decides whether the ONNX inference stack is compiled into the binary at all.

```bash
# Default build: BM25/Tantivy lexical only — no ONNX stack, no model download
cargo build --release -p zim-search

# Adds dense-vector support via ONNX; the AllMiniLML6V2Q model (384 dims) is downloaded on first use
cargo build --release -p zim-search --features semantic
```

- A default build compiles no ONNX code and cannot generate or search dense vectors (`SEMANTIC_SUPPORTED` is `false`).
- ONNX cost: `fastembed` pulls roughly 200 extra crates and a ~105 MB static ONNX Runtime into the link.
- Dense vectors therefore need both levels: the feature at build time, then the per-run/per-source opt-ins described under [Semantic Indexing (Opt-In)](#semantic-indexing-opt-in).

---

## MCP Server (for LLMs)

Start the stdio MCP server (with no flags it falls back to the absolute data locations
`/mnt/extradisk/kiwix` and `/mnt/extradisk/kiwixidx`):

```bash
wip/target/release/zim-search mcp
```

Or pass explicit paths (absolute or relative) with `--kiwix-dir` and `--index`:

```bash
wip/target/release/zim-search mcp \
  --kiwix-dir /mnt/extradisk/kiwix \
  --index /mnt/extradisk/kiwixidx
```

### Client Configuration

```json
{
  "mcpServers": {
    "zim-search": {
      "command": "/home/fredrik/marmel/wip/target/release/zim-search",
      "args": [
        "mcp",
        "--kiwix-dir", "/mnt/extradisk/kiwix",
        "--index", "/mnt/extradisk/kiwixidx"
      ]
    }
  }
}
```

### Tools (with Pagination)

- `list_archives`: List discovered ZIM archives (`offset`, `limit`, `has_more`).
- `list_articles`: List articles inside an archive with optional filter (`archive`, `filter`, `offset`, `limit`, `has_more`).
- `search`: Search archives with auto-detected retrieval — dense vectors when the index has them, BM25 lexical otherwise; `mode` can force `semantic`, `hybrid`, or `bm25` (explicit vector modes degrade to BM25 when the index has no vectors) (`query`, `archive`, `mode`, `offset`, `limit`, `total_matches`, `has_more`). In a default build the tool and `mode` descriptions state that this build "retrieves LEXICALLY (BM25) only" because it has no dense-vector support.
- `read_article`: Retrieve plain text or HTML for an article with character pagination (`archive`, `url`, `offset_chars`, `limit_chars`, `total_chars`, `has_more`).
- `index_archive`: Build an index on demand — BM25 lexical only unless `"semantic": true` (`archive`, `semantic`, `semantic_options` with `lead_only`, `max_chunks`, `batch_size`, legacy `mode`). In a default build the `semantic` property is still declared but described as "UNAVAILABLE in this build", and the call stays BM25-only.
- `get_archive_info`: Return archive metadata (`archive`).

---

## CLI Commands

### View Archive Metadata
```bash
wip/target/release/zim-search info ppc_2026-06.zim
```

### List Archives / Articles
```bash
# List archives in directory
wip/target/release/zim-search list . --index-dir indexes

# List articles within an archive
wip/target/release/zim-search list ppc_2026-06.zim --filter "stack"
```

### Read an Article
```bash
wip/target/release/zim-search read mariokartwii.com/ppc/ch24.html --path ppc_2026-06.zim
```

### Index an Archive
```bash
# Default: BM25 lexical index only (fast, no dense vectors, no model download)
wip/target/release/zim-search index ppc_2026-06.zim --index-dir indexes

# Same as the default, spelled out
wip/target/release/zim-search index ppc_2026-06.zim --index-dir indexes --mode bm25

# Opt in: dense semantic vectors for every archive in the directory
wip/target/release/zim-search index . --index-dir indexes --semantic

# Opt in per source: only the named archive gets vectors (repeatable, alias --semantic-source)
wip/target/release/zim-search index . --index-dir indexes --semantic-archive ppc_2026-06

# Add vectors to an already-indexed archive (per-source on demand, no --force needed)
wip/target/release/zim-search index . --index-dir indexes --semantic-archive ppc_2026-06.zim
```

### Semantic Indexing (Opt-In)

Dense vector embeddings are never generated unless you ask for them; the BM25 lexical index is always built. Both levels are required: the binary must first be built with `--features semantic` (see [Build features](#build-features)), and each run or source must then opt in with the flags below. In a default (feature-off) build, `--semantic` and `--semantic-archive <name>` fail fast — "Semantic vector indexing is not available in this build: recompile with `cargo build --features semantic`" — and the only way to run a vector pass in that binary is `--mock-embedder`, because `MockEmbedder` is pure Rust and always compiled.

- `--semantic` — generate vectors for every archive in scope.
- `--semantic-archive <name>` (repeatable, alias `--semantic-source`) — generate vectors for the named source only: archive name, file stem, or `.zim` filename.
- `--mode hybrid` or `--mode semantic` — legacy opt-in, equivalent to `--semantic`; omitting `--mode` (or `--mode bm25`) stays lexical-only.
- Cost knobs, honoured in single-file and directory/batch mode alike: `--semantic-lead-only` (embed only the lead passage of each article), `--max-semantic-chunks <n>` (hard cap on vectors), `--batch-size <n>` (passages per embedder batch, default 256), `--mock-embedder` (offline hashing embedder instead of ONNX).
- Markers on disk: `<stem>.tantivy/meta.json` means the BM25 index exists, `<stem>.tantivy/vectors.meta.json` means its vectors are complete. Re-running `index` with `--semantic-archive <name>` upgrades a `meta.json`-only archive without `--force`.
- MCP clients opt in the same way: `index_archive` with `"semantic": true`, optionally with `semantic_options { lead_only, max_chunks, batch_size }`.
- The ONNX model (`AllMiniLML6V2Q`) is downloaded only when you opt in; use `--mock-embedder` for offline testing.

### Search
```bash
# Auto-detect (default): dense vectors when the index has them, otherwise BM25
wip/target/release/zim-search search "how do I configure the pagetable?" --path . --index-dir indexes

# Explicit lexical search (works with or without vectors)
wip/target/release/zim-search search "r14 thru r31" --path ppc_2026-06.zim --index-dir indexes --mode bm25

# Vector search requires an index built with the opt-in flag
wip/target/release/zim-search index ppc_2026-06.zim --index-dir indexes --semantic
wip/target/release/zim-search search "r14 thru r31" --path ppc_2026-06.zim --index-dir indexes --mode semantic
```

Asking for `--mode semantic` or `--mode hybrid` on an index without vectors prints a one-line notice and falls back to BM25. In a default build (no `semantic` feature) every search is BM25: if the index carries `vectors.meta.json` from a semantic build, the binary still searches it with BM25 and prints a notice that this binary cannot read those vectors.

### Web Interface
```bash
wip/target/release/zim-search serve ppc_2026-06.zim --index-dir indexes --port 8080
```

The mode is preselected as Hybrid only when some archive in the library has dense vectors, otherwise BM25. In a default build (no `semantic` feature) Hybrid and Semantic are never preselected, whatever the index contains.

## Running the full verification

```bash
bash wip/crates/zim-search/verify_semantic_feature.sh   # or: --only-builds | --only-tests | --skip-clippy | --help
```

One pass over both build configurations: `cargo build` ×2, `cargo check --no-default-features`, unit and integration tests ×2, `cargo tree` probes proving the default graph is ONNX-free and the `semantic` graph is not, `index --help` from each binary, and the feature-off fail-fast (`index <empty tmpdir> --semantic` must exit non-zero and quote `cargo build --features semantic`). It downloads nothing, indexes nothing, and exits 0 only if no gate failed; a readable `.zim` (via `ZIM_TEST_FILE`) makes the fixture-gated tests run instead of SKIP.
