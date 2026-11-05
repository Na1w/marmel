//! Integration harness for the **"semantic indexing is opt-in and per-source"** CLI contract of
//! `zim-search index`.
//!
//! It drives the *compiled binary* (`env!("CARGO_BIN_EXE_zim-search")`) — not the library — so it
//! guards the user-visible contract rather than the internal plumbing
//! (`src/main.rs::resolve_semantic_scope`, the batch/single-file Phase 2 gates in `src/main.rs`,
//! and `ArchiveManager::semantic_in_scope` in `src/manager/mod.rs`).
//!
//! Contract under test:
//! 1. `--mode <bm25|hybrid|semantic>` is OPTIONAL with **no default**: an absent `--mode` behaves
//!    like `bm25`, i.e. the BM25/Tantivy stage is built and **no dense vectors are generated**.
//! 2. `--semantic` opts in to vectors for every archive; `--semantic-archive <name>` (repeatable,
//!    visible alias `--semantic-source`) opts in *and* narrows vector generation to the named
//!    archives (archive name, file stem, or `.zim` filename).
//! 3. A batch/library run prints `=== Phase 2/2: Generating semantic vector embeddings (opt-in) ===`
//!    plus a `Semantic scope: ...` line only when opted in; otherwise it prints the
//!    `Semantic vectors skipped: pass --semantic (or --semantic-archive <name>) ...` notice.
//! 4. An unknown `--semantic-archive` selector prints a `Notice: --semantic-archive '<x>' matched no
//!    archive` line (stderr) and still exits 0 — scoping mistakes are a warning, not a failure.
//! 5. On-disk markers: `<index-dir>/<stem>.tantivy/meta.json` = BM25 stage present,
//!    `vectors.meta.json` = semantic stage complete.
//! 6. Search over a vectors-less index degrades to BM25 and warns on stderr, naming the opt-in flag.
//!
//! Every indexing run that reaches the vector pass passes `--mock-embedder`, so the offline 384-dim
//! `MockEmbedder` is used and no ONNX model is ever downloaded. The single exception is
//! `semantic_opt_in_without_the_feature_fails_fast_with_rebuild_hint`, which deliberately omits it:
//! `--mock-embedder` is the documented escape hatch that makes an opt-in runnable without the feature,
//! so the fail-fast guard can only be observed without it.
//!
//! Three gates live here, and each one prints its own distinct skip notice:
//! * **Always-run** assertions: need nothing but an empty temp library/index directory. They run in
//!   BOTH build configurations (see the feature gate below), including the feature-off fail-fast
//!   contract in `semantic_opt_in_without_the_feature_fails_fast_with_rebuild_hint`.
//! * **Fixture-gated** on-disk assertions: need a real `.zim` archive, resolved from `$ZIM_TEST_FILE`
//!   or, failing that, the first `*.zim` inside `/mnt/extradisk/kiwix` (the absolute
//!   archive-library default, then the crate directory). When no
//!   fixture is reachable the tests print a skip notice and return — they never download anything and
//!   never fail the suite for a missing fixture.
//! * **Feature-gated** vector-generation assertions: dense-vector *generation* only exists when the
//!   optional `semantic` Cargo feature is compiled in (`Cargo.toml` ships `default = []`, so the
//!   default build links no ONNX model at all). The tests that assert `vectors.meta.json` appears
//!   therefore skip — with a message that names the rebuild, never the fixture — when this target
//!   was built without the feature. Assertions that hold with or without vectors (BM25-only default,
//!   vectors-absent markers, the BM25 fallback in `search`) stay active in both configurations.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use tempfile::TempDir;

/// Compiled binary under test (cargo injects the absolute path for `[[bin]] zim-search`).
const BIN: &str = env!("CARGO_BIN_EXE_zim-search");

/// Whether THIS test target was built with the optional `semantic` Cargo feature (`Cargo.toml` has
/// `default = []`, `semantic = ["dep:fastembed"]`), i.e. whether the binary under test can generate
/// dense vectors at all. Mirrors the CLI's own `zim_search::SEMANTIC_SUPPORTED` switch: the harness
/// must be honest about which half of the contract it is actually entitled to assert.
const SEMANTIC_FEATURE_ENABLED: bool = cfg!(feature = "semantic");

/// Batch/library Phase 2 banner, printed only when the caller opted in.
const PHASE2_BANNER: &str = "=== Phase 2/2: Generating semantic vector embeddings (opt-in) ===";
/// Opt-in reminder printed instead of Phase 2 when nothing was requested.
const SKIPPED_NOTICE: &str =
    "Semantic vectors skipped: pass --semantic (or --semantic-archive <name>) to generate them.";
/// Prefix of the scope echo line printed inside Phase 2.
const SCOPE_PREFIX: &str = "Semantic scope:";
/// Prefix of the per-selector warning emitted when a selector matches no archive.
const NO_MATCH_NOTICE_PREFIX: &str = "Notice: --semantic-archive ";
/// Prefix of the Phase 2 summary that counts archives which received vectors.
const VECTORS_RECEIVED: &str = "archive(s) received semantic vectors";
/// Fallback warning emitted by `search` when no archive in scope carries vectors.
const BM25_FALLBACK_NOTICE: &str = "No dense semantic vectors in scope";

/// First directory the fixture scan tries: the absolute archive-library location the CLI itself
/// falls back to (`src/main.rs::DEFAULT_ZIM_DIR`). The CWD-relative `kiwix_dir` symlink that used
/// to sit next to the crate is gone, so scanning that name would only ever find nothing.
const ZIM_DIR_DEFAULT: &str = "/mnt/extradisk/kiwix";

/// Full per-selector warning line for a selector that matched no archive in the library.
fn no_match_notice(selector: &str) -> String {
    format!("{NO_MATCH_NOTICE_PREFIX}'{selector}' matched no archive")
}

/// Captured result of one binary invocation.
struct Run {
    cmdline: String,
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Run {
    /// stdout and stderr concatenated, for assertions that do not care about the stream.
    fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    fn expect_success(&self) -> &Self {
        assert!(
            self.status.success(),
            "`{BIN} {cmdline}` should exit successfully, got {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            status = self.status,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }

    fn expect_failure(&self) -> &Self {
        assert!(
            !self.status.success(),
            "`{BIN} {cmdline}` should fail, but it exited {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            status = self.status,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }

    fn expect_stdout_contains(&self, needle: &str) -> &Self {
        assert!(
            self.stdout.contains(needle),
            "`{BIN} {cmdline}` stdout should contain {needle:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            needle = needle,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }

    fn expect_stdout_missing(&self, needle: &str) -> &Self {
        assert!(
            !self.stdout.contains(needle),
            "`{BIN} {cmdline}` stdout must NOT contain {needle:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            needle = needle,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }

    fn expect_output_contains(&self, needle: &str) -> &Self {
        let combined = self.combined();
        assert!(
            combined.contains(needle),
            "`{BIN} {cmdline}` output (stdout+stderr) should contain {needle:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            needle = needle,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }

    fn expect_output_missing(&self, needle: &str) -> &Self {
        let combined = self.combined();
        assert!(
            !combined.contains(needle),
            "`{BIN} {cmdline}` output must NOT contain {needle:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            needle = needle,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }

    fn expect_stderr_contains(&self, needle: &str) -> &Self {
        assert!(
            self.stderr.contains(needle),
            "`{BIN} {cmdline}` stderr should contain {needle:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            cmdline = self.cmdline,
            needle = needle,
            stdout = self.stdout,
            stderr = self.stderr,
        );
        self
    }
}

/// Run the binary with `args`, capturing both streams (never inheriting stdout/stderr).
fn run(args: &[&str]) -> Run {
    let output = Command::new(BIN)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to launch `{BIN} {}`: {e}", args.join(" ")));

    Run {
        cmdline: args.join(" "),
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// `zim-search index <lib> --index-dir <idx> --mock-embedder [extra...]`.
///
/// `--mock-embedder` is unconditional: the offline mock embedder keeps the opt-in semantic pass free
/// of any ONNX model download.
fn run_index(lib: &Path, index_dir: &Path, extra: &[&str]) -> Run {
    let owned = [
        "index".to_string(),
        lib.display().to_string(),
        "--index-dir".to_string(),
        index_dir.display().to_string(),
        "--mock-embedder".to_string(),
    ];
    let mut args: Vec<&str> = owned.iter().map(String::as_str).collect();
    args.extend(extra);
    run(&args)
}

/// `<index-dir>/<stem>.tantivy` — the per-archive Tantivy index directory.
fn archive_index_dir(index_dir: &Path, stem: &str) -> PathBuf {
    index_dir.join(format!("{}.tantivy", stem))
}

/// `meta.json` — the marker proving the BM25 (Tantivy) stage exists.
fn bm25_marker(index_dir: &Path, stem: &str) -> PathBuf {
    archive_index_dir(index_dir, stem).join("meta.json")
}

/// `vectors.meta.json` — the marker proving the semantic vector stage completed.
fn vector_marker(index_dir: &Path, stem: &str) -> PathBuf {
    archive_index_dir(index_dir, stem).join("vectors.meta.json")
}

/// Direct children of `dir` (sorted), used for "nothing was written" assertions and failure context.
fn children(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read directory {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    entries
}

fn assert_bm25_stage_present(index_dir: &Path, stem: &str) {
    let marker = bm25_marker(index_dir, stem);
    assert!(
        marker.is_file(),
        "expected BM25 stage marker {} for archive '{stem}'",
        marker.display()
    );
}

fn assert_semantic_stage_complete(index_dir: &Path, stem: &str) {
    let marker = vector_marker(index_dir, stem);
    assert!(
        marker.is_file(),
        "expected semantic stage marker {} for archive '{stem}', but it is missing. \
         Archive index dir contains: {:?}",
        marker.display(),
        children(&archive_index_dir(index_dir, stem))
    );
}

fn assert_semantic_stage_absent(index_dir: &Path, stem: &str) {
    let marker = vector_marker(index_dir, stem);
    assert!(
        !marker.exists(),
        "archive '{stem}' must NOT have a semantic stage (opt-in was not requested for it), \
         but {} exists. Archive index dir contains: {:?}",
        marker.display(),
        children(&archive_index_dir(index_dir, stem))
    );
}

/// Help block clap prints for `flag` (its description lines, up to the next flag line).
///
/// `flag` is matched as a substring of the option line, e.g. `"--mode <MODE>"`.
fn help_block_for_flag(help: &str, flag: &str) -> String {
    let lines: Vec<&str> = help.lines().collect();
    let idx = lines
        .iter()
        .position(|line| line.contains(flag))
        .unwrap_or_else(|| {
            panic!("`{BIN} index --help` should document {flag:?}; help was:\n{help}")
        });

    let mut block = Vec::new();
    for line in &lines[idx + 1..] {
        let trimmed = line.trim_start();
        let starts_new_option = trimmed.starts_with("--") || trimmed.starts_with("-h,");
        if starts_new_option {
            break;
        }
        block.push(*line);
    }
    block.join("\n")
}

/// Locate an optional `.zim` fixture: `$ZIM_TEST_FILE` first, then the first `*.zim` in
/// `/mnt/extradisk/kiwix` (the absolute archive-library default), then the first `*.zim` in the
/// crate directory. Never downloads anything.
fn locate_fixture() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("ZIM_TEST_FILE") {
        let candidate = PathBuf::from(&raw);
        if candidate.is_file() {
            return Some(candidate);
        }
        eprintln!(
            "[harness] ZIM_TEST_FILE='{}' is not a readable file; falling back to a directory scan.",
            candidate.display()
        );
    }

    first_zim_in(Path::new(ZIM_DIR_DEFAULT)).or_else(|| first_zim_in(Path::new(".")))
}

/// First `*.zim` file directly inside `dir`, chosen deterministically (sorted).
fn first_zim_in(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("zim"))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Feature gate for the assertions that **require dense vectors to be generated**; it extends
/// [`fixture_or_skip`] one step earlier. A feature-off (default) build links no embedding model, so
/// those assertions cannot be honoured even with a perfect fixture on disk. The notice deliberately
/// does NOT reuse the missing-fixture wording: nothing here was skipped for lack of a fixture.
fn semantic_feature_or_skip(test: &str) -> bool {
    if SEMANTIC_FEATURE_ENABLED {
        return true;
    }
    eprintln!(
        "SKIP: this test target was built without the 'semantic' feature; rebuild the tests with \
         --features semantic to exercise vector generation. Nothing was skipped for lack of a \
         fixture. [{test}]"
    );
    false
}

/// Fixture gate: return the fixture path, or print a skip notice and return `None`.
fn fixture_or_skip(test: &str) -> Option<PathBuf> {
    match locate_fixture() {
        Some(path) => {
            eprintln!("[harness] {test}: using fixture '{}'.", path.display());
            Some(path)
        }
        None => {
            eprintln!(
                "SKIP {test}: no .zim fixture reachable (set ZIM_TEST_FILE=/path/to/archive.zim, or \
                 place a *.zim in /mnt/extradisk/kiwix) — fixture-gated on-disk assertions skipped. \
                 Nothing was downloaded and the suite is not failed."
            );
            None
        }
    }
}

/// Stage the fixture as two archives (`first.zim`, `second.zim`) inside a private library directory.
///
/// Hard links are tried first because real Kiwix archives are multi-gigabyte; a copy is the fallback
/// when the temp dir lives on another filesystem.
fn stage_two_archive_library(fixture: &Path) -> (TempDir, PathBuf) {
    let lib = TempDir::new().expect("temp library dir");
    for name in ["first.zim", "second.zim"] {
        let target = lib.path().join(name);
        if std::fs::hard_link(fixture, &target).is_err() {
            std::fs::copy(fixture, &target).unwrap_or_else(|e| {
                panic!(
                    "cannot stage fixture '{}' as {}: {e}",
                    fixture.display(),
                    target.display()
                )
            });
        }
    }
    let staged_path = lib.path().to_path_buf();
    (lib, staged_path)
}

// ---------------------------------------------------------------------------------------
// Always-run contract assertions (no fixture required)
// ---------------------------------------------------------------------------------------

#[test]
fn index_help_documents_opt_in_flags_and_leaves_mode_without_default() {
    let help = run(&["index", "--help"]);
    help.expect_success();

    // The opt-in surface must be discoverable in `--help`, alias included. These three substrings hold
    // in BOTH configurations: every flag is defined in both (`src/main.rs` duplicates only the
    // doc-comment help under `#[cfg]`, so the feature-on wording stays as-is while the ONNX-free
    // default build adds "requires a build with --features semantic"), and `--semantic-source` remains
    // a `visible_alias` on both sides of that `#[cfg]`. Only the prose differs, never the flag spellings.
    for flag in ["--semantic", "--semantic-archive", "--semantic-source"] {
        assert!(
            help.stdout.contains(flag),
            "`{BIN} index --help` should document {flag:?}; help was:\n{}",
            help.stdout
        );
    }

    // `--mode` is optional with NO default: neither its own help block nor the whole help may show
    // a `hybrid` default (the pre-opt-in behaviour this contract removes).
    let mode_block = help_block_for_flag(&help.stdout, "--mode <MODE>");
    assert!(
        !mode_block.contains("default:"),
        "`--mode` must have no clap default_value; its help block still shows one: {mode_block}"
    );
    assert!(
        !mode_block.contains("default_value"),
        "`--mode` must have no clap default_value; help block: {mode_block}"
    );
    assert!(
        !help.stdout.contains("default: hybrid"),
        "`--mode` must not default to hybrid; help was:\n{}",
        help.stdout
    );
    // The help still explains the bm25-equivalent behaviour of an omitted --mode.
    assert!(
        mode_block.contains("bm25"),
        "`--mode` help should explain the bm25-equivalent default behaviour: {mode_block}"
    );
}

#[test]
fn index_without_semantic_flags_skips_vector_phase() {
    let lib = TempDir::new().expect("empty temp library dir");
    let idx = TempDir::new().expect("empty temp index dir");

    let out = run_index(lib.path(), idx.path(), &[]);
    out.expect_success();

    out.expect_stdout_contains(SKIPPED_NOTICE);
    out.expect_stdout_missing(PHASE2_BANNER);
    out.expect_stdout_missing(SCOPE_PREFIX);
    out.expect_stdout_missing(VECTORS_RECEIVED);

    // BM25-only default is still a successful batch run, and with 0 archives nothing was written.
    out.expect_stdout_contains("Phase 1/2");
    assert!(
        children(idx.path()).is_empty(),
        "no index directories should be created for an empty library, found {:?}",
        children(idx.path())
    );
}

#[test]
fn invalid_index_mode_is_a_hard_error() {
    let lib = TempDir::new().expect("empty temp library dir");
    let idx = TempDir::new().expect("empty temp index dir");

    let out = run(&[
        "index",
        "--mode",
        "bogus",
        lib.path().to_str().expect("utf-8 temp path"),
        "--index-dir",
        idx.path().to_str().expect("utf-8 temp path"),
    ]);
    out.expect_failure();
    out.expect_stderr_contains("Invalid index mode");

    assert!(
        children(idx.path()).is_empty(),
        "a rejected mode must not leave index artefacts behind, found {:?}",
        children(idx.path())
    );
}

#[test]
fn unknown_semantic_archive_selector_warns_but_succeeds() {
    let lib = TempDir::new().expect("empty temp library dir");
    let idx = TempDir::new().expect("empty temp index dir");

    let out = run_index(
        lib.path(),
        idx.path(),
        &["--semantic-archive", "definitely-not-there"],
    );
    out.expect_success();

    // Opt-in was requested, so Phase 2 ran — but the selector matched nothing.
    out.expect_stdout_contains(PHASE2_BANNER);
    out.expect_stdout_contains("Semantic scope: 1 selected archive(s): definitely-not-there");
    out.expect_output_contains(&no_match_notice("definitely-not-there"));
    out.expect_stdout_contains("0 archive(s) received semantic vectors");
    out.expect_stdout_missing(SKIPPED_NOTICE);

    assert!(
        children(idx.path()).is_empty(),
        "an unmatched selector must not create index artefacts, found {:?}",
        children(idx.path())
    );
}

#[test]
fn semantic_archive_selector_is_repeatable_and_alias_is_accepted() {
    let lib = TempDir::new().expect("empty temp library dir");
    let idx = TempDir::new().expect("empty temp index dir");

    // Primary long form plus the visible alias, proving both are accepted and repeatable.
    let out = run_index(
        lib.path(),
        idx.path(),
        &[
            "--semantic-archive",
            "nope-a",
            "--semantic-source",
            "nope-b",
        ],
    );
    out.expect_success();
    out.expect_stdout_contains("Semantic scope: 2 selected archive(s): nope-a, nope-b");
    out.expect_output_contains(&no_match_notice("nope-a"));
    out.expect_output_contains(&no_match_notice("nope-b"));
    out.expect_stdout_contains("0 archive(s) received semantic vectors");
}

/// Feature-off contract, asserted **only** in the ONNX-free default build (`default = []`): an
/// explicit `--semantic` opt-in that this binary cannot honour must fail FAST with the actionable
/// rebuild command (`src/main.rs::semantic_request_supported`), never a silent BM25 fallback.
///
/// `--mock-embedder` is deliberately absent from this invocation: it is the documented escape hatch
/// that keeps the always-run vector-surface tests above executable in both configurations, because
/// `MockEmbedder` is compiled in every configuration.
#[cfg(not(feature = "semantic"))]
#[test]
fn semantic_opt_in_without_the_feature_fails_fast_with_rebuild_hint() {
    let lib = TempDir::new().expect("empty temp library dir");
    let idx = TempDir::new().expect("empty temp index dir");

    let out = run(&[
        "index",
        lib.path().to_str().expect("utf-8 temp path"),
        "--index-dir",
        idx.path().to_str().expect("utf-8 temp path"),
        "--semantic",
    ]);
    out.expect_failure();

    // The refusal must name the feature AND the exact rebuild command, so the user knows how to get
    // vectors rather than silently receiving a BM25-only index.
    out.expect_stderr_contains("semantic");
    out.expect_stderr_contains("--features semantic");

    assert!(
        children(idx.path()).is_empty(),
        "a refused semantic opt-in must not leave index artefacts behind, found {:?}",
        children(idx.path())
    );
}

// ---------------------------------------------------------------------------------------
// Fixture-gated on-disk assertions: per-source scoping proven with real archives.
// The two vector-generation tests are ALSO feature-gated (`semantic_feature_or_skip`): without the
// `semantic` Cargo feature the binary cannot produce `vectors.meta.json` at all. The vectors-absent
// and BM25-fallback tests stay active in both configurations.
// ---------------------------------------------------------------------------------------

#[test]
fn fixture_default_index_emits_bm25_markers_but_no_vectors() {
    let Some(fixture) = fixture_or_skip("fixture_default_index_emits_bm25_markers_but_no_vectors")
    else {
        return;
    };
    let (_lib, lib_dir) = stage_two_archive_library(&fixture);
    let idx = TempDir::new().expect("temp index dir");

    let out = run_index(&lib_dir, idx.path(), &[]);
    out.expect_success();
    out.expect_stdout_contains("Found 2 ZIM archives");
    out.expect_stdout_contains(SKIPPED_NOTICE);
    out.expect_stdout_missing(PHASE2_BANNER);

    // BM25 stage for both archives, semantic stage for neither.
    for stem in ["first", "second"] {
        assert_bm25_stage_present(idx.path(), stem);
        assert_semantic_stage_absent(idx.path(), stem);
    }
}

#[test]
fn fixture_semantic_flag_generates_vectors_for_every_archive() {
    // Vectors must be generatable: without the feature this assertion has no chance of holding, and
    // the skip says so instead of blaming a missing fixture.
    if !semantic_feature_or_skip("fixture_semantic_flag_generates_vectors_for_every_archive") {
        return;
    }
    let Some(fixture) =
        fixture_or_skip("fixture_semantic_flag_generates_vectors_for_every_archive")
    else {
        return;
    };
    let (_lib, lib_dir) = stage_two_archive_library(&fixture);
    let idx = TempDir::new().expect("temp index dir");

    // Tiny embedding budget: four vectors per archive, mock embedder, no model download.
    let out = run_index(
        &lib_dir,
        idx.path(),
        &["--semantic", "--max-semantic-chunks", "4"],
    );
    out.expect_success();
    out.expect_stdout_contains(PHASE2_BANNER);
    out.expect_stdout_contains("Semantic scope: all archives");
    out.expect_stdout_contains("2 archive(s) received semantic vectors");
    out.expect_stdout_missing(SKIPPED_NOTICE);

    for stem in ["first", "second"] {
        assert_bm25_stage_present(idx.path(), stem);
        assert_semantic_stage_complete(idx.path(), stem);
    }
}

#[test]
fn fixture_semantic_archive_generates_vectors_for_one_archive_only() {
    // Per-source vector generation also requires the feature; the skip message names the rebuild.
    if !semantic_feature_or_skip("fixture_semantic_archive_generates_vectors_for_one_archive_only")
    {
        return;
    }
    let Some(fixture) =
        fixture_or_skip("fixture_semantic_archive_generates_vectors_for_one_archive_only")
    else {
        return;
    };
    let (_lib, lib_dir) = stage_two_archive_library(&fixture);
    let idx = TempDir::new().expect("temp index dir");

    // `first` is the file stem of `first.zim` — the per-source selector form.
    let out = run_index(
        &lib_dir,
        idx.path(),
        &["--semantic-archive", "first", "--max-semantic-chunks", "4"],
    );
    out.expect_success();
    out.expect_stdout_contains(PHASE2_BANNER);
    out.expect_stdout_contains("Semantic scope: 1 selected archive(s): first");
    out.expect_stdout_contains("1 archive(s) received semantic vectors");
    out.expect_stdout_missing(SKIPPED_NOTICE);
    out.expect_output_missing(&no_match_notice("first"));

    // Both archives always get the BM25 stage; only the selected one gets vectors.
    assert_bm25_stage_present(idx.path(), "first");
    assert_bm25_stage_present(idx.path(), "second");
    assert_semantic_stage_complete(idx.path(), "first");
    assert_semantic_stage_absent(idx.path(), "second");
}

#[test]
fn fixture_search_on_vectorless_index_falls_back_to_bm25() {
    let Some(fixture) = fixture_or_skip("fixture_search_on_vectorless_index_falls_back_to_bm25")
    else {
        return;
    };
    let (_lib, lib_dir) = stage_two_archive_library(&fixture);
    let idx = TempDir::new().expect("temp index dir");

    // Case A equivalent: default (no semantic flags) run leaves the library without vectors.
    run_index(&lib_dir, idx.path(), &[]).expect_success();
    for stem in ["first", "second"] {
        assert_bm25_stage_present(idx.path(), stem);
        assert_semantic_stage_absent(idx.path(), stem);
    }

    // Default search (no `--mode` flag) runs BM25 as standard without fallback warnings.
    let default_search = run(&[
        "search",
        "page",
        "--path",
        lib_dir.to_str().expect("utf-8 temp path"),
        "--index-dir",
        idx.path().to_str().expect("utf-8 temp path"),
        "--top",
        "3",
    ]);
    default_search.expect_success();
    default_search.expect_stdout_contains("Query: 'page'");
    default_search.expect_stdout_contains("Retrieved");

    // Explicit vector/hybrid search on a vectorless index degrades gracefully to BM25 with a notice.
    let search = run(&[
        "search",
        "page",
        "--mode",
        "hybrid",
        "--path",
        lib_dir.to_str().expect("utf-8 temp path"),
        "--index-dir",
        idx.path().to_str().expect("utf-8 temp path"),
        "--top",
        "3",
    ]);
    search.expect_success();
    search.expect_stderr_contains(BM25_FALLBACK_NOTICE);
    search.expect_stderr_contains("--semantic");
    search.expect_stdout_contains("Query: 'page'");
    search.expect_stdout_contains("Retrieved");
}
