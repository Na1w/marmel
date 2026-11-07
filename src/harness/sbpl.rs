//! macOS Seatbelt SBPL profile emitter.
//!
//! Renders one flat SBPL s-expression profile (the format `sandbox_init_with_parameters`
//! compiles) from the platform-neutral policy list of
//! [`crate::harness::sandbox::policy_paths`], so the Landlock ruleset and the Seatbelt
//! profile are built from one and the same allow-list, with one and the same grant polarity.
//!
//! The module is deliberately **not** `#[cfg]`-gated: nothing in it touches a syscall, a
//! kernel API or the filesystem, so the emitted text is unit-testable on any host. The
//! `#[cfg(target_os = "macos")]` boundary belongs to the backend that calls this and hands
//! the string to libSystem (see `.marmel/research/seatbelt-feasibility.md` §2 and §4).
//!
//! What the emitter adds on top of the shared policy list, and why:
//!
//! * **Path translations.** SBPL filters match **literal path strings**, not resolved file
//!   descriptors — the exact opposite of Landlock's `PathFd` rules (gotcha 1 of
//!   `.marmel/research/seatbelt-macos.md`). `/tmp`, `/var` and `/etc` are symlinks into
//!   `/private`, so every policy entry below those prefixes is emitted under **both**
//!   spellings: a rule on the link spelling alone misses the path the kernel opens.
//! * **`$TMPDIR`.** The per-user temp folder on macOS is not `/tmp` but a folder under
//!   `/private/var/folders`, and the Linux policy has no entry for it. Without it `rustc`,
//!   `cc`, `mktemp` and `tempfile` cannot write (gotcha 2), so it is always granted
//!   read-write.
//! * **A non-file operation block.** Seatbelt mediates `sysctl`, Mach lookups, signals,
//!   sockets, network and `execve`/`fork`, which Landlock never mediates, so `(deny default)`
//!   breaks a plain shell unless those are granted explicitly (gotcha 3 and 5). They are
//!   emitted from [`NON_FILE_ALLOW_BLOCK`] as data, and the vocabulary is Apple-private: the
//!   block is marked `[verify-on-Mac]` because it was never observed on a Mac from this
//!   repository (research note §3 and §8).
//!
//! What the emitter does **not** do:
//!
//! * It never opens a path. SBPL matches text, so an entry that does not exist on the host
//!   (`/lib64`, `/nix/store`, `~/.npm`, an absent Xcode) never matches and grants nothing —
//!   which is also why [`PolicyPath::required`](crate::harness::sandbox::PolicyPath) has no
//!   effect on the rendered text.
//! * It emits no explicit `deny` clauses for sensitive directories (`~/.ssh`, `~/.gnupg`).
//!   The deny-default baseline already denies them; making the denial log-visible needs the
//!   home paths the platform-neutral list does not carry, and that `[verify-on-Mac]`
//!   refinement belongs to the backend (research note §5).

use crate::harness::sandbox::{Grant, PolicyPath, policy_paths};
use std::path::{Path, PathBuf};

/// The environment key holding the per-user temporary folder on macOS.
///
/// `std::env::var_os` is how the existing sandbox code reads the environment (this crate has
/// no `dirs` dependency): [`policy_paths`] reads `HOME`, `CARGO_HOME` and `RUSTUP_HOME` the
/// same way.
pub const TMPDIR_ENV: &str = "TMPDIR";

/// Where a confined process falls back to when [`TMPDIR_ENV`] is unset: the real root of the
/// per-user folders, so a granted folder below it is covered either way.
const TMPDIR_FALLBACK: &str = "/private/var/folders";

/// FHS prefixes macOS serves as symlinks into `/private`, as `(link, real)` pairs. A policy
/// entry at or below one of them is emitted under both spellings (gotcha 1).
const PRIVATE_PATH_PREFIXES: &[(&str, &str)] = &[
    ("/tmp", "/private/tmp"),
    ("/var", "/private/var"),
    ("/etc", "/private/etc"),
];

/// The policy entries known to be single files rather than directories: the character devices
/// of the shared allow-list. `subpath` matches a path **and everything below it**, `literal`
/// matches that one path alone, and a `literal` filter on a directory grants nothing beneath
/// it — so the choice cannot be made from a filesystem probe without making the emitted
/// profile depend on what happens to exist on the host.
const SINGLE_FILE_POLICY_PATHS: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/urandom",
    "/dev/random",
];

/// Basenames of policy entries that are plain files wherever they appear, e.g.
/// `$HOME/.gitconfig`. `/dev/pts` and `/dev/shm` are deliberately absent from the device
/// table: they are directories, so they have to be emitted as `subpath`.
const SINGLE_FILE_BASENAMES: &[&str] = &[".gitconfig", ".CFUserTextEncoding"];

/// Read-only roots a macOS shell and toolchain needs that the Linux allow-list has no
/// counterpart for: the system volume, the plain `/Library` tree, the Command Line Tools
/// `ld`/`libtool` that `rustc` execs in its link steps, and Xcode when it is installed
/// (gotcha 6). A rule on a root the host does not have grants nothing.
const MACOS_READONLY_ADDITIONS: &[&str] = &[
    "/System",
    "/Library",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app",
];

/// Section header comment for the policy-derived clauses, so a profile on disk is readable
/// without the emitter at hand.
const SECTION_POLICY_COMMENT: &[&str] = &[
    "; --- 1. The shared policy allow-list (sandbox::policy_paths), in policy order and with",
    ";     the grant that list states: file-read* + file-write* for a read-write entry and",
    ";     file-read* for a read-only one. Entries that only exist on a Linux host (`/lib64`,",
    ";     `/proc`, `~/.npm`) match nothing here and therefore grant nothing, which is why",
    ";     they are rendered verbatim rather than filtered out. `subpath` covers a path and",
    ";     everything below it, `literal` the exact path alone.",
];

/// Section header comment for the grants with no Linux counterpart.
const SECTION_MACOS_COMMENT: &[&str] = &[
    "; --- 2. macOS-only file grants, with no counterpart in the Linux policy: the TMPDIR of",
    ";     the confined process (writable, because rustc, cc, mktemp and tempfile all die",
    ";     without it) and the toolchain roots a Mac needs. [verify-on-Mac] for those roots.",
];

/// Section header comment for the non-file block.
const SECTION_NON_FILE_COMMENT: &[&str] = &[
    "; --- 3. Non-file operations Seatbelt mediates and Landlock never does. The vocabulary is",
    ";     Apple-private, so every clause below is [verify-on-Mac] against the target host:",
    ";     the breadth is the floor a shell plus a toolchain needs, not a tuned policy.",
];

/// Section header comment for the dynamic-linker block.
const SECTION_DYLD_COMMENT: &[&str] = &[
    "; --- 4. Dynamic linker block, inlined from /System/Library/Sandbox/Profiles/",
    ";     dyld-support.sb (Chromium common.sb:276-282), [verify-on-Mac].",
];

/// The non-file operations Seatbelt mediates and Landlock never does, which `(deny default)`
/// would otherwise break: every `execve` ([`NON_FILE_ALLOW_BLOCK`]'s `process-exec`, needed
/// for `/bin/sh` and for every tool the shell starts — the analogue of the Landlock host
/// probe that requires `sh` to be reachable after `restrict_self()`), `process-fork` for
/// pipelines and subshells, signals for job control, `sysctl-read` for the `hw`/`kern` reads
/// a runtime makes at startup, Mach bootstrap lookups (`cfprefsd` and friends), POSIX shared
/// memory, sockets and network access so DNS and package downloads keep working, and
/// `file-map-executable` for the dynamic linker and any JIT.
///
/// The names come from the shipped `/System/Library/Sandbox/Profiles/*.sb` and from
/// Chromium's `sandbox/policy/mac/common.sb` (research note §2 and §7 of
/// `.marmel/research/seatbelt-macos.md`); tightening them to per-name filters
/// (`sysctl-name`, `global-name`, `remote tcp`) is the on-host tuning problem that
/// `.marmel/research/seatbelt-feasibility.md` §1 marks as the only real open question.
const NON_FILE_ALLOW_BLOCK: &[&str] = &[
    // Exec and fork: without them the wrapped payload cannot even start a shell.
    "(allow process-exec process-fork)",
    // Job control and Ctrl-C. `(target any)` is wider and [verify-on-Mac].
    "(allow signal (target self))",
    // Runtime startup reads of hw.* and kern.* names.
    "(allow sysctl-read)",
    // Mach bootstrap lookups: cfprefsd, the XPC services, the WindowServer family.
    "(allow mach-lookup)",
    "(allow ipc-posix-shm)",
    // Sockets and network: DNS resolution and builds must keep working.
    "(allow system-socket)",
    "(allow network-outbound network-inbound network-bind)",
    // proc_pidinfo, used by shell job control and by test harnesses.
    "(allow process-info-pidinfo (target self))",
    // Device ioctls: `stty`/`ulimit` in the wrapped payload, PTY size queries (TIOCGWINSZ)
    // and the dtrace helper node (gotcha 8).
    "(allow file-ioctl (subpath \"/dev\") (literal \"/dev/dtracehelper\"))",
    // Mapping an executable image is its own operation on macOS 11+ (gotcha 4): the read
    // grants alone are not enough for the dyld shared cache.
    "(allow file-map-executable)",
];

/// The dynamic-linker block inlined from `/System/Library/Sandbox/Profiles/dyld-support.sb` —
/// verbatim from Chromium's `sandbox/policy/mac/common.sb:276-282`, because a profile handed
/// to `sandbox_init*` is one flat string that cannot `import` it (gotcha 4).
const DYLD_SUPPORT_BLOCK: &str = "(allow file-read* file-map-executable \
    (subpath \"/System/Cryptexes/App\") \
    (subpath \"/System/Cryptexes/OS\") \
    (subpath \"/System/Volumes/Preboot/Cryptexes/App/System\") \
    (subpath \"/System/Volumes/Preboot/Cryptexes/OS\"))";

/// Render the whole Seatbelt profile for a run: the shared policy list for `workspace_root`
/// with the operator's extra read-only paths, plus the macOS-only additions, with the
/// process' own `TMPDIR` ([`TMPDIR_ENV`]) granted read-write.
///
/// Deterministic by construction: the clauses follow the order of
/// [`policy_paths`], which is itself stable and environment-ordered.
#[must_use]
pub fn render_profile(workspace_root: &Path, extra_readonly_paths: &[String]) -> String {
    let policy = policy_paths(workspace_root, extra_readonly_paths);
    render_policy_profile(&policy, tmpdir_from_env().as_deref())
}

/// The value of [`TMPDIR_ENV`] as a path, or `None` when it is unset or empty.
#[must_use]
pub fn tmpdir_from_env() -> Option<PathBuf> {
    std::env::var_os(TMPDIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The pure half of [`render_profile`]: render `policy` in list order with the grant each
/// entry states, plus the read-write grant for `tmpdir`, falling back to [`TMPDIR_FALLBACK`]
/// when `tmpdir` is `None`. Everything the renderer needs is an argument, so the emitted text
/// is testable without a filesystem, an environment or a Mac.
#[must_use]
pub fn render_policy_profile(policy: &[PolicyPath], tmpdir: Option<&Path>) -> String {
    let mut lines: Vec<String> = vec![
        "(version 1)".to_string(),
        String::new(),
        "; Everything below is an allow-list on top of this deny-default baseline.".to_string(),
    ];
    lines.push("(deny default)".to_string());
    lines.push(String::new());

    // 1. The shared allow-list: one clause per policy entry, in policy order.
    lines.extend(
        SECTION_POLICY_COMMENT
            .iter()
            .map(|line| (*line).to_string()),
    );
    for entry in policy {
        lines.extend(policy_rules(entry));
    }

    // 2. Grants the Linux policy has no entry for.
    lines.push(String::new());
    lines.extend(SECTION_MACOS_COMMENT.iter().map(|line| (*line).to_string()));
    lines.extend(tmpdir_rules(tmpdir));
    for path in MACOS_READONLY_ADDITIONS {
        lines.push(format!("(allow file-read* (subpath {}))", quote(path)));
    }

    // 3. The operations Landlock never mediates.
    lines.push(String::new());
    lines.extend(
        SECTION_NON_FILE_COMMENT
            .iter()
            .map(|line| (*line).to_string()),
    );
    for rule in NON_FILE_ALLOW_BLOCK {
        lines.push((*rule).to_string());
    }

    // 4. The dynamic linker block.
    lines.push(String::new());
    lines.extend(SECTION_DYLD_COMMENT.iter().map(|line| (*line).to_string()));
    lines.push(DYLD_SUPPORT_BLOCK.to_string());

    lines.join("\n")
}

/// The clauses for one policy entry: the grant's operations over every macOS spelling of the
/// path, filtered as `literal` or `subpath` by the file-shape tables above.
#[must_use]
fn policy_rules(entry: &PolicyPath) -> Vec<String> {
    let operations: &[&str] = match entry.grant {
        Grant::ReadWrite => &["file-read*", "file-write*"],
        Grant::ReadOnly => &["file-read*"],
    };
    let kind = if is_single_file(&entry.path) {
        "literal"
    } else {
        "subpath"
    };

    let mut rules = Vec::new();
    for spelling in macos_spellings(&entry.path) {
        let filter = format!("({kind} {})", quote(&spelling));
        for operation in operations {
            rules.push(format!("(allow {operation} {filter})"));
        }
    }
    rules
}

/// The read-write grant for the process' own temporary folder, in both spellings. An entry
/// whose folder the policy already covers (a `TMPDIR` under `/tmp`, say) is simply granted
/// twice, which SBPL tolerates.
#[must_use]
fn tmpdir_rules(tmpdir: Option<&Path>) -> Vec<String> {
    let entry = PolicyPath {
        path: tmpdir.map_or_else(|| PathBuf::from(TMPDIR_FALLBACK), Path::to_path_buf),
        grant: Grant::ReadWrite,
        required: false,
    };
    policy_rules(&entry)
}

/// Every spelling of `path` a Seatbelt filter has to match: the spelling the policy states,
/// plus the `/private/...` real path for the prefixes macOS serves as symlinks.
#[must_use]
fn macos_spellings(path: &Path) -> Vec<String> {
    let text = path.to_string_lossy().to_string();
    let mut spellings = vec![text.clone()];

    for &(link, real) in PRIVATE_PATH_PREFIXES {
        let rest = match text.strip_prefix(link) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
            _ => continue,
        };
        let translated = format!("{real}{rest}");
        if translated != text {
            spellings.push(translated);
        }
        break;
    }

    spellings
}

/// Whether a policy entry names a single file rather than a directory, decided from the
/// policy's own knowledge of the entry ([`SINGLE_FILE_POLICY_PATHS`],
/// [`SINGLE_FILE_BASENAMES`]) rather than from the filesystem.
#[must_use]
fn is_single_file(path: &Path) -> bool {
    let text = path.to_string_lossy();
    if SINGLE_FILE_POLICY_PATHS.contains(&&*text) {
        return true;
    }
    path.file_name().is_some_and(|name| {
        SINGLE_FILE_BASENAMES
            .iter()
            .any(|base| *base == name.to_string_lossy().as_ref())
    })
}

/// `path` as an SBPL string literal: the text quoted for the SBPL lexer.
#[must_use]
pub fn quote_path(path: &Path) -> String {
    quote(path.to_string_lossy().as_ref())
}

/// The quoting half of [`quote_path`], taking an already-textual spelling.
///
/// SBPL string literals are `"`-delimited with backslash escapes, so `"` and `\` are the only
/// characters that need escaping. `$`, spaces, `(` and `)` are ordinary characters inside
/// quotes — which is what makes an arbitrary workspace path safe to interpolate at all, and
/// what a bare unquoted token would not survive. Non-UTF-8 bytes go through `to_string_lossy`,
/// the same way the text reaches libSystem as a C string.
#[must_use]
fn quote(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for ch in text.chars() {
        if ch == '"' || ch == '\\' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, grant: Grant) -> PolicyPath {
        PolicyPath {
            path: PathBuf::from(path),
            grant,
            required: false,
        }
    }

    /// The fixed policy the golden assertion renders: a required workspace root, a temporary
    /// directory, a device node, a system toolchain root and a plain-file configuration
    /// entry. Fake paths, so the text never depends on the host.
    fn golden_policy() -> Vec<PolicyPath> {
        vec![
            PolicyPath {
                path: PathBuf::from("/policy-surface/workspace-root"),
                grant: Grant::ReadWrite,
                required: true,
            },
            entry("/tmp", Grant::ReadWrite),
            entry("/dev/null", Grant::ReadWrite),
            entry("/usr", Grant::ReadOnly),
            entry("/home/tester/.gitconfig", Grant::ReadOnly),
        ]
    }

    /// A fixed fake `TMPDIR` in the shape macOS actually uses.
    const GOLDEN_TMPDIR: &str = "/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T";

    /// The whole rendered profile, asserted as one exact string: this text is what is handed
    /// to `sandbox_init_with_parameters`, so a lost or a stray clause is a policy change.
    #[test]
    fn test_rendered_profile_is_the_golden_text() {
        let profile = render_policy_profile(&golden_policy(), Some(Path::new(GOLDEN_TMPDIR)));
        let expected = r#"(version 1)

; Everything below is an allow-list on top of this deny-default baseline.
(deny default)

; --- 1. The shared policy allow-list (sandbox::policy_paths), in policy order and with
;     the grant that list states: file-read* + file-write* for a read-write entry and
;     file-read* for a read-only one. Entries that only exist on a Linux host (`/lib64`,
;     `/proc`, `~/.npm`) match nothing here and therefore grant nothing, which is why
;     they are rendered verbatim rather than filtered out. `subpath` covers a path and
;     everything below it, `literal` the exact path alone.
(allow file-read* (subpath "/policy-surface/workspace-root"))
(allow file-write* (subpath "/policy-surface/workspace-root"))
(allow file-read* (subpath "/tmp"))
(allow file-write* (subpath "/tmp"))
(allow file-read* (subpath "/private/tmp"))
(allow file-write* (subpath "/private/tmp"))
(allow file-read* (literal "/dev/null"))
(allow file-write* (literal "/dev/null"))
(allow file-read* (subpath "/usr"))
(allow file-read* (literal "/home/tester/.gitconfig"))

; --- 2. macOS-only file grants, with no counterpart in the Linux policy: the TMPDIR of
;     the confined process (writable, because rustc, cc, mktemp and tempfile all die
;     without it) and the toolchain roots a Mac needs. [verify-on-Mac] for those roots.
(allow file-read* (subpath "/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-write* (subpath "/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-read* (subpath "/private/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-write* (subpath "/private/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-read* (subpath "/System"))
(allow file-read* (subpath "/Library"))
(allow file-read* (subpath "/Library/Developer/CommandLineTools"))
(allow file-read* (subpath "/Applications/Xcode.app"))

; --- 3. Non-file operations Seatbelt mediates and Landlock never does. The vocabulary is
;     Apple-private, so every clause below is [verify-on-Mac] against the target host:
;     the breadth is the floor a shell plus a toolchain needs, not a tuned policy.
(allow process-exec process-fork)
(allow signal (target self))
(allow sysctl-read)
(allow mach-lookup)
(allow ipc-posix-shm)
(allow system-socket)
(allow network-outbound network-inbound network-bind)
(allow process-info-pidinfo (target self))
(allow file-ioctl (subpath "/dev") (literal "/dev/dtracehelper"))
(allow file-map-executable)

; --- 4. Dynamic linker block, inlined from /System/Library/Sandbox/Profiles/
;     dyld-support.sb (Chromium common.sb:276-282), [verify-on-Mac].
(allow file-read* file-map-executable (subpath "/System/Cryptexes/App") (subpath "/System/Cryptexes/OS") (subpath "/System/Volumes/Preboot/Cryptexes/App/System") (subpath "/System/Volumes/Preboot/Cryptexes/OS"))"#;

        assert_eq!(profile, expected);
        // The header and the baseline are mandatory, and always in that order.
        assert!(
            profile.starts_with("(version 1)")
                && profile.find("(version 1)").expect("header")
                    < profile.find("(deny default)").expect("baseline"),
            "the version header comes first and the deny-default baseline before any grant"
        );
    }

    #[test]
    fn test_macos_spellings_cover_the_private_real_paths() {
        for (link, real) in PRIVATE_PATH_PREFIXES {
            assert_eq!(
                macos_spellings(Path::new(link)),
                vec![link.to_string(), real.to_string()],
                "the policy spelling and its real path must both be emitted for {link}"
            );
            assert_eq!(
                macos_spellings(Path::new("/var/tmp")),
                vec!["/var/tmp".to_string(), "/private/var/tmp".to_string()],
                "a path below a translated prefix must carry the rest of the path over"
            );
        }
        // A path that merely starts with the prefix text is not a path below it.
        assert_eq!(
            macos_spellings(Path::new("/tmpx")),
            vec!["/tmpx".to_string()],
            "no translation may happen for /tmpx"
        );
    }

    /// The rendering is stable: the clause order comes from the policy list, so the same policy
    /// always produces byte-identical text, which is what a golden test needs.
    #[test]
    fn test_rendering_is_deterministic() {
        let policy = golden_policy();
        let first = render_policy_profile(&policy, Some(Path::new(GOLDEN_TMPDIR)));
        let second = render_policy_profile(&policy, Some(Path::new(GOLDEN_TMPDIR)));
        assert_eq!(first, second);
    }

    /// The macOS spellings the kernel actually opens must be granted: `/private/tmp` and
    /// `/private/var/tmp` for the temporary directories, `/private/etc` for the built-in
    /// read-only list, and the `TMPDIR` folder below `/private/var/folders` — and an operator
    /// extra has to be granted verbatim. Device nodes are single files and are filtered with
    /// `literal`, directories with `subpath`: a `literal` filter on a directory matches the
    /// directory entry alone and grants nothing below it.
    #[test]
    fn test_private_spellings_and_filter_kinds() {
        let workspace = Path::new("/policy-surface/workspace-root");
        let extra = "/policy-surface/operator-extra".to_string();
        let profile = render_policy_profile(
            &policy_paths(workspace, std::slice::from_ref(&extra)),
            Some(Path::new(GOLDEN_TMPDIR)),
        );

        for granted in [
            "(subpath \"/private/tmp\")".to_string(),
            "(subpath \"/private/var/tmp\")".to_string(),
            "(subpath \"/private/etc\")".to_string(),
            format!("(subpath {})", quote(GOLDEN_TMPDIR)),
            "(subpath \"/policy-surface/operator-extra\")".to_string(),
        ] {
            assert!(
                profile.contains(&granted),
                "the profile must grant {granted}: {profile}"
            );
        }

        for device in [
            "/dev/null",
            "/dev/zero",
            "/dev/full",
            "/dev/tty",
            "/dev/urandom",
            "/dev/random",
        ] {
            assert!(
                profile.contains(&format!("(literal \"{device}\")")),
                "{device} must be granted as the exact path it is, not as a subpath"
            );
        }

        for directory in ["/dev/pts", "/dev/shm", "/usr", "/private/etc"] {
            assert!(
                profile.contains(&format!("(subpath \"{directory}\")")),
                "{directory} must be granted as a subpath"
            );
            assert!(
                !profile.contains(&format!("(literal \"{directory}\")")),
                "{directory} must never be granted as a single file"
            );
        }
    }

    /// The grant polarity of the shared policy list survives the rendering: a read-write entry
    /// gets both `file-read*` and `file-write*`, a read-only entry only `file-read*`, so no
    /// profile can widen what an operator extra is allowed to do. An unset `TMPDIR` still
    /// grants the folder root writable, because that is where a confined build writes.
    #[test]
    fn test_grants_render_their_polarity() {
        let policy = vec![
            entry("/srv/writable", Grant::ReadWrite),
            entry("/srv/read-only", Grant::ReadOnly),
        ];
        let profile = render_policy_profile(&policy, None);

        assert!(profile.contains("(allow file-read* (subpath \"/srv/writable\"))"));
        assert!(profile.contains("(allow file-write* (subpath \"/srv/writable\"))"));
        assert!(profile.contains("(allow file-read* (subpath \"/srv/read-only\"))"));
        assert!(
            !profile.contains("(allow file-write* (subpath \"/srv/read-only\"))"),
            "a read-only entry must never gain a write clause: {profile}"
        );
        assert!(
            profile.contains("(allow file-write* (subpath \"/private/var/folders\"))"),
            "an unset TMPDIR must still grant the folder root writable: {profile}"
        );
    }

    /// A path with a space, an embedded quote and a backslash survives quoting: the SBPL lexer
    /// sees one string literal and the clause around it stays well-formed.
    #[test]
    fn test_odd_paths_are_escaped() {
        let odd = "/policy-surface/odd \"name\" dir\\x";
        let profile = render_policy_profile(&[entry(odd, Grant::ReadOnly)], None);
        assert!(
            profile.contains(
                "(allow file-read* (subpath \"/policy-surface/odd \\\"name\\\" dir\\\\x\"))"
            ),
            "quotes and backslashes must be escaped for the SBPL lexer: {profile}"
        );

        assert_eq!(
            quote_path(Path::new("/a\"b\\c")),
            "\"/a\\\"b\\\\c\"",
            "the lexer's own metacharacters are the only ones escaped"
        );
        // Spaces, `$`, `(` and `)` are ordinary characters inside a quoted literal.
        assert_eq!(quote_path(Path::new("/tmp/$x (y)")), "\"/tmp/$x (y)\"");
    }

    /// The public entry composes the shared policy list with the running process' own `TMPDIR`:
    /// every policy path is rendered, and the temporary folder is granted whatever spelling the
    /// environment states (falling back to the folder root when `TMPDIR` is unset).
    #[test]
    fn test_render_profile_entry_grants_the_policy_and_tmpdir() {
        let workspace = Path::new("/policy-surface/workspace-root");
        let profile = render_profile(workspace, &[]);

        assert!(
            profile.contains("(allow file-read* (subpath \"/policy-surface/workspace-root\"))")
        );
        assert!(
            profile.contains("(allow file-write* (subpath \"/policy-surface/workspace-root\"))")
        );

        let granted = tmpdir_from_env().map_or_else(
            || "(subpath \"/private/var/folders\")".to_string(),
            |dir| format!("(subpath {})", quote(dir.to_string_lossy().as_ref())),
        );
        assert!(
            profile.contains(&granted),
            "the process TMPDIR must be granted, as {granted}: {profile}"
        );
    }
}
