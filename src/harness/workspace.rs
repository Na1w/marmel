//! Centralized handling of the `.marmel` workspace directory.
//!
//! This module owns the workspace directory: [`Workspace::new`] creates it and
//! validates write permissions with a probe file exactly once at boot, and every
//! canonical path (execution plan, session log, forced-phase override, archive)
//! resolves through this single struct.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Plan file name inside the marmel directory (reused from `manager::phase`).
pub const PLAN_FILE: &str = crate::manager::phase::PLAN_FILE;
/// Session log file name inside the marmel directory.
pub const LOG_FILE: &str = "marmel.log";
/// Phase-override file name inside the marmel directory (reused from `manager::phase`).
pub const FORCED_PHASE_FILE: &str = crate::manager::phase::FORCED_PHASE_FILE;
/// Archive subdirectory name inside the marmel directory.
pub const ARCHIVE_DIR: &str = "archive";

/// Centralized owner of the `.marmel` workspace directory.
///
/// Constructing a [`Workspace`] via [`Workspace::new`] creates the directory
/// (if missing) and verifies it is writable by writing and removing a probe
/// file, so a read-only filesystem fails fast at boot instead of surfacing
/// confusing errors later. The directory is configurable so tests can isolate
/// against a temp dir, but defaults to `./.marmel` for normal operation.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::at(crate::manager::phase::MARMEL_DIR)
    }
}

impl Workspace {
    /// Create a workspace rooted at `dir` (defaults to `./.marmel`).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { root: dir.into() }
    }

    /// Boot-time constructor: create the workspace directory and validate that
    /// it is writable.
    ///
    /// This creates the directory (if missing) and verifies write access by
    /// writing and removing a probe file. Returns an error when the directory
    /// cannot be created or is not writable.
    pub fn new() -> Result<Self> {
        let ws = Self::default();
        ws.ensure_writable()?;
        Ok(ws)
    }

    /// The workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical path of the execution plan file.
    pub fn plan_path(&self) -> PathBuf {
        self.root.join(PLAN_FILE)
    }

    /// Canonical path of the session log file.
    pub fn log_path(&self) -> PathBuf {
        self.root.join(LOG_FILE)
    }

    /// Canonical path of the forced-phase override file.
    pub fn forced_phase_path(&self) -> PathBuf {
        self.root.join(FORCED_PHASE_FILE)
    }

    /// Canonical path of the archive subdirectory.
    pub fn archive_dir(&self) -> PathBuf {
        self.root.join(ARCHIVE_DIR)
    }

    /// Canonical path of the synthesized prompts directory (`.marmel/prompts/`).
    pub fn prompts_dir(&self) -> PathBuf {
        self.root.join("prompts")
    }

    /// Path to a specific synthesized prompt markdown file
    /// (`.marmel/prompts/<task_id>.md`).
    ///
    /// The task id is validated as a single path segment before it is joined onto
    /// the prompts directory, so an id such as `../../etc/x`, `a/b` or
    /// `..\\..\\x` is reported as an error instead of silently becoming a path
    /// outside that directory. The id is never sanitized, trimmed or clamped —
    /// callers must propagate [`crate::task_id::TaskIdError`].
    pub fn prompt_path_for_task(
        &self,
        task_id: &str,
    ) -> std::result::Result<PathBuf, crate::task_id::TaskIdError> {
        let clean = crate::task_id::normalize_task_id_ref(task_id);
        let id = crate::task_id::validate_task_id(clean)?;
        Ok(self.prompts_dir().join(format!("{id}.md")))
    }

    /// Create the workspace directory and validate it is writable by writing
    /// and removing a probe file.
    pub fn ensure_writable(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("creating workspace dir {}", self.root.display()))?;
        let probe = self
            .root
            .join(format!(".marmel_probe_{}", std::process::id()));
        std::fs::write(&probe, "marmel-write-probe")
            .with_context(|| format!("workspace {} is not writable", self.root.display()))?;
        std::fs::remove_file(&probe)
            .with_context(|| format!("cleaning up probe file {}", probe.display()))?;
        Ok(())
    }
}

/// Helper to construct numbered backup paths: `marmel.log.1`, `marmel.log.2`, etc.
pub fn backup_path(path: &Path, n: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Rotate a log file if its size exceeds `max_bytes` (or unconditionally if `max_bytes == 0`).
pub fn rotate_log_file(path: &Path, max_bytes: u64, backups: u32) {
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if max_bytes > 0 && size <= max_bytes {
        return;
    }

    for i in (1..backups).rev() {
        let src = backup_path(path, i);
        let dst = backup_path(path, i + 1);
        let _ = std::fs::rename(&src, &dst);
    }

    let _ = std::fs::rename(path, backup_path(path, 1));

    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
}

/// Best-effort snapshot of the git state around a workspace, produced by
/// [`detect_git_workspace`] from filesystem metadata alone — never by running
/// `git`, and never by failing.
///
/// Every read is tolerant: an IO error simply leaves the matching field `None`, so
/// detection cannot fail or panic. [`format_git_prompt_block`] renders the
/// snapshot into the compact prompt fragment both prompt paths append.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitWorkspace {
    /// `true` when a `.git` entry (a directory, or a file as in a linked worktree
    /// or submodule checkout) was found at or above the inspected root.
    pub is_git_repo: bool,
    /// The working tree holding the `.git` entry that was found — the repository
    /// root for a plain checkout, the linked worktree itself for a `.git` file.
    pub git_root: Option<PathBuf>,
    /// The directory `HEAD` was read from: the `.git` entry itself for a plain
    /// checkout, the resolved `gitdir:` target for a `.git` file.
    pub git_dir: Option<PathBuf>,
    /// Current branch (`HEAD` as `ref: refs/heads/<name>` → `<name>`), the short
    /// path of any other symbolic ref, or `"HEAD (detached)"` for a bare object id.
    pub branch: Option<String>,
    /// Entries under `worktrees/` inside [`Self::git_dir`]: the linked worktrees of
    /// a main checkout. `None` when that directory does not exist, which is the
    /// normal case for a linked worktree itself.
    pub worktree_count: Option<usize>,
    /// The directory the inspection started from, or `None` when that path did not
    /// exist as a directory — i.e. when no verdict was actually observed, which is
    /// exactly when [`format_git_prompt_block`] declines to state one.
    pub inspected_root: Option<PathBuf>,
}

/// Inspect `root` — then its ancestors, nearest first — for git metadata, without
/// spawning a single process.
///
/// The nearest `.git` entry wins, so the tree an agent actually works in is the one
/// reported rather than an incidental outer repository. Branch is read from
/// `<git dir>/HEAD`; the worktree count from `<git dir>/worktrees` when that
/// directory exists. Missing files, unreadable files and unparseable contents all
/// degrade to `None`.
#[must_use]
pub fn detect_git_workspace(root: &Path) -> GitWorkspace {
    let inspected_root = if root.is_dir() {
        Some(root.to_path_buf())
    } else {
        None
    };
    let mut detected = GitWorkspace {
        inspected_root: inspected_root.clone(),
        ..GitWorkspace::default()
    };
    let Some(inspected_root) = inspected_root else {
        return detected;
    };

    for dir in inspected_root.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(metadata) = std::fs::metadata(&dot_git) else {
            continue;
        };
        detected.is_git_repo = true;
        detected.git_root = Some(dir.to_path_buf());
        detected.git_dir = if metadata.is_dir() {
            Some(dot_git)
        } else {
            read_gitdir_reference(&dot_git)
        };
        break;
    }

    if let Some(git_dir) = detected.git_dir.take() {
        detected.branch = read_head_branch(&git_dir);
        detected.worktree_count = count_linked_worktrees(&git_dir);
        detected.git_dir = Some(git_dir);
    }
    detected
}

/// Resolve the `gitdir: <path>` indirection of a `.git` **file** (linked worktree
/// or submodule checkout) into the git directory it points at, relative to the
/// file's own directory when the target is relative. `None` when the file cannot
/// be read, is not a `gitdir:` reference, or names a target that does not exist.
fn read_gitdir_reference(dot_git: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(dot_git).ok()?;
    let target = raw.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(target);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        dot_git.parent().map(|dir| dir.join(candidate))?
    };
    let resolved = normalize_lexically(&joined);
    resolved.is_dir().then_some(resolved)
}

/// Resolve `.` and `..` components lexically (no filesystem access), so a relative
/// `gitdir:` reference is reported as a clean path instead of one carrying
/// parent-directory components. An unresolvable leading `..` is kept, since that is
/// what it means to the input.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => clean.push(prefix.as_os_str()),
            std::path::Component::RootDir => clean.push(std::path::Component::RootDir.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !clean.pop() {
                    clean.push(std::path::Component::ParentDir.as_os_str());
                }
            }
            std::path::Component::Normal(segment) => clean.push(segment),
        }
    }
    clean
}

/// Derive the branch a checkout is on from `<git dir>/HEAD`.
///
/// `ref: refs/heads/<name>` → `<name>`; any other symbolic ref keeps its path
/// (e.g. a remote-tracking base ref); a bare object id is a detached head, spelled
/// `"HEAD (detached)"`. An absent or empty `HEAD` yields `None`.
fn read_head_branch(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let line = head.lines().map(str::trim).find(|l| !l.is_empty())?;
    if let Some(rest) = line.strip_prefix("ref:") {
        let reference = rest.trim();
        if reference.is_empty() {
            return None;
        }
        let short = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        return Some(short.to_string());
    }
    Some("HEAD (detached)".to_string())
}

/// Count the entries of `<git dir>/worktrees`, i.e. the linked worktrees recorded
/// by a main checkout. `None` when the directory is absent or unreadable.
fn count_linked_worktrees(git_dir: &Path) -> Option<usize> {
    std::fs::read_dir(git_dir.join("worktrees"))
        .ok()
        .map(|entries| entries.count())
}

/// Render a [`GitWorkspace`] as the compact `## Git workspace state` prompt
/// fragment that the Manager and specialist prompt paths append.
///
/// The fragment states only the detected state plus a pointer to the full
/// procedure (the `git_workflow` skill and the role prompts own the rules); it
/// stays well under ten lines so the prompt stays cheap. It is empty only when the
/// verdict is not trustworthy: no repository was found **and** the root did not
/// exist to be inspected — the same "append nothing" gate both call sites honour.
#[must_use]
pub fn format_git_prompt_block(git: &GitWorkspace) -> String {
    if git.is_git_repo {
        let root = git
            .git_root
            .as_deref()
            .or(git.inspected_root.as_deref())
            .map_or_else(|| "unknown".to_string(), |path| path.display().to_string());
        let branch = git.branch.as_deref().unwrap_or("unknown");
        let worktrees = git
            .worktree_count
            .map_or_else(String::new, |count| format!(" (linked worktrees: {count})"));
        return format!(
            "\n## Git workspace state\n\
             - The workspace IS a git repository: root `{root}`, branch `{branch}`{worktrees}\n\
             - Prefer an isolated `git worktree` for this session's work, created only under the \
             gitignored in-root `.worktrees/<branch>`, and always address it as `git -C <path>` \
             — the `git_workflow` skill and your role prompt own the full procedure.\n\
             - Keep commits small and frequent; never commit `.marmel/`.\n",
        );
    }
    let Some(root) = git.inspected_root.as_deref() else {
        return String::new();
    };
    format!(
        "\n## Git workspace state\n\
         - The workspace is NOT a git repository (no `.git` entry at `{}` or above it).\n\
         - For a new project here, before real work starts: `git init`, write a `.gitignore` \
         covering `.marmel/`, then make an initial commit.\n\
         - The `git_workflow` skill and your role prompt own the full procedure.\n",
        root.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Workspace::at` + `ensure_writable` creates the directory and validates
    /// write access; canonical paths resolve inside it.
    #[test]
    fn test_workspace_creates_and_validates() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("isolated_workspace");
        assert!(!dir.exists());
        let ws = Workspace::at(&dir);
        ws.ensure_writable().unwrap();
        assert!(dir.is_dir(), "workspace dir must be created");

        assert_eq!(ws.plan_path(), dir.join(PLAN_FILE));
        assert_eq!(ws.log_path(), dir.join(LOG_FILE));
        assert_eq!(ws.forced_phase_path(), dir.join(FORCED_PHASE_FILE));
        assert_eq!(ws.archive_dir(), dir.join(ARCHIVE_DIR));

        // No probe file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            leftovers.is_empty(),
            "probe must be cleaned up: {leftovers:?}"
        );
    }

    /// `Workspace::new()` defaults to the canonical `.marmel` directory and
    /// validates writability.
    #[test]
    fn test_workspace_new_default() {
        let ws = Workspace::new().unwrap();
        assert_eq!(ws.root(), Path::new(crate::manager::phase::MARMEL_DIR));
        assert!(ws.root().is_dir());
        assert_eq!(ws.plan_path(), ws.root().join(PLAN_FILE));
        assert_eq!(ws.log_path(), ws.root().join(LOG_FILE));
    }

    /// An accepted task id resolves to a single file name *inside* the prompts
    /// directory — decoration is normalized, the rest is verbatim.
    #[test]
    fn prompt_path_for_task_resolves_inside_the_prompts_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ws_prompt_paths");
        let ws = Workspace::at(&root);
        ws.ensure_writable().expect("workspace must be writable");

        for (raw, file_name) in [
            ("t-001", "t-001.md"),
            ("  [t-001]  ", "t-001.md"),
            ("task-t-046", "task-t-046.md"),
            ("t_val_01", "t_val_01.md"),
        ] {
            let path = ws
                .prompt_path_for_task(raw)
                .unwrap_or_else(|err| panic!("{raw:?} must be accepted: {err}"));
            assert_eq!(path, ws.prompts_dir().join(file_name), "for {raw:?}");
            let relative = path.strip_prefix(&root).unwrap_or_else(|_| {
                panic!("{raw:?} escaped the workspace root: {}", path.display())
            });
            assert_eq!(relative, Path::new("prompts").join(file_name));
        }
    }

    /// A hostile task id is refused outright. The call reports the typed rejection
    /// and leaves nothing behind — no file in the workspace, and nothing at the
    /// location an unvalidated join would have reached.
    #[test]
    fn prompt_path_for_task_rejects_hostile_ids_and_creates_no_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("ws_hostile_ids");
        let ws = Workspace::at(&root);
        ws.ensure_writable().expect("workspace must be writable");

        for hostile in [
            "../../etc/x",
            "../../escape",
            "a/b",
            "..\\..\\escape",
            "..",
            ".",
            ".hidden",
            "t-001/extra",
            "t 001",
            "",
            "   ",
        ] {
            let err = ws
                .prompt_path_for_task(hostile)
                .err()
                .unwrap_or_else(|| panic!("{hostile:?} must be refused"));
            assert!(
                !err.to_string().is_empty(),
                "rejection of {hostile:?} must carry a reason"
            );
        }

        // Nothing was created anywhere in the workspace.
        let entries: Vec<String> = std::fs::read_dir(&root)
            .expect("read workspace")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.is_empty(), "no file may be created: {entries:?}");

        // Control: the unvalidated join injects parent-directory components,
        // i.e. it leaves the workspace root as soon as the path is resolved (it
        // would land in the temp directory that holds this workspace).
        let would_be = root.join("prompts").join("../../escape.md");
        assert!(
            would_be
                .components()
                .any(|c| c == std::path::Component::ParentDir),
            "control: the ungated join must carry a '..' component: {would_be:?}"
        );
        let resolved_escape = temp.path().join("escape.md");
        assert!(
            !resolved_escape.exists(),
            "the escape target {resolved_escape:?} must not exist"
        );
    }

    /// A plain `.git` **directory** checkout is detected from a subdirectory of the
    /// repository: the reported root is the tree holding the `.git` entry, and the
    /// branch comes from `HEAD`.
    #[test]
    fn detect_git_workspace_reports_a_plain_checkout_and_its_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(repo.join("src")).expect("fixture");
        std::fs::create_dir_all(&git_dir).expect("fixture git dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/feature/x\n").expect("HEAD");
        // Linked worktrees recorded by the main checkout are counted.
        std::fs::create_dir_all(git_dir.join("worktrees/a")).expect("fixture");
        std::fs::create_dir_all(git_dir.join("worktrees/b")).expect("fixture");
        let nested = repo.join("src");

        let detected = detect_git_workspace(&nested);
        assert!(detected.is_git_repo, "the checkout must be detected");
        assert_eq!(
            detected.git_root.as_deref(),
            Some(repo.as_path()),
            "the tree holding `.git` is the reported root, not the inspected cwd"
        );
        assert_eq!(detected.git_dir.as_deref(), Some(git_dir.as_path()));
        assert_eq!(
            detected.branch.as_deref(),
            Some("feature/x"),
            "`ref: refs/heads/<name>` must shorten to `<name>`"
        );
        assert_eq!(detected.worktree_count, Some(2));
        assert_eq!(detected.inspected_root.as_deref(), Some(nested.as_path()));
    }

    /// A `.git` **file** (linked worktree / submodule style) resolves through the
    /// `gitdir:` reference — absolute and relative forms both — so the branch of the
    /// linked worktree, read from *its* own `HEAD`, is what gets reported.
    #[test]
    fn detect_git_workspace_resolves_a_gitdir_file_reference() {
        let temp = tempfile::tempdir().expect("tempdir");
        let main_git = temp.path().join("main_repo/.git");
        let linked_git = main_git.join("worktrees/feature-x");
        std::fs::create_dir_all(linked_git.as_path()).expect("fixture");
        std::fs::write(linked_git.join("HEAD"), "ref: refs/heads/feature-x\n").expect("HEAD");

        // Absolute `gitdir:` target, the shape `git worktree add` writes.
        let worktree = temp.path().join("feature-x");
        std::fs::create_dir_all(&worktree).expect("fixture");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", linked_git.display()),
        )
        .expect("fixture");

        let detected = detect_git_workspace(&worktree);
        assert!(detected.is_git_repo, "a `.git` file is still a repository");
        assert_eq!(
            detected.git_root.as_deref(),
            Some(worktree.as_path()),
            "the linked worktree itself is the reported root"
        );
        assert_eq!(detected.git_dir.as_deref(), Some(linked_git.as_path()));
        assert_eq!(detected.branch.as_deref(), Some("feature-x"));
        // A linked worktree records no sub-worktrees of its own.
        assert_eq!(detected.worktree_count, None);

        // Relative `gitdir:` target, resolved against the `.git` file's directory.
        let relative = temp.path().join("rel_wt");
        std::fs::create_dir_all(&relative).expect("fixture");
        std::fs::write(relative.join(".git"), "gitdir: ../main_repo/.git\n").expect("fixture");

        let detected = detect_git_workspace(&relative);
        assert_eq!(detected.git_dir.as_deref(), Some(main_git.as_path()));
        assert_eq!(
            detected.branch, None,
            "the main checkout's HEAD is absent here, so no branch may be invented"
        );
    }

    /// A directory with no `.git` entry inside it is reported as it is: a bare
    /// directory is never reported as a repository rooted at itself. (A hit further
    /// up the ancestor chain — an incidental outer checkout, e.g. the temp dir
    /// inside one — can only ever be an ancestor of the fixture, which the
    /// assertion below pins down.)
    #[test]
    fn detect_git_workspace_reports_a_plain_directory_as_no_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("plain_project");
        std::fs::create_dir_all(project.join("src")).expect("fixture");
        std::fs::write(project.join("src/main.rs"), "fn main() {}").expect("fixture");

        let detected = detect_git_workspace(&project);
        assert!(
            !detected.is_git_repo,
            "a fresh directory with no `.git` entry must be reported as no repository: \
             {detected:?}"
        );
        assert_eq!(
            detected.git_root, None,
            "no repository root may be reported"
        );
        assert_eq!(detected.branch, None, "no branch may be invented");
        assert_eq!(detected.worktree_count, None);
        assert_eq!(
            detected.inspected_root.as_deref(),
            Some(project.as_path()),
            "the inspected root is reported verbatim, so the no-repository verdict names it"
        );
    }

    /// `HEAD` parsing covers the three shapes the file actually holds: a local
    /// branch, a non-`refs/heads` symbolic ref kept verbatim, and a bare object id
    /// (detached head). An empty or missing `HEAD` reports no branch at all.
    #[test]
    fn detect_git_workspace_parses_branch_from_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        for (head, expected) in [
            ("ref: refs/heads/main\n", Some("main")),
            (
                "ref: refs/remotes/upstream/release\n",
                Some("refs/remotes/upstream/release"),
            ),
            (
                "1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c\n",
                Some("HEAD (detached)"),
            ),
            ("", None),
        ] {
            let repo = temp.path().join("head-case");
            let git_dir = repo.join(".git");
            std::fs::create_dir_all(&git_dir).expect("fixture");
            std::fs::write(git_dir.join("HEAD"), head).expect("HEAD");

            let detected = detect_git_workspace(&repo);
            assert!(detected.is_git_repo, "the fixture is a checkout");
            assert_eq!(
                detected.branch.as_deref(),
                expected,
                "HEAD {head:?} must map to {expected:?}"
            );
            std::fs::remove_file(git_dir.join("HEAD")).expect("drop HEAD");
            let detected = detect_git_workspace(&repo);
            assert_eq!(detected.branch, None, "no `HEAD` file means no branch");
            assert!(
                detected.is_git_repo,
                "an unreadable branch must not unset `is_git_repo`"
            );
            std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("restore HEAD");
        }
    }

    /// The prompt fragment is a state pointer, not a procedure: it stays compact,
    /// names the detected root/branch/worktrees, points at the `git_workflow` skill,
    /// and says nothing when no verdict was observed.
    #[test]
    fn format_git_prompt_block_states_detected_state_compactly() {
        let repo = GitWorkspace {
            is_git_repo: true,
            git_root: Some("/repo/.worktrees/feature".into()),
            git_dir: Some("/repo/.git".into()),
            branch: Some("feature".into()),
            worktree_count: Some(2),
            inspected_root: Some("/repo/.worktrees/feature".into()),
        };
        let block = format_git_prompt_block(&repo);
        assert!(block.starts_with("\n## Git workspace state\n"), "{block}");
        assert!(block.contains("IS a git repository"), "{block}");
        assert!(block.contains("root `/repo/.worktrees/feature`"), "{block}");
        assert!(block.contains("branch `feature`"), "{block}");
        assert!(block.contains("(linked worktrees: 2)"), "{block}");
        assert!(block.contains(".worktrees/"), "{block}");
        assert!(block.contains("git -C <path>"), "{block}");
        assert!(block.contains("never commit `.marmel/`"), "{block}");
        assert!(block.contains("git_workflow"), "{block}");
        assert!(
            block.lines().count() <= 10,
            "the fragment must stay short:\n{block}"
        );

        let new_project = GitWorkspace {
            inspected_root: Some("/srv/new-project".into()),
            ..GitWorkspace::default()
        };
        let block = format_git_prompt_block(&new_project);
        assert!(block.contains("NOT a git repository"), "{block}");
        assert!(block.contains("/srv/new-project"), "{block}");
        assert!(block.contains("git init"), "{block}");
        assert!(block.contains(".gitignore"), "{block}");
        assert!(block.contains("initial commit"), "{block}");
        assert!(
            block.lines().count() <= 10,
            "the fragment must stay short:\n{block}"
        );

        // A repository whose branch and worktrees are unknown still states the
        // detected facts, spelling the gaps out instead of inventing them.
        let block = format_git_prompt_block(&GitWorkspace {
            is_git_repo: true,
            ..GitWorkspace::default()
        });
        assert!(block.contains("root `unknown`"), "{block}");
        assert!(block.contains("branch `unknown`"), "{block}");
        assert!(!block.contains("linked worktrees"), "{block}");

        // The gate both prompt paths honour: no inspection, no verdict, no block.
        assert_eq!(format_git_prompt_block(&GitWorkspace::default()), "");
    }
}
