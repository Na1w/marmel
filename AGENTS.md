# AGENTS.md

Working rules for AI agents and contributors in this repository (Rust, edition 2024, workspace with `crates/zim-search`). Marmel injects this file into the prompts of its own agents. Custom role definitions live in `ARCHETYPES.md`, not here.

## Git workflow

- Do all work on a new branch created from an up-to-date `main`. Never commit directly to `main`.
- Branch names: `feature/<short-name>`, `fix/<short-name>` or `chore/<short-name>`.
- Make small, focused commits with clear messages (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`).
- Never force-push shared branches, rewrite published history or skip hooks (`--no-verify`).

## What must never be committed

- Temporary files, scratch scripts, logs, backups and debug output.
- Preliminary investigations, research notes, analysis dumps and plans (e.g. `execution_plan*.md`, `research_*.md`). Keep these in `.marmel/research/`, never in `docs/`.
- Marmel working state: `.marmel/`, `.marmel.old*`, `.kvaser/`, `.session_*.json`, `forced_phase.txt`.
- Local configs and secrets (`marmel.toml`, `marmel.toml.cloud`, `config.toml`, API keys, tokens).
- Build artifacts (`target/`, `*.rs.bk`, `.fastembed_cache/`, `*.idx.bin`).
- Before every commit, run `git status` and `git diff --staged` and verify that only intended files are included. Stage files explicitly; avoid `git add -A` and `git add .`.
- If you create a new kind of temporary file, add it to `.gitignore` instead of leaving it untracked.

## Required checks (before every commit and PR)

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --doc
```

- These mirror CI (`.github/workflows/ci.yml`). Fix the code; do not silence the lints.
- `unused`, `dead_code`, `unused_imports` and `clippy::all` are denied in `Cargo.toml`.
- Do not add `#[allow(...)]` to bypass a warning without a comment explaining why.
- Code touching platform-specific paths (e.g. `src/harness/sandbox.rs`) must also pass `scripts/cross_platform_check.sh`.
- The `semantic` feature (ONNX/fastembed) is opt-in and manual; do not enable it in the default build.

## Code conventions

- Keep changes minimal and scoped to the task. No drive-by refactors or unrelated formatting changes.
- No `unwrap()`/`expect()` in non-test code for fallible runtime operations; use `anyhow`/`thiserror` and propagate errors.
- Use `tracing` for logging, not `println!`/`eprintln!` (the TUI owns the terminal).
- Do not add a dependency without a clear need.
- Delete dead code instead of commenting it out.
- All code, comments, documentation and log messages are written in English.
- Keep existing comments and docstrings that are unrelated to your change.

## Tests

- New behavior and bug fixes come with tests (unit tests next to the code, integration tests in `tests/`).
- Tests must not touch the real workspace, network or user home; use `tempfile` and `wiremock`.
- Never delete or weaken an existing test to make a change pass.

## Documentation and pull requests

- `docs/` is only for real project and user documentation and may be committed. Research notes and investigations do not belong there.
- Update `README.md` when user-visible behavior, config keys or tools change. Keep it high level: document features and how to use them, not implementation details.
- One logical change per PR, opened from your branch against `main`. Describe what and why, and confirm the checks above pass.
