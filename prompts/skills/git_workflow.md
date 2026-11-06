---
id: git_workflow
name: Git Workflow, Worktree Isolation & Commit Discipline
description: Isolated worktrees, branch strategy, small frequent commits and git command hygiene inside the sandboxed workspace.
suggested_tools:
  - run_command
  - read_file
  - write_file
  - replace
  - grep_search
---

## Git Workflow, Worktree Isolation & Commit Discipline

- **WHEN TO USE:**
  - The workspace is already a git repository (`git rev-parse --is-inside-work-tree` returns `true`): prefer worktree isolation so parallel specialists never share one working tree.
  - The workspace is a new project with no `.git` directory: run `git init` and write `.gitignore` before touching source files (see below).
  - Reach for an isolated worktree when the mission rewrites build or configuration files (`Cargo.toml`, `Cargo.lock`, `.cargo/config.toml`), starts long builds that would race another specialist, or spans several specialists at once.
- **WORKTREE ISOLATION (PREFERRED):**
  - The sandbox allows writes ONLY inside the workspace root, so the idiomatic sibling layout (`git worktree add ../<branch>`) is denied — anything outside the root fails with a permission error.
  - Create the worktree inside the root, under a dedicated gitignored directory. Add `.worktrees/` to `.gitignore` first (edit `.gitignore` with `replace`/`write_file`), and commit that rule on its own before creating the worktree:
    - `git add .gitignore`
    - `git commit -m "chore: ignore .worktrees/ for isolated worktrees"`
  - Then branch from the tip of the current integration branch and verify the worktree exists:
    - `git worktree add .worktrees/<branch> -b feature/<name>`
    - `git worktree list`
    - `git -C .worktrees/<branch> branch --show-current`
  - If `.worktrees/` already holds a worktree from an earlier run, list and prune stale entries before creating a new one (`git worktree prune`, then `git worktree list`).
  - Address every git operation at the worktree explicitly — cwd is fixed to the workspace root:
    - `git -C .worktrees/<branch> status --short`
    - `git -C .worktrees/<branch> add crates/<crate>/src/<file>.rs`
    - `git -C .worktrees/<branch> commit -m "feat: <concise summary>"`
  - A gitignored subtree is pruned by the workspace walker, so `glob` may not list files under `.worktrees/`. Open them with `read_file` and search them with `grep_search` using explicit paths, or inspect them through `run_command`.
  - Clean up when the branch is merged or abandoned:
    - `git worktree remove .worktrees/<branch>`
    - `git worktree prune`
  - FALL BACK to a plain branch (`git checkout -b feature/<name>`) when a worktree is impossible: shallow or restricted history, detached `HEAD`, or uncommitted local changes that must stay shared with the main checkout.
  - MERGE THE WORK BACK from the main checkout once the branch is green:
    - `git status --short` (main checkout must be clean)
    - `git merge --no-ff feature/<name> -m "feat: merge <name> work"`
    - `git worktree remove .worktrees/<branch>` then `git branch -d feature/<name>`
- **NEW PROJECTS UNDER GIT:**
  - Run `git init`, then write `.gitignore` BEFORE any real work so artefacts never enter the index: `target/`, `.marmel/`, `marmel.toml`, `*.idx.bin`, `.fastembed_cache/`.
  - Make the initial commit immediately (`git add .gitignore Cargo.toml && git commit -m "chore: initial commit"`), then commit early and often from the first edit onwards.
- **SMALL FREQUENT COMMITS:**
  - One commit per coherent step: a completed subtask, a green build, or a refactor step together with the tests that cover it.
  - Stage explicitly with `git add <paths>` — never `git add -A` or `git add .`, which silently stage artefacts and unrelated files.
  - Confirm the staged set before committing: `git status --short` and `git --no-pager diff --staged --stat`.
  - Prefix messages with conventional verbs: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`.
  - Run the required checks first and commit only when they pass: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-targets --all-features`.
  - Never commit research or analysis artefacts — keep plans, notes and dumps under `.marmel/`, out of the repository.
- **COMMAND HYGIENE (CRITICAL IN THIS ENVIRONMENT):**
  - cwd is fixed to the workspace root, so always use `git -C <path>` for the main checkout, any worktree and any subdirectory.
  - Never open an editor: `git commit -m "..."`, never bare `git commit`, never `git rebase -i`, never `git merge` without `-m` or `--no-edit`. An interactive editor hangs until the timeout kills the command.
  - Disable the pager on every read-only inspection: `git --no-pager log --oneline -20`, `git --no-pager diff`, `git --no-pager show --stat HEAD`.
  - `git` exit codes are NOT reported on Linux, so confirm each step out of band instead of trusting the exit code: `git status --short`, `git --no-pager log --oneline -1`, `git worktree list`.
  - Pass `timeout_seconds` on long-running git calls (`git fetch`, a whole-repo `git log`, a large `git diff`) so nothing blocks the mission.
  - `git config --global` writes are denied by the sandbox. Supply the identity per call with `git -c user.name="<name>" -c user.email="<addr>" commit -m "..."`, or set it once repo-local: `git -C <path> config user.name "<name>"` and `git -C <path> config user.email "<addr>"`.
- **NEVER DO:**
  - Force-push a shared branch (`git push --force`); use `--force-with-lease` only for your own branch.
  - Skip hooks with `--no-verify`.
  - Rewrite published history (`rebase -i`, `commit --amend`, `reset --hard`) once others may have based work on it.
  - Commit `.marmel/`, `target/`, or other build artefacts and local configs.
  - Delete or weaken an existing test just to make a check pass.
