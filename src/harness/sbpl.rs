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
//! the string to libSystem.
//!
//! What the emitter adds on top of the shared policy list, and why:
//!
//! * **Path translations.** SBPL filters match **literal path strings**, not resolved file
//!   descriptors — the exact opposite of Landlock's `PathFd` rules. `/tmp`, `/var` and `/etc`
//!   are symlinks into `/private`, so every policy entry below those prefixes is emitted under
//!   **both** spellings: a rule on the link spelling alone misses the path the kernel opens.
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
//! * **The proven shell floor.** Beyond the shared policy list the emitter also grants the
//!   clauses a known-working reference profile (the `profiles/default.pb` of the upstream
//!   `painless-belt` project) runs its whole tool set on: bare
//!   `file-read-metadata`, the root directory and `$HOME/.CFUserTextEncoding` as `literal`
//!   filters, the real `/private` tree read-only, `$HOME/Library/Caches` and
//!   `$HOME/Library/Keychains` read-write, the `/dev/ttys…` pty range and every `$PATH`
//!   directory. Those clauses are emitted from the macOS-only tables, never from
//!   [`policy_paths`], so the Landlock ruleset is untouched.
//!
//! What the emitter does **not** do:
//!
//! * It never opens a path. SBPL matches text, so an entry that does not exist on the host
//!   (`/lib64`, `/nix/store`, `~/.npm`, an absent Xcode) never matches and grants nothing —
//!   which is also why [`PolicyPath::required`](crate::harness::sandbox::PolicyPath) has no
//!   effect on the rendered text.
//! * The only explicit `deny` clauses are the ones for the sensitive per-user homes
//!   (`~/.ssh` and `~/.gnupg`, [`SENSITIVE_DENIED_BASENAMES`]). `(deny default)` already
//!   denies them, so the clauses are redundant on purpose: they make the denial log-visible,
//!   they name the paths the enforcement probe of `harness::sandbox_seatbelt` asks libSystem
//!   about, and they are emitted after every allow clause, so an operator grant of `$HOME`
//!   cannot shadow them.

use crate::harness::sandbox::{Grant, PolicyPath, policy_paths};
use std::collections::BTreeSet;
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
/// `ld`/`libtool` that `rustc` execs in its link steps, the real root of the `/tmp`, `/var`
/// and `/etc` symlink targets, and Xcode when it is installed (gotcha 6). A rule on a root
/// the host does not have grants nothing. `/private` is read-only here: writes stay scoped to
/// the individual entries (`/private/tmp`, the `TMPDIR` folder), never to the whole tree.
const MACOS_READONLY_ADDITIONS: &[&str] = &[
    "/System",
    "/Library",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app",
    "/private",
];

/// Exact paths that have to be readable without granting the tree around them, filtered with
/// `literal` because a `subpath` on the root itself would grant the whole filesystem: `/` is
/// what `stat(2)` and every path-component resolution needs, and the per-user text-encoding
/// file is read at process startup by anything that touches `Foundation`. The
/// `$HOME`-relative entry below is joined onto the caller's home directory per host.
const MACOS_LITERAL_READONLY: &[&str] = &["/"];

/// Basenames below the home directory that are single files and readable, never writable —
/// the same knowledge [`is_single_file`] has for policy entries, stated here so this table
/// does not depend on that heuristic.
const MACOS_HOME_LITERAL_READONLY: &[&str] = &[".CFUserTextEncoding"];

/// The macOS counterpart of the Linux `~/.cache` half of the allow-list: the per-user cache
/// tree, plus the login-keychain directory every Apple framework and security tool opens at
/// startup. Joined onto the home directory and granted read-write, the polarity the policy
/// states for `~/.cache` and `~/.cargo`.
const MACOS_HOME_READWRITE: &[&str] = &["Library/Caches", "Library/Keychains"];

/// Device access a macOS terminal needs by `(operation, filter kind, path)` triples: the pty
/// character devices a real terminal is a controlling tty on are `/dev/ttys000`… — the policy
/// lists the fixed name `/dev/tty` only, which resolves to the pty and leaves the actual node
/// ungranted — while `/dev/fd` and the three standard-stream aliases are the per-fd views the
/// shell reopens. Kept to the individual nodes rather than whole-`/dev` read-write, so the
/// rest of `/dev` stays read-only as the policy states it.
const MACOS_DEVICE_EXTRA: &[(&str, &str, &str)] = &[
    ("file-write*", "prefix", "/dev/ttys"),
    ("file-read*", "literal", "/dev/fd"),
    ("file-read*", "literal", "/dev/stdin"),
    ("file-read*", "literal", "/dev/stdout"),
    ("file-read*", "literal", "/dev/stderr"),
];

/// The `$HOME`-relative directories that must never be readable by the confined command: the
/// SSH home and the GnuPG home. They are denied by name here rather than left to
/// `(deny default)` alone for one reason: the Seatbelt backend asks libSystem whether a read
/// of exactly these paths is denied, and that probe is only meaningful if the rendered profile
/// states the denial explicitly. The list is shared with that backend
/// (`harness::sandbox_seatbelt`), which joins the same basenames onto the same `$HOME`, so the
/// probe can never ask about a path the profile does not deny.
pub const SENSITIVE_DENIED_BASENAMES: &[&str] = &[".ssh", ".gnupg"];

/// The environment key holding the executable search path.
pub const PATH_ENV: &str = "PATH";

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
    ";     The rest of the section is the floor the known-working reference profiles run on:",
    ";     the root directory, `$HOME/.CFUserTextEncoding`, the private tree read-only, the",
    ";     per-user caches and keychain directory, the pty nodes behind `/dev/tty` and every",
    ";     directory of `$PATH`.",
];

/// Section-local comment for the explicit denials at the end of the file-grant section, so a
/// profile on disk explains itself.
const SECTION_DENY_COMMENT: &[&str] = &[
    "; Explicit denial of the SSH and GnuPG homes. (deny default) already covers them, so these",
    "; clauses are redundant by design: they make the denial log-visible and they outlast any",
    "; grant of $HOME higher up in this profile. They are also the paths the enforcement probe",
    "; of harness::sandbox_seatbelt asks libSystem about, which is why they are spelled here.",
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
/// Chromium's `sandbox/policy/mac/common.sb`; tightening them to per-name filters
/// (`sysctl-name`, `global-name`, `remote tcp`) is the on-host tuning problem that
/// remains the only real open question for this backend.
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
    // Bare `stat`/lstat access: without it, a process cannot even resolve or test a path in a
    // directory it is allowed to read, which is why the proven reference profiles grant it
    // unfiltered rather than per tree.
    "(allow file-read-metadata)",
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
    render_policy_profile(
        &policy,
        tmpdir_from_env().as_deref(),
        home_from_env().as_deref(),
        &path_dirs(),
    )
}

/// The value of the `HOME` key as a path, or `None` when it is unset or empty.
///
/// [`policy_paths`] reads the same key for its `$HOME`-relative entries; the macOS-only tables
/// join onto it per host, which is why the pure renderer takes it as an argument instead of
/// reading the environment itself.
#[must_use]
pub fn home_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The value of [`TMPDIR_ENV`] as a path, or `None` when it is unset or empty.
#[must_use]
pub fn tmpdir_from_env() -> Option<PathBuf> {
    std::env::var_os(TMPDIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The directories of [`PATH_ENV`], as an absolute, deduplicated and lexicographically ordered
/// list: the confined process resolves its executables through these, so the profile has to
/// grant them whatever the host's toolchain layout. Relative and empty entries are dropped —
/// a relative entry would grant a directory the policy never named, and SBPL matches text.
#[must_use]
pub fn path_dirs() -> Vec<PathBuf> {
    path_dirs_from_os(std::env::var_os(PATH_ENV).as_deref())
}

/// The pure half of [`path_dirs`]: the same list from one raw search-path value, so the
/// de-duplication, the absolute-only rule and the ordering are testable without touching the
/// environment.
#[must_use]
fn path_dirs_from_os(raw: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    if let Some(raw) = raw {
        dirs.extend(std::env::split_paths(&raw.to_os_string()).filter(|dir| dir.is_absolute()));
    }
    dirs.into_iter().collect()
}

/// The pure half of [`render_profile`]: render `policy` in list order with the grant each
/// entry states, plus the read-write grant for `tmpdir`, falling back to [`TMPDIR_FALLBACK`]
/// when `tmpdir` is `None`, the `$HOME`-relative macOS tables joined onto `home`, and one
/// read-only clause per `path_dirs` directory. Everything the renderer needs is an argument, so
/// the emitted text is testable without a filesystem, an environment or a Mac.
#[must_use]
pub fn render_policy_profile(
    policy: &[PolicyPath],
    tmpdir: Option<&Path>,
    home: Option<&Path>,
    path_dirs: &[PathBuf],
) -> String {
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
    // The clauses the known-working reference profiles run on and the policy list has no entry
    // for. Each table states its own filter kind, because `is_single_file` decides the shape of
    // a *policy* entry from what the policy knows about it and can say nothing about a table
    // entry or about a directory the caller's `$HOME` names.
    lines.extend(macos_floor_rules(home, path_dirs));
    // The denials are emitted last in this section, after every allow clause that could
    // otherwise cover the same paths (an operator-granted `$HOME` lands in section 1).
    lines.push(String::new());
    lines.extend(SECTION_DENY_COMMENT.iter().map(|line| (*line).to_string()));
    lines.extend(sensitive_deny_rules(home));

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

/// The clauses of the proven shell floor that the shared policy list has no entry for: each
/// table emitted with the filter kind and the operation that table states, joined onto `home`
/// where the entry is `$HOME`-relative and emitted verbatim where it is not, plus the read-only
/// grant for every directory of `path_dirs`. None of these is spelled through
/// [`macos_spellings`]: they are real paths on a macOS host, and `/private` is granted in its
/// own name by [`MACOS_READONLY_ADDITIONS`].
#[must_use]
fn macos_floor_rules(home: Option<&Path>, path_dirs: &[PathBuf]) -> Vec<String> {
    let mut rules = Vec::new();

    for path in MACOS_LITERAL_READONLY {
        rules.push(format!("(allow file-read* (literal {}))", quote(path)));
    }

    if let Some(home) = home {
        for name in MACOS_HOME_LITERAL_READONLY {
            rules.push(format!(
                "(allow file-read* (literal {}))",
                quote_path(&home.join(name)),
            ));
        }
        for name in MACOS_HOME_READWRITE {
            let granted = quote_path(&home.join(name));
            rules.push(format!("(allow file-read* (subpath {granted}))"));
            rules.push(format!("(allow file-write* (subpath {granted}))"));
        }
    }

    for (operation, kind, path) in MACOS_DEVICE_EXTRA {
        rules.push(format!("(allow {operation} ({kind} {}))", quote(path),));
    }

    rules.extend(render_path_dirs(path_dirs));
    rules
}

/// The explicit denials of [`SENSITIVE_DENIED_BASENAMES`], joined onto `home` and filtered with
/// `subpath` so the denial covers everything below each directory. Nothing is emitted when
/// `home` is `None`: the emitter never invents a home path, and the shared policy list emits
/// its `$HOME` entries under the same condition.
#[must_use]
fn sensitive_deny_rules(home: Option<&Path>) -> Vec<String> {
    let Some(home) = home else {
        return Vec::new();
    };
    SENSITIVE_DENIED_BASENAMES
        .iter()
        .map(|base| {
            format!(
                "(deny file-read* (subpath {}))",
                quote_path(&home.join(base)),
            )
        })
        .collect()
}

/// One read-only clause per `$PATH` directory: the confined process looks its executables up by
/// name, so every directory it can reach by that name has to be readable (`/usr/bin`,
/// `/opt/homebrew/bin`, `$HOME/.cargo/bin`). A directory the policy already granted is granted
/// twice, which SBPL tolerates.
#[must_use]
fn render_path_dirs(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter()
        .map(|dir| format!("(allow file-read* (subpath {}))", quote_path(dir.as_path()),))
        .collect()
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

    /// A fixed fake home directory and a fixed fake `$PATH`, so the `$HOME`-relative tables and
    /// the `$PATH` grant are pinned by the golden text without reading the host environment.
    const GOLDEN_HOME: &str = "/Users/goldentester";
    const GOLDEN_PATH_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/bin", "/usr/local/bin"];

    fn golden_path_dirs() -> Vec<PathBuf> {
        GOLDEN_PATH_DIRS.map(PathBuf::from).to_vec()
    }

    /// The whole rendered profile, asserted as one exact string: this text is what is handed
    /// to `sandbox_init_with_parameters`, so a lost or a stray clause is a policy change.
    #[test]
    fn test_rendered_profile_is_the_golden_text() {
        let profile = render_policy_profile(
            &golden_policy(),
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
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
;     The rest of the section is the floor the known-working reference profiles run on:
;     the root directory, `$HOME/.CFUserTextEncoding`, the private tree read-only, the
;     per-user caches and keychain directory, the pty nodes behind `/dev/tty` and every
;     directory of `$PATH`.
(allow file-read* (subpath "/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-write* (subpath "/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-read* (subpath "/private/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-write* (subpath "/private/var/folders/zz/zyxvpxvqnvq_cas/0000000000/T"))
(allow file-read* (subpath "/System"))
(allow file-read* (subpath "/Library"))
(allow file-read* (subpath "/Library/Developer/CommandLineTools"))
(allow file-read* (subpath "/Applications/Xcode.app"))
(allow file-read* (subpath "/private"))
(allow file-read* (literal "/"))
(allow file-read* (literal "/Users/goldentester/.CFUserTextEncoding"))
(allow file-read* (subpath "/Users/goldentester/Library/Caches"))
(allow file-write* (subpath "/Users/goldentester/Library/Caches"))
(allow file-read* (subpath "/Users/goldentester/Library/Keychains"))
(allow file-write* (subpath "/Users/goldentester/Library/Keychains"))
(allow file-write* (prefix "/dev/ttys"))
(allow file-read* (literal "/dev/fd"))
(allow file-read* (literal "/dev/stdin"))
(allow file-read* (literal "/dev/stdout"))
(allow file-read* (literal "/dev/stderr"))
(allow file-read* (subpath "/opt/homebrew/bin"))
(allow file-read* (subpath "/usr/bin"))
(allow file-read* (subpath "/usr/local/bin"))

; Explicit denial of the SSH and GnuPG homes. (deny default) already covers them, so these
; clauses are redundant by design: they make the denial log-visible and they outlast any
; grant of $HOME higher up in this profile. They are also the paths the enforcement probe
; of harness::sandbox_seatbelt asks libSystem about, which is why they are spelled here.
(deny file-read* (subpath "/Users/goldentester/.ssh"))
(deny file-read* (subpath "/Users/goldentester/.gnupg"))

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
(allow file-read-metadata)
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
        let first = render_policy_profile(
            &policy,
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
        let second = render_policy_profile(
            &policy,
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
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
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
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
        let profile = render_policy_profile(&policy, None, None, &[]);

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
        let profile = render_policy_profile(&[entry(odd, Grant::ReadOnly)], None, None, &[]);
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

    /// The whole rendered profile carries the clause set the known-working reference profiles
    /// run on: every one of their base clauses must be present, otherwise a lost grant reads as
    /// a working profile that dies on the first `stat`, `ls` or compiler link step.
    #[test]
    fn test_profile_carries_the_known_working_reference_floor() {
        let profile = render_policy_profile(
            &golden_policy(),
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
        for clause in [
            "(allow file-read-metadata)",
            "(allow file-read* (literal \"/\"))",
            "(allow file-read* (subpath \"/System\"))",
            "(allow file-read* (subpath \"/private\"))",
            "(allow file-read* (subpath \"/Library\"))",
            "(allow file-read* (subpath \"/usr\"))",
            "(allow file-write* (subpath \"/private/tmp\"))",
            "(allow file-map-executable)",
            "(allow mach-lookup)",
            "(allow sysctl-read)",
            "(allow ipc-posix-shm)",
            "(allow signal (target self))",
            "(allow network-outbound",
            "(allow process-exec",
            "process-fork",
            "(allow system-socket)",
        ] {
            assert!(
                profile.contains(clause),
                "the profile must contain the floor clause {clause}: {profile}"
            );
        }
    }

    /// The root directory and the per-user text-encoding file are readable without granting a
    /// tree: `subpath "/"` would grant every path on the volume and `subpath` on a plain file
    /// grants nothing below it, so both are filtered `literal`.
    #[test]
    fn test_root_and_text_encoding_are_literal_not_subpath() {
        let profile = render_policy_profile(
            &golden_policy(),
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
        assert!(
            profile.contains("(allow file-read* (literal \"/\"))"),
            "the root directory must be granted as the exact path: {profile}"
        );
        assert!(
            !profile.contains("(subpath \"/\""),
            "no clause may grant the whole filesystem as a subpath: {profile}"
        );
        assert!(
            !profile.contains("(allow file-write* (subpath \"/\"))"),
            "the root directory must never become writable: {profile}"
        );
        assert!(
            profile.contains(
                "(allow file-read* (literal \"/Users/goldentester/.CFUserTextEncoding\"))"
            ),
            "the per-user text-encoding file must be granted as the exact file: {profile}"
        );
    }

    /// The macOS cache and keychain directories are the counterpart of the Linux `~/.cache`
    /// entry, and carry the same read-write polarity: both operations, filtered `subpath`, joined
    /// onto the home directory the caller supplies.
    #[test]
    fn test_macos_home_caches_and_keychains_are_read_write() {
        let profile = render_policy_profile(
            &golden_policy(),
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
        for dir in ["Library/Caches", "Library/Keychains"] {
            let granted = format!("(subpath \"/Users/goldentester/{dir}\")");
            assert!(
                profile.contains(&format!("(allow file-read* {granted})")),
                "{dir} must be granted read access: {profile}"
            );
            assert!(
                profile.contains(&format!("(allow file-write* {granted})")),
                "{dir} must be granted write access: {profile}"
            );
        }

        // Without a home directory the two tables simply grant nothing, rather than emitting an
        // empty path or a literal `$HOME`.
        let without_home = render_policy_profile(&golden_policy(), None, None, &[]);
        assert!(
            !without_home.contains("goldentester"),
            "no host-specific clause may be emitted for an unset home: {without_home}"
        );
    }

    /// The pty nodes a real terminal is built on and the standard-stream aliases have to be
    /// granted: the policy names `/dev/tty` alone, which is the symlink, not the node a shell
    /// writes its job control through.
    #[test]
    fn test_pty_range_and_standard_streams_are_granted() {
        let profile = render_policy_profile(
            &golden_policy(),
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );
        assert!(
            profile.contains("(allow file-write* (prefix \"/dev/ttys\"))"),
            "the pty character devices macOS actually use must be writable: {profile}"
        );
        for node in ["/dev/fd", "/dev/stdin", "/dev/stdout", "/dev/stderr"] {
            assert!(
                profile.contains(&format!("(allow file-read* (literal \"{node}\"))")),
                "{node} must be readable by that name: {profile}"
            );
        }
        // The rest of `/dev` stays read-only: the pty range is the only write grant beyond the
        // named device nodes of the policy.
        assert!(
            !profile.contains("(allow file-write* (subpath \"/dev\"))"),
            "whole `/dev` must never become writable: {profile}"
        );
    }

    /// The enforcement probe of the Seatbelt backend asks libSystem whether a read of the SSH
    /// and GnuPG homes is denied, and that question is only meaningful if the rendered profile
    /// states the denial. `(deny default)` alone is not enough: an operator extra of `$HOME`
    /// (`sandbox_extra_readonly_paths`) would otherwise grant the very path the probe asks
    /// about, and a working profile would read as a broken one.
    #[test]
    fn test_sensitive_homes_are_denied_by_name_and_come_last() {
        let profile = render_policy_profile(
            &golden_policy(),
            Some(Path::new(GOLDEN_TMPDIR)),
            Some(Path::new(GOLDEN_HOME)),
            &golden_path_dirs(),
        );

        for name in SENSITIVE_DENIED_BASENAMES {
            let granted = format!("{GOLDEN_HOME}/{name}");
            let clause = format!("(deny file-read* (subpath \"{granted}\"))");
            assert!(
                profile.contains(&clause),
                "{name} must be denied by its real path, joined onto the caller's home: {profile}"
            );
            assert!(
                !profile.contains(&format!("(allow file-read* (subpath \"{granted}\")")),
                "nothing may grant a sensitive home as an exact subpath: {profile}"
            );
        }

        // Placement is part of the guarantee: the denial is the last file clause of the profile,
        // so it outranks every allow that could cover the same path.
        let first_allow = profile.find("(allow").expect("allow clauses exist");
        let last_deny = profile
            .rfind("(deny file-read*")
            .expect("the sensitive denials exist");
        assert!(
            last_deny > first_allow,
            "the explicit denials must be emitted after every grant: {profile}"
        );
        assert_eq!(
            profile.matches("(deny file-read*").count(),
            SENSITIVE_DENIED_BASENAMES.len(),
            "one denial per sensitive directory, and no stray deny clause"
        );

        // An unset home emits nothing home-specific, rather than inventing a path.
        let without_home = render_policy_profile(&golden_policy(), None, None, &[]);
        assert!(
            !without_home.contains("(deny file-read*"),
            "no denial may be emitted for an unset HOME: {without_home}"
        );
    }

    /// Every directory of the search path is granted read-only, deduplicated, absolute-only and
    /// in a stable order, so the emitted clauses never depend on the order the host's `PATH`
    /// happens to state.
    #[test]
    fn test_path_dirs_are_read_only_deduplicated_and_sorted() {
        let raw = std::ffi::OsString::from(
            "/usr/local/bin:/usr/bin:/opt/homebrew/bin:/usr/bin::.:/tmp/tools",
        );
        let dirs = path_dirs_from_os(Some(raw.as_os_str()));
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/tmp/tools"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/usr/local/bin"),
            ],
            "the entries must be absolute, deduplicated and ordered, and a relative entry grants nothing",
        );

        let profile =
            render_policy_profile(&golden_policy(), None, Some(Path::new(GOLDEN_HOME)), &dirs);
        for dir in [
            "/opt/homebrew/bin",
            "/tmp/tools",
            "/usr/bin",
            "/usr/local/bin",
        ] {
            assert!(
                profile.contains(&format!("(allow file-read* (subpath \"{dir}\"))")),
                "{dir} must be granted read-only as a subpath: {profile}"
            );
            assert!(
                !profile.contains(&format!("(allow file-write* (subpath \"{dir}\"))")),
                "a search path may never become writable: {profile}"
            );
        }
        // Nothing is emitted for an unset or empty search path.
        assert_eq!(path_dirs_from_os(None), Vec::<PathBuf>::new());
        assert_eq!(
            path_dirs_from_os(Some(std::ffi::OsStr::new(""))),
            Vec::<PathBuf>::new(),
            "an empty search path grants nothing"
        );
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

        // The floor clauses come from the environment too: the bare metadata grant, the root
        // directory, the pty range, and whatever the process' own `$HOME` and `$PATH` state.
        for clause in [
            "(allow file-read-metadata)",
            "(allow file-read* (literal \"/\"))",
            "(allow file-write* (prefix \"/dev/ttys\"))",
            "(allow file-read* (subpath \"/private\"))",
        ] {
            assert!(
                profile.contains(clause),
                "the rendered profile must contain {clause}: {profile}"
            );
        }
        if let Some(home) = home_from_env() {
            let caches = quote_path(&home.join("Library/Caches"));
            assert!(
                profile.contains(&format!("(allow file-read* (subpath {caches}))"))
                    && profile.contains(&format!("(allow file-write* (subpath {caches}))")),
                "the per-user cache directory must be granted read-write as {caches}: {profile}"
            );
        }
        for dir in path_dirs() {
            let granted = quote_path(&dir);
            assert!(
                profile.contains(&format!("(allow file-read* (subpath {granted}))")),
                "every directory of `$PATH` must be granted read-only, {granted} missing: \
                 {profile}"
            );
        }
    }
}
