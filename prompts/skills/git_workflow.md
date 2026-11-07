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
  - Create isolated worktrees per-task inside the root under the gitignored `.worktrees/` directory. This ensures multiple subagents running in parallel never clash on uncommitted files, index locks, or build artefacts.
  - Add `.worktrees/` to `.gitignore` first (edit `.gitignore` with `replace`/`write_file`), and commit that rule on its own before creating worktrees:
    - `git add .gitignore`
    - `git commit -m "chore: ignore .worktrees/ for isolated worktrees"`
  - For each coding task (`<task_id>`, e.g. `t-001`), branch from the active feature branch into an isolated task worktree:
    - `git worktree add .worktrees/<task_id> -b task/<task_id> <feature-branch>`
    - `git worktree list`
    - `git -C .worktrees/<task_id> branch --show-current`
  - If `.worktrees/` already holds a stale worktree from an earlier task or aborted run, list and prune stale entries (`git worktree prune`, then `git worktree list`).
  - Address every git operation at the per-task worktree explicitly — cwd is fixed to the workspace root:
    - `git -C .worktrees/<task_id> status --short`
    - `git -C .worktrees/<task_id> add <changed-files>`
    - `git -C .worktrees/<task_id> commit -m "feat(<task_id>): <concise summary>"`
  - A gitignored subtree is pruned by the workspace walker, so `glob` may not list files under `.worktrees/`. Open them with `read_file` and search them with `grep_search` using explicit paths, or inspect them through `run_command`.
  - MERGE THE WORK BACK to the feature branch and clean up when the task is verified and green:
    - `git -C <feature-branch-dir-or-root> merge --no-ff task/<task_id> -m "feat(<task_id>): merge completed <task_id> work"`
    - `git worktree remove .worktrees/<task_id>`
    - `git branch -d task/<task_id>`
    - `git worktree prune`
  - FALL BACK to a plain branch (`git checkout -b task/<task_id>`) only when a worktree is genuinely impossible (shallow history, detached `HEAD`).
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
  - Always commit with author identity "Marmel Coder": supply it per call with `git -c user.name="Marmel Coder" -c user.email="coder@marmel.local" commit -m "..."`, or set it once locally in the worktree/repository: `git -C <path> config user.name "Marmel Coder"` and `git -C <path> config user.email "coder@marmel.local"` (global config writes are denied by the sandbox).
- **NEVER DO:**
  - Force-push a shared branch (`git push --force`); use `--force-with-lease` only for your own branch.
  - Skip hooks with `--no-verify`.
  - Rewrite published history (`rebase -i`, `commit --amend`, `reset --hard`) once others may have based work on it.
  - Commit `.marmel/`, `target/`, or other build artefacts and local configs.
  - Delete or weaken an existing test just to make a check pass.
