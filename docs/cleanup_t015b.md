# t-015b — `tests/test_session_config.rs` made fully green

Scope actually touched: **only** `tests/test_session_config.rs` plus this note
(`docs/cleanup_t015b.md`). No `src/**` file, no other test file, no `docs/**`
file other than this one. Nothing committed, no repo-wide `cargo fmt`.

---

## 1. Gate numbers (final, in the required order)

| # | Command | Result |
|---|---------|--------|
| 1 | `cargo test --test test_session_config` (run A) | `test result: ok. 35 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out` |
| 2 | `cargo test --test test_session_config` (run B, repeat) | `test result: ok. 35 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out` |
| 3 | `cargo test --test test_session_config -- --test-threads=1` (run twice) | `35 passed; 0 failed; 0 ignored` both times → no env/cwd race |
| 4 | `cargo clippy --all-targets --all-features -- -D warnings` | exit **0** (no warnings, log `/tmp/clippy_t015b_2.log`) |
| 5 | `rustfmt --edition 2024 tests/test_session_config.rs` (edition read from `Cargo.toml:4`) | exit **0**, scoped to that single file |
| 6 | Extra hermeticity probe: the compiled test binary run directly from `cwd=/tmp` with `HOME=/nonexistent-home MARMEL_RESUME=1 MARMEL_SESSION_DISABLE=1 MARMEL_SESSION_COMMIT_MIN_MS=7` and `--test-threads=1` | `35 passed; 0 failed; 0 ignored` |

Before this task: `18 passed / 1 failed` — `load_none_uses_the_searched_paths_and_never_panics`.

---

## 2. Root cause of the environmental failure (not a code defect)

`config::load(None)` (`src/config.rs:360`) calls `resolve_config_path(None)`
(`src/config.rs:460-492`), which returns the **first** existing candidate out of:

* cwd block (`src/config.rs:464-476`): `./marmel.toml`, `./.marmel.toml`,
  `./.marmel/marmel.toml`, `./.marmel/config.toml`;
* HOME block (`src/config.rs:477-489`), rooted at `home_dir()` — which is
  `$HOME` whenever `HOME` is set (`src/config.rs:694`):
  `$HOME/.marmel/marmel.toml`, `$HOME/.marmel/config.toml`,
  `$HOME/.config/marmel/config.toml`, `$HOME/.config/marmel/marmel.toml`.

The old test only scrubbed the three `MARMEL_*` session env keys; it inherited
the **real** `$HOME`. In this sandbox `/home/fredrik` is `drwxr-x---` and
directory reads are blocked (`ls /home/fredrik` → `Permission denied`), while
`stat()` on a known path still succeeds. So `candidate.exists()` returned `true`
for the operator's own `/home/fredrik/.marmel/marmel.toml`, `std::fs::read_to_string`
then failed with `EACCES`, and `load` propagated it with the
`reading <path>` context (`src/config.rs:362-365`):

```
panicked at tests/test_session_config.rs:537:26:
load(None) must not fail on a well-formed checkout: reading /home/fredrik/.marmel/marmel.toml
Caused by:
    Permission denied (os error 13)
```

i.e. the test was asserting a property of the *machine*, not of the loader. The
same failure was already flagged as environmental in `docs/verification_t014f.md` §5.

---

## 3. The hermetic fix

The loader already exposes a path-injection seam — `HOME` is honoured verbatim
by `home_dir()` — and the cwd half is process state that a test may relocate.
`tests/test_session_config.rs` therefore gained:

* **`PathIsolation`** (`:205-241`) — owns two `tempfile::TempDir`s plus the saved
  `HOME`/cwd, and in `Drop` restores the **cwd first** (the temp dirs are fields,
  so they outlive the `Drop` body and the process must not be parked inside a
  directory that is about to be deleted) and then `HOME`
  (`set_var` if it was present, `remove_var` if it was absent — never an empty value).
* **`SessionEnvGuard::isolated_paths(&self)`** (`:246-268`) — installs the
  isolation. It takes `&self`, i.e. the caller must already own a
  `SessionEnvGuard`. That is deliberate: `SESSION_ENV_TEST_SERIAL` (`:120`) is a
  non-reentrant `Mutex`, so a second `lock()` inside the isolation would
  self-deadlock; borrowing the env guard is the compile-time proof that the
  process-wide serialization lock is already held across the `HOME` write, the
  `chdir`, every `load(None)` in the test, and the restore.
* **`write_candidate(root, relative, marker)`** (`:300-322`) — writes one of the
  searched candidate paths with a marker value in
  `session_worker_mirror_max_messages` (plus a derived `session_max_bundle_bytes`),
  so a single `load(None)` reveals *which* candidate won.
* **`SessionEnvGuard::resume_handoff(&self, resume, fresh)` + `ResumeRequestGuard`**
  (`:270-296`) — same lock discipline for the process-global
  `ui::session::RESUME_REQUESTED` / `FRESH_REQUESTED` statics, restored to the
  `(false, false)` boot state in `Drop`.

`load_none_uses_the_searched_paths_and_never_panics` was rewritten around that
seam (`:716-759`): both path authorities now point at **empty** temp roots, so
"no candidate exists anywhere" is a state the test creates. No assertion was
weakened — the original `assert_ne!(…, 0)` guards are still there and are now
joined by exact equality against the documented defaults, and the
`load(None)`-must-not-fail property is asserted against the fixture, not the host.

Search-path order is now asserted explicitly rather than waved at as
"environment-dependent by design":

* `load_none_walks_the_home_candidates_in_documented_order` (`:762-819`);
* `load_none_prefers_cwd_candidates_over_home_candidates` (`:822-856`) — a 4-step
  ladder proving the cwd block precedes the HOME block *and* the internal cwd order;
* `load_none_reports_an_unreadable_candidate_as_an_error_not_a_panic` (`:859-892`, `#[cfg(unix)]`)
  — mode `0000` fixture: `exists()` true, `open()` → `EACCES`, exactly the sandbox
  shape, asserted to surface as `Err("reading …")`;
* `load_none_reports_an_unreadable_directory_candidate_as_an_error` (`:895-922`)
  — portable sibling (a *directory* named `marmel.toml`, `EISDIR`).

**Authoring correction (no existing assertion was touched):** my first draft of
the home-order test asserted that `.config/marmel/marmel.toml` outranks
`.config/marmel/config.toml`. It does not: `src/config.rs:481-482` searches
`config.toml` (3rd) **before** `marmel.toml` (4th) — the mirror image of the
`.marmel` pair. The new test was corrected to the source order; the failure it
caught was in the test, not in the loader.

---

## 4. Final per-test list (35 tests, all `ok`, 0 ignored)

| Test (line) | Pins | Status |
|---|---|---|
| `defaults_and_public_consts_match_documented_values` (`:354`) | six defaults `src/config.rs:239-244`, consts `:30,34,37`, env names `:42,47,50` | kept |
| `env_flag_truthy_table_returns_some_true` (`:399`) | `1/true/yes/on` (+case/whitespace) | kept |
| `env_flag_falsy_table_returns_some_false` (`:411`) | `0/false/no/off` | kept |
| `env_flag_garbage_or_unset_returns_none` (`:423`) | junk/unset ⇒ "leave alone" | kept |
| `apply_session_env_resume_truthy_turns_resume_on` (`:438`) | `:428-437` | kept |
| `apply_session_env_resume_falsy_turns_resume_off` (`:456`) | `:428-437` | kept |
| `apply_session_env_disable_truthy_turns_capture_off` (`:470`) | `:439` | kept |
| `apply_session_env_disable_is_one_way_and_never_reenables` (`:484`) | one-way at `:439` | kept |
| `apply_session_env_min_ms_non_integer_keeps_previous_and_never_zero` (`:519`) | `:448-453` warn-and-keep | kept |
| `apply_session_env_min_ms_zero_is_applied_verbatim_without_clamping` (`:542`) | no lower clamp | kept |
| `apply_session_env_min_ms_valid_large_value_is_applied_verbatim` (`:561`) | `u64` verbatim | kept |
| `apply_session_env_with_unset_env_is_pure_and_idempotent` (`:579`) | purity over all six keys | kept |
| `load_full_toml_honours_all_six_session_keys` (`:607`) | six keys through `PartialConfig`/`merge` | kept |
| `load_partial_toml_keeps_defaults_for_unspecified_keys` (`:636`) | `merge` `None` arms `:605-621` | kept |
| `load_typo_or_unknown_key_is_silently_ignored` (`:659`) | no `deny_unknown_fields` (hazard documented) | kept |
| `load_missing_path_is_an_error_and_never_panics` (`:684`) | explicit path is verbatim | kept |
| `load_none_uses_the_searched_paths_and_never_panics` (`:716`) | empty search ⇒ no error, exact defaults | **fixed (hermetic)** |
| `load_none_walks_the_home_candidates_in_documented_order` (`:762`) | HOME block order `:477-489` | **new** |
| `load_none_prefers_cwd_candidates_over_home_candidates` (`:822`) | cwd block `:464-476` beats HOME | **new** |
| `load_none_reports_an_unreadable_candidate_as_an_error_not_a_panic` (`:861`, unix) | unreadable ⇒ reported, never defaulted | **new** |
| `load_none_reports_an_unreadable_directory_candidate_as_an_error` (`:895`) | non-file candidate ⇒ reported | **new** |
| `load_toml_type_mismatch_is_an_error_not_a_silent_default` (`:925`) | typed `PartialConfig` fields | kept |
| `load_session_disable_env_overrides_file_value_one_way` (`:948`) | env `:391` → file, one-way | kept |
| `load_resume_env_truthy_overrides_a_false_file_value` (`:974`) | env read `:390` end-to-end | **new** |
| `load_resume_env_falsy_overrides_a_true_file_value` (`:995`) | `:390` + `:428-437` | **new** |
| `load_commit_min_ms_env_overrides_the_file_floor` (`:1008`) | `:392` + `:446-453` | **new** |
| `load_junk_session_env_values_keep_the_file_values` (`:1021`) | all three junk ⇒ file survives | **new** |
| `load_disable_and_resume_env_apply_together_and_disable_stays_one_way` (`:1056`) | three env reads in one pass | **new** |
| `exit_restore_refused_const_is_three` (`:1088`) | `session/mod.rs:50 == 3` | **new** |
| `resume_handoff_fresh_outranks_the_resume_flag_and_the_config_spelling` (`:1100`) | `--fresh` > `--resume` > config/env | **new** |
| `resume_handoff_fresh_alone_outranks_the_config_spelling` (`:1118`) | escape hatch | **new** |
| `resume_handoff_resume_flag_restores_without_a_config_spelling` (`:1133`) | `--resume` alone | **new** |
| `resume_handoff_with_no_flag_falls_back_to_the_config_spelling` (`:1149`) | config spelling / fresh start | **new** |
| `resume_env_beats_the_file_and_the_handoff_agrees` (`:1168`) | env > file, then seam | **new** |
| `resume_handoff_fresh_outranks_the_resume_env_end_to_end` (`:1188`) | full chain `--fresh` > env | **new** |

---

## 5. Edits (file:line, post-`rustfmt` numbering, single file)

`tests/test_session_config.rs`:

1. `:22-33` — module-doc "What is covered here": `load()` bullet updated; new
   bullets for the hermetic `load(None)` search-path family and for the flow §3.3
   resume precedence / `EXIT_RESTORE_REFUSED`.
2. `:35-52` — `# What is deliberately NOT covered here (documented gap)` rewritten
   to name the unreachable binary-target slices (`src/main.rs:256`, `:302`, `:105`)
   and the `PartialConfig`/`merge` visibility boundary with correct line refs
   (`:516-521`, `:605-621`).
3. `:54-73` — env-serialization paragraph: added that the *same* mutex also
   serializes the resume-handoff statics.
4. `:75-93` — new `# Path isolation for the `load(None)` family` section, including
   the recorded sandbox root cause.
5. `:95-117` — imports: `std::path::{Path, PathBuf}`, `#[cfg(unix)] std::os::unix::fs::PermissionsExt`,
   `marmennill::session::EXIT_RESTORE_REFUSED`,
   `marmennill::ui::session::{resume_requested, set_resume_request}`.
6. `:205-241` — `PathIsolation` struct + accessors + `Drop` (cwd-then-HOME restore).
7. `:244-282` — second `impl SessionEnvGuard`: `isolated_paths()` (`:246-268`) and
   `resume_handoff()` (`:270-281`).
8. `:285-296` — `ResumeRequestGuard<'env>` + its `Drop` (restores the handoff to
   `(false, false)`).
9. `:300-322` — `write_candidate()` fixture writer.
10. `:716-759` — `load_none_uses_the_searched_paths_and_never_panics` rewritten hermetically
    (this is the fix for the single red test).
11. `:762-922` — four new `load(None)` search-path tests.
12. `:974-1198` — fifteen new coverage tests (env end-to-end, exit-code const, resume precedence).
13. Whole file — `rustfmt --edition 2024` applied (scoped to this file only; it also
    normalized pre-existing formatting inside this same file).

---

## 6. Coverage map for the required slices

| Required slice | Where |
|---|---|
| six keys `config.rs:198,201,203,205,208,211` | `:607` (full file), `:636` (partial), `:579` (pure seam), `:716` (empty search), `:762`/`:822` (candidate order) |
| defaults `:239-244` | `:354`, `:716` |
| consts `:30,34,37` | `:354`, `:716` |
| `PartialConfig` `:516-521` / `merge` `:605-621` | `:607` (all six `Some` arms), `:636` (four `None` arms keep defaults), `:925` (typed fields reject wrong TOML type), `:659` (unknown keys dropped) |
| env spellings `1/true/yes/on`, `0/false/no/off` | `:399`, `:411`, plus end-to-end `yes`/`off` at `:974`/`:995` |
| `SESSION_RESUME_ENV:42`, `SESSION_DISABLE_ENV:47` one-way `:439` | `:354` (names), `:470`, `:484`, `:948`, `:1056` |
| `SESSION_COMMIT_MIN_MS_ENV:50`, read `:390-392`, applied `:428-453` | `:519`, `:542`, `:561`, `:1008`, `:1021`, `:1056` |
| precedence `--fresh` > `--resume` > `MARMEL_RESUME`/`session_resume` | `:1100`, `:1118`, `:1133`, `:1149`, `:1168`, `:1188` |
| `session::EXIT_RESTORE_REFUSED == 3` | `:1088` |

---

## 7. Documented gaps and observability boundary (nothing hidden)

1. **Binary-only slices — unreachable from `tests/`, left uncovered by design.**
   `--resume` / `--fresh` flag parsing (`src/main.rs:256`), the contradictory-flag
   usage error (exit 2, `src/main.rs:302`) and the literal
   `std::process::exit(marmennill::session::EXIT_RESTORE_REFUSED)`
   (`src/main.rs:105`) live in the `marmel` **binary** target. An integration test
   links the library only, and spawning the binary is out of scope for this task,
   so no CLI exit code is observable from here. Covered instead through the public
   seam those flags feed (`ui::session::set_resume_request` /
   `ui::session::resume_requested`, `src/ui/session.rs:244`, `:264`) plus the
   public const `session::EXIT_RESTORE_REFUSED`. The parser itself remains covered
   by the `#[cfg(test)]` unit tests in `src/main.rs:395-600`.
2. **`PartialConfig` / `merge` are module-private**, so their behaviour is only
   observable through `load()`. Because `load` always starts from
   `Config::default()` (`src/config.rs:361`), `merge`'s `None` arms can only be
   asserted as "absent key keeps the *default*", never "absent key keeps a
   pre-set non-default base". Not a weakening — a visibility boundary; no `src/`
   visibility was changed.
3. **"Unreadable `$HOME`" is emulated, not inherited.** The precise host
   condition (`stat` on `$HOME/…` succeeds, `open()` returns `EACCES` on the
   operator's real home) cannot be recreated inside the test without touching the
   real `$HOME`, which would make the test non-hermetic again. The observable
   property is reproduced exactly with a fixture the test owns: a `0000`-mode
   candidate (`:861`) gives the same `exists() == true` + `EACCES` shape, and a
   directory candidate (`:895`) gives the portable `EISDIR` shape. Both assert the
   loader's contract: the read failure is reported with its `reading <path>`
   context and never turns into a panic or a silent default. What is *not*
   asserted anywhere is a hypothetical "skip the unreadable candidate and keep
   walking" behaviour — the loader does not do that (`src/config.rs:362-365`),
   and this file now pins that it propagates.
4. **`#[cfg(unix)]` on one test.** `load_none_reports_an_unreadable_candidate_as_an_error_not_a_panic`
   uses `std::os::unix::fs::PermissionsExt::from_mode`, so it is unix-gated
   (`:859`). Its portable twin (`:895`) covers the same contract on any platform.
   No `#[allow]`, no `#[ignore]` was added, so the counted totals are identical on
   Linux either way.

## 8. Hard-rule compliance

No assertion was weakened, removed or watered down (the fixed test kept both
original `assert_ne!(…, 0)` guards and gained exact-default assertions); no
`#[allow(...)]`; no `#[ignore]`; no `src/` visibility change; the binary was never
spawned; only `tests/test_session_config.rs` and this note were written; no git
commit; `rustfmt` was scoped to the single file.
