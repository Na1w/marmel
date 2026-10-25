//! Regression suite for the Windows path-spelling / containment fix
//! (`docs/windows_ci_root_cause.md` §1–§3, §5).
//!
//! Every test here pins a **spelling** bug class, i.e. the family of Windows-only
//! failures where the two operands of a containment (or lock-shard) decision were
//! produced by *different* normalization pipelines:
//!
//! * `std::fs::canonicalize` → `\\?\`-verbatim, long name, on-disk casing;
//! * `std::env::temp_dir()` → raw `%TEMP%` string, DOS spelling, routinely 8.3-shortened
//!   (`C:\Users\RUNNER~1\AppData\Local\Temp`).
//!
//! The whole point of [`PathFamily`] is that the Windows branch is exercised **on a
//! Linux runner** — CI's `cross-check` job only *compiles* the Windows targets, so a
//! `#[cfg(windows)]`-only guard would be a guard that never runs. Section A/B are
//! therefore family-pinned and platform-independent; section D holds the few tests
//! that genuinely need a Windows file system.
//!
//! **Pinned boundary (do not "fix" it):** the string fold in
//! [`normalize_for_compare_in_family`] never expands an 8.3 alias — at string level
//! `runner~1 != RUNNER~1 != runneradmin`. The 8.3 case is carried by
//! [`canonical_existing_prefix`] (`GetFinalPathNameByHandleW`) being applied to
//! **both** operands, so these tests assert the *pair* behaviour, never a fold that
//! could merge two different files.

use super::*;
use std::fs;

// ──────────────────────────────────────────────────────────────────────────
// Section A — pure helpers over an explicit family (run on every platform)
// ──────────────────────────────────────────────────────────────────────────

/// The exact `%TEMP%` root spelling of the failing CI job, long-name form
/// (`docs/windows_ci_root_cause.md`:38, §1).
const WINDOWS_TEMP_ROOT: &str = r"C:\Users\runneradmin\AppData\Local\Temp";
/// The exact path the deny message named (`docs/windows_ci_root_cause.md`:14), with
/// the `%TEMP%` root spelled out in full instead of interpolated.
const CI_TARGET: &str = r"C:\Users\runneradmin\AppData\Local\Temp\.tmp2Tiuen\shared_target.txt";
/// The same root as the 8.3-shortened string `GetTempPath2W` actually returns.
const WINDOWS_TEMP_ROOT_8_3: &str = r"C:\Users\RUNNER~1\AppData\Local\Temp";
/// The same target in the 8.3-shortened spelling from the CI message.
const CI_TARGET_8_3: &str = r"C:\Users\RUNNER~1\AppData\Local\Temp\.tmp2Tiuen\shared_target.txt";
/// The same root in the `\\?\`-verbatim spelling `canonicalize` produces
/// (`Prefix::VerbatimDisk` vs `Prefix::Disk`, the bug class itself).
const VERBATIM_TEMP_ROOT: &str = r"\\?\C:\Users\runneradmin\AppData\Local\Temp";
/// The same target in the `\\?\`-verbatim, long-name spelling.
const VERBATIM_CI_TARGET: &str =
    r"\\?\C:\Users\runneradmin\AppData\Local\Temp\.tmp2Tiuen\shared_target.txt";
/// The target with the drive letter and every ASCII case flipped, and `/` separators.
const MIXED_CASE_SLASH_TARGET: &str =
    "c:/users/RUNNERADMIN/AppData/Local/Temp/.tmp2Tiuen/SHARED_TARGET.TXT";

/// Scratch directory for a throwaway scoped workspace root (callers use
/// `<scratch>/ws` as the root and put escape targets in `<scratch>` itself), placed
/// so it is provably **outside** the `std::env::temp_dir()` allow-list entry —
/// otherwise a denial test could be made green by the second allow-list entry.
/// Lives under `target/` (scratch, gitignored).
fn scoped_root_outside_temp() -> PathBuf {
    let unique = format!(
        "marmel-path-tests-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos()
    );
    let root = std::env::current_dir()
        .expect("cwd for a scoped workspace root")
        .join("target")
        .join(unique);
    fs::create_dir_all(&root).expect("create scoped workspace root scratch dir");
    assert!(
        !root.starts_with(std::env::temp_dir()),
        "test premise broken: scoped root {} sits inside the temp allow-list entry {}",
        root.display(),
        std::env::temp_dir().display()
    );
    root
}

/// [`resolve_safe_path`] against an explicitly scoped workspace root, so no test
/// depends on (or mutates) the repository root it happens to run from.
fn resolve_at(root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let root = root.to_path_buf();
    let path = path.to_string();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime for a scoped workspace root");
    rt.block_on(crate::harness::with_workspace_root(root, async move {
        resolve_safe_path(&path, TOOL_READ_FILE)
    }))
}

/// Owned `str` spelling of a path, for feeding the string-taking entry points.
fn spell(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Assert the *only* acceptable denial shape: [`ToolError::Forbidden`] carrying the
/// single documented wording. `Ok(..)` is the Windows CI failure itself.
fn assert_escape_denied(outcome: Result<PathBuf, ToolError>, spelling: &str) {
    match outcome {
        Ok(path) => panic!(
            "'{spelling}' must be denied, but resolve_safe_path allowed {}",
            path.display()
        ),
        Err(ToolError::Forbidden { ref caller, .. }) => assert!(
            caller.contains("escapes workspace root"),
            "deny wording must stay 'escapes workspace root' for '{spelling}', got: {caller}"
        ),
        Err(err) => panic!(
            "expected ToolError::Forbidden for '{spelling}', got {}: {err}",
            std::any::type_name_of_val(&err)
        ),
    }
}

/// The exact CI scenario: the temp root and the file the CI job was denied for.
/// Pins bug class: **spelling-mismatch containment** — a file that really *is*
/// inside the temp root looked like an escape on `windows-latest` only.
#[test]
fn ci_literal_temp_root_contains_ci_literal_target_on_windows_family() {
    assert!(
        path_within_in_family(
            Path::new(WINDOWS_TEMP_ROOT),
            Path::new(CI_TARGET),
            PathFamily::Windows
        ),
        "the CI literals must be contained once both sides go through the fold"
    );
    // Both sides in the *same* short-name spelling also matches — the fold is
    // spelling-invariant, it is the alias expansion that is carried by canonicalizing.
    assert!(
        path_within_in_family(
            Path::new(WINDOWS_TEMP_ROOT_8_3),
            Path::new(CI_TARGET_8_3),
            PathFamily::Windows
        ),
        "a matched 8.3 pair must stay allowed"
    );
}

/// Pins bug class: **`Prefix::VerbatimDisk != Prefix::Disk`** — the canonicalized
/// (`\\?\`) side and the raw `%TEMP%` (DOS) side could never component-match, in
/// either direction of the allow-list operand.
#[test]
fn verbatim_vs_dos_verbatim_prefix_spelling_stays_inside_temp_root() {
    // verbatim anchor, DOS candidate
    assert!(
        path_within_in_family(
            Path::new(VERBATIM_TEMP_ROOT),
            Path::new(CI_TARGET),
            PathFamily::Windows
        ),
        "a `\\\\?\\`-verbatim root must still contain the DOS spelling"
    );
    // DOS anchor, verbatim candidate
    assert!(
        path_within_in_family(
            Path::new(WINDOWS_TEMP_ROOT),
            Path::new(VERBATIM_CI_TARGET),
            PathFamily::Windows
        ),
        "a DOS root must still contain the `\\\\?\\`-verbatim spelling"
    );
    // both verbatim
    assert!(
        path_within_in_family(
            Path::new(VERBATIM_TEMP_ROOT),
            Path::new(VERBATIM_CI_TARGET),
            PathFamily::Windows
        ),
        "two verbatim spellings must match"
    );
    // The prefix is trimmed, never left as a stray component that could shadow a
    // real component list.
    assert_eq!(
        containment_key_in_family(Path::new(VERBATIM_CI_TARGET), PathFamily::Windows),
        containment_key_in_family(Path::new(CI_TARGET), PathFamily::Windows),
        "verbatim and DOS spellings of one file must collapse onto one key"
    );
}

/// Pins bug class: **drive-letter and ASCII case + `\` vs `/`**, which Windows file
/// systems treat as equal and `Path::starts_with` treats as different — while the
/// Unix guard must keep folding *nothing*.
#[test]
fn drive_letter_case_and_mixed_separators_are_equal_on_windows_but_not_on_unix() {
    for root in [
        r"c:\users\runneradmin\appdata\local\temp",
        r"C:\USERS\RunnerAdmin\AppData\Local\Temp",
        "C:/Users/runneradmin/AppData/Local/Temp",
        "c:/users/runneradmin/appdata/local/temp",
    ] {
        assert!(
            path_within_in_family(Path::new(&root), Path::new(CI_TARGET), PathFamily::Windows),
            "root '{root}' must contain the CI target on Windows"
        );
        assert!(
            path_within_in_family(
                Path::new(WINDOWS_TEMP_ROOT),
                Path::new(&root.replace("temp", "temp/extra/shared_target.txt")),
                PathFamily::Windows
            ),
            "a `/`-spelled candidate must match a `\\`-spelled root on Windows"
        );
    }

    // The same operand pairs on Unix: case is significant and `\` is an ordinary
    // character, so nothing may be folded.
    assert!(path_within_in_family(
        Path::new("/tmp/Temp"),
        Path::new("/tmp/Temp/x"),
        PathFamily::Unix
    ));
    assert!(
        !path_within_in_family(
            Path::new("/tmp/Temp"),
            Path::new("/tmp/temp/x"),
            PathFamily::Unix
        ),
        "Unix containment must stay case-sensitive (the fold is Windows-only)"
    );
    assert!(
        !path_within_in_family(
            Path::new("/tmp/a"),
            Path::new(r"/tmp/a\b\c"),
            PathFamily::Unix
        ),
        "a backslash is not a separator on Unix and must not be folded into one"
    );
    assert_eq!(
        normalize_for_compare_in_family(r"/tmp/a\b.txt", PathFamily::Unix),
        r"/tmp/a\b.txt",
        "the Unix fold must be byte-identical"
    );
    assert_eq!(
        normalize_for_compare_in_family(r"C:\tmp\a\b.txt", PathFamily::Windows),
        r"C:/tmp/a/b.txt",
        "the Windows fold unifies separators only"
    );
}

/// Pins bug class: **trailing separator** on the allow-list entry (the `%TEMP%`
/// string conventionally ends in `\`), which used to add an empty component.
#[test]
fn trailing_separator_on_the_root_never_changes_containment() {
    for root in [
        format!(r"{WINDOWS_TEMP_ROOT}\\"),
        format!(r"{WINDOWS_TEMP_ROOT}/"),
        format!(r"{VERBATIM_TEMP_ROOT}\\"),
    ] {
        assert!(
            path_within_in_family(Path::new(&root), Path::new(CI_TARGET), PathFamily::Windows),
            "trailing separator on root '{root}' must not change containment"
        );
    }
    assert_eq!(
        key_components(&containment_key_in_family(
            Path::new(r"C:\Users\runneradmin\AppData\Local\Temp\"),
            PathFamily::Windows
        )),
        vec!["C:", "users", "runneradmin", "appdata", "local", "temp"],
        "a trailing separator must not become a component"
    );
    // Unix side of the same rule.
    assert!(path_within_in_family(
        Path::new("/tmp/marmel/"),
        Path::new("/tmp/marmel/x"),
        PathFamily::Unix
    ));
}

/// Pins bug class: the guard must not become an **over-allowing wildcard** while it
/// is being made spelling-invariant.
#[test]
fn documents_path_under_the_temp_root_is_never_contained() {
    assert!(
        !path_within_in_family(
            Path::new(WINDOWS_TEMP_ROOT),
            Path::new(r"C:\Users\runneradmin\Documents\x.txt"),
            PathFamily::Windows
        ),
        "a sibling profile directory is not inside Temp"
    );
    assert!(
        !path_within_in_family(
            Path::new(VERBATIM_TEMP_ROOT),
            Path::new(r"\\?\C:\Users\runneradmin\Documents\x.txt"),
            PathFamily::Windows
        ),
        "same denial in verbatim spelling"
    );
}

/// Pins bug class: **sibling-prefix bypass** — a byte-prefix check accepts
/// `C:\Temp\foobar\x` under `C:\Temp\foo`; component-wise comparison must not.
#[test]
fn sibling_dir_prefix_never_passes_containment() {
    for (root, target) in [
        (r"C:\Temp\foo", r"C:\Temp\foobar\x"),
        (r"C:\Temp\foo\", r"C:\Temp\foobar\x"),
        (r"\\?\C:\Temp\foo", r"C:\Temp\foobar\x"),
        (r"C:\Temp\foo", r"\\?\C:\Temp\foobar\x"),
    ] {
        assert!(
            !path_within_in_family(Path::new(root), Path::new(target), PathFamily::Windows),
            "sibling prefix '{target}' must never be contained in '{root}'"
        );
    }
    // Unix half of the same rule.
    assert!(
        !path_within_in_family(
            Path::new("/tmp/foo"),
            Path::new("/tmp/foobar/x"),
            PathFamily::Unix
        ),
        "sibling-prefix bypass must stay dead on Unix too"
    );
}

/// Pins bug class: **device / namespace escapes** (`\\.\`, `\\?\UNC\`,
/// `\\?\GLOBALROOT`) reaching a volume allow-list entry, plus the `??\` form that
/// the prefix table deliberately does not list.
#[test]
fn device_namespace_and_unc_spellings_are_never_inside_a_volume_root() {
    for escape in [
        r"\\?\UNC\server\share\x",
        r"\\.\PhysicalDrive0",
        r"\\?\GLOBALROOT\Device\x",
        r"\\?\Device\HarddiskVolume1\Windows\x",
        r"\\server\share\x",
        r"??\C:\Windows\x",
        r"\\.\Global\marmel",
    ] {
        for root in [WINDOWS_TEMP_ROOT, VERBATIM_TEMP_ROOT, r"C:\Temp\foo"] {
            assert!(
                !path_within_in_family(Path::new(root), Path::new(escape), PathFamily::Windows),
                "escape '{escape}' must never be contained in '{root}'"
            );
        }
    }
    // The device spellings survive as relative-looking keys, which is exactly why
    // they can never component-match a rooted key.
    assert_eq!(
        containment_key_in_family(Path::new(r"\\.\PhysicalDrive0"), PathFamily::Windows),
        "physicaldrive0"
    );
    assert_eq!(
        containment_key_in_family(Path::new(r"\\?\GLOBALROOT\Device\x"), PathFamily::Windows),
        "device/x"
    );
}

/// Pins bug class: a **literal `..` in an operand** — `C:\Temp\sub\..\..\..\Windows\x`
/// *does* share its leading components with `C:\Temp`, so a component-wise prefix
/// match would open the door. Failing closed is the contract.
#[test]
fn literal_parent_components_fail_closed_instead_of_matching_leading_components() {
    for target in [
        r"C:\Temp\sub\..\..\..\Windows\x",
        r"C:\Temp\..\..\Windows\x",
        r"\\?\C:\Temp\sub\..\..\Windows\x",
    ] {
        assert!(
            !path_within_in_family(
                Path::new(r"C:\Temp"),
                Path::new(target),
                PathFamily::Windows
            ),
            "a raw '..' operand ('{target}') must be refused, not folded"
        );
    }
    assert!(
        !path_within_in_family(
            Path::new(r"C:\Temp\.."),
            Path::new(r"C:\Temp\x"),
            PathFamily::Windows
        ),
        "a '..' in the *anchor* is equally refused"
    );
    assert!(
        !path_within_in_family(
            Path::new("/tmp/a"),
            Path::new("/tmp/a/../../etc/x"),
            PathFamily::Unix
        ),
        "the same fail-closed rule applies on Unix"
    );
}

/// Pins the **documented boundary of the fold**: it never expands an 8.3 alias, so
/// it must not be credited with doing so — and the alias case is carried by
/// canonicalizing *both* operands, which the second half of this test pins.
#[test]
fn eight_dot_three_alias_is_not_merged_by_the_string_fold_by_design() {
    // 8.3 root vs long-name candidate: the fold alone cannot bridge them.
    assert!(
        !path_within_in_family(
            Path::new(WINDOWS_TEMP_ROOT_8_3),
            Path::new(CI_TARGET),
            PathFamily::Windows
        ),
        "the fold must NOT pretend to expand 8.3 aliases (RUNNER~1 != runneradmin)"
    );
    assert!(
        !path_within_in_family(
            Path::new(CI_TARGET_8_3),
            Path::new(WINDOWS_TEMP_ROOT),
            PathFamily::Windows
        ),
        "nor in the opposite direction"
    );
    assert_ne!(
        containment_key_in_family(Path::new(WINDOWS_TEMP_ROOT_8_3), PathFamily::Windows),
        containment_key_in_family(Path::new(WINDOWS_TEMP_ROOT), PathFamily::Windows),
        "the fold may not merge two files that only differ by an alias"
    );
    // What actually carries the CI case: both sides canonicalized → both verbatim,
    // long-name → contained again.
    assert!(
        path_within_in_family(
            Path::new(VERBATIM_TEMP_ROOT),
            Path::new(VERBATIM_CI_TARGET),
            PathFamily::Windows
        ),
        "once both operands come from canonical_existing_prefix the CI pair is allowed"
    );
}

/// Pins bug class: **containment-key shape** — the Unix key may not change a single
/// byte (otherwise the Unix guard silently weakens), while the Windows key must drop
/// `\\?\`, unify separators and upper-case the drive letter.
#[test]
fn containment_key_is_unix_identity_and_windows_normalized_spelling() {
    for raw in [
        "/tmp/MixEd/A.MD",
        "src/a.md",
        r"\\?\C:\x\a.md",
        "/tmp/./weird/../bytes",
        "",
    ] {
        assert_eq!(
            containment_key_in_family(Path::new(raw), PathFamily::Unix),
            raw,
            "the Unix key must be byte-for-byte identity"
        );
    }
    assert_eq!(
        containment_key_in_family(Path::new(VERBATIM_CI_TARGET), PathFamily::Windows),
        "C:/users/runneradmin/appdata/local/temp/.tmp2tiuen/shared_target.txt",
        "Windows key: no `\\\\?\\`, `/` separators, upper-case drive letter, folded tail"
    );
    assert_eq!(
        containment_key_in_family(Path::new(r"\\?\C:\x\A.MD"), PathFamily::Windows),
        "C:/x/a.md"
    );
    assert_eq!(
        containment_key_in_family(Path::new(r"c:\x\A.MD"), PathFamily::Windows),
        "C:/x/a.md",
        "a lower-case drive spelling must collapse onto the same key"
    );
    // No drive letter → the whole key is folded, and the verbatim prefix is gone.
    assert_eq!(
        containment_key_in_family(Path::new(r"\\localhost\C$\tmp\a.md"), PathFamily::Windows),
        "c$/tmp/a.md"
    );
    // The platform-pinned entry point is exactly the family-pinned one.
    for raw in ["/tmp/x", r"C:\x\y"] {
        assert_eq!(
            containment_key(Path::new(raw)),
            containment_key_in_family(Path::new(raw), PathFamily::current())
        );
    }
}

/// Pins the historical expectation `src/harness/search_tests.rs` relies on
/// (`normalize_for_compare("\\\\?\\C:\\x\\a.md") == "C:/x/a.md"` on Windows,
/// identity on Unix), reached through the family-pinned implementation instead of a
/// second copy of the rules.
#[test]
fn normalize_for_compare_in_family_matches_the_search_tests_historical_expectation() {
    assert_eq!(
        normalize_for_compare_in_family(r"\\?\C:\x\a.md", PathFamily::Windows),
        "C:/x/a.md"
    );
    assert_eq!(
        normalize_for_compare_in_family(r"\\?\C:\x\a.md", PathFamily::Unix),
        r"\\?\C:\x\a.md"
    );
    // The test alias must agree with the family-pinned helper on this platform,
    // which is what keeps search_tests and the containment guard on one fold.
    for raw in [
        r"\\?\C:\x\a.md",
        r"\\.\C:\tmp\x",
        r"\\localhost\C$\tmp\a.md",
        r"C:\tmp\src\a.md",
        "/tmp/marmel_search_1/adir/first.txt",
        "adir/first.txt",
        "{literal}.rs",
    ] {
        assert_eq!(
            normalize_for_compare(raw),
            normalize_for_compare_in_family(raw, PathFamily::current()),
            "normalize_for_compare drifted from the family-pinned fold: {raw}"
        );
    }
}

/// Pins bug class: a verbatim/namespace prefix trimmed as **two** units, leaving a
/// stray `UNC\` / `GLOBALROOT\` component (the longest-first table order).
#[test]
fn strip_windows_path_prefix_trims_namespace_prefixes_as_one_unit() {
    assert_eq!(
        strip_windows_path_prefix(r"\\?\UNC\server\share"),
        r"server\share"
    );
    assert_eq!(
        strip_windows_path_prefix(r"\\?\GLOBALROOT\Device\x"),
        r"Device\x"
    );
    assert_eq!(strip_windows_path_prefix(r"\\?\C:\x\a.md"), r"C:\x\a.md");
    assert_eq!(
        strip_windows_path_prefix(r"\\.\PhysicalDrive0"),
        "PhysicalDrive0"
    );
    assert_eq!(strip_windows_path_prefix(r"\\localhost\C$\tmp"), r"C$\tmp");
    assert_eq!(
        strip_windows_path_prefix(r"//?/unc/server/share"),
        r"server/share"
    );
    assert_eq!(
        strip_windows_path_prefix(r"//./PhysicalDrive0"),
        "PhysicalDrive0"
    );
    // Non-matching input passes through untouched, including a plain UNC path.
    assert_eq!(
        strip_windows_path_prefix(r"\\server\share\x"),
        r"\\server\share\x"
    );
    assert_eq!(strip_windows_path_prefix(r"C:\x"), r"C:\x");
    assert_eq!(strip_windows_path_prefix(""), "");
    assert_eq!(strip_windows_path_prefix(r"\\?\"), "");
}

/// Pins the invariants [`strip_windows_path_prefix`] relies on: ASCII-only entries
/// (so a byte-prefix match always ends on a `char` boundary), no empty entry
/// (which would make the strip a silent no-op), and longest-first ordering.
#[test]
fn windows_path_prefix_table_is_longest_first_and_ascii_char_safe() {
    assert!(!WINDOWS_PATH_PREFIXES.is_empty());
    for prefix in WINDOWS_PATH_PREFIXES {
        assert!(!prefix.is_empty(), "an empty prefix entry would be a no-op");
        assert!(
            prefix.is_ascii(),
            "non-ASCII prefix entry '{prefix}' could split a char boundary"
        );
    }
    for (i, shorter) in WINDOWS_PATH_PREFIXES.iter().enumerate() {
        for (j, longer) in WINDOWS_PATH_PREFIXES.iter().enumerate() {
            if i >= j {
                continue;
            }
            assert!(
                !(longer.len() > shorter.len()
                    && longer.as_bytes()[..shorter.len()].eq_ignore_ascii_case(shorter.as_bytes())),
                "prefix '{shorter}' precedes the longer '{longer}': strip would leave a stray component"
            );
        }
    }
}

/// Pins bug class: **swapped containment operands** — the parameter order is
/// (allow-list anchor, candidate); reading it the other way round turns an allow
/// into a deny, which is exactly how the CI deny looked.
#[test]
fn path_within_argument_order_is_anchor_then_candidate() {
    assert!(path_within_in_family(
        Path::new(WINDOWS_TEMP_ROOT),
        Path::new(CI_TARGET),
        PathFamily::Windows
    ));
    assert!(
        !path_within_in_family(
            Path::new(CI_TARGET),
            Path::new(WINDOWS_TEMP_ROOT),
            PathFamily::Windows
        ),
        "swapping (root, target) must flip the answer, not keep allowing"
    );
    assert!(
        !path_within_in_family(Path::new("/tmp/a/b"), Path::new("/tmp/a"), PathFamily::Unix),
        "same order rule on Unix"
    );
}

/// Pins bug class: an **anchor with no components** becoming a wildcard allow
/// (`""`, `"."`, `"./"` or an unrooted relative name proves nothing).
#[test]
fn relative_or_empty_anchor_never_allows_anything() {
    for anchor in ["", ".", "./", "no_anchor_dir"] {
        for target in [CI_TARGET, "relative/x.txt", "/tmp/x"] {
            for family in [PathFamily::Windows, PathFamily::Unix] {
                assert!(
                    !path_within_in_family(Path::new(anchor), Path::new(target), family),
                    "anchor '{anchor}' must never allow '{target}' under {family:?}"
                );
            }
        }
    }
}

/// Pins the **drive-root anchor** semantics the fold gives a volume-level
/// allow-list entry: it contains everything on that volume and nothing off it. The
/// bare drive-relative spelling `C:` collapses onto the same key as `C:\` — sound
/// only because [`canonical_existing_prefix`] + [`join_normalized`] can never hand a
/// drive-relative anchor to the comparison (the tail may not start with the
/// anchor's own `Prefix`/`RootDir`).
#[test]
fn a_drive_root_anchor_contains_its_own_volume_and_nothing_else() {
    assert!(path_within_in_family(
        Path::new(r"C:\"),
        Path::new(CI_TARGET),
        PathFamily::Windows
    ));
    for other in [
        r"D:\Users\runneradmin\AppData\Local\Temp\x",
        r"\\.\PhysicalDrive0",
        r"\\?\UNC\server\share\x",
        r"\\?\GLOBALROOT\Device\x",
    ] {
        assert!(
            !path_within_in_family(Path::new(r"C:\"), Path::new(other), PathFamily::Windows),
            "a C: drive root must never contain '{other}'"
        );
    }
    assert_eq!(
        key_components(&containment_key_in_family(
            Path::new(r"C:\"),
            PathFamily::Windows
        )),
        key_components(&containment_key_in_family(
            Path::new("C:"),
            PathFamily::Windows
        )),
        "the bare drive spelling and the drive root collapse onto the same components"
    );
}

/// Pins that the family seam itself is wired the way production needs: the pinned
/// entry points must keep using the *build* platform's rules.
#[test]
fn path_family_current_is_the_build_platform() {
    let expected = if cfg!(windows) {
        PathFamily::Windows
    } else {
        PathFamily::Unix
    };
    assert_eq!(PathFamily::current(), expected);
}

// ──────────────────────────────────────────────────────────────────────────
// Section B — lock-shard spelling invariance (the second real bug)
// ──────────────────────────────────────────────────────────────────────────

/// [`lock_shard_for`] with the family pinned. Production pins
/// [`PathFamily::current`], so this is the *only* way to exercise the Windows
/// shard-key from a Linux runner; the test below pins that it is exactly
/// `lock_shard_for`'s body.
fn lock_shard_for_in_family(path: &Path, family: PathFamily) -> usize {
    hash_path(Path::new(&containment_key_in_family(path, family))) % REPLACE_LOCK_SHARDS
}

/// Pins bug class: **shard split by spelling** — if the shard key were the raw
/// spelling, two `replace` calls naming one file differently would take different
/// shards and a later rename would silently discard an earlier edit.
#[test]
fn lock_shard_is_keyed_on_the_containment_key_not_the_raw_spelling() {
    for raw in [
        "/tmp/x/y.txt",
        "/tmp/x/../y.txt",
        r"C:\x\y.txt",
        r"\\?\C:\x\y.txt",
        "",
    ] {
        let path = Path::new(raw);
        assert_eq!(
            lock_shard_for(path),
            lock_shard_for_in_family(path, PathFamily::current()),
            "lock_shard_for must be hash_path(containment_key(path)) — raw spelling {raw:?}"
        );
    }
}

/// Pins bug class: **DOS vs verbatim (and any case/separator) spelling landing on
/// different lock shards**. Asserted from Linux through the family-pinned key,
/// because `PathFamily::current()` is Unix on this runner; the real
/// [`lock_shard_for`] pair is asserted under `#[cfg(windows)]` below.
#[test]
fn lock_shard_is_spelling_invariant_across_windows_spellings() {
    let dos = Path::new(CI_TARGET);
    let shard = lock_shard_for_in_family(dos, PathFamily::Windows);
    for other in [
        Path::new(VERBATIM_CI_TARGET),
        Path::new(MIXED_CASE_SLASH_TARGET),
        Path::new(&CI_TARGET.to_lowercase()),
        Path::new("C:/Users/runneradmin/AppData/Local/Temp/.tmp2Tiuen/shared_target.txt"),
    ] {
        assert_eq!(
            shard,
            lock_shard_for_in_family(other, PathFamily::Windows),
            "'{}' and '{CI_TARGET}' are one file and must share a shard",
            other.display()
        );
    }
    // Why the fold is load-bearing: without it the two keys differ, so a raw
    // spelling key could never be spelling-invariant.
    assert_ne!(
        containment_key_in_family(dos, PathFamily::Unix),
        containment_key_in_family(Path::new(VERBATIM_CI_TARGET), PathFamily::Unix),
        "the un-normalized spellings are different strings — that is the pre-fix state"
    );
    // An alias pair is *not* merged by the key either (documented boundary): only
    // canonicalizing both sides makes them one shard, which the next test pins.
    assert_ne!(
        lock_shard_for_in_family(Path::new(CI_TARGET_8_3), PathFamily::Windows),
        lock_shard_for_in_family(dos, PathFamily::Windows),
        "an unexpanded 8.3 alias may not silently share a key with the long name"
    );
}

/// Pins bug class: **two spellings of one real file** (on-disk path vs a symlink
/// alias standing in for an 8.3 alias) must land on one lock shard *after* the gate
/// normalized them, or concurrent `replace` calls lose edits.
#[test]
#[cfg(unix)]
fn lock_shard_is_spelling_invariant_for_a_symlink_alias_after_normalization() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real_dir");
    fs::create_dir_all(&real).expect("create real dir");
    let file = real.join("shared_target.txt");
    fs::write(&file, "slot0: original\n").expect("write fixture");
    let alias = dir.path().join("alias_dir");
    symlink(&real, &alias).expect("create symlink alias");

    let via_real = resolve_safe_path(&spell(&file), TOOL_READ_FILE).expect("real spelling allowed");
    let via_alias = resolve_safe_path(&spell(&alias.join("shared_target.txt")), TOOL_READ_FILE)
        .expect("alias spelling allowed");
    assert_eq!(
        via_real, via_alias,
        "the gate must hand one canonical spelling to the shard key"
    );
    assert_eq!(
        lock_shard_for(&via_real),
        lock_shard_for(&via_alias),
        "two spellings of one file must share a lock shard"
    );
    // The returned path is a fixed point of the gate, so the shard is stable under
    // re-resolution (that is what `replace_path_lock` relies on).
    let again = resolve_safe_path(&spell(&via_alias), TOOL_READ_FILE).expect("re-resolve allowed");
    assert_eq!(again, via_alias);
    assert_eq!(
        lock_shard_for(&via_real),
        lock_shard_for(&again),
        "re-resolving the canonical spelling must not move the shard"
    );
}

/// Pins bug class: the same shard split on the **real Windows file system**, where
/// `canonicalize` hands back a `\\?\`-verbatim spelling and `%TEMP%` a DOS one.
#[test]
#[cfg(windows)]
fn lock_shard_matches_across_dos_and_verbatim_spellings_on_windows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("shared_target.txt");
    fs::write(&file, "slot0: original\n").expect("write fixture");
    let verbatim =
        fs::canonicalize(&file).expect("canonicalize on Windows yields a `\\\\?\\` path");
    let verbatim_str = spell(&verbatim);
    assert!(
        verbatim_str.starts_with(r"\\?\"),
        "expected a verbatim canonical path, got {verbatim_str}"
    );
    let dos = PathBuf::from(strip_windows_path_prefix(&verbatim_str));
    assert!(
        !spell(&dos).starts_with(r"\\?\"),
        "DOS spelling still verbatim: {dos:?}"
    );
    assert_eq!(
        lock_shard_for(&verbatim),
        lock_shard_for(&dos),
        "verbatim vs DOS spelling of one file must share a lock shard"
    );
    let upper = PathBuf::from(verbatim_str.to_uppercase());
    assert_eq!(
        lock_shard_for(&verbatim),
        lock_shard_for(&upper),
        "case-only spelling difference must share a lock shard"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Section C — resolve_safe_path behaviour on real paths (Linux-runnable)
// ──────────────────────────────────────────────────────────────────────────

/// Pins bug class: **the CI denial itself** — a file inside the process temp dir is
/// an allow-list hit and must never be reported as an escape (Unix equivalent of
/// `C:\Users\RUNNER~1\AppData\Local\Temp\.tmp2Tiuen\shared_target.txt`).
#[test]
fn resolve_safe_path_accepts_a_file_inside_the_process_temp_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("shared_target.txt");
    fs::write(&file, "x\n").expect("write fixture");

    let resolved = resolve_safe_path(&spell(&file), TOOL_READ_FILE)
        .expect("a file inside std::env::temp_dir() must be allowed");
    assert_eq!(
        resolved,
        fs::canonicalize(&file).expect("canonicalize fixture"),
        "the gate must return the canonical spelling"
    );

    // `%TEMP%`-style spelling noise: trailing separator, `.` and a non-escaping `..`.
    let messy = format!(
        "{}/./nested/../shared_target.txt",
        dir.path().to_string_lossy()
    );
    let resolved_messy = resolve_safe_path(&messy, TOOL_READ_FILE)
        .expect("spelling noise must not change the verdict");
    assert_eq!(resolved_messy, resolved);
}

/// Pins bug class: **the returned path must be one canonical spelling**, so the
/// 8.3/verbatim alias pair (here: a symlink alias directory) cannot produce two
/// different downstream keys.
#[test]
#[cfg(unix)]
fn resolve_safe_path_returns_one_canonical_spelling_for_a_symlink_alias_directory() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real_dir");
    fs::create_dir_all(&real).expect("create real dir");
    let file = real.join("shared_target.txt");
    fs::write(&file, "x\n").expect("write fixture");
    let alias = dir.path().join("alias_dir");
    symlink(&real, &alias).expect("create alias symlink");

    let alias_file = alias.join("shared_target.txt");
    let via_alias = resolve_safe_path(&spell(&alias_file), TOOL_READ_FILE).expect("alias allowed");
    let via_real = resolve_safe_path(&spell(&file), TOOL_READ_FILE).expect("real allowed");
    assert_eq!(
        via_alias, via_real,
        "alias and real must resolve identically"
    );
    assert_eq!(
        via_real,
        fs::canonicalize(&file).expect("canonical fixture")
    );
    assert_ne!(
        via_alias, alias_file,
        "the alias spelling must not survive into the returned path"
    );
}

/// Pins bug class: **reparse-point/symlink escape** — a link inside the allow-list
/// entry pointing at a file outside both entries must be denied, as a file operand
/// and as a directory component.
#[test]
#[cfg(unix)]
fn resolve_safe_path_denies_a_symlink_inside_the_root_that_points_outside() {
    use std::os::unix::fs::symlink;
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::create_dir_all(&root).expect("create scoped root");
    let outside = base.join("outside_target.txt");
    fs::write(&outside, "secret\n").expect("write outside fixture");
    symlink(&outside, root.join("escape_link")).expect("create escaping symlink");
    symlink(Path::new("/etc/passwd"), root.join("etc_link")).expect("create /etc symlink");

    for spelling in [
        "escape_link",
        "escape_link/",
        "./escape_link",
        "etc_link",
        "etc_link/passwd",
    ] {
        assert_escape_denied(resolve_at(&root, spelling), spelling);
    }
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins bug class: **`..` traversal** must be denied with the documented wording
/// even though the raw join starts inside the allow-list entry.
#[test]
fn resolve_safe_path_denies_parent_directory_traversal() {
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::write(base.join("outside.txt"), "secret\n").expect("write outside fixture");
    fs::create_dir_all(root.join("deep/nested")).expect("create nested dirs");

    // NOT in this list on purpose: `deep/../outside.txt` pops back *into* the root,
    // so it names an in-root missing leaf and must be allowed (pinned in
    // `resolve_safe_path_allows_an_in_root_parent_mix_that_never_escapes`).
    for spelling in [
        "../outside.txt",
        "./../outside.txt",
        "deep/nested/../../../outside.txt",
        "deep/nested/../../../../../../etc/passwd",
        "../../../../../../../etc/passwd",
    ] {
        assert_escape_denied(resolve_at(&root, spelling), spelling);
    }
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins bug class: an **absolute path outside both allow-list entries** must be
/// denied — including a `/proc/self/root` spelling that canonicalizes onto `/etc`.
#[test]
fn resolve_safe_path_denies_absolute_paths_outside_both_allow_list_entries() {
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::create_dir_all(&root).expect("create scoped root");
    for spelling in [
        "/etc/passwd",
        "/proc/self/root/etc/passwd",
        "/root/.ssh/id_rsa",
        "/etc/../etc/passwd",
    ] {
        assert_escape_denied(resolve_at(&root, spelling), spelling);
    }
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins bug class: the **`sub/../../../../` mix** — a candidate whose *leading*
/// components are inside the root and whose tail walks out of it.
#[test]
fn resolve_safe_path_denies_a_traversal_mix_that_starts_inside_the_root() {
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::create_dir_all(root.join("sub")).expect("create sub dir");
    fs::write(base.join("outside.txt"), "secret\n").expect("write outside fixture");

    for spelling in [
        "sub/../../../../outside.txt",
        "sub/../../../../../../etc/passwd",
        "sub/./../sub/../../outside.txt",
    ] {
        assert_escape_denied(resolve_at(&root, spelling), spelling);
    }
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins the guard must not **over-deny**: a `..` mix that stays inside the
/// allow-list entry is the same file and must be allowed with the canonical answer.
#[test]
fn resolve_safe_path_allows_an_in_root_parent_mix_that_never_escapes() {
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::create_dir_all(root.join("sub")).expect("create sub dir");
    fs::create_dir_all(root.join("deep/nested")).expect("create nested dirs");
    let file = root.join("inside.txt");
    fs::write(&file, "inside\n").expect("write fixture");

    for spelling in [
        "sub/../inside.txt",
        "./inside.txt",
        "sub/.././inside.txt",
        "sub/../../ws/inside.txt",
        "deep/nested/../../inside.txt",
        "deep/../inside.txt",
    ] {
        let resolved = resolve_at(&root, spelling).unwrap_or_else(|err| {
            panic!("'{spelling}' stays inside the root but was denied: {err}")
        });
        assert_eq!(
            resolved,
            fs::canonicalize(&file).expect("canonical fixture"),
            "'{spelling}' must normalize onto the same file"
        );
    }

    // A `..` mix that pops back into the root names an in-root **missing leaf**, which
    // must stay allowed — the CI temp scenario depends on missing leaves.
    let mixed_missing = "deep/nested/../../not-created-yet.txt";
    let resolved_missing = resolve_at(&root, mixed_missing).unwrap_or_else(|err| {
        panic!("'{mixed_missing}' stays inside the root but was denied: {err}")
    });
    assert_eq!(
        resolved_missing,
        fs::canonicalize(&root)
            .expect("canonical root")
            .join("not-created-yet.txt"),
        "'{mixed_missing}' must fold onto the in-root leaf"
    );
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins bug class: **missing-leaf allowance** — a not-yet-existing file under a real
/// directory must still resolve (writers create it), with the anchor canonicalized
/// and the missing tail re-joined lexically.
#[test]
fn resolve_safe_path_allows_a_missing_leaf_inside_a_real_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let leaf = dir.path().join("nested/not-yet-there.txt");
    let resolved =
        resolve_safe_path(&spell(&leaf), TOOL_REPLACE).expect("a missing leaf must stay allowed");
    assert_eq!(
        resolved, leaf,
        "canonical anchor + lexically re-joined tail"
    );
    assert!(!leaf.exists(), "the gate must not create the leaf");

    // And it must not be *loosened* into an escape: the same missing leaf outside
    // both allow-list entries is denied.
    assert_escape_denied(
        resolve_safe_path(
            "/definitely-not-an-allow-list-entry/marmel/x.txt",
            TOOL_REPLACE,
        ),
        "/definitely-not-an-allow-list-entry/marmel/x.txt",
    );
}

/// Pins bug class: **silent raw fallback** — an anchor that cannot be canonicalized
/// must be an error the caller turns into a denial, never a raw 8.3/`%TEMP%`
/// spelling handed to the comparison.
#[test]
fn resolve_safe_path_fails_closed_when_no_existing_ancestor_can_be_resolved() {
    let err = canonical_existing_prefix(
        TOOL_READ_FILE,
        Path::new("definitely-missing-marmel-anchor/nested/deeper.txt"),
    )
    .expect_err("a path with no existing ancestor has no raw fallback");
    match err {
        ToolError::BadArguments { ref detail, .. } => assert!(
            detail.contains("cannot resolve an existing ancestor"),
            "unexpected fail-closed detail: {detail}"
        ),
        other => panic!("expected BadArguments from canonical_existing_prefix, got {other}"),
    }

    // The deepest existing ancestor is canonicalized and the tail re-joined: no raw
    // spelling ever reaches the containment test.
    let dir = tempfile::tempdir().expect("tempdir");
    let canonical_dir = fs::canonicalize(dir.path()).expect("canonical tempdir");
    let resolved = canonical_existing_prefix(
        TOOL_READ_FILE,
        &dir.path().join("./nested/../missing-leaf.txt"),
    )
    .expect("existing anchor canonicalizes");
    assert_eq!(resolved, canonical_dir.join("missing-leaf.txt"));

    // And a target whose directory chain cannot be canonicalized is still decided on
    // the canonicalized anchor, i.e. denied rather than guessed.
    //
    // NOTE (family honesty): the Windows device spelling `\\.\PhysicalDrive0` is *not*
    // an escape on the Unix family — there it is an ordinary file name, and the
    // workspace-relative join puts it inside the root, which is the correct Unix
    // verdict. The device-escape verdict belongs to the Windows family and is pinned
    // by `device_namespace_and_unc_spellings_are_never_inside_a_volume_root` (pure
    // helper, runs here) and by
    // `resolve_safe_path_denies_device_namespace_and_unc_escapes` (`#[cfg(windows)]`).
    assert!(
        resolve_safe_path(r"\\.\PhysicalDrive0", TOOL_READ_FILE)
            .is_ok_and(|path| path.to_string_lossy().ends_with(r"\\.\PhysicalDrive0")),
        "the Unix family must treat the device-looking spelling as an ordinary name, \
         not silently rewrite it"
    );
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::create_dir_all(&root).expect("create scoped root");
    for spelling in [
        "/definitely-missing-marmel-anchor/x.txt",
        "/definitely-missing-marmel-anchor/deeper/x.txt",
    ] {
        assert_escape_denied(resolve_at(&root, spelling), spelling);
    }
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins the **deny wording contract** (`fs.rs` `escape_denied`), which the existing
/// `fs_tests.rs:238-265` assertions and callers both key off.
#[test]
fn deny_message_is_the_single_documented_string() {
    let base = scoped_root_outside_temp();
    let root = base.join("ws");
    fs::create_dir_all(&root).expect("create scoped root");
    for spelling in ["../outside.txt", "/etc/passwd"] {
        let err = resolve_at(&root, spelling).expect_err("must be denied");
        match err {
            ToolError::Forbidden {
                ref tool,
                ref caller,
            } => {
                assert_eq!(tool, TOOL_READ_FILE);
                assert_eq!(
                    caller,
                    &format!("access denied: path '{spelling}' escapes workspace root")
                );
            }
            other => panic!("expected Forbidden, got {other}"),
        }
    }
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

/// Pins bug class: **sibling-prefix bypass through the real gate** — `../foobar/x`
/// under a root named `foo` shares a byte prefix with the root but not a component.
#[test]
fn resolve_safe_path_denies_a_sibling_directory_prefix() {
    let base = scoped_root_outside_temp();
    let root = base.join("foo");
    fs::create_dir_all(&root).expect("create root");
    let sibling = base.join("foobar");
    fs::create_dir_all(&sibling).expect("create sibling");
    fs::write(sibling.join("x.txt"), "x\n").expect("write sibling file");

    assert_escape_denied(resolve_at(&root, "../foobar/x.txt"), "../foobar/x.txt");
    assert_escape_denied(
        resolve_at(&root, &spell(&sibling.join("x.txt"))),
        &spell(&sibling.join("x.txt")),
    );
    // The root's own file is still allowed — the rule is not a blanket deny.
    fs::write(root.join("own.txt"), "x\n").expect("write root file");
    assert!(resolve_at(&root, "own.txt").is_ok());
    fs::remove_dir_all(&base).expect("cleanup scoped root");
}

// ──────────────────────────────────────────────────────────────────────────
// Section D — tests that genuinely need a Windows file system
// ──────────────────────────────────────────────────────────────────────────

// Query the volume's 8.3 alias for a path. Returns `None` when the volume has 8.3
// name generation disabled or the API is unavailable, so the caller can skip
// honestly instead of asserting something the file system does not provide.
// (Plain comment, not a doc comment: `unused` is denied repo-wide and rustdoc
// generates no documentation for an `extern` block — caught by the windows-gnu
// cross-check leg.)
#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetShortPathNameW(lpszpath: *const u16, lpszshortpath: *mut u16, cchbuffer: u32) -> u32;
}

// `GetShortPathNameW` wrapper over a wide-char buffer: retry with a bigger buffer
// while the returned length does not fit.
#[cfg(windows)]
fn short_path_name(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    for size in [260u32, 1024, 8192] {
        let mut buffer: Vec<u16> = vec![0; size as usize];
        let written = unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), size) };
        if written == 0 {
            return None; // call failed (or 8.3 generation is off)
        }
        if (written as usize) < buffer.len() {
            buffer.truncate(written as usize);
            return String::from_utf16(&buffer).ok();
        }
    }
    None
}

/// Pins bug class: **the CI input itself** — the `\\?\`-verbatim spelling produced
/// by `canonicalize` of a file inside the temp root must be accepted.
#[test]
#[cfg(windows)]
fn resolve_safe_path_accepts_a_verbatim_prefixed_file_in_the_temp_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let leaf = dir.path().join(".tmp2Tiuen").join("shared_target.txt");
    fs::create_dir_all(dir.path().join(".tmp2Tiuen")).expect("create leaf dir");
    fs::write(&leaf, "x\n").expect("write fixture");

    let verbatim = fs::canonicalize(&leaf).expect("canonicalize fixture");
    let verbatim_str = spell(&verbatim);
    assert!(
        verbatim_str.starts_with(r"\\?\"),
        "expected a verbatim path from canonicalize, got {verbatim_str}"
    );
    let resolved = resolve_safe_path(&verbatim_str, TOOL_READ_FILE)
        .expect("verbatim spelling must be allowed");
    assert_eq!(resolved, verbatim);
    // The DOS spelling of the same file resolves to the identical path.
    let dos = spell(&PathBuf::from(strip_windows_path_prefix(&verbatim_str)));
    let resolved_dos =
        resolve_safe_path(&dos, TOOL_READ_FILE).expect("DOS spelling must be allowed");
    assert_eq!(resolved_dos, resolved, "verbatim and DOS must agree");
}

/// Pins bug class: **device / namespace escapes on the real platform** — they must
/// be denied, not canonicalized into an allow.
#[test]
#[cfg(windows)]
fn resolve_safe_path_denies_device_namespace_and_unc_escapes() {
    for spelling in [
        r"\\.\PhysicalDrive0",
        r"\\?\UNC\server\share\x",
        r"\\?\GLOBALROOT\Device\x",
        r"\\?\Device\HarddiskVolume1\Windows\x",
        r"\\server\share\x",
    ] {
        assert_escape_denied(resolve_safe_path(spelling, TOOL_READ_FILE), spelling);
    }
}

/// Pins bug class: **the 8.3 short name** against a real alias, i.e. the exact
/// `C:\Users\RUNNER~1\…` vs long-name split from CI. The boundary is respected:
/// acceptance comes from canonicalizing both operands, not from the string fold.
#[test]
#[cfg(windows)]
fn eight_dot_three_alias_of_a_temp_directory_resolves_to_the_same_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    // >8 chars, no dot: a name that gets an 8.3 alias when generation is enabled.
    let long = dir.path().join("marmel-eight-dot-three-alias-target");
    fs::create_dir_all(&long).expect("create long-named dir");
    let file = long.join("shared_target.txt");
    fs::write(&file, "x\n").expect("write fixture");
    let canonical = fs::canonicalize(&file).expect("canonicalize fixture");

    let Some(short) = short_path_name(&long) else {
        eprintln!(
            "skip: this volume reports no 8.3 alias for {}",
            long.display()
        );
        return;
    };
    if short == long.to_string_lossy() {
        eprintln!("skip: 8.3 name generation is disabled ({short} == long name)");
        return;
    }
    let short_file = Path::new(&short).join("shared_target.txt");
    assert_ne!(
        containment_key_in_family(&short_file, PathFamily::Windows),
        containment_key_in_family(&file, PathFamily::Windows),
        "the fold alone must not expand the alias it just obtained"
    );
    let resolved_short =
        resolve_safe_path(&spell(&short_file), TOOL_READ_FILE).expect("8.3 alias must resolve");
    assert_eq!(
        resolved_short, canonical,
        "canonicalizing both sides is what carries the 8.3 alias"
    );
    assert_eq!(
        lock_shard_for(&resolved_short),
        lock_shard_for(&canonical),
        "the 8.3 alias must land on the same lock shard"
    );
}

/// Pins bug class: **case and trailing-separator spellings of an allowed file**
/// (docs §5 item 14) — the same file must be allowed and must normalize identically.
#[test]
#[cfg(windows)]
fn resolve_safe_path_accepts_case_variants_and_trailing_separator() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("shared_target.txt");
    fs::write(&file, "x\n").expect("write fixture");
    let canonical = fs::canonicalize(&file).expect("canonicalize fixture");
    let canonical_str = spell(&canonical);

    let upper = canonical_str.to_uppercase();
    let with_trailing = format!(r"{canonical_str}\");
    let slashed = canonical_str.replace('\\', "/");
    for spelling in [&upper, &with_trailing, &slashed] {
        let resolved = resolve_safe_path(spelling, TOOL_READ_FILE)
            .unwrap_or_else(|err| panic!("'{spelling}' must be allowed, got {err}"));
        assert_eq!(
            containment_key_in_family(&resolved, PathFamily::Windows),
            containment_key_in_family(&canonical, PathFamily::Windows),
            "'{spelling}' must normalize onto the canonical file"
        );
    }
}
