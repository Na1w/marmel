//! Integration coverage for the six session-config keys of `marmennill::config`:
//! `session_enabled`, `session_resume`, `session_commit_min_interval_ms`,
//! `session_worker_mirror_max_messages`, `session_commit_on_tool_result`,
//! `session_max_bundle_bytes`.
//!
//! Covered: the `Config::default()` values and the public `DEFAULT_SESSION_*` /
//! `MARMEL_*` env-key consts; the `env_flag` tri-state table (anything outside
//! `1|true|yes|on` / `0|false|no|off` means "leave the file value alone");
//! `apply_session_env` precedence through its pure seam (a config plus three
//! `Option<String>` env values in, a config out, no process env involved);
//! `load()` over throwaway `marmel.toml` files — full, partial, typo'd keys, a
//! TOML type mismatch, a missing explicit path, the env-beats-file overrides; and
//! `load(None)`'s `resolve_config_path` walk over the four cwd and four `$HOME`
//! candidates in priority order, where a candidate that exists but cannot be read
//! is reported — never panicked over, never silently defaulted.
//!
//! Not covered: `--resume` / `--fresh` flag parsing, the contradictory-flag usage
//! error (exit 2) and the `std::process::exit(EXIT_RESTORE_REFUSED)` call — all in
//! the `marmel` **binary** target. The seam resolving that precedence,
//! `ui::session::set_resume_request` / `resume_requested`, is covered here.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use marmennill::config::Config;
use marmennill::config::DEFAULT_SESSION_COMMIT_MIN_INTERVAL_MS;
use marmennill::config::DEFAULT_SESSION_MAX_BUNDLE_BYTES;
use marmennill::config::DEFAULT_SESSION_WORKER_MIRROR_MAX_MESSAGES;
use marmennill::config::SESSION_COMMIT_MIN_MS_ENV;
use marmennill::config::SESSION_DISABLE_ENV;
use marmennill::config::SESSION_RESUME_ENV;
use marmennill::config::apply_session_env;
use marmennill::config::env_flag;
use marmennill::config::load;
use marmennill::session::EXIT_RESTORE_REFUSED;
use marmennill::ui::session::resume_requested;
use marmennill::ui::session::set_resume_request;

/// Serializes every test that reads or writes the three session env keys.
static SESSION_ENV_TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Take the env serialization lock. A poisoned lock is tolerated on purpose:
/// poisoning can only come from a sibling test that panicked while holding it,
/// and that test has already failed, so failing a second, unrelated test with an
/// opaque poisoning panic would only hide the real diagnosis.
fn session_env_lock() -> MutexGuard<'static, ()> {
    SESSION_ENV_TEST_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Snapshot, scrub, optionally install, restore — for the duration of one test.
///
/// Holds `SESSION_ENV_TEST_SERIAL` for its whole lifetime, so the env reads
/// performed by `load()` inside the same test cannot race a sibling test's write.
struct SessionEnvGuard {
    _serial: MutexGuard<'static, ()>,
    saved: [(&'static str, Option<OsString>); 3],
}

impl SessionEnvGuard {
    /// Remove all three session keys for the duration of one test.
    fn cleared() -> Self {
        Self::with_env(&[])
    }

    /// Remove all three session keys, then set exactly one of them.
    fn set(key: &'static str, value: &str) -> Self {
        Self::with_env(&[(key, value)])
    }

    fn with_env(overrides: &[(&'static str, &str)]) -> Self {
        let serial = session_env_lock();
        let saved = [
            (SESSION_RESUME_ENV, std::env::var_os(SESSION_RESUME_ENV)),
            (SESSION_DISABLE_ENV, std::env::var_os(SESSION_DISABLE_ENV)),
            (
                SESSION_COMMIT_MIN_MS_ENV,
                std::env::var_os(SESSION_COMMIT_MIN_MS_ENV),
            ),
        ];
        // SAFETY: the invariant is process-global *test* state, not thread count.
        // `SESSION_ENV_TEST_SERIAL` is held from just before these writes until
        // after the `Drop` restore below (the `MutexGuard` is a field and so
        // drops last), and every test in this binary that touches these keys
        // takes the same lock, so no env read can race with these mutations.
        unsafe {
            for (key, _) in &saved {
                std::env::remove_var(*key);
            }
            for (key, value) in overrides {
                std::env::set_var(*key, *value);
            }
        }
        Self {
            _serial: serial,
            saved,
        }
    }
}

impl Drop for SessionEnvGuard {
    fn drop(&mut self) {
        // SAFETY: `_serial` is still held here (fields drop only after the
        // `Drop::drop` body returns), so this restore cannot race either. A
        // `None` snapshot means "the key was absent", restored by removing the
        // key again rather than by writing an empty value.
        unsafe {
            for (key, previous) in self.saved.iter_mut() {
                match previous.take() {
                    Some(previous) => std::env::set_var(*key, previous),
                    None => std::env::remove_var(*key),
                }
            }
        }
    }
}

/// Throwaway `HOME` + cwd for one `load(None)` test, so the searched paths are
/// the ones the test creates and never the developer's real ones.
///
/// Constructed exclusively through [`SessionEnvGuard::isolated_paths`]: taking
/// `&SessionEnvGuard` proves `SESSION_ENV_TEST_SERIAL` is already held, and the
/// env reads and cwd/`HOME` writes performed here all happen under that lock.
struct PathIsolation {
    saved_home: Option<OsString>,
    saved_cwd: PathBuf,
    home_dir: tempfile::TempDir,
    cwd_dir: tempfile::TempDir,
}

impl PathIsolation {
    /// Root of the `$HOME` candidate search for this test.
    fn home(&self) -> &Path {
        self.home_dir.path()
    }

    /// Root of the cwd candidate search for this test.
    fn cwd(&self) -> &Path {
        self.cwd_dir.path()
    }
}

impl Drop for PathIsolation {
    fn drop(&mut self) {
        // The cwd is released *before* anything else: both temp dirs are still
        // alive (they are fields, so they drop after this body), and a process
        // parked inside a directory that is about to be deleted is the one state
        // that would make the cleanup unreliable for every later test.
        std::env::set_current_dir(&self.saved_cwd)
            .expect("the isolated cwd must be releasable back to the checkout");
        // SAFETY: the `SessionEnvGuard` that created this isolation is still
        // alive (it is bound in the test body and drops after it), so the
        // process-wide env lock is still held across this restore.
        unsafe {
            match &self.saved_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

impl SessionEnvGuard {
    /// Relocate `HOME` and the process cwd into fresh, empty temp directories.
    ///
    /// Requires `&self`, i.e. the caller must already own a `SessionEnvGuard`,
    /// which is what keeps `SESSION_ENV_TEST_SERIAL` held (see the struct docs).
    fn isolated_paths(&self) -> PathIsolation {
        let home_dir = tempfile::tempdir().expect("tempdir for the isolated HOME");
        let cwd_dir = tempfile::tempdir().expect("tempdir for the isolated cwd");
        let saved_home = std::env::var_os("HOME");
        let saved_cwd = std::env::current_dir().expect("the test process must have a cwd");
        // SAFETY: same rationale as `SessionEnvGuard::with_env` — the mutex is
        // held by the guard this method was called on, no other test in this
        // binary can be reading `HOME` or the cwd at this point, and the restore
        // happens in `PathIsolation::drop` while that lock is still held.
        unsafe {
            std::env::set_var("HOME", home_dir.path());
        }
        std::env::set_current_dir(cwd_dir.path()).expect("the isolated cwd must be enterable");
        PathIsolation {
            saved_home,
            saved_cwd,
            home_dir,
            cwd_dir,
        }
    }

    /// Install the one-shot CLI resume request (`--resume` / `--fresh`) into the
    /// `ui::session` handoff statics and hand it back for restoration.
    ///
    /// `RESUME_REQUESTED` / `FRESH_REQUESTED` in `ui::session`
    /// are process-global statics written exactly once by `main` in production,
    /// so a test that writes them must hold the same process-wide mutex as the
    /// env keys and must leave them at the `(false, false)` boot state. Requiring
    /// `&self` is what supplies that lock without re-entering the (non-reentrant)
    /// mutex.
    fn resume_handoff(&self, resume: bool, fresh: bool) -> ResumeRequestGuard<'_> {
        set_resume_request(resume, fresh);
        ResumeRequestGuard { _env: self }
    }
}

/// Restore-the-handoff half of [`SessionEnvGuard::resume_handoff`]. The
/// `&_env` field exists to tie this guard's lifetime to the env guard that owns
/// the serialization lock, so it cannot outlive it.
struct ResumeRequestGuard<'env> {
    _env: &'env SessionEnvGuard,
}

impl Drop for ResumeRequestGuard<'_> {
    fn drop(&mut self) {
        set_resume_request(false, false);
    }
}

/// Write a session-marker config into one of the searched candidate locations
/// under `root` (e.g. `.marmel/marmel.toml`) and return the path written.
///
/// `marker` seeds `session_worker_mirror_max_messages` (and a distinct byte
/// ceiling derived from it) so a single `load(None)` tells the caller *which*
/// candidate won.
fn write_candidate(root: &Path, relative: &str, marker: usize) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("candidate parent dir must be creatable");
    }
    std::fs::write(
        &path,
        format!(
            "session_worker_mirror_max_messages = {marker}\n\
             session_max_bundle_bytes = {}\n",
            u64::try_from(marker)
                .expect("marker must fit u64")
                .saturating_mul(1024)
        ),
    )
    .expect("candidate config must be writable");
    path
}

/// Minimal stand-in for the process environment: key -> raw value. Lets the
/// `apply_session_env` tests name the real env keys without touching the process.
type EnvMap = BTreeMap<&'static str, String>;

fn env_map(entries: &[(&'static str, &'static str)]) -> EnvMap {
    entries
        .iter()
        .map(|(key, value)| (*key, (*value).to_string()))
        .collect()
}

/// Feed `env` into the pure seam exactly the way `load()` feeds the live process
/// environment into `apply_session_env`.
fn apply_from_map(cfg: Config, env: &EnvMap) -> Config {
    apply_session_env(
        cfg,
        env.get(SESSION_RESUME_ENV).cloned(),
        env.get(SESSION_DISABLE_ENV).cloned(),
        env.get(SESSION_COMMIT_MIN_MS_ENV).cloned(),
    )
}

/// Write a throwaway `marmel.toml` into `dir` and return its path as a `String`
/// suitable for `load(Some(..))`.
fn write_toml(dir: &tempfile::TempDir, contents: &str) -> String {
    let path = dir.path().join("marmel.toml");
    std::fs::write(&path, contents).expect("temp marmel.toml must be writable");
    path.to_str().expect("temp path must be utf-8").to_string()
}

#[test]
fn defaults_and_public_consts_match_documented_values() {
    let cfg = Config::default();
    assert!(cfg.session_enabled, "capture defaults ON (Design A D2)");
    assert!(
        !cfg.session_resume,
        "a fresh start is the default behaviour (flow §3.2)"
    );
    assert_eq!(
        cfg.session_commit_min_interval_ms, 250,
        "default floor 250 ms"
    );
    assert_eq!(
        cfg.session_worker_mirror_max_messages, 4_000,
        "default mirror cap"
    );
    assert!(cfg.session_commit_on_tool_result, "T-3 commits by default");
    assert_eq!(cfg.session_max_bundle_bytes, 33_554_432, "default 32 MiB");

    // The defaults must come from the documented public consts, not from
    // duplicated literals inside `Default::default`.
    assert_eq!(
        cfg.session_commit_min_interval_ms, DEFAULT_SESSION_COMMIT_MIN_INTERVAL_MS,
        "the floor default must be the public const"
    );
    assert_eq!(
        cfg.session_worker_mirror_max_messages, DEFAULT_SESSION_WORKER_MIRROR_MAX_MESSAGES,
        "the mirror cap default must be the public const"
    );
    assert_eq!(
        cfg.session_max_bundle_bytes, DEFAULT_SESSION_MAX_BUNDLE_BYTES,
        "the bundle ceiling default must be the public const"
    );

    // And the env keys must be the operator-facing names the docs promise. They
    // are copied into owned locals first so the assertions compare values rather
    // than two compile-time constants.
    let resume_key = SESSION_RESUME_ENV.to_string();
    let disable_key = SESSION_DISABLE_ENV.to_string();
    let min_ms_key = SESSION_COMMIT_MIN_MS_ENV.to_string();
    assert_eq!(resume_key, "MARMEL_RESUME");
    assert_eq!(disable_key, "MARMEL_SESSION_DISABLE");
    assert_eq!(min_ms_key, "MARMEL_SESSION_COMMIT_MIN_MS");
}

#[test]
fn env_flag_truthy_table_returns_some_true() {
    for raw in ["1", "true", "yes", "on", "TRUE", "Yes", " ON ", "\tyes\n"] {
        let value = raw.to_string();
        assert_eq!(
            env_flag(Some(&value)),
            Some(true),
            "`{raw}` must read as true (trim + ascii-lowercase)"
        );
    }
}

#[test]
fn env_flag_falsy_table_returns_some_false() {
    for raw in ["0", "false", "no", "off", "FALSE", "No", " Off "] {
        let value = raw.to_string();
        assert_eq!(
            env_flag(Some(&value)),
            Some(false),
            "`{raw}` must read as false (trim + ascii-lowercase)"
        );
    }
}

#[test]
fn env_flag_garbage_or_unset_returns_none() {
    // `None` is the "leave the file/default value alone" signal, so junk must
    // never be coerced into a boolean — and never into `false` by accident.
    for raw in ["maybe", "", "   ", "2", "0.0", "truye", "enabled"] {
        let value = raw.to_string();
        assert_eq!(
            env_flag(Some(&value)),
            None,
            "`{raw}` must not be a boolean"
        );
    }
    assert_eq!(env_flag(None), None, "unset must be None");
}

#[test]
fn apply_session_env_resume_truthy_turns_resume_on() {
    let env = env_map(&[(SESSION_RESUME_ENV, "1")]);
    let out = apply_from_map(Config::default(), &env);
    assert!(
        out.session_resume,
        "MARMEL_RESUME=1 turns resume on for this run"
    );
    assert!(
        out.session_enabled,
        "resume must not disturb the capture switch"
    );
    assert_eq!(
        out.session_commit_min_interval_ms, 250,
        "resume must not disturb the commit floor"
    );
}

#[test]
fn apply_session_env_resume_falsy_turns_resume_off() {
    let base = Config {
        session_resume: true,
        ..Default::default()
    };
    let env = env_map(&[(SESSION_RESUME_ENV, "off")]);
    let out = apply_from_map(base, &env);
    assert!(
        !out.session_resume,
        "a recognized falsy value overrides a `session_resume = true` from the file"
    );
}

#[test]
fn apply_session_env_disable_truthy_turns_capture_off() {
    let env = env_map(&[(SESSION_DISABLE_ENV, "true")]);
    let out = apply_from_map(Config::default(), &env);
    assert!(
        !out.session_enabled,
        "MARMEL_SESSION_DISABLE=true turns capture off"
    );
    assert!(
        !out.session_resume,
        "disabling capture must not imply resume"
    );
}

#[test]
fn apply_session_env_disable_is_one_way_and_never_reenables() {
    // `SESSION_DISABLE_ENV` is documented as one-way and
    // `apply_session_env` confirms it: the only write this
    // function makes to `session_enabled` is `false`, under
    // `matches!(env_flag(disable), Some(true))`. A falsy, unrecognized or absent
    // value therefore cannot bring capture back on.
    let base = Config {
        session_enabled: false,
        ..Default::default()
    };

    for raw in ["0", "false", "no", "off"] {
        let env = env_map(&[(SESSION_DISABLE_ENV, raw)]);
        let out = apply_from_map(base.clone(), &env);
        assert!(
            !out.session_enabled,
            "`{raw}` must not re-enable capture: one-way switch"
        );
    }

    let out = apply_from_map(base.clone(), &env_map(&[]));
    assert!(
        !out.session_enabled,
        "an unset disable key keeps capture off"
    );

    let env = env_map(&[(SESSION_DISABLE_ENV, "please")]);
    let out = apply_from_map(base, &env);
    assert!(
        !out.session_enabled,
        "junk in the disable key keeps capture off (warn-and-ignore)"
    );
}

#[test]
fn apply_session_env_min_ms_non_integer_keeps_previous_and_never_zero() {
    // `apply_session_env` parses the floor with `raw.trim().parse::<u64>()`; the
    // `Err` arm only warns and leaves the field alone, so a bad value must never
    // silently become a 0 ms floor (which would disable commit throttling).
    for raw in ["soon", "abc", "", "   ", "1500ms", "-5", "1_000", "2.5"] {
        let base = Config {
            session_commit_min_interval_ms: 900,
            ..Default::default()
        };
        let env = env_map(&[(SESSION_COMMIT_MIN_MS_ENV, raw)]);
        let out = apply_from_map(base, &env);
        assert_eq!(
            out.session_commit_min_interval_ms, 900,
            "`{raw}` must be ignored and the previous floor kept"
        );
        assert_ne!(
            out.session_commit_min_interval_ms, 0,
            "an unparseable floor must never yield 0"
        );
    }
}

#[test]
fn apply_session_env_min_ms_zero_is_applied_verbatim_without_clamping() {
    // LOAD-BEARING case: `"0"` is a *valid* `u64`, so the
    // `Ok(value)` arm of `apply_session_env` stores it verbatim — there is no
    // lower clamp anywhere in `apply_session_env`, unlike the documented clamp for
    // `command_timeout_secs`. "never yields 0" holds only for *unparseable*
    // input (see the sibling test).
    for raw in ["0", " 0 "] {
        let env = env_map(&[(SESSION_COMMIT_MIN_MS_ENV, raw)]);
        let out = apply_from_map(Config::default(), &env);
        assert_eq!(
            out.session_commit_min_interval_ms, 0,
            "`{raw}` parses as a valid u64 and is stored verbatim"
        );
    }
}

#[test]
fn apply_session_env_min_ms_valid_large_value_is_applied_verbatim() {
    let base = Config {
        session_commit_min_interval_ms: 250,
        ..Default::default()
    };

    let env = env_map(&[(SESSION_COMMIT_MIN_MS_ENV, "9007199254740993")]);
    let out = apply_from_map(base.clone(), &env);
    assert_eq!(out.session_commit_min_interval_ms, 9_007_199_254_740_993);

    // Whitespace is trimmed before parsing, and a large-but-sane value wins over
    // the file/default value.
    let env = env_map(&[(SESSION_COMMIT_MIN_MS_ENV, "  3600000  ")]);
    let out = apply_from_map(base, &env);
    assert_eq!(out.session_commit_min_interval_ms, 3_600_000);
}

#[test]
fn apply_session_env_with_unset_env_is_pure_and_idempotent() {
    // No env key set => the seam must be a no-op, twice over, even for a config
    // that deliberately differs from every default.
    let base = Config {
        session_enabled: false,
        session_resume: true,
        session_commit_min_interval_ms: 1200,
        session_worker_mirror_max_messages: 7,
        session_commit_on_tool_result: false,
        session_max_bundle_bytes: 4096,
        ..Default::default()
    };

    let empty = env_map(&[]);
    let once = apply_from_map(base.clone(), &empty);
    let twice = apply_from_map(once.clone(), &empty);

    for out in [&once, &twice] {
        assert!(!out.session_enabled);
        assert!(out.session_resume);
        assert_eq!(out.session_commit_min_interval_ms, 1200);
        assert_eq!(out.session_worker_mirror_max_messages, 7);
        assert!(!out.session_commit_on_tool_result);
        assert_eq!(out.session_max_bundle_bytes, 4096);
    }
}

#[test]
fn load_full_toml_honours_all_six_session_keys() {
    // `load()` reads the process env last, so scrub the three session keys first.
    let _guard = SessionEnvGuard::cleared();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(
        &dir,
        r#"
session_enabled = true
session_resume = true
session_commit_min_interval_ms = 750
session_worker_mirror_max_messages = 120
session_commit_on_tool_result = false
session_max_bundle_bytes = 1048576
"#,
    );

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(cfg.session_enabled);
    assert!(
        cfg.session_resume,
        "`session_resume = true` in the file is honored"
    );
    assert_eq!(cfg.session_commit_min_interval_ms, 750);
    assert_eq!(cfg.session_worker_mirror_max_messages, 120);
    assert!(!cfg.session_commit_on_tool_result);
    assert_eq!(cfg.session_max_bundle_bytes, 1_048_576);
}

#[test]
fn load_partial_toml_keeps_defaults_for_unspecified_keys() {
    let _guard = SessionEnvGuard::cleared();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(
        &dir,
        r#"
session_commit_min_interval_ms = 500
session_max_bundle_bytes = 4096
"#,
    );

    let cfg = load(Some(path.as_str())).expect("a partial marmel.toml must load");
    assert_eq!(cfg.session_commit_min_interval_ms, 500);
    assert_eq!(cfg.session_max_bundle_bytes, 4_096);
    // Absent keys keep their defaults: every `PartialConfig` session field is an
    // `Option<_>`, and `merge` only assigns the `Some` ones.
    assert!(cfg.session_enabled);
    assert!(!cfg.session_resume);
    assert_eq!(cfg.session_worker_mirror_max_messages, 4_000);
    assert!(cfg.session_commit_on_tool_result);
}

#[test]
fn load_typo_or_unknown_key_is_silently_ignored() {
    // HAZARD DOCUMENTED ON PURPOSE: nothing in this crate uses
    // `#[serde(deny_unknown_fields)]`, so a misspelled session key is dropped
    // without a word and the default silently wins. If a future change adds
    // `deny_unknown_fields`, this test must be inverted to assert an error.
    let _guard = SessionEnvGuard::cleared();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(
        &dir,
        r#"
session_comit_min_interval_ms = 999
session_commit_min_interval_ms_typo = 999
sesion_enabled = false
"#,
    );

    let cfg = load(Some(path.as_str())).expect("an unknown key must not be an error");
    assert_eq!(
        cfg.session_commit_min_interval_ms, 250,
        "the typo'd floor key was ignored, so the default survived"
    );
    assert!(cfg.session_enabled, "the typo'd capture key was ignored");
}

#[test]
fn load_missing_path_is_an_error_and_never_panics() {
    // Observed behaviour of `load`: an explicit path is used
    // verbatim by `resolve_config_path` and a failed read
    // is propagated with `?` + `.with_context(|| format!("reading {}", ..))`.
    // A missing *explicit* path is therefore an error, not a silent default.
    let _guard = SessionEnvGuard::cleared();
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir
        .path()
        .join("absent-marmel-config.toml")
        .to_str()
        .expect("temp path must be utf-8")
        .to_string();

    match load(Some(missing.as_str())) {
        Ok(_) => panic!("a missing explicit config path must be an error, not a default"),
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("reading"),
                "the error must name the read: {message}"
            );
            assert!(
                message.contains("absent-marmel-config.toml"),
                "the error must name the path: {message}"
            );
        }
    }
}

#[test]
fn load_none_uses_the_searched_paths_and_never_panics() {
    // HERMETIC: `load(None)` walks the cwd candidates and then the
    // `$HOME` candidates through `resolve_config_path`; both authorities are moved
    // into fresh *empty* temp dirs, so "no candidate exists anywhere" is a state
    // this test creates rather than one it hopes the machine happens to be in —
    // a real `$HOME` can hold an unreadable `$HOME/.marmel/marmel.toml`.
    let env_guard = SessionEnvGuard::cleared();
    let paths = env_guard.isolated_paths();
    assert!(
        paths.home().exists() && paths.cwd().exists(),
        "the isolated path roots must exist"
    );

    let cfg = load(None).expect("load(None) must not fail when no searched path exists");
    // Nothing was found, so every session key must still carry the documented
    // default — the half-applied-config failure mode this test has always
    // guarded, now asserted exactly instead of only "non-zero".
    assert!(cfg.session_enabled, "capture defaults ON");
    assert!(!cfg.session_resume, "a fresh start is the default");
    assert_eq!(
        cfg.session_commit_min_interval_ms, DEFAULT_SESSION_COMMIT_MIN_INTERVAL_MS,
        "the commit floor must be the documented default"
    );
    assert_ne!(
        cfg.session_worker_mirror_max_messages, 0,
        "mirror cap must be set"
    );
    assert!(cfg.session_commit_on_tool_result, "T-3 commits by default");
    assert_ne!(
        cfg.session_max_bundle_bytes, 0,
        "bundle ceiling must be set"
    );
    assert_eq!(
        cfg.session_worker_mirror_max_messages, DEFAULT_SESSION_WORKER_MIRROR_MAX_MESSAGES,
        "an empty search must leave the mirror cap at its default"
    );
    assert_eq!(
        cfg.session_max_bundle_bytes, DEFAULT_SESSION_MAX_BUNDLE_BYTES,
        "an empty search must leave the bundle ceiling at its default"
    );
}

#[test]
fn load_none_walks_the_home_candidates_in_documented_order() {
    // `resolve_config_path`'s `$HOME` block searches, in this order:
    //   $HOME/.marmel/marmel.toml, $HOME/.marmel/config.toml,
    //   $HOME/.config/marmel/config.toml, $HOME/.config/marmel/marmel.toml
    // and returns the FIRST hit, so each step below names the winner by the
    // marker value it carries.
    let env_guard = SessionEnvGuard::cleared();
    let paths = env_guard.isolated_paths();

    // Only the *second* home candidate exists: the walk must fall through to it.
    let found = write_candidate(paths.home(), ".marmel/config.toml", 11);
    let cfg = load(None).expect("the 2nd home candidate must be found");
    assert_eq!(
        cfg.session_worker_mirror_max_messages,
        11,
        "`{}` must be consulted when the 1st home candidate is absent",
        found.display()
    );
    assert_eq!(
        cfg.session_max_bundle_bytes,
        11 * 1024,
        "same file, same merge"
    );

    // The 1st candidate now exists too: first hit wins, the 2nd is never read.
    let winner = write_candidate(paths.home(), ".marmel/marmel.toml", 12);
    let cfg = load(None).expect("the 1st home candidate must win");
    assert_eq!(
        cfg.session_worker_mirror_max_messages,
        12,
        "`{}` outranks `.marmel/config.toml`",
        winner.display()
    );

    // Drop both `.marmel` candidates: the walk must fall through to the
    // `.config/marmel` pair. Note the deliberate asymmetry in
    // `resolve_config_path`'s `$HOME` list — the 3rd home candidate is
    // `config.toml` and the 4th is `marmel.toml`, the
    // mirror image of the `.marmel` pair above.
    std::fs::remove_dir_all(paths.home().join(".marmel"))
        .expect("the .marmel fixture must be removable");
    let last = write_candidate(paths.home(), ".config/marmel/marmel.toml", 14);
    let cfg = load(None).expect("the last home candidate must be found");
    assert_eq!(
        cfg.session_worker_mirror_max_messages,
        14,
        "`{}` must be consulted when the three higher-priority home candidates are absent",
        last.display()
    );

    let winner = write_candidate(paths.home(), ".config/marmel/config.toml", 13);
    let cfg = load(None).expect("the 3rd home candidate must outrank the 4th");
    assert_eq!(
        cfg.session_worker_mirror_max_messages,
        13,
        "`{}` (3rd) must outrank `.config/marmel/marmel.toml` (4th)",
        winner.display()
    );
}

#[test]
fn load_none_prefers_cwd_candidates_over_home_candidates() {
    // `resolve_config_path` searches the cwd block
    //   ./marmel.toml, ./.marmel.toml, ./.marmel/marmel.toml, ./.marmel/config.toml
    // *before* the `$HOME` block, and returns the first hit. The ladder below
    // pins the whole order: a HOME value loses to any cwd hit, and within the
    // cwd block each earlier name outranks each later one.
    let env_guard = SessionEnvGuard::cleared();
    let paths = env_guard.isolated_paths();

    let home = write_candidate(paths.home(), ".marmel/marmel.toml", 21);
    let cfg = load(None).expect("HOME must be searched when the cwd block is empty");
    assert_eq!(
        cfg.session_worker_mirror_max_messages,
        21,
        "`{}` must be used while no cwd candidate exists",
        home.display()
    );

    let expected: [(&str, usize); 4] = [
        (".marmel/config.toml", 22),
        (".marmel/marmel.toml", 23),
        (".marmel.toml", 24),
        ("marmel.toml", 25),
    ];
    for (relative, marker) in expected {
        let winner = write_candidate(paths.cwd(), relative, marker);
        let cfg = load(None).expect("a cwd candidate must be found");
        assert_eq!(
            cfg.session_worker_mirror_max_messages,
            marker,
            "`{}` must outrank every already-present lower-priority candidate \
             (cwd block before HOME block)",
            winner.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn load_none_reports_an_unreadable_candidate_as_an_error_not_a_panic() {
    // The candidate exists (`Path::exists` = stat succeeds) but cannot be
    // opened. The observable contract asserted here is the one `load`
    // implements — the read failure is propagated with its `reading <path>`
    // context, never panicked over and never swallowed into a default.
    // "Unreadable" is spelled as mode 000 rather than by inheriting the real
    // `$HOME`, because that is the part the sandbox actually blocks.
    let env_guard = SessionEnvGuard::cleared();
    let paths = env_guard.isolated_paths();
    let candidate = write_candidate(paths.home(), ".marmel/marmel.toml", 31);
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o000))
        .expect("the fixture candidate must be made unreadable");

    match load(None) {
        Ok(cfg) => panic!(
            "an unreadable candidate must not be silently defaulted (mirror={})",
            cfg.session_worker_mirror_max_messages
        ),
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("reading"),
                "the error must name the read: {message}"
            );
            assert!(
                message.contains(".marmel/marmel.toml"),
                "the error must name the unreadable path: {message}"
            );
        }
    }
}

#[test]
fn load_none_reports_an_unreadable_directory_candidate_as_an_error() {
    // Portable sibling of the mode-000 case above: a searched path that exists
    // but is not a readable regular file (EISDIR on unix) must surface as the
    // same reported read failure. This is the branch that keeps a stray
    // directory named `marmel.toml` from taking the process down.
    let env_guard = SessionEnvGuard::cleared();
    let paths = env_guard.isolated_paths();
    let candidate = paths.home().join(".marmel/marmel.toml");
    std::fs::create_dir_all(&candidate).expect("the directory fixture must be creatable");

    match load(None) {
        Ok(cfg) => panic!(
            "a directory where a config file belongs must not be defaulted (mirror={})",
            cfg.session_worker_mirror_max_messages
        ),
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("reading"),
                "the error must name the read: {message}"
            );
            assert!(
                message.contains(".marmel/marmel.toml"),
                "the error must name the offending path: {message}"
            );
        }
    }
}

#[test]
fn load_toml_type_mismatch_is_an_error_not_a_silent_default() {
    // The mirror image of the typo'd-key case: a *known* key carrying the wrong
    // TOML type fails `toml::from_str::<PartialConfig>` and `load` propagates it
    // instead of shrugging.
    let _guard = SessionEnvGuard::cleared();
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(&dir, "session_enabled = \"yes\"\n");

    match load(Some(path.as_str())) {
        Ok(cfg) => panic!(
            "a string where a bool belongs must be rejected, not defaulted \
             (session_enabled={})",
            cfg.session_enabled
        ),
        Err(err) => {
            // Only the presence of an error is asserted: the exact diagnostic
            // text belongs to the `toml` crate (UNSURE by design, not pinned).
            assert!(!err.to_string().is_empty(), "the error must be reportable");
        }
    }
}

#[test]
fn load_session_disable_env_overrides_file_value_one_way() {
    // End-to-end precedence: `load` merges the file first, then applies
    // `apply_session_env` with the live process env, so env beats file — but
    // only in the disabling direction, and only for the keys it knows.
    let _guard = SessionEnvGuard::set(SESSION_DISABLE_ENV, "1");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(
        &dir,
        r#"
session_enabled = true
session_commit_min_interval_ms = 600
"#,
    );

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(
        !cfg.session_enabled,
        "MARMEL_SESSION_DISABLE=1 must override `session_enabled = true` from the file"
    );
    assert_eq!(
        cfg.session_commit_min_interval_ms, 600,
        "the file floor still applies"
    );
}

#[test]
fn load_resume_env_truthy_overrides_a_false_file_value() {
    // `load` reads the live process env for the three session keys and feeds
    // them to `apply_session_env` *after* the TOML merge, so a recognized
    // `MARMEL_RESUME` outranks the file.
    let _guard = SessionEnvGuard::set(SESSION_RESUME_ENV, "yes");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(&dir, "session_resume = false\n");

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(
        cfg.session_resume,
        "`MARMEL_RESUME=yes` must override `session_resume = false` from the file"
    );
    assert!(
        cfg.session_enabled,
        "a resume request must not disturb the capture switch"
    );
}

#[test]
fn load_resume_env_falsy_overrides_a_true_file_value() {
    let _guard = SessionEnvGuard::set(SESSION_RESUME_ENV, "off");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(&dir, "session_resume = true\n");

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(
        !cfg.session_resume,
        "`MARMEL_RESUME=off` must override `session_resume = true` from the file"
    );
}

#[test]
fn load_commit_min_ms_env_overrides_the_file_floor() {
    let _guard = SessionEnvGuard::set(SESSION_COMMIT_MIN_MS_ENV, "450");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(&dir, "session_commit_min_interval_ms = 999\n");

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert_eq!(
        cfg.session_commit_min_interval_ms, 450,
        "`MARMEL_SESSION_COMMIT_MIN_MS=450` must override the file floor"
    );
}

#[test]
fn load_junk_session_env_values_keep_the_file_values() {
    // Every one of the three keys carrying an unrecognized value must land in the
    // warn-and-ignore arm of `apply_session_env`: the file values survive intact,
    // and capture is never switched by junk in a one-way key.
    let _guard = SessionEnvGuard::with_env(&[
        (SESSION_RESUME_ENV, "maybe"),
        (SESSION_DISABLE_ENV, "nope"),
        (SESSION_COMMIT_MIN_MS_ENV, "soon"),
    ]);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(
        &dir,
        r#"
session_enabled = true
session_resume = true
session_commit_min_interval_ms = 640
"#,
    );

    let cfg = load(Some(path.as_str())).expect("junk env must not be an error");
    assert!(
        cfg.session_resume,
        "unrecognized `MARMEL_RESUME` keeps the file value"
    );
    assert_eq!(
        cfg.session_commit_min_interval_ms, 640,
        "unrecognized `MARMEL_SESSION_COMMIT_MIN_MS` keeps the file floor"
    );
    assert!(
        cfg.session_enabled,
        "junk in the one-way disable key keeps capture on"
    );
}

#[test]
fn load_disable_and_resume_env_apply_together_and_disable_stays_one_way() {
    // All three env reads happen in `load` on one pass, so the
    // keys must not interfere: resume turns on, the floor keeps the file value,
    // and `MARMEL_SESSION_DISABLE=0` cannot re-enable an explicit
    // `session_enabled = false` from the file.
    let _guard =
        SessionEnvGuard::with_env(&[(SESSION_RESUME_ENV, "1"), (SESSION_DISABLE_ENV, "0")]);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(
        &dir,
        r#"
session_enabled = false
session_commit_min_interval_ms = 700
"#,
    );

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(
        cfg.session_resume,
        "`MARMEL_RESUME=1` turns resume on for this run"
    );
    assert_eq!(
        cfg.session_commit_min_interval_ms, 700,
        "the file floor still applies"
    );
    assert!(
        !cfg.session_enabled,
        "`MARMEL_SESSION_DISABLE=0` must not re-enable capture: one-way switch"
    );
}

#[test]
fn exit_restore_refused_const_is_three() {
    // The exit code a refused restore is carried out as. Only the constant is
    // observable from an integration test; the
    // `std::process::exit(EXIT_RESTORE_REFUSED)` call sits in the binary target
    // (`main`), which this file cannot link or spawn.
    assert_eq!(
        EXIT_RESTORE_REFUSED, 3,
        "refused-restore exit code (flow §3.5)"
    );
}

#[test]
fn resume_handoff_fresh_outranks_the_resume_flag_and_the_config_spelling() {
    // Precedence resolved by the public seam the binary hands its flags to
    // (`ui::session::set_resume_request`, `resume_requested`). The flags
    // themselves are parsed in `main`, which is unreachable from here — see the
    // module header.
    let env_guard = SessionEnvGuard::cleared();
    let _handoff = env_guard.resume_handoff(true, true);
    let cfg = Config {
        session_resume: true,
        ..Default::default()
    };

    assert!(
        !resume_requested(&cfg),
        "`--fresh` must win over `--resume` and over the MARMEL_RESUME/session_resume spelling"
    );
}

#[test]
fn resume_handoff_fresh_alone_outranks_the_config_spelling() {
    let env_guard = SessionEnvGuard::cleared();
    let _handoff = env_guard.resume_handoff(false, true);
    let cfg = Config {
        session_resume: true,
        ..Default::default()
    };

    assert!(
        !resume_requested(&cfg),
        "`--fresh` alone must still outrank `session_resume = true`"
    );
}

#[test]
fn resume_handoff_resume_flag_restores_without_a_config_spelling() {
    let env_guard = SessionEnvGuard::cleared();
    let _handoff = env_guard.resume_handoff(true, false);
    let cfg = Config::default();
    assert!(
        !cfg.session_resume,
        "the default config spells a fresh start"
    );

    assert!(
        resume_requested(&cfg),
        "`--resume` must request a restore even with `session_resume = false`"
    );
}

#[test]
fn resume_handoff_with_no_flag_falls_back_to_the_config_spelling() {
    let env_guard = SessionEnvGuard::cleared();
    let _handoff = env_guard.resume_handoff(false, false);

    let from_config = Config {
        session_resume: true,
        ..Default::default()
    };
    assert!(
        resume_requested(&from_config),
        "with no CLI flag, `session_resume = true` requests the restore"
    );
    assert!(
        !resume_requested(&Config::default()),
        "nothing requested => fresh start, byte-for-byte the pre-session behaviour"
    );
}

#[test]
fn resume_env_beats_the_file_and_the_handoff_agrees() {
    // The whole chain in one assertion pair: `MARMEL_RESUME` overrides the file
    // inside `load`, and the no-flag handoff then defers to that resolved config.
    let env_guard = SessionEnvGuard::set(SESSION_RESUME_ENV, "1");
    let _handoff = env_guard.resume_handoff(false, false);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(&dir, "session_resume = false\n");

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(
        cfg.session_resume,
        "`MARMEL_RESUME=1` must outrank `session_resume = false`"
    );
    assert!(
        resume_requested(&cfg),
        "with no CLI flag the resolved config alone decides the trigger"
    );
}

#[test]
fn resume_handoff_fresh_outranks_the_resume_env_end_to_end() {
    // Top of the precedence chain: `--fresh` is the documented escape hatch, so
    // even an env value that already won inside `load` must not force a restore.
    let env_guard = SessionEnvGuard::set(SESSION_RESUME_ENV, "1");
    let _handoff = env_guard.resume_handoff(false, true);
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_toml(&dir, "session_resume = false\n");

    let cfg = load(Some(path.as_str())).expect("a valid marmel.toml must load");
    assert!(cfg.session_resume, "the env value still wins inside `load`");
    assert!(
        !resume_requested(&cfg),
        "`--fresh` must outrank `MARMEL_RESUME=1`"
    );
}
