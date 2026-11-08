//! Windows sandbox backend: the AppContainer profile and DACL policy layer.
//!
//! Windows has no equivalent of `landlock_restrict_self(2)` or `sandbox_init(3)`: nothing takes
//! the **already running** process and narrows its access token in place. Every Windows
//! privilege-reduction path either re-execs the process (the re-entry protocol modelled by
//! [`crate::harness::sandbox`]) or confines a process that does not exist yet. AppContainer is
//! the second, and the confinement is therefore not a property of this process: it is a
//! property of the child that this process launches.
//!
//! Confinement is declared at launch time, on the process about to be created:
//!
//! * `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` (attached through
//!   `UpdateProcThreadAttribute`) puts a `SECURITY_CAPABILITIES` block naming an AppContainer
//!   SID, and the capabilities that AppContainer permits, onto the new process.
//! * The new process, and everything below it, then runs inside that AppContainer. It is a
//!   *non-inherited* security principal: it sees only what an **explicit** grant lets it see.
//!
//! So confinement is established by the re-entry child launching the command inside a **fresh
//! AppContainer profile** created for this run, whose SID the policy below grants rights to.
//! That is also why the policy here is expressed as ACEs on concrete paths rather than as a
//! ruleset: an AppContainer's view of the filesystem is whatever the DACL of each object says,
//! and only the ACEs naming its SID (or the built-in application-package groups) can allow it.
//!
//! What this module provides:
//!
//! 1. [`AppContainerProfileGuard`] — a fresh, ephemeral AppContainer profile per run, named
//!    deterministically from the workspace root plus a random component, holding the profile
//!    name and its SID, and deleting the profile when the run ends.
//! 2. [`CapabilitySids`] — the capability list (`internetClient`, `S-1-15-3-1`) built from the
//!    app-package authority, as the `SID_AND_ATTRIBUTES` records that `SECURITY_CAPABILITIES`
//!    block needs.
//! 3. [`windows_policy_entries`] — the allow-list rendered for the Windows object model, using
//!    the same `PolicyPath` entries (read-write / read-only / required) and the same skip
//!    reasons the shared policy layer publishes.
//! 4. [`AclGrantGuard`] — apply exactly one grant ACE for the AppContainer SID on one path,
//!    and put the saved DACL back when the run ends.
//! 5. [`attempt_windows_sandbox`] — the attempt surface wired to "profile created, required
//!    grants applied, and one confined probe process really launched inside the profile", mapped
//!    onto the fail-closed `SandboxAttempt` contract.
//! 6. [`AttributeListGuard`] and [`ChildHandles`] — the `PROC_THREAD_ATTRIBUTE_LIST` that carries
//!    the confinement declaration, and the process and thread handles of the child, released on
//!    every exit path including a `?` return.
//! 7. [`spawn_in_appcontainer`] — the launch itself: `<comspec> /C <command>` confined in the
//!    prepared profile, waited for, with the child's exit code as the answer.
//! 8. [`run_in_appcontainer`] — the composite entry point of the re-entry child: prepare, probe,
//!    launch the command, return its exit code, and only then release the guards.
//!
//! One Windows-specific wrinkle shapes the read-only half of the policy. An unprivileged process
//! cannot just add an ACE to `C:\Windows` or `C:\Program Files`: those objects need `WRITE_DAC`,
//! which a non-elevated token does not hold, and on a normal install they already carry
//! read-only ACEs for the built-in `S-1-15-2-2` ("ALL APPLICATION PACKAGES") and `S-1-15-2-3`
//! ("ALL RESTRICTED APPLICATION PACKAGES") groups — groups every AppContainer process belongs
//! to. A read-only entry is therefore first **verified** against the DACL that exists
//! ([`AclCoverage`]) and only mutated when the grant really is missing; a mutation refused with
//! `ERROR_ACCESS_DENIED` or `ERROR_PRIVILEGE_NOT_HELD` is a skip note plus a warning for a
//! non-required entry, and a rule failure only for a required one — and exactly one entry is ever
//! required: the workspace root. The temporary directories are the read-write entries that are
//! **not** required, so a refusal there costs one write target and warns, it does not fail the run.
//!
//! The module comes in two layers, and they meet at one hand-off. The policy layer creates the
//! profile and puts the ACEs on the objects; the launch layer hands that profile to
//! `CreateProcessW`, runs one confined probe process, and keeps every guard alive until the child
//! has exited. The hand-off is [`attempt_windows_sandbox_with_setup`], which returns the attempt
//! *and* the [`WindowsPolicySetup`] it built, and [`take_windows_policy_setup`], which is how the
//! composite entry point [`run_in_appcontainer`] picks that setup up again without preparing twice.
//!
//! [`AppContainerProfileGuard`]: self::AppContainerProfileGuard
//! [`CapabilitySids`]: self::CapabilitySids
//! [`AclCoverage`]: self::AclCoverage

use crate::harness::sandbox::{
    self as sandbox, ExtraPathSkip, Grant, PolicyPath, SandboxAttempt, SandboxError,
};
use crate::harness::sbpl;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `ACCESS_ALLOWED_ACE_TYPE` spelled as the `u8` the ACE header field needs: the constant in
/// `windows_sys::Win32::System::SystemServices` is typed `u32` and lives in a module this crate
/// does not enable.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// The failure surface of the Windows backend, before it is mapped onto [`SandboxError`].
///
/// Each variant names the stage that failed, so [`attempt_windows_sandbox`] can map it onto the
/// right `SandboxAttempt` variant instead of guessing from a string. Every variant carries a
/// non-empty reason, because the reason is what the operator is shown.
///
/// [`attempt_windows_sandbox`]: self::attempt_windows_sandbox
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WindowsSandboxError {
    /// The AppContainer profile for this run could not be created: no confinement identity
    /// exists, so this is an availability failure.
    #[error("AppContainer profile {name} could not be created: {reason}")]
    Profile {
        name: String,
        reason: String,
        /// The `HRESULT` (or Win32 code) the failing `CreateAppContainerProfile` answered with,
        /// spelled out in the reason line as hex **and** decimal. `0` where the failure was
        /// decided locally — an invalid generated name — rather than by a call.
        code: WIN32_ERROR,
    },
    /// A SID could not be allocated, converted or released, for the same reason: there is no
    /// AppContainer identity to build a policy around.
    #[error("the AppContainer security identifier could not be handled: {reason}")]
    Sid {
        reason: String,
        /// The Win32 error the failing SID call reported, `0` where the failure was decided
        /// locally (too many sub-authorities) rather than by a call.
        code: WIN32_ERROR,
    },
    /// The policy names an entry it cannot grant: the workspace root is itself inside (or is)
    /// one of the directories the policy denies, so no honest policy exists for this run.
    #[error("sandbox policy could not be built for {path}: {reason}")]
    RuleSetup { path: String, reason: String },
    /// The DACL of `path` could not be read, so the policy for that path cannot be verified.
    #[error("the DACL of {path} could not be read: {reason}")]
    AclRead {
        path: String,
        reason: String,
        /// The Win32 error the read answered with (`ERROR_ACCESS_DENIED`,
        /// `ERROR_NO_SECURITY_ON_OBJECT`, …). It is carried here as well as in the prose, so the
        /// structured log field and the operator text cannot disagree about which code it was.
        code: WIN32_ERROR,
    },
    /// The DACL of `path` could not be changed. `code` is the Win32 error, and
    /// [`WindowsSandboxError::is_access_refusal`] says whether it is the expected
    /// "this process may not rewrite that DACL" outcome rather than a policy error.
    #[error("the DACL of {path} could not be changed: {reason} (win32 error {code})")]
    AclMutate {
        path: String,
        reason: String,
        code: WIN32_ERROR,
    },
    /// An ACE was applied, but re-reading the object shows the confinement the policy promises
    /// is not there: the rule exists, the object says otherwise.
    #[error("the DACL of {path} does not show the grant this policy applied: {reason}")]
    AclVerification { path: String, reason: String },
    /// The `PROC_THREAD_ATTRIBUTE_LIST` for the launch could not be sized, allocated or filled:
    /// the confinement is declared correctly, but it cannot be handed to `CreateProcessW`.
    #[error("the launch attribute list could not be built: {reason}")]
    AttributeList {
        reason: String,
        /// The Win32 error the sizing or filling call reported (`ERROR_BAD_LENGTH` or
        /// `ERROR_INVALID_PARAMETER`). `0` where the call reported none.
        code: WIN32_ERROR,
    },
    /// The confined process could not be created, or the wait for it and the read of its exit code
    /// failed. `code` is the Win32 error as `GetLastError` reported it (0 where no Win32 error is
    /// involved), which is what renders the failure through
    /// `io::Error::from_raw_os_error` rather than through a guess.
    #[error("the confined process could not be launched: {reason}")]
    Launch { reason: String, code: WIN32_ERROR },
    /// The confined probe process was created, but what it reported says the confinement is not
    /// real: a crash status instead of a command failure, or anything other than the exit code the
    /// probe itself asks for.
    #[error("the confined probe process did not confirm the confinement: {reason}")]
    Probe {
        reason: String,
        /// The status the confined probe answered with — an `NTSTATUS` crash status, or the exit
        /// code it returned — or, for a probe that never started, the Win32 error the launch
        /// reported. It is the number that tells "the confinement killed the child" from "a
        /// command inside the confinement failed", so it is carried structurally and not only in
        /// the prose, which already names it (and is why the verdict text does not repeat it).
        code: WIN32_ERROR,
    },
}

impl WindowsSandboxError {
    /// Whether the failure is the expected "this process may not read or rewrite this DACL" case:
    /// `ERROR_ACCESS_DENIED` (no `READ_CONTROL`/`WRITE_DAC` on the object) or
    /// `ERROR_PRIVILEGE_NOT_HELD`.
    ///
    /// Both halves of a grant are covered, because both are refused by the same token limitation:
    /// the DACL **read** asks for `READ_CONTROL`, and the DACL **write** for `WRITE_DAC` — through
    /// the handle [`open_security_handle`] opens, which refuses the open itself before
    /// `SetSecurityInfo` is ever reached, so the write half arrives as `AclMutate` from either
    /// call.
    ///
    /// This is what separates a skip note from a hard rule failure, and what makes the hard rule
    /// failure say *why* (t-034 wired it into `required_grant_refusal`, where a genuine refusal on
    /// the required workspace-root grant is reported as an actionable `WRITE_DAC`/`icacls` verdict
    /// instead of as the catch-all rule failure it used to be). It stays an exact match on those
    /// two codes rather than a heuristic over the reason text.
    ///
    /// [`open_security_handle`]: self::open_security_handle
    #[must_use]
    pub fn is_access_refusal(&self) -> bool {
        let code = match self {
            WindowsSandboxError::AclMutate { code, .. }
            | WindowsSandboxError::AclRead { code, .. } => *code,
            _ => return false,
        };
        code == ERROR_ACCESS_DENIED || code == ERROR_PRIVILEGE_NOT_HELD
    }

    /// The machine-readable class of this failure, named after the stage it happened in.
    ///
    /// This is the backend's own vocabulary (`SandboxError` has the backend-neutral one); the
    /// log record carries it as `kind`, because "the DACL write was refused" and "the probe did
    /// not confirm" both reach the operator as the same `rule-setup`/`enforcement` class and are
    /// not the same diagnosis.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            WindowsSandboxError::RuleSetup { .. } => "rule-setup",
            WindowsSandboxError::AclRead { .. } => "acl-read",
            WindowsSandboxError::AclMutate { .. } => "acl-mutate",
            WindowsSandboxError::AclVerification { .. } => "acl-verification",
            WindowsSandboxError::Profile { .. } => "profile",
            WindowsSandboxError::Sid { .. } => "sid",
            WindowsSandboxError::AttributeList { .. } => "attribute-list",
            WindowsSandboxError::Probe { .. } => "probe",
            WindowsSandboxError::Launch { .. } => "launch",
        }
    }

    /// The Win32 error this failure carries: the code a failing call reported (`GetLastError`), or
    /// the `HRESULT` of a call that answers in those terms, `0` when no Win32 call is behind it.
    ///
    /// Every variant that *is* the outcome of a call keeps that code, so the structured log field
    /// (`log_windows_sandbox_failure`) and the prose always name the same one. `AclRead` and
    /// `Profile` used to answer `0` here while their prose carried `5` and an `HRESULT`
    /// respectively — the asymmetry that made two ranks of a desktop refusal (t-030 §5-§6)
    /// indistinguishable in the log, and impossible to tell apart from the code field alone. A
    /// `Probe` failure used to answer `0` for the same reason: it reported the status the confined
    /// child answered with, which is the whole diagnosis for a probe rank, and dropped it on the
    /// way to the log record. The variants that stay at `0` are the ones decided before or between
    /// calls, where reporting a code would be a lie.
    #[must_use]
    pub fn win32_code(&self) -> WIN32_ERROR {
        match self {
            WindowsSandboxError::AclMutate { code, .. }
            | WindowsSandboxError::AclRead { code, .. }
            | WindowsSandboxError::Profile { code, .. }
            | WindowsSandboxError::Sid { code, .. }
            | WindowsSandboxError::AttributeList { code, .. }
            | WindowsSandboxError::Probe { code, .. }
            | WindowsSandboxError::Launch { code, .. } => *code,
            // No Win32 call failed behind these two: the rule failure is a policy judgement, and
            // the verification failure is a DACL that does not match the policy — the calls that
            // read it both answered `ERROR_SUCCESS`, which is why the object says no.
            WindowsSandboxError::RuleSetup { .. } | WindowsSandboxError::AclVerification { .. } => {
                0
            }
        }
    }

    /// The object this failure is about, empty when the failure names none.
    ///
    /// A refused `SetNamedSecurityInfoW` is only actionable together with the path it names, so
    /// the path is a log field in its own right (`path`), not only part of the prose.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            WindowsSandboxError::RuleSetup { path, .. }
            | WindowsSandboxError::AclRead { path, .. }
            | WindowsSandboxError::AclMutate { path, .. }
            | WindowsSandboxError::AclVerification { path, .. } => path,
            WindowsSandboxError::Profile { .. }
            | WindowsSandboxError::Sid { .. }
            | WindowsSandboxError::AttributeList { .. }
            | WindowsSandboxError::Probe { .. }
            | WindowsSandboxError::Launch { .. } => "",
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 bindings actually used by this module
// ---------------------------------------------------------------------------

use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_NO_SECURITY_ON_OBJECT,
    ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    STATUS_ACCESS_DENIED, STATUS_ACCESS_VIOLATION, STATUS_DLL_NOT_FOUND,
    STATUS_ENTRYPOINT_NOT_FOUND, STATUS_IN_PAGE_ERROR, STILL_ACTIVE, WAIT_OBJECT_0, WIN32_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    BuildTrusteeWithSidW, ConvertSidToStringSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SE_KERNEL_OBJECT,
    SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, AllocateAndInitializeSid,
    DACL_SECURITY_INFORMATION, EqualSid, FreeSid, GetAce, GetAclInformation, INHERIT_ONLY_ACE,
    NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SECURITY_APP_PACKAGE_AUTHORITY,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SID_IDENTIFIER_AUTHORITY,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, GetLongPathNameW, OPEN_EXISTING,
    READ_CONTROL, SYNCHRONIZE, WRITE_DAC,
};
// The launch half of the backend: the process-attribute list that carries the confinement
// declaration, `CreateProcessW` and the wait for the child, and the std-handle inspection that
// decides how the child is given its standard streams. `GetStdHandle` and `GetConsoleMode` need
// `Win32_System_Console`, which is a required feature exactly because the std handles of this
// process are how a confined child reaches the terminal it was started from.
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

// ---------------------------------------------------------------------------
// wide-string plumbing
// ---------------------------------------------------------------------------

/// A NUL-terminated UTF-16 buffer: the argument form every `*W` Win32 API accepts.
///
/// The buffer and the pointer that backs it are kept together on purpose: the dangling-`PCWStr`
/// family of bugs is what happens when they are not.
struct WideString {
    buf: Vec<u16>,
}

impl WideString {
    /// Encode a profile name, SID string or any other ASCII-ish string.
    fn new(value: &str) -> Self {
        let mut buf: Vec<u16> = value.encode_utf16().collect();
        buf.push(0);
        Self { buf }
    }

    /// Encode a path exactly as the Windows process environment holds it: a path may carry
    /// surrogate pairs, so `OsStrExt::encode_wide` is the only correct encoding.
    fn from_path(path: &Path) -> Self {
        let mut buf: Vec<u16> = path.as_os_str().encode_wide().collect();
        buf.push(0);
        Self { buf }
    }

    /// The `PCWStr` view of the buffer, valid while `self` lives.
    fn pcwstr(&self) -> *const u16 {
        self.buf.as_ptr()
    }
}

impl Default for WideString {
    fn default() -> Self {
        Self::new("")
    }
}

/// Read a NUL-terminated UTF-16 buffer that Win32 allocated and handed back (a `PWSTR`).
fn wide_cstr_to_string(raw: *const u16) -> String {
    if raw.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: `raw` points at a NUL-terminated buffer allocated by Win32; scanning forward for
    // the terminator is how the C side reads the same buffer, and `len` stops on the terminator.
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
    }
    // SAFETY: the `len` code units behind `raw` are the string proper, bounded by the terminator
    // located above.
    unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(raw, len)) }
}

/// The operator-facing spelling of a path: what an error message and a skip note name.
fn path_text(path: &Path) -> String {
    path.display().to_string()
}

/// The last Win32 error, as the pair a failure record needs: the code and the OS text for it.
///
/// The code is kept next to the text rather than folded into it, because the structured log field
/// and the prose must never be able to disagree about which code a failing call reported.
///
/// `windows_sys` 0.59 carries no error object, so `GetLastError` plus
/// `io::Error::from_raw_os_error` is the spelling this crate uses.
fn last_win32_error_detail() -> (WIN32_ERROR, String) {
    // SAFETY: GetLastError reads thread-local error state and takes no arguments.
    let code = unsafe { GetLastError() };
    (
        code,
        std::io::Error::from_raw_os_error(code as i32).to_string(),
    )
}

/// The last Win32 error as text.
fn last_win32_error() -> String {
    last_win32_error_detail().1
}

/// The `WIN32` facility of an `HRESULT`: the value that says "the code below is a Win32 error"
/// (`FACILITY_WIN32` in `winerror.h`), so `0x8007…` statuses can be traced back to the
/// `ERROR_ACCESS_DENIED` and friends the operator can look up.
const WIN32_FACILITY: WIN32_ERROR = 7;

/// The Win32 code an `HRESULT` wraps, when its facility is the Win32 facility.
///
/// `0x80070005` is severity `E`, facility `FACILITY_WIN32` (7) and code `ERROR_ACCESS_DENIED` (5):
/// a status in that family *is* a Win32 error, and the mapping has to be spelled out for the
/// operator who is handed both numbers. A status with another facility has no Win32 code to
/// report, and `None` says so rather than inventing one.
#[must_use]
const fn win32_code_of_hresult(code: WIN32_ERROR) -> Option<WIN32_ERROR> {
    if code & 0x8000_0000 != 0 && (code >> 16) & 0x1FFF == WIN32_FACILITY {
        Some(code & 0xFFFF)
    } else {
        None
    }
}

/// The code a failing Win32 call reported, spelled out so it can be read without a lookup: hex
/// **and** decimal, plus the Win32 code an `0x8007…`-class `HRESULT` wraps.
///
/// `io::Error::from_raw_os_error` renders an `HRESULT` as a bare negative number
/// (`Access is denied. (os error -2147024891)`), which looks exactly like a Win32 code and is not
/// one — the diagnostic asymmetry t-030 §6 ranks as a hazard for the AppContainer-profile rank of
/// refusal. This clause is what makes the operator-facing reason line say which number is which.
#[must_use]
fn hresult_clauses(code: windows_sys::core::HRESULT) -> String {
    let unsigned = code as WIN32_ERROR;
    if unsigned & 0x8000_0000 == 0 {
        // Not an `HRESULT`: a plain Win32 error, whose decimal number is the whole story.
        return format!("(win32 code {unsigned})");
    }
    let wrapped = win32_code_of_hresult(unsigned)
        .map(|wrapped| format!(", wrapping win32 code {wrapped}"))
        .unwrap_or_default();
    format!("(HRESULT 0x{unsigned:08X}, decimal {code}{wrapped})")
}

/// Render a Win32 `HRESULT` as the OS error it carries, for operator-visible text.
fn hresult_text(code: windows_sys::core::HRESULT) -> String {
    std::io::Error::from_raw_os_error(code).to_string()
}

/// The OS text for a Win32 status the call reported **itself**, as opposed to whatever
/// `GetLastError` happens to hold.
///
/// `GetNamedSecurityInfoW` and `GetSecurityInfo` answer with the code their own prose already
/// names, so the structured `code` field and the text have to come from the same number.
fn status_text(code: WIN32_ERROR) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

// ---------------------------------------------------------------------------
// SID building blocks
// ---------------------------------------------------------------------------

/// The authority that owns every `S-1-15-*` identity: AppContainer SIDs, the capability SIDs
/// below them, and the application-package groups.
fn app_package_authority() -> SID_IDENTIFIER_AUTHORITY {
    SECURITY_APP_PACKAGE_AUTHORITY
}

/// Allocate one `S-1-15-<sub-authorities>` identity from the app-package authority.
///
/// Windows exposes no constants for these SIDs, so they are built from the authority and the
/// sub-authorities (verified in `.marmel/research/windows_appcontainer_api.md`): `&[3, 1]` is
/// `internetClient`, `&[2, 2]` is "ALL APPLICATION PACKAGES", `&[2, 3]` is "ALL RESTRICTED
/// APPLICATION PACKAGES".
///
/// The result is released with `FreeSid`. A profile SID from `CreateAppContainerProfile` is not:
/// it is released with `LocalFree` (see [`AppContainerProfileGuard`]).
///
/// [`AppContainerProfileGuard`]: self::AppContainerProfileGuard
fn allocate_app_package_sid(sub_authorities: &[u32]) -> Result<PSID, WindowsSandboxError> {
    let count = u8::try_from(sub_authorities.len()).map_err(|_| WindowsSandboxError::Sid {
        reason: format!(
            "an app-package SID takes between 1 and 8 sub-authorities, got {}",
            sub_authorities.len()
        ),
        code: 0,
    })?;
    let mut sub = [0u32; 8];
    sub[..sub_authorities.len()].copy_from_slice(sub_authorities);
    let authority = app_package_authority();
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: the authority and the output pointer are valid, and all eight sub-authority slots
    // are always passed as the full array.
    let ok = unsafe {
        AllocateAndInitializeSid(
            &authority, count, sub[0], sub[1], sub[2], sub[3], sub[4], sub[5], sub[6], sub[7],
            &mut sid,
        )
    };
    if ok == 0 || sid.is_null() {
        let (code, text) = last_win32_error_detail();
        return Err(WindowsSandboxError::Sid {
            reason: format!(
                "AllocateAndInitializeSid(S-1-15-{}) failed: {text}",
                sub_authorities
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<String>>()
                    .join("-"),
            ),
            code,
        });
    }
    Ok(sid)
}

/// `ALL APPLICATION PACKAGES` (`S-1-15-2-2`) and `ALL RESTRICTED APPLICATION PACKAGES`
/// (`S-1-15-2-3`): the two built-in groups every AppContainer process belongs to, released with
/// `FreeSid` on drop.
struct ApplicationPackagesSid {
    sid: PSID,
}

impl ApplicationPackagesSid {
    fn all() -> Result<Self, WindowsSandboxError> {
        Ok(Self::from(allocate_app_package_sid(&[2, 2])?))
    }

    fn restricted() -> Result<Self, WindowsSandboxError> {
        Ok(Self::from(allocate_app_package_sid(&[2, 3])?))
    }

    const fn from(sid: PSID) -> Self {
        Self { sid }
    }

    const fn sid(&self) -> PSID {
        self.sid
    }
}

impl Drop for ApplicationPackagesSid {
    fn drop(&mut self) {
        // SAFETY: the SID was allocated by `AllocateAndInitializeSid`, whose documented release
        // is `FreeSid`.
        unsafe { FreeSid(self.sid) };
    }
}

/// Render a SID as its canonical string form (e.g. `S-1-15-3-1`), releasing the string the
/// conversion API allocated.
///
/// # Safety
///
/// `sid` must be a valid SID: anything allocated by this module (a profile SID from
/// `CreateAppContainerProfile` or an app-package SID from `allocate_app_package_sid`) satisfies
/// this. Win32 reads the memory behind the pointer.
pub unsafe fn sid_to_string(sid: PSID) -> Result<String, WindowsSandboxError> {
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: the output pointer is valid; the string it receives is read below and freed here.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut raw) };
    if ok == 0 || raw.is_null() {
        let (code, text) = last_win32_error_detail();
        return Err(WindowsSandboxError::Sid {
            reason: format!("ConvertSidToStringSidW failed: {text}"),
            code,
        });
    }
    let text = wide_cstr_to_string(raw);
    // SAFETY: the conversion API allocated `raw`, and it is read no more.
    unsafe {
        LocalFree(raw as *mut c_void);
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// capability list
// ---------------------------------------------------------------------------

/// The capability list an AppContainer is declared with, alongside its SID.
///
/// Capabilities decide what an AppContainer may reach beyond the ACEs that name it: a container
/// with none of them is a filesystem-only identity, and `internetClient` (`S-1-15-3-1`) is what
/// lets a sandboxed `curl`/`cargo` still talk to the network. Win32 has no constant for that
/// SID, so it is built from the app-package authority (see
/// `.marmel/research/windows_appcontainer_api.md`).
pub struct CapabilitySids {
    /// Owned capability SIDs, each released with `FreeSid` on drop.
    sids: Vec<PSID>,
    /// The `SID_AND_ATTRIBUTES` array `SECURITY_CAPABILITIES.Capabilities` points at.
    attributes: Vec<SID_AND_ATTRIBUTES>,
}

impl CapabilitySids {
    /// The capability list this backend declares: `internetClient`, `S-1-15-3-1`.
    #[must_use = "the capability SIDs are allocated by Win32 and must stay owned by this value"]
    pub fn internet_client() -> Result<Self, WindowsSandboxError> {
        let sid = allocate_app_package_sid(&[3, 1])?;
        Ok(Self {
            sids: vec![sid],
            attributes: vec![SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: 0,
            }],
        })
    }

    /// The number of capabilities, as `SECURITY_CAPABILITIES.CapabilityCount` wants it.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.attributes.len() as u32
    }

    /// The array `SECURITY_CAPABILITIES.Capabilities` wants, valid while `self` lives.
    #[must_use]
    fn as_sid_attributes(&self) -> *mut SID_AND_ATTRIBUTES {
        self.attributes.as_ptr().cast_mut()
    }

    /// The capability SIDs in canonical string form, for diagnostics and tests.
    pub fn strings(&self) -> Result<Vec<String>, WindowsSandboxError> {
        self.sids
            .iter()
            .copied()
            .map(|sid| unsafe { sid_to_string(sid) })
            .collect::<Result<Vec<String>, WindowsSandboxError>>()
    }
}

impl Drop for CapabilitySids {
    fn drop(&mut self) {
        for sid in &self.sids {
            // SAFETY: these SIDs came from `AllocateAndInitializeSid`, whose documented release
            // is `FreeSid`.
            unsafe { FreeSid(*sid) };
        }
    }
}

// ---------------------------------------------------------------------------
// AppContainer profile: naming, creation, deletion
// ---------------------------------------------------------------------------

use std::marker::PhantomData;

/// Prefix every ephemeral AppContainer profile this backend creates, so the leftovers of a
/// crashed run can be recognised (and removed) by an operator.
const APPCONTAINER_PROFILE_PREFIX: &str = "marmel-sandbox";

/// The longest AppContainer profile name Windows accepts.
///
/// A profile name is also a directory name under the profile directory, and Windows caps the
/// AppContainer name at 64 characters; the generated name keeps the prefix and the workspace
/// hash and shortens the random part instead of overflowing.
const APPCONTAINER_PROFILE_MAX_LEN: usize = 64;

/// The character set an AppContainer profile name accepts.
///
/// The name reaches the profile directory on disk, the shell, and the text form of a
/// security descriptor, so anything outside this set (a separator, `:`, `*`, a space) is
/// dropped rather than smuggled through.
#[must_use]
fn is_profile_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')
}

/// The stable half of the profile name: a hash of the workspace root.
///
/// The root itself cannot go into the name (it carries separators and a drive letter), but the
/// name should still say which workspace it belongs to, so the identity is folded into a fixed
/// 16-hex-char FNV-1a digest. It is a hash, not an identifier: a rerun on the same root gets
/// the same slug, and the random salt below is what keeps two concurrent runs apart.
#[must_use]
fn workspace_root_slug(workspace_root: &Path) -> String {
    let text = workspace_root.to_string_lossy();
    let mut hash: u64 = 0xcbf2_9ce4_86f2_9cea; // FNV-1a 64-bit offset basis
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a prime
    }
    format!("{hash:016x}")
}

/// The deterministic, ephemeral AppContainer profile name for one workspace root.
///
/// Two parts: the root slug above, plus `salt`. At the creation site the salt comes from
/// `uuid::Uuid::new_v4().simple()`, which the already-present `uuid` dependency gives us
/// for free: a v4 UUID is unique across processes and reboots where `std::process::id()`
/// alone is only unique per host, and a nanosecond timestamp alone is only unique per
/// instant. The name is therefore *derived from* the workspace root (its slug) plus a random
/// component, and never a fixed name two runs could fight over.
#[must_use]
pub fn appcontainer_profile_name(workspace_root: &Path, salt: &str) -> String {
    let slug = workspace_root_slug(workspace_root);
    let budget = APPCONTAINER_PROFILE_MAX_LEN
        .saturating_sub(APPCONTAINER_PROFILE_PREFIX.len() + slug.len() + 2) // two separators
        .max(1);
    let salt: String = salt
        .chars()
        .filter(|ch| is_profile_name_char(*ch))
        .take(budget)
        .collect();
    format!("{APPCONTAINER_PROFILE_PREFIX}-{slug}-{salt}")
}

/// Whether `name` can be handed to `CreateAppContainerProfile` unchanged.
#[must_use]
fn is_valid_profile_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(APPCONTAINER_PROFILE_PREFIX) else {
        return false;
    };
    !name.is_empty()
        && !suffix.is_empty()
        && name.len() <= APPCONTAINER_PROFILE_MAX_LEN
        && name.chars().all(is_profile_name_char)
}

/// The AppContainer identity of one sandboxed run: the profile, its SID, and the capability
/// list it is launched with.
///
/// A profile SID belongs to the profile, and the launch names that SID in its
/// `SECURITY_CAPABILITIES` block, so the profile has to outlive the launch. This guard owns it
/// for exactly that lifetime, and [`Drop`] deletes the profile and releases the SID.
///
/// Threading: `PSID` is a raw pointer, so this type is `!Send + !Sync` by construction, and the
/// phantom marker keeps that true if the field types ever change. Confining the guard to one
/// thread is not a limitation but what the launch path needs — the attribute list that names
/// the SID is written and read on a single thread.
pub struct AppContainerProfileGuard {
    name: String,
    sid: PSID,
    capabilities: CapabilitySids,
    /// Non-`Sync` marker, see the type comment.
    _single_thread: PhantomData<*mut ()>,
}

impl AppContainerProfileGuard {
    /// Create a fresh AppContainer profile for this run, with the capabilities the launch will
    /// declare.
    ///
    /// The SID is the one `CreateAppContainerProfile` derives from the generated name: the profile
    /// is created for this run and deleted when it ends, so nothing has to be re-derived from the
    /// name later. Taking the capabilities by value is deliberate: the guard keeps them alive for
    /// the launch.
    pub fn create(
        workspace_root: &Path,
        capabilities: CapabilitySids,
    ) -> Result<Self, WindowsSandboxError> {
        let salt = uuid::Uuid::new_v4().simple().to_string();
        let name = appcontainer_profile_name(workspace_root, &salt);
        if !is_valid_profile_name(&name) {
            return Err(WindowsSandboxError::Profile {
                name,
                reason: "the generated name is not a valid AppContainer profile name".to_string(),
                code: 0,
            });
        }
        let wide_name = WideString::new(&name);
        // The display name is a separate string Windows stores next to the profile and shows in
        // its own tools; reusing the profile name keeps them from drifting apart.
        let wide_display = WideString::new(&name);
        let attributes = capabilities.as_sid_attributes();
        let count = capabilities.count();
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: every pointer argument is owned for the duration of the call (`wide_*` own
        // their buffers, `attributes` is owned by `capabilities`, which the caller moved in).
        let result = unsafe {
            CreateAppContainerProfile(
                wide_name.pcwstr(),
                wide_display.pcwstr(),
                std::ptr::null(),
                attributes,
                count,
                &mut sid,
            )
        };
        if result < 0 || sid.is_null() {
            return Err(WindowsSandboxError::Profile {
                name,
                reason: format!(
                    "CreateAppContainerProfile failed: {} {}",
                    hresult_text(result),
                    hresult_clauses(result)
                ),
                // The `HRESULT` of the call, kept as the raw status so the log field and the
                // reason line name the same one (the reason line spells it out in hex too).
                code: result as WIN32_ERROR,
            });
        }
        Ok(Self {
            name,
            sid,
            capabilities,
            _single_thread: PhantomData,
        })
    }

    /// The profile name, as the launch-time `SECURITY_CAPABILITIES` block and
    /// `DeleteAppContainerProfile` want it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The AppContainer SID. Owned by this guard, released when it drops.
    #[must_use]
    pub fn sid(&self) -> PSID {
        self.sid
    }

    /// The capability list the profile was created with, for the launch to reuse.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilitySids {
        &self.capabilities
    }

    /// The `SECURITY_CAPABILITIES` block `UpdateProcThreadAttribute` attaches to
    /// `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` in the launch layer.
    ///
    /// The block borrows the AppContainer SID and the capability array from this guard, so it
    /// must not outlive it.
    #[must_use]
    pub fn security_capabilities(&self) -> SECURITY_CAPABILITIES {
        SECURITY_CAPABILITIES {
            AppContainerSid: self.sid,
            Capabilities: self.capabilities.as_sid_attributes(),
            CapabilityCount: self.capabilities.count(),
            Reserved: 0,
        }
    }
}

impl Drop for AppContainerProfileGuard {
    fn drop(&mut self) {
        // Deleting the profile is best-effort: a failure leaves a stale profile an operator can
        // see and remove, and it must never turn into a panic during unwinding.
        let wide_name = WideString::new(&self.name);
        let result = unsafe { DeleteAppContainerProfile(wide_name.pcwstr()) };
        if result < 0 {
            tracing::warn!(
                profile = %self.name,
                hresult = result,
                "failed to delete the AppContainer profile this run created"
            );
        }
        // SAFETY: the profile SID is owned by this guard. `LocalFree` is the documented release
        // for a `CreateAppContainerProfile` SID; `FreeSid` belongs to
        // `AllocateAndInitializeSid` SIDs and must not be used here.
        unsafe { LocalFree(self.sid) };
        self.sid = std::ptr::null_mut();
    }
}

// ---------------------------------------------------------------------------
// The policy table, rendered for the Windows object model
// ---------------------------------------------------------------------------

/// The host facts the policy is built from, read from the process environment.
///
/// Injected as a value so the table is testable without reading the real environment of the
/// process under test, and without ever writing to a real user's home.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowsHostPaths {
    /// `%USERPROFILE%`: the directory the sensitive ones live under, and the one directory that
    /// must never become a policy entry itself.
    pub user_profile: Option<PathBuf>,
    /// The resolved temporary directories: `%TEMP%`, `%TMP%` and `%TMPDIR%` when set. On a
    /// normal install they point at `%USERPROFILE%\AppData\Local\Temp`, and they are resolved here
    /// rather than hardcoded because a host may point them anywhere.
    pub temp_dirs: Vec<PathBuf>,
    /// `CARGO_HOME` when set, `%USERPROFILE%\.cargo` otherwise.
    pub cargo_home: Option<PathBuf>,
    /// `RUSTUP_HOME` when set, `%USERPROFILE%\.rustup` otherwise.
    pub rustup_home: Option<PathBuf>,
    /// `%SystemRoot%`/`%windir%`: `C:\Windows` on a default install.
    pub system_root: Option<PathBuf>,
    /// Every spelling of the program-files directories that exists here: `%ProgramFiles%`,
    /// `%ProgramW6432%` and `%ProgramFiles(x86)%`.
    pub program_files: Vec<PathBuf>,
}

impl WindowsHostPaths {
    /// Read the host facts from this process' environment.
    #[must_use]
    pub fn from_env() -> Self {
        let user_profile = env_dir(&["USERPROFILE"]);
        let cargo_home = env_dir(&["CARGO_HOME"])
            .or_else(|| user_profile.as_ref().map(|home| home.join(".cargo")));
        let rustup_home = env_dir(&["RUSTUP_HOME"])
            .or_else(|| user_profile.as_ref().map(|home| home.join(".rustup")));
        Self {
            user_profile,
            temp_dirs: env_dirs(&["TEMP", "TMP", "TMPDIR"]),
            cargo_home,
            rustup_home,
            system_root: env_dir(&["SystemRoot", "windir"]),
            program_files: env_dirs(&["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]),
        }
    }

    /// `<CARGO_HOME>/bin`, the directory a sandboxed `cargo` execs tools from.
    #[must_use]
    pub fn cargo_bin(&self) -> Option<PathBuf> {
        self.cargo_home.as_ref().map(|home| home.join("bin"))
    }

    /// `<SystemRoot>\System32`, where `cmd.exe` (`%ComSpec%`) lives: a sandboxed command that
    /// cannot exec it fails before the policy has any visible effect.
    #[must_use]
    pub fn system32(&self) -> Option<PathBuf> {
        self.system_root.as_ref().map(|root| root.join("System32"))
    }

    /// The directories the policy denies outright: the sensitive ones the shared policy layer
    /// names, joined onto the user profile.
    #[must_use]
    pub fn sensitive_dirs(&self) -> Vec<PathBuf> {
        self.user_profile
            .iter()
            .flat_map(|home| {
                sbpl::SENSITIVE_DENIED_BASENAMES
                    .iter()
                    .map(move |name| home.join(name))
                    .collect::<Vec<PathBuf>>()
            })
            .collect()
    }

    /// The read-write half of the built-in list: the temporary directories.
    #[must_use]
    pub fn read_write_paths(&self) -> Vec<PathBuf> {
        self.temp_dirs.clone()
    }

    /// The read-and-execute half of the built-in list that this host could have: toolchain
    /// directories under the user profile (honouring `CARGO_HOME`/`RUSTUP_HOME`), the Windows
    /// directory and its `System32`, and every program-files spelling.
    #[must_use]
    pub fn read_only_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = [
            self.cargo_bin(),
            self.cargo_home.clone(),
            self.rustup_home.clone(),
            self.system32(),
            self.system_root.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        paths.extend(self.program_files.iter().cloned());
        paths
    }

    /// The user-profile root, the one directory whose absence from the allow-list is an
    /// invariant rather than an oversight.
    #[must_use]
    pub fn user_profile_root(&self) -> Option<&Path> {
        self.user_profile.as_deref()
    }
}

/// One spelling of an environment variable that this process has set, out of the names listed.
fn env_dir(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|name| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    })
}

/// Every distinct spelling among `names`, in the order they are listed.
fn env_dirs(names: &[&str]) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for name in names {
        if let Some(path) = env_dir(std::slice::from_ref(name))
            && seen.insert(path.clone())
        {
            paths.push(path);
        }
    }
    paths
}

/// Compare two path spellings the way the Windows filesystem does: case-insensitively, with the
/// separators and the verbatim markers flattened away and the 8.3 short components resolved, so
/// every spelling of one location names the same location.
fn paths_match(left: &Path, right: &Path) -> bool {
    comparable_components(left) == comparable_components(right)
}

/// The components of `path`, normalised for [`paths_match`]: the spelling put into this module's
/// one comparison domain ([`comparison_spelling`]), lower-cased and split on the separator.
///
/// The spelling a path is written in is not part of its identity, and the spellings this policy
/// meets are not the same one: the workspace root arrives canonicalised
/// (`\\?\C:\Users\fredrik\source\marmel`, the shape `canonicalize()` produces and that the
/// name-based ACL APIs are handed), the denied directories come from the environment in their
/// plain spelling (`C:\Users\fredrik\.ssh`), and the same environment can spell the profile
/// directory by its 8.3 short name (`C:\Users\RUNNER~1`, which is what `%TEMP%` holds on a GitHub
/// runner). Comparing the drive **prefix** as a component made the first two spellings different
/// locations, so the sensitive-directory invariant — the hard `RuleSetup` failure for a workspace
/// root that is, or contains, a denied directory — never fired for a real root, only for the
/// plain-spelled fixtures the tests used; comparing the components literally makes the short and
/// the long spelling of the same directory different locations, which is the same bypass one step
/// further along (t-034, and the `RUNNER~1` table this module now refuses).
///
/// What has to go is everything that says *how* a location is written rather than *which* location
/// it is: the markers (`\\?\C:\Users` and `C:\Users` are one volume, `\\?\UNC\server\share` and
/// `\\server\share` one share — a mapped, redirected or WSL workspace root canonicalises to the UNC
/// spelling), the separator spelling, and the short form of a component the host can name in long.
fn comparable_components(path: &Path) -> Vec<String> {
    comparison_spelling(path)
        .split('\\')
        // An empty segment is what a separator pair leaves behind, and a `.` segment says "this
        // directory": neither names a part of the location.
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .map(str::to_owned)
        .collect()
}

/// The marker Windows puts in front of a path to say "interpret this literally, do not normalise
/// it": `\\` + `?` + `\` in front of a drive, a UNC share or a device name.
const VERBATIM_MARKER: &str = "\\\\?\\";

/// The marker of a verbatim **share**: the marker above, plus the `UNC` word that names the share
/// form (matched case-insensitively, because a share spelling arrives with whatever case the caller
/// or the host wrote it in).
const VERBATIM_UNC_MARKER: &str = "\\\\?\\unc\\";

/// The device form of the marker above (`\\` + `.` + `\`), which names a device namespace such as
/// `\\.\COM1` rather than a volume.
const DEVICE_MARKER: &str = "\\\\.\\";

/// The share marker as this module **writes** it: [`VERBATIM_UNC_MARKER`] is the same prefix folded
/// to lower case so it can match whatever case the host spelled it in, and an operator-facing entry
/// is spelled the way Windows spells it.
const VERBATIM_UNC_PREFIX: &str = "\\\\?\\UNC\\";

/// The separator that opens a plain share spelling. A verbatim share rewrites onto the two of
/// these that a plain spelling opens with, so both spellings keep one component list.
const UNC_SEPARATOR: &str = "\\";

/// How a spelling is prefixed: with nothing, or with one of the markers Windows reads as "interpret
/// the rest literally" — see [`strip_windows_path_markers`] for why the marker is not part of the
/// location, and [`normalised_spelling`] for the one case where it has to survive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathMarker {
    /// No marker: the API parses the path, normalises it and holds it to `MAX_PATH`.
    None,
    /// `\\?\` in front of a drive (or of a device namespace written the verbatim way).
    Verbatim,
    /// `\\?\UNC\` in front of a share: the verbatim spelling of `\\server\share`.
    Unc,
    /// `\\.\` in front of a device namespace (`\\.\COM1`): a namespace, not a volume.
    Device,
}

/// The marker `spelling` is written with, recognising it in either separator spelling and in either
/// case (the marker is itself spelled with separators and with letters, so both are flattened and
/// folded before it is looked for).
#[must_use]
fn path_marker(spelling: &str) -> PathMarker {
    let probe = spelling.replace('/', "\\").to_lowercase();
    if probe.starts_with(VERBATIM_UNC_MARKER) {
        PathMarker::Unc
    } else if probe.starts_with(DEVICE_MARKER) {
        PathMarker::Device
    } else if probe.starts_with(VERBATIM_MARKER) {
        PathMarker::Verbatim
    } else {
        PathMarker::None
    }
}

/// Drop the verbatim and device markers from a path spelling, and rewrite a verbatim UNC to the
/// plain share spelling.
///
/// A marker says how Windows is to **interpret** a path, not which location it names, so the two
/// spellings of one location have to lose the marker before they can be compared or stored:
/// `\\?\C:\Users` becomes `C:\Users` and `\\?\UNC\server\share` becomes `\\server\share`. A
/// spelling without a marker comes back with its separators flattened, which is what puts it in
/// the same domain as one that was written with them the other way.
///
/// Every separator spelling Windows accepts is flattened **before** the marker is looked for, and
/// the order of those two steps is the whole point (t-034): a marker is itself spelled with
/// separators, so `//?/C:/Users` — a verbatim path that reached this code through a channel which
/// normalises `\` to `/` — was never recognised as a marker and left a leading `?` segment behind.
/// The drive prefix of that spelling (`?`, then `c:`) could then never equal the drive prefix of
/// the plain one (`c:`): the same `\\?\c:` versus `c:` mismatch the backslash spelling used to
/// have, surviving in the one spelling the strip could not see.
fn strip_windows_path_markers(spelling: &str) -> String {
    let flattened = spelling.replace('/', "\\");
    // The marker is matched case-insensitively but sliced out of the original spelling, so the
    // location behind it keeps the case it was handed. Every byte of every marker is ASCII, so the
    // fold cannot change the length of the prefix and the offset below is a character boundary.
    let probe = flattened.to_lowercase();
    if probe.starts_with(VERBATIM_UNC_MARKER) {
        // The marker ends with the separator that closes the `UNC` word, so the rest starts at the
        // share name. A plain spelling opens that same share name with two separators, and the
        // rewrite supplies exactly those two.
        let share = &flattened[VERBATIM_UNC_MARKER.len()..];
        return format!("{UNC_SEPARATOR}{UNC_SEPARATOR}{share}");
    }
    if probe.starts_with(DEVICE_MARKER) {
        return flattened[DEVICE_MARKER.len()..].to_owned();
    }
    if probe.starts_with(VERBATIM_MARKER) {
        return flattened[VERBATIM_MARKER.len()..].to_owned();
    }
    flattened
}

/// `windef.h` `MAX_PATH`: at or beyond this many UTF-16 code units a path can only be handed to the
/// file and security APIs with a verbatim marker in front of it, so below the ceiling the marker is
/// noise this module removes, and at or above it the marker is the only way to name the object.
const MAX_PATH_CEILING: usize = 260;

/// The ceiling of a `\\?\` path — 32 767 UTF-16 code units. An answer longer than that from the
/// long-name call is a broken host rather than a long path, and is declined rather than trusted.
const VERBATIM_PATH_CEILING: usize = 32_767;

/// The spelling of `path` in this module's **one** comparison domain: marker-free, separator-
/// flattened, long-name resolved, lower-cased.
///
/// This is the text every containment and deny-list decision is taken in ([`paths_match`],
/// [`is_ancestor_or_self`] and therefore [`sensitive_conflict`]). Entry construction puts the same
/// text into the stored [`PolicyPath`] through [`normalise_policy_path`], so the table, the grants
/// and the checks cannot disagree about which directory a spelling names.
fn comparison_spelling(path: &Path) -> String {
    let plain = strip_windows_path_markers(&path.to_string_lossy());
    match long_path_spelling(&plain) {
        Some(resolved) => resolved.to_lowercase(),
        None => plain.to_lowercase(),
    }
}

/// The one spelling this policy stores: the comparison domain, with the verbatim marker kept only
/// where the path could not be named without it.
///
/// Normalising at construction is what makes the containment checks and the deny-list suppression
/// agree with the table they are read from. Before it, one CI table held three spellings of one
/// location at once — the canonicalised root in its verbatim long form, and every entry derived
/// from the environment in the 8.3 short form of the same profile directory — so a workspace root
/// inside `%USERPROFILE%\.ssh` was not seen to be inside `%USERPROFILE%\.ssh`, and the deny-list
/// that is the only way Windows has of closing a directory silently did not fire.
#[must_use]
fn normalise_policy_path(path: &Path) -> PathBuf {
    PathBuf::from(normalised_spelling(&path.to_string_lossy()))
}

/// The domain spelling of `spelling`: separators flattened, marker removed, short components
/// resolved to long ([`long_path_spelling`]).
///
/// Two spellings are deliberately left alone. A device namespace (`\\.\COM1`) names no volume, so
/// there is no long-name domain to resolve it into and the marker is the name. And a path at or
/// beyond [`MAX_PATH_CEILING`] keeps the marker it arrived with, because the marker is what makes a
/// path that long nameable by `CreateFileW`, `GetNamedSecurityInfoW` and `SetNamedSecurityInfoW` at
/// all: dropping it would turn a workspace root the sandbox can grant into one it can only refuse,
/// and the point of this function is to name locations better, not to lose them.
fn normalised_spelling(spelling: &str) -> String {
    let flattened = spelling.replace('/', "\\");
    let marker = path_marker(&flattened);
    if marker == PathMarker::Device {
        return flattened;
    }
    let plain = strip_windows_path_markers(&flattened);
    let resolved = match long_path_spelling(&plain) {
        Some(resolved) => resolved,
        None => plain,
    };
    if marker != PathMarker::None && resolved.encode_utf16().count() >= MAX_PATH_CEILING {
        return with_verbatim_marker(marker, &resolved);
    }
    resolved
}

/// Put `plain` back into the spelling a path this long has to be handed to Win32 in.
fn with_verbatim_marker(marker: PathMarker, plain: &str) -> String {
    match marker {
        PathMarker::Unc => {
            // The plain share spelling opens with the two separators the marker replaces, and the
            // marker supplies them in its own words.
            let share = plain
                .strip_prefix(UNC_SEPARATOR)
                .and_then(|share| share.strip_prefix(UNC_SEPARATOR))
                .unwrap_or(plain);
            format!("{VERBATIM_UNC_PREFIX}{share}")
        }
        // A drive spelled verbatim, and the device case, which [`normalised_spelling`] has already
        // returned by now.
        PathMarker::Verbatim | PathMarker::Device | PathMarker::None => {
            format!("{VERBATIM_MARKER}{plain}")
        }
    }
}

/// The last Win32 status behind a declined long-name resolution, in the one record this module
/// writes for it: which call declined, which spelling, and the code it reported — decimal and
/// `code_hex`, because `0x00000003` and `3` are the same answer and only one of them is recognisable.
fn log_long_path_decline(call: &str, spelling: &str, code: WIN32_ERROR) {
    tracing::debug!(
        path = spelling,
        call,
        code,
        code_hex = format!("{code:#010x}").as_str(),
        "the host could not resolve the long-name spelling of this path: the policy keeps the \
         spelling it was handed"
    );
}

/// The long-name spelling of a marker-free absolute `spelling`, or `None` when this host cannot name
/// one for it.
///
/// `GetLongPathNameW` is the only API that turns the 8.3 component of a path (`RUNNER~1`) into the
/// long component the same directory answers under a `canonicalize()` — and hosts really do hand out
/// short spellings, in `%TEMP%` and in whatever `Path::canonicalize` or an operator's config copied
/// from it, which is how one location reached this module as both `C:\Users\runneradmin\…` and
/// `C:\Users\RUNNER~1\…`.
///
/// It is used best-effort, and that is exactly what makes it safe to call on a path this policy may
/// never have seen before:
///
/// * the sizing call answers with the number of code units the answer needs **including** the
///   terminating NUL, so the second call is handed a buffer of precisely that size;
/// * a call that answers `0` failed and the caller keeps the spelling it already had — the
///   component that cannot be resolved is most often one that does not exist (`ERROR_FILE_NOT_FOUND`,
///   `ERROR_PATH_NOT_FOUND`), and a malformed spelling is rejected outright (`ERROR_INVALID_NAME`);
/// * a spelling that is not absolute is left alone instead of being resolved against the current
///   directory of whoever happened to call last, which would invent a location for a relative path
///   the policy is about to reject as relative anyway;
/// * an answer past [`VERBATIM_PATH_CEILING`] is declined rather than trusted.
///
/// Every one of those outcomes is "the spelling was not improved": the function has no error path,
/// so a policy that meets a path the host cannot resolve behaves exactly as it did before.
fn long_path_spelling(spelling: &str) -> Option<String> {
    if spelling.is_empty() || !Path::new(spelling).is_absolute() {
        return None;
    }
    let wide = WideString::new(spelling);
    let mut probe = [0u16; 1];
    // SAFETY: the input is owned by `wide` and NUL-terminated, `probe` is a writable one-code-unit
    // buffer, and a size too small is the documented way to ask for the size the answer needs.
    let needed = unsafe { GetLongPathNameW(wide.pcwstr(), probe.as_mut_ptr(), probe.len() as u32) };
    if needed == 0 {
        let (code, _) = last_win32_error_detail();
        log_long_path_decline("GetLongPathNameW (sizing)", spelling, code);
        return None;
    }
    let Some(needed) = usize::try_from(needed)
        .ok()
        .filter(|needed| *needed <= VERBATIM_PATH_CEILING)
    else {
        log_long_path_decline("GetLongPathNameW (sizing)", spelling, 0);
        return None;
    };
    let mut buffer: Vec<u16> = vec![0; needed];
    // SAFETY: `buffer` holds exactly `needed` code units, which is the size the sizing call asked
    // for, and the pointer handed to the API is that buffer's own.
    let written = unsafe {
        GetLongPathNameW(
            wide.pcwstr(),
            buffer.as_mut_ptr(),
            u32::try_from(needed).ok()?,
        )
    };
    if written == 0
        || usize::try_from(written)
            .ok()
            .is_none_or(|written| written >= buffer.len())
    {
        let (code, _) = last_win32_error_detail();
        log_long_path_decline("GetLongPathNameW", spelling, code);
        return None;
    }
    let resolved = wide_cstr_to_string(buffer.as_ptr());
    if resolved.is_empty() {
        log_long_path_decline("GetLongPathNameW", spelling, 0);
        return None;
    }
    Some(resolved)
}

/// Whether `candidate` names an ancestor of (or the same directory as) `target`.
fn is_ancestor_or_self(candidate: &Path, target: &Path) -> bool {
    let candidate = comparable_components(candidate);
    let target = comparable_components(target);
    !candidate.is_empty()
        && candidate.len() <= target.len()
        && candidate
            .iter()
            .zip(target.iter())
            .all(|(left, right)| left == right)
}

/// Why one path cannot appear in the allow-list, in the terms the policy denies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveConflict {
    /// The denied directory the entry would expose (`%USERPROFILE%\.ssh` and friends, from
    /// [`crate::harness::sbpl::SENSITIVE_DENIED_BASENAMES`]).
    pub denied: PathBuf,
    /// Whether the entry contains that directory, or sits inside it.
    pub kind: SensitiveConflictKind,
}

impl SensitiveConflict {
    /// The operator-facing half: one short clause, no trailing punctuation, because a note is
    /// printed on a single line.
    #[must_use]
    pub fn note(&self) -> String {
        match self.kind {
            SensitiveConflictKind::Contains => format!(
                "contains the sensitive directory {} the sandbox denies, and would expose it through inheritance",
                path_text(&self.denied)
            ),
            SensitiveConflictKind::Inside => format!(
                "sits inside the sensitive directory {} the sandbox denies",
                path_text(&self.denied)
            ),
        }
    }
}

/// The two ways an entry can collide with a denied directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveConflictKind {
    /// The entry is, or contains, a denied directory: granting it hands out the keys inside by
    /// inheritance. This is why `%USERPROFILE%` itself is never granted.
    Contains,
    /// The entry sits inside a denied directory: the grant is in the part of the filesystem the
    /// sandbox closes, so it means nothing.
    Inside,
}

/// Find the denied directory one candidate entry would expose, if any.
fn sensitive_conflict(candidate: &Path, sensitive_dirs: &[PathBuf]) -> Option<SensitiveConflict> {
    for denied in sensitive_dirs {
        if is_ancestor_or_self(candidate, denied) {
            return Some(SensitiveConflict {
                denied: denied.clone(),
                kind: SensitiveConflictKind::Contains,
            });
        }
        if is_ancestor_or_self(denied, candidate) {
            return Some(SensitiveConflict {
                denied: denied.clone(),
                kind: SensitiveConflictKind::Inside,
            });
        }
    }
    None
}

/// What the Windows policy layer decided about the allow-list it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsPolicyTable {
    /// Every entry that became a grant, in the order the ACEs are applied.
    pub entries: Vec<PolicyPath>,
    /// One line per entry that did **not** become a grant, in the order it was considered, with
    /// the same shape as `sandbox::extra_path_skip_note` so one vocabulary covers both backends.
    pub notes: Vec<String>,
}

impl WindowsPolicyTable {
    /// The entries whose mutation the confinement cannot lose: the **one** entry the shared policy
    /// list marks `required`, i.e. the workspace root. Everything else can be declined with a
    /// note: the temporary directories are read-write entries too, but a policy that cannot grant
    /// one of them still confines the run and only lacks one of the write targets it asked for,
    /// which is reported as a partially enforced attempt instead of being turned into a rule
    /// failure that exits every command on that host.
    #[must_use = "the entries whose ACEs the confinement cannot lose, for the caller that must apply them or report the failure"]
    pub fn confinement_critical(&self) -> impl Iterator<Item = &PolicyPath> {
        self.entries.iter().filter(|entry| entry.required)
    }

    /// The read-write entries that are **not** required: the temporary directories, i.e. the
    /// entries the confinement applies when it can and degrades to a note when it cannot.
    #[must_use = "the optional read-write entries, for the caller that declines them rather than losing the confinement with them"]
    pub fn optional_read_write_entries(&self) -> impl Iterator<Item = &PolicyPath> {
        self.entries
            .iter()
            .filter(|entry| !entry.required && entry.grant == Grant::ReadWrite)
    }

    /// The read-only half, for the caller that verifies rather than mutates.
    #[must_use = "the read-only entries, which the confinement verifies rather than mutates"]
    pub fn read_only_entries(&self) -> impl Iterator<Item = &PolicyPath> {
        self.entries
            .iter()
            .filter(|entry| entry.grant == Grant::ReadOnly)
    }
}

/// Render the sandbox allow-list into the concrete Windows grant list.
///
/// This reuses the shared vocabulary of `harness::sandbox` — `PolicyPath`, `Grant`,
/// `ExtraPathSkip` and the skip note built from it — instead of defining a second one, so the
/// operator sees the same entry list and the same reasons on both backends.
///
/// The filtering that decides which operator-supplied extras become ACEs is deliberately
/// replicated from `sandbox::extra_readonly_rule_paths`, which is private to that module and
/// shaped for Landlock anchors: trim, require an absolute path, de-duplicate against what the
/// table already carries, and treat a path absent on this host as a skip (never fatal). The
/// Windows-specific rule added on top is the sensitive-directory invariant: an entry that
/// contains, or is inside, one of the directories the policy denies is rejected here, because on
/// Windows the *only* way to deny a directory is not to grant its parent.
pub fn windows_policy_entries(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> Result<WindowsPolicyTable, WindowsSandboxError> {
    build_policy_table(
        workspace_root,
        extra_readonly_paths,
        &WindowsHostPaths::from_env(),
    )
}

fn build_policy_table(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
    host: &WindowsHostPaths,
) -> Result<WindowsPolicyTable, WindowsSandboxError> {
    let mut entries: Vec<PolicyPath> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let sensitive = host.sensitive_dirs();

    // 1. the workspace root: read-write, and the only entry that is always required. Checking
    //    its own conflict is a hard failure rather than a skip: a workspace that *is* a denied
    //    directory has no honest policy at all, and granting it would widen the sandbox past
    //    what the shared contract denies.
    if let Some(conflict) = sensitive_conflict(workspace_root, &sensitive) {
        return Err(WindowsSandboxError::RuleSetup {
            path: path_text(workspace_root),
            reason: conflict.note(),
        });
    }
    entries.push(PolicyPath {
        // The entry is stored in the domain, not in the spelling the caller happened to use, so
        // every later comparison against the table — duplicate suppression, verification, the
        // operator-facing entry list — is made against one spelling of one directory.
        path: normalise_policy_path(workspace_root),
        grant: Grant::ReadWrite,
        required: true,
    });

    // 2. the temporary directories, read-write: `%TEMP%` is where a toolchain puts its build
    //    output, locks and swap files, and it is resolved from the environment because a host
    //    may point it anywhere.
    for dir in host.read_write_paths() {
        push_entry(
            &mut entries,
            &mut notes,
            &dir,
            Grant::ReadWrite,
            host,
            &sensitive,
            None,
        );
    }

    // 3. read-and-execute: the toolchain directories that exist here, honouring the
    //    `CARGO_HOME`/`RUSTUP_HOME` overrides, plus the Windows directory, its `System32` and
    //    the program-files spelling(s).
    for dir in host.read_only_paths() {
        push_entry(
            &mut entries,
            &mut notes,
            &dir,
            Grant::ReadOnly,
            host,
            &sensitive,
            None,
        );
    }

    // 4. the operator's own additions, which can only ever add read-and-execute rights.
    for raw in extra_readonly_paths {
        let entry = raw.trim();
        if entry.is_empty() {
            continue;
        }
        push_entry(
            &mut entries,
            &mut notes,
            Path::new(entry),
            Grant::ReadOnly,
            host,
            &sensitive,
            Some(raw),
        );
    }

    Ok(WindowsPolicyTable { entries, notes })
}

/// Add one entry to the table unless it cannot become a grant at all, recording why not.
fn push_entry(
    entries: &mut Vec<PolicyPath>,
    notes: &mut Vec<String>,
    path: &Path,
    grant: Grant,
    host: &WindowsHostPaths,
    sensitive: &[PathBuf],
    extra: Option<&String>,
) {
    let text = path_text(path);
    // From here the entry is handled in this module's one spelling domain, while `text` stays the
    // spelling the operator (or the environment) actually wrote: a note has to name what was
    // rejected, and the entry has to name a location the rest of the module can compare against.
    let path = &normalise_policy_path(path);

    // The user-profile root first, ahead of the generic conflict check below. That check *would*
    // also reject this entry (the root contains every denied directory under it), but its wording
    // names one denied directory and the inheritance mechanism, while this one names the actual
    // reason the operator's entry is hopeless: the profile root itself is off the allow-list. On a
    // real host the two notes would otherwise collapse into the same generic sentence, and the
    // profile-root note would be unreachable in production.
    if let Some(home) = host.user_profile_root()
        && paths_match(path, home)
    {
        notes.push(format!(
            "{text}: the user profile directory itself is never granted, because the sensitive directories below it would become reachable through it"
        ));
        return;
    }

    // The invariant, enforced rather than merely documented: an entry that is, or contains, a
    // denied directory would make `.ssh` and `.gnupg` reachable through the inheritance of the
    // grant itself. An entry inside a denied directory is the other honest answer: the grant would
    // sit in the part of the filesystem the policy closes.
    if let Some(conflict) = sensitive_conflict(path, sensitive) {
        notes.push(format!("{text}: {}", conflict.note()));
        return;
    }

    // The shared skip rules, replicated for the reasons documented on `windows_policy_entries`.
    if extra.is_some() && !path.is_absolute() {
        notes.push(sandbox::extra_path_skip_note(
            &text,
            ExtraPathSkip::NotAbsolute,
        ));
        return;
    }
    if entries.iter().any(|entry| paths_match(&entry.path, path)) {
        notes.push(sandbox::extra_path_skip_note(
            &text,
            ExtraPathSkip::Duplicate,
        ));
        return;
    }
    if !path.exists() {
        notes.push(sandbox::extra_path_skip_note(&text, ExtraPathSkip::Missing));
        return;
    }

    entries.push(PolicyPath {
        path: path.to_path_buf(),
        grant,
        required: false,
    });
}

// ---------------------------------------------------------------------------
// ACEs: verify first, then apply, then restore
// ---------------------------------------------------------------------------

/// The rights one grant hands out, as an ACE mask.
///
/// A read-write entry gets the full file-rights mask so newly created children inherit a usable
/// grant; a read-and-execute entry gets the file-object read and execute masks, which is exactly
/// the `READ & EXECUTE` value the built-in application-package ACEs on `C:\Windows` carry — so
/// the verification step recognises an existing ACE as covering the entry instead of insisting
/// on a mutation the operator has no `WRITE_DAC` to make.
#[must_use]
fn mask_for_grant(grant: Grant) -> u32 {
    match grant {
        Grant::ReadWrite => FILE_ALL_ACCESS,
        Grant::ReadOnly => FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
    }
}

/// What a policy entry needs from one object's DACL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclCoverage {
    /// An allow ACE already names a principal an AppContainer process belongs to with the rights
    /// the entry asks for, so the entry needs no mutation.
    Covered,
    /// The object has a NULL DACL: every principal already has full access, which is wider than
    /// the confinement model likes but needs no mutation either.
    NoDacl,
    /// Nothing in the DACL hands the rights out, so an explicit ACE naming the AppContainer SID
    /// is required.
    Missing,
}

/// Verify the read-only half of the policy against the DACL that already exists.
///
/// This is the step that keeps `C:\Windows` and `C:\Program Files` out of the mutation path:
/// those objects already carry read-only ACEs for the application-package groups on a normal
/// install, and a non-elevated process has no `WRITE_DAC` there anyway.
#[must_use = "whether the object already hands out the rights the entry asks for"]
pub fn acl_coverage(
    path: &Path,
    app_sid: PSID,
    grant: Grant,
) -> Result<AclCoverage, WindowsSandboxError> {
    let snapshot = DaclSnapshot::read(path)?;
    if snapshot.is_null() {
        return Ok(AclCoverage::NoDacl);
    }
    if snapshot.covers(path, app_sid, grant)? {
        Ok(AclCoverage::Covered)
    } else {
        Ok(AclCoverage::Missing)
    }
}

/// The DACL of one object, exactly as it stands before any mutation.
///
/// Keeping the security descriptor it came from (rather than copying the ACL bytes) is what
/// makes the restore exact: the saved DACL is the original, alignment and all.
struct DaclSnapshot {
    sd: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl DaclSnapshot {
    fn read(path: &Path) -> Result<Self, WindowsSandboxError> {
        let wide = WideString::from_path(path);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the object name is owned by `wide` for the duration of the call, and each
        // out-parameter is a valid pointer the API writes into.
        let code = unsafe {
            GetNamedSecurityInfoW(
                wide.pcwstr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                &mut sacl,
                &mut sd,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(WindowsSandboxError::AclRead {
                path: path_text(path),
                reason: format!(
                    "GetNamedSecurityInfoW failed (win32 error {code}): {}",
                    status_text(code)
                ),
                // The code the call answered with, kept structurally so the log field and the
                // prose cannot drift apart.
                code,
            });
        }
        let _ = sacl; // the SACL is not part of this policy
        Ok(Self { sd, dacl })
    }

    /// Whether the object has no DACL at all, which on Windows means "everyone may do
    /// everything here".
    fn is_null(&self) -> bool {
        self.dacl.is_null()
    }

    const fn dacl(&self) -> *mut ACL {
        self.dacl
    }

    /// Whether the saved DACL already hands an AppContainer process the rights `grant` asks for.
    ///
    /// Either an allow ACE names the AppContainer SID itself, or — for a read-and-execute entry
    /// — one names either of the two built-in application-package groups, which every
    /// AppContainer process belongs to.
    fn covers(
        &self,
        path: &Path,
        app_sid: PSID,
        grant: Grant,
    ) -> Result<bool, WindowsSandboxError> {
        let mask = mask_for_grant(grant);
        if self.has_ace_for(path, app_sid, mask)? {
            return Ok(true);
        }
        if grant == Grant::ReadOnly {
            let all = ApplicationPackagesSid::all()?;
            let restricted = ApplicationPackagesSid::restricted()?;
            for sid in [all.sid(), restricted.sid()] {
                if self.has_ace_for(path, sid, mask)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether the saved DACL carries a non-inherit-only allow ACE for `sid` covering `mask`.
    fn has_ace_for(&self, path: &Path, sid: PSID, mask: u32) -> Result<bool, WindowsSandboxError> {
        if self.dacl.is_null() {
            return Ok(false);
        }
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: the ACL pointer belongs to the descriptor this snapshot owns, and the
        // information buffer is a full `ACL_SIZE_INFORMATION`.
        let sized = unsafe {
            GetAclInformation(
                self.dacl,
                std::ptr::from_mut(&mut size).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        if sized == 0 {
            let (code, text) = last_win32_error_detail();
            return Err(WindowsSandboxError::AclRead {
                path: path_text(path),
                reason: format!("GetAclInformation failed: {text}"),
                code,
            });
        }
        for index in 0..size.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `GetAce` reads the ACL it is given and writes one pointer.
            let ok = unsafe { GetAce(self.dacl, index, &mut ace) };
            if ok == 0 || ace.is_null() {
                let (code, text) = last_win32_error_detail();
                return Err(WindowsSandboxError::AclRead {
                    path: path_text(path),
                    reason: format!("GetAce(index {index}) failed: {text}"),
                    code,
                });
            }
            // SAFETY: an access-allowed ACE begins with its header, and the SID follows the
            // mask field; `GetAce` guarantees the pointer for the ACE's own size.
            let typed = ace.cast::<ACCESS_ALLOWED_ACE>();
            // SAFETY: `typed` was just read from the ACL and is non-null, non-aliasing memory
            // owned by that descriptor.
            let allowed = unsafe { &*typed };
            if allowed.Header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            if u32::from(allowed.Header.AceFlags) & INHERIT_ONLY_ACE != 0 {
                continue;
            }
            let ace_sid = std::ptr::from_ref(&allowed.SidStart)
                .cast::<c_void>()
                .cast_mut();
            // SAFETY: both SID arguments are valid: the ACE's own SID starts at `SidStart`, and
            // the compared SID is an allocated or profile identity.
            let same_owner = unsafe { EqualSid(sid, ace_sid) };
            if same_owner != 0 && (allowed.Mask & mask) == mask {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Drop for DaclSnapshot {
    fn drop(&mut self) {
        if !self.sd.is_null() {
            // SAFETY: the security descriptor was allocated by `GetNamedSecurityInfoW`, whose
            // documented release is `LocalFree` on the descriptor.
            unsafe { LocalFree(self.sd) };
        }
    }
}

/// One applied ACE, the handle the write was issued through, and the handle on the DACL it
/// replaced.
///
/// Adding an ACE for the AppContainer SID changes an object the operator owns; putting the
/// saved DACL back when the run ends is what stops a failed or interrupted run from leaking a
/// grant that every later process would inherit. `Drop` performs that restore, so the guard is
/// the whole contract: as long as it drops on every path, no ACE outlives the run.
///
/// The handle is part of that contract since t-034: the grant is written and restored through the
/// **same** open on one object, so neither half of the pair ever resolves a path name — and the
/// handle has to outlive the confined child exactly as the ACE does, which is what keeping it in
/// the guard guarantees.
pub struct AclGrantGuard {
    path: PathBuf,
    /// The security descriptor read before the ACE was added; its DACL is what the restore
    /// writes back, so it is kept alive until then.
    saved: DaclSnapshot,
    /// The ACL `SetEntriesInAclW` built for this grant, released on drop.
    applied_dacl: *mut ACL,
    /// The handle [`open_security_handle`] opened for the write, closed by this guard once the
    /// saved DACL is back where it was.
    handle: HANDLE,
    /// Whether the saved DACL was a NULL DACL, which the restore has to put back as such rather
    /// than as an empty ACL.
    saved_null: bool,
    /// `Some` when the grant is on the object but did not propagate to what already exists under
    /// it; the clause is the operator note for that decline (see [`propagate_grant`]).
    propagation: Option<String>,
    /// Non-`Sync` marker: the saved descriptor is only ever read and written on one thread.
    _single_thread: PhantomData<*mut ()>,
}

impl AclGrantGuard {
    /// Apply the ACE `entry` asks for, for the AppContainer SID `app_sid`.
    ///
    /// The guard is returned only when a mutation was really needed: an entry already covered
    /// by an existing ACE ([`AclCoverage::Covered`]) and an object with a NULL DACL
    /// ([`AclCoverage::NoDacl`]) need neither a change nor a restore.
    ///
    /// Read by name, written by handle — the deliberate asymmetry, and it is chosen rather than
    /// incidental (t-034). The DACL **read** is issued by name exactly as it always was, because
    /// it asks only for `READ_CONTROL`: an object this token may read but not rewrite
    /// (`C:\Windows`, `C:\Program Files`) answers the read with the DACL that already carries the
    /// application-package ACEs the coverage check exists to find, and `apply` then does nothing.
    /// The DACL **write** is issued through a handle on the object, because that is the step that
    /// must never touch anything else (see [`open_security_handle`]), and a handle asking for
    /// `WRITE_DAC` is the right thing for *that* call alone. Moving the read to a handle too would
    /// have asked `WRITE_DAC` of the objects above — turning "already covered, nothing to do" into
    /// a refusal on every read-only toolchain entry — which is why the pair is not symmetric while
    /// both halves still address one object. Verification reads by name as well
    /// ([`grant_is_visible`]), so read and verify keep one spelling rule and only the write moved.
    #[must_use = "the guard restores the saved DACL when the run ends, so no applied ACE leaks"]
    pub fn apply(app_sid: PSID, entry: &PolicyPath) -> Result<Option<Self>, WindowsSandboxError> {
        let path = entry.path.as_path();
        let saved = DaclSnapshot::read(path)?;
        let saved_null = saved.is_null();
        if saved_null || saved.covers(path, app_sid, entry.grant)? {
            return Ok(None);
        }
        let mask = mask_for_grant(entry.grant);
        // The trustee names the SID rather than a name: an AppContainer SID is something the
        // account-name lookup would not resolve, so the SID form is the only honest spelling.
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: app_sid as *mut u16,
        };
        let explicit = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: GRANT_ACCESS,
            // Newly created children of the workspace or the temporary directory must inherit
            // the grant, or a build that writes a new directory gets an object the AppContainer
            // cannot enter.
            grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: trustee,
        };
        let mut applied: *mut ACL = std::ptr::null_mut();
        // SAFETY: `explicit` is owned by the call, and the old ACL pointer belongs to the
        // descriptor the guard keeps alive.
        let code = unsafe { SetEntriesInAclW(1, &explicit, saved.dacl(), &mut applied) };
        if code != ERROR_SUCCESS {
            return Err(WindowsSandboxError::AclMutate {
                path: path_text(path),
                reason: format!("SetEntriesInAclW failed: {}", last_win32_error()),
                code,
            });
        }
        // The authoritative write goes through a handle on the object itself (t-034). The
        // name-based call this replaces resolves a *path*, and a call that resolves a path is free
        // to touch everything that path contains: on a real repository root — `target\`, `.git\`,
        // junctions, cloud placeholders, a descendant this token cannot open for the security
        // rights — that was the one step whose refusal (`ERROR_ACCESS_DENIED`) reached
        // `apply_policy_entries` as a rule failure on the **required** entry, i.e. an exit
        // `SANDBOX_REFUSE_EXIT_CODE` on every command the host ran, while an empty fixture root
        // stayed green. A handle has no name to resolve and nothing below it to walk: it writes
        // exactly one object's security descriptor, which is the grant this entry asks for.
        // The handle open can fail — the object may refuse `WRITE_DAC`, or it may no longer be
        // there at all — and at this point the ACL already exists. Ownership of `applied` moves
        // into the guard further below, so a refusal here has to release it on the spot, or the
        // ACL `SetEntriesInAclW` just allocated is stranded (t-040).
        let handle = match open_security_handle(path) {
            Ok(handle) => handle,
            Err(error) => {
                // SAFETY: `applied` is the ACL `SetEntriesInAclW` allocated above, it has not been
                // handed to any object or guard, and `LocalFree` is its documented release.
                unsafe { LocalFree(applied as *mut c_void) };
                return Err(error);
            }
        };
        // SAFETY: `handle` is the handle just opened on this object, and `applied` is the ACL the
        // guard takes ownership of from here on.
        let set = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                applied,
                std::ptr::null(),
            )
        };
        if set != ERROR_SUCCESS {
            // SAFETY: the ACL that was just built is freed here rather than kept, because the
            // object was never changed, and the handle made for the write is released with it.
            unsafe {
                LocalFree(applied as *mut c_void);
                CloseHandle(handle);
            };
            return Err(WindowsSandboxError::AclMutate {
                path: path_text(path),
                reason: format!("SetSecurityInfo failed: {}", last_win32_error()),
                code: set,
            });
        }
        // Pre-existing descendants are reached by propagation and by nothing else, and propagation
        // is best effort: its refusal becomes a note on the guard, never an `Err` from here, so it
        // can never reach the fatal arm of `apply_policy_entries`. See [`propagate_grant`].
        let propagation = propagate_grant(path, applied);
        Ok(Some(Self {
            path: path.to_path_buf(),
            saved,
            applied_dacl: applied,
            handle,
            saved_null,
            propagation,
            _single_thread: PhantomData,
        }))
    }

    /// The path this guard changed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Some` when this grant is written on the object but did not propagate to the objects that
    /// already existed under it: the clause of the operator note about that decline, which
    /// [`apply_policy_entries`] reports on the same channel as every other declined grant.
    #[must_use]
    fn propagation_note(&self) -> Option<&str> {
        self.propagation.as_deref()
    }
}

impl Drop for AclGrantGuard {
    fn drop(&mut self) {
        // Restore the saved DACL, exactly as it was read: a NULL DACL goes back as NULL, because
        // an empty ACL would deny everything instead. The restore runs through the same handle the
        // write ran through — one object, already opened, and no path name to resolve on the way
        // out of a run whose command has long since started writing inside this directory.
        let dacl: *const ACL = if self.saved_null || self.saved.dacl().is_null() {
            std::ptr::null()
        } else {
            self.saved.dacl()
        };
        // SAFETY: `self.handle` is the handle `apply` opened for this guard and kept alive for the
        // run; the ACL is the saved one, still owned by `saved`, which drops after this call.
        let code = unsafe {
            SetSecurityInfo(
                self.handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if code != ERROR_SUCCESS {
            tracing::warn!(
                path = %self.path.display(),
                win32_error = code,
                "failed to restore the DACL this run changed: the AppContainer ACE may still be set"
            );
        }
        // SAFETY: the handle is the one `open_security_handle` made for this guard and nothing else
        // references it, and `SetEntriesInAclW` allocated the applied ACL, which nothing else
        // references either. The saved descriptor is released by `DaclSnapshot::drop`, which runs
        // after this body.
        unsafe {
            CloseHandle(self.handle);
            LocalFree(self.applied_dacl as *mut c_void);
        };
    }
}

/// The right a DACL write needs on the object it is issued against, spelled the way the operator
/// has to name it to `icacls` when the grant is refused.
const REQUIRED_WRITE_RIGHT: &str = "WRITE_DAC";

/// Open one object so that its DACL can be rewritten through a handle instead of through its name.
///
/// Every argument here is a decision, so each is justified:
///
/// * `READ_CONTROL | WRITE_DAC` — the two rights the DACL pair asks of the handle it works
///   through: the read for `READ_CONTROL`, `SetSecurityInfo` for `WRITE_DAC`. Nothing else is
///   requested, so opening the object never asks this token for a right the security calls
///   themselves would not have asked for (a grant must not, for instance, want `SYNCHRONIZE` or
///   the data rights of the object it is only publishing an ACE on).
/// * `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` — this handle is held for the whole
///   run, because the ACE has to outlive the confined child. Refusing to share would make the
///   sandbox hold the workspace (or a temporary directory) closed against the very build, test run
///   or `git` invocation it exists to run, and refusing `DELETE` would change what a rename of the
///   root costs the run for no security gain: the ACE, not the handle, is the enforcement.
/// * `OPEN_EXISTING` — a policy publishes a grant on an object that is already there; it never
///   creates, replaces or truncates anything to do its job.
/// * `FILE_FLAG_BACKUP_SEMANTICS` — the required entry is a **directory**, and `CreateFileW` opens
///   a directory only with this flag; without it the open of a container answers
///   `ERROR_ACCESS_DENIED`/`ERROR_SHARING_VIOLATION`, which would make the sandbox refuse the one
///   entry it cannot lose. Backup/restore *privileges* are deliberately not requested, so the open
///   is judged by the ordinary access check and reports the same refusal the DACL calls did.
fn open_security_handle(path: &Path) -> Result<HANDLE, WindowsSandboxError> {
    let wide_path = WideString::from_path(path);
    // SAFETY: the name is owned by `wide_path` for the duration of the call; null security
    // attributes mean "the default descriptor, not inherited", which an existing object ignores
    // anyway; a null template handle is what the API documents.
    let handle = unsafe {
        CreateFileW(
            wide_path.pcwstr(),
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        let (code, text) = last_win32_error_detail();
        return Err(WindowsSandboxError::AclMutate {
            path: path_text(path),
            reason: format!(
                "CreateFileW({REQUIRED_WRITE_RIGHT} on the object, the handle the DACL write is issued \
                 through) failed: {text}"
            ),
            code,
        });
    }
    Ok(handle)
}

/// Ask the security runtime to push the applied ACE down to the objects that already existed under
/// `path`, and report a refusal as a note rather than as a failure.
///
/// What genuinely needs propagation, precisely: the ACE this policy writes is inheritable
/// (`SUB_CONTAINERS_AND_OBJECTS_INHERIT`), so every object created **after** the grant is given it
/// at creation time and needs nothing from anyone. Two cases are not covered by that:
///
/// * an object that already existed when the grant was written. Its own DACL was materialised
///   earlier, so the only way the new parent ACE reaches it is the security runtime propagating the
///   change — which is exactly what this name-based call asks for, and exactly the step that can
///   fail one descendant at a time on a large tree (an unrenameable `target\` entry, a junction, an
///   offline cloud placeholder, a child this token cannot open).
/// * a descendant whose DACL is **protected** — switched off with `icacls /inheritance:d`, or
///   carried in by a copy from another location. Inheritance into that object is blocked *by the
///   object itself*, so no parent-side mechanism reaches inside it: propagation declines for the
///   tree, and the only thing that opens it again is resetting that descendant's own DACL
///   (`icacls "<descendant>" /reset`), which this run does not presume to do to the operator's files.
///
/// A decline here is therefore a **degraded** run and never a fatal one. The required grant itself
/// is on the object — that is the handle-based write in [`AclGrantGuard::apply`], which *is* fatal
/// when it fails — so the confined command can write everything it creates and everything created
/// from now on inherits. Failing the run over the rest is the difference between one warning on a
/// host and that host exiting `SANDBOX_REFUSE_EXIT_CODE` on every command, which is the failure this
/// module exists to stop.
fn propagate_grant(path: &Path, applied: *const ACL) -> Option<String> {
    let wide_path = WideString::from_path(path);
    // SAFETY: the name is owned by `wide_path`, and `applied` is the ACL the caller has already
    // written to the object itself and which the guard keeps owned until it drops.
    let code = unsafe {
        SetNamedSecurityInfoW(
            wide_path.pcwstr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            applied,
            std::ptr::null(),
        )
    };
    if code == ERROR_SUCCESS {
        return None;
    }
    Some(propagation_note(path, code))
}

/// The operator note for a propagation this policy asked for and did not get: what is missing, why
/// it does not fail the run, and the two commands that show and open whatever blocks it.
///
/// The clause is stored **without** the `warning: ` prefix, because it is read three ways: logged
/// as it stands, printed on the console as a note (prefixed, the `declined_grant_note` shape), and
/// named inside the reason of a partially enforced attempt, where a second "warning" would read as
/// a warning about a warning.
fn propagation_note(path: &Path, code: WIN32_ERROR) -> String {
    let object = path_text(path);
    format!(
        "the AppContainer grant on {object} was written on the object itself but could not be \
         propagated to what already exists under it: SetNamedSecurityInfoW failed (win32 error \
         {code}): {}. Everything created from now on inherits the grant; an object that already \
         existed stays out of reach for the confined command while its own DACL keeps it there — \
         most often a descendant whose inheritance is switched off, which no grant on its parent \
         can cross. Show the DACL with `icacls \"{object}\"`, and open the affected descendant \
         again with `icacls \"{object}\\<name>\" /reset /c /q`.",
        status_text(code),
    )
}

/// The deliberate handling of a failure on the **required** entry (t-034).
///
/// The decision, stated because it is the decision the whole backend hangs on: a genuine access
/// refusal on the required workspace-root grant stays **fatal**, because the mission is fail-closed
/// unless the operator opts out ([`crate::harness::pty::SANDBOX_OPTOUT_ENV`]) and the required
/// entry is the one rule the mission rests on — the workspace is where the confined command has to
/// be able to write. Reporting `PartiallyEnforced` instead would start the command inside a
/// confinement that cannot write its own workspace: every command would then fail on its first
/// write, with a shell error message instead of a diagnosis, which is a worse version of the
/// refusal this is trying to avoid. What propagation degrades to a note (see
/// [`propagate_grant`]), the required grant itself may not: that failure is on the object this run
/// was asked to grant, so it is terminal *and* actionable.
///
/// Being actionable is the half `is_access_refusal()` pays for: it says "this token may not rewrite
/// that DACL", which is a sentence the operator can do something about, so the reason is rewritten
/// to name the right, the object, where the failure happened, and the commands that show and fix
/// it. Every other code (a name that does not resolve, a path that is too long, an object with no
/// security at all) is returned as it came: those are not this case, and dressing them up in
/// `icacls` advice would send the operator to a lever that cannot move them.
fn required_grant_refusal(entry: &PolicyPath, err: WindowsSandboxError) -> WindowsSandboxError {
    if !err.is_access_refusal() {
        return err;
    }
    let object = {
        let named = err.path();
        if named.is_empty() {
            path_text(&entry.path)
        } else {
            named.to_string()
        }
    };
    let guidance = required_write_guidance(&object);
    match err {
        WindowsSandboxError::AclMutate { path, reason, code } => WindowsSandboxError::AclMutate {
            path,
            reason: format!("{reason} — {guidance}"),
            code,
        },
        WindowsSandboxError::AclRead { path, reason, code } => WindowsSandboxError::AclRead {
            path,
            reason: format!("{reason} — {guidance}"),
            code,
        },
        // `is_access_refusal()` is exact about the variants that carry a code, so this arm is
        // unreachable for a refusal; spelled out rather than `other => other` folded in, so a
        // future variant that reports a refusal cannot lose the guidance silently.
        other => other,
    }
}

/// The operator-facing half of a refused required grant: the right this run needed, where the
/// failure happened, the commands that show and fix it, and the opt-out that runs the command
/// unsandboxed anyway.
#[must_use]
fn required_write_guidance(path: &str) -> String {
    let opt_out = crate::harness::pty::SANDBOX_OPTOUT_ENV;
    format!(
        "this failure is on the required workspace-root grant itself: the propagation to objects \
         that already existed under it is best effort and can only warn, so nothing here degraded \
         quietly. The right this run needs on {path} is {REQUIRED_WRITE_RIGHT} (together with \
         READ_CONTROL to read the DACL back), because the AppContainer identity of this run is \
         written into that object's DACL. Show who holds it with `icacls \"{path}\"`, then give it \
         to this account from an elevated prompt with `icacls \"{path}\" /grant \
         \"%USERNAME%\":(OI)(CI)F`, or restore the inherited permissions with `icacls \"{path}\" \
         /reset`, and run the command again. {opt_out}=1 runs it unsandboxed."
    )
}

/// The one-line note for a grant this policy asked for and did not get: the entry, the polarity it
/// was asked for, and the reason. The shape is the one `sandbox::extra_path_skip_note` uses for an
/// operator-supplied extra path, so a skipped rule reads the same way on every backend.
fn declined_grant_note(entry: &PolicyPath, reason: impl std::fmt::Display) -> String {
    let polarity = match entry.grant {
        Grant::ReadWrite => "read-write",
        Grant::ReadOnly => "read-only",
    };
    format!(
        "warning: ignoring sandbox {polarity} path {}: {reason}",
        path_text(&entry.path)
    )
}

/// What one run of [`apply_policy_entries`] produced: the guards that restore what changed, plus
/// the optional read-write entries (the temporary directories) whose grant was asked for and not
/// got, named by path, plus the entries whose grant is on the object but did not reach what already
/// existed under it.
struct PolicyApplication {
    /// The guards for every ACE that really was added, in application order.
    guards: Vec<AclGrantGuard>,
    /// One entry per read-write entry that is **not** granted, in the order it was declined.
    /// Read-only entries that declined are not listed here: losing a toolchain directory does not
    /// narrow what the confinement can *write*, and the confinement itself is unaffected.
    declined_readwrite: Vec<String>,
    /// One clause per entry whose grant was written on the object itself but could not be
    /// propagated to the objects that already existed under it, in application order. Every entry
    /// in this list is a **degraded** run, never a failed one: see [`propagate_grant`].
    propagation_notes: Vec<String>,
}

/// Whether the grant `entry` asks for is visible on the object it names, using the same snapshot
/// logic as [`AclCoverage`]: a NULL DACL covers everything, and every other object answers for
/// itself.
fn grant_is_visible(app_sid: PSID, entry: &PolicyPath) -> Result<bool, WindowsSandboxError> {
    let snapshot = DaclSnapshot::read(&entry.path)?;
    if snapshot.is_null() {
        return Ok(true);
    }
    snapshot.covers(&entry.path, app_sid, entry.grant)
}

/// Report one declined read-write entry the way every other skipped entry is reported: the note on
/// stderr and through `tracing`, and the path recorded as declined.
fn decline_read_write_entry(
    declined: &mut Vec<String>,
    entry: &PolicyPath,
    reason: impl std::fmt::Display,
) {
    let note = declined_grant_note(entry, reason);
    tracing::warn!("{note}");
    eprintln!("{note}");
    declined.push(path_text(&entry.path));
}

/// Apply every entry of the table, and hand back the guards that restore what changed together
/// with the entries that declined.
///
/// The classification follows the fail-closed contract, with one deliberate distinction. The
/// **required** entry — the workspace root, the one entry the shared policy list insists on — must
/// be granted or the policy fails outright, because the policy that can then be enforced is not
/// the policy that was asked for. Every other entry is optional: a read-only entry whose mutation
/// is refused keeps the confinement and loses one toolchain directory, and a read-write entry
/// (a temporary directory) whose mutation is refused keeps the confinement and loses one write
/// target. Both are reported as a skip note plus a warning — the same shape an operator-supplied
/// extra path that cannot become a rule takes on a Landlock host — and the read-write decline is
/// additionally recorded, because it is what turns the attempt from `FullyEnforced` into
/// `PartiallyEnforced` rather than an exit for every command on that host.
///
/// The third outcome class is neither a decline nor a failure: a grant that was written on the
/// object itself but could not be pushed down onto the objects that already existed under it. That
/// is a degraded run, reported as a note, and never terminal — see [`propagate_grant`] for the one
/// case that needs propagation at all.
fn apply_policy_entries(
    app_sid: PSID,
    table: &WindowsPolicyTable,
) -> Result<PolicyApplication, WindowsSandboxError> {
    let mut guards: Vec<AclGrantGuard> = Vec::new();
    let mut declined_readwrite: Vec<String> = Vec::new();
    let mut propagation_notes: Vec<String> = Vec::new();
    for entry in &table.entries {
        match AclGrantGuard::apply(app_sid, entry) {
            Ok(Some(guard)) => {
                if let Some(note) = guard.propagation_note() {
                    // The authoritative grant is on the object; only pre-existing descendants with
                    // a protected DACL missed it. Warn the operator the same way a declined grant
                    // is reported and keep going.
                    let note = note.to_owned();
                    tracing::warn!("{note}");
                    eprintln!("warning: {note}");
                    propagation_notes.push(note);
                }
                guards.push(guard);
            }
            Ok(None) => {}
            Err(err) if !entry.required => {
                // A non-required refusal: the confinement stands and one entry is simply not
                // granted. The re-entry child has no logging subscriber yet, so the operator
                // also sees the note on stderr (the `extra_path_skip_note` precedent).
                let note = declined_grant_note(entry, &err);
                tracing::warn!("{note}");
                eprintln!("{note}");
                if entry.grant == Grant::ReadWrite {
                    declined_readwrite.push(path_text(&entry.path));
                }
            }
            Err(err) => {
                // The required entry (the workspace root) failed. Fail-closed is deliberate here —
                // see [`required_grant_refusal`] for why a `PartiallyEnforced` answer would be the
                // worse failure — but the refusal is rewritten so it names the right, the object and
                // the commands that fix it.
                return Err(required_grant_refusal(entry, err));
            }
        }
    }
    Ok(PolicyApplication {
        guards,
        declined_readwrite,
        propagation_notes,
    })
}

// ---------------------------------------------------------------------------
// launch: the confined child process
// ---------------------------------------------------------------------------
//
// Everything above this line is a promise about objects; this is the half that acts on it. The
// confinement identity exists and the ACEs are on the objects, so the one thing left is to create
// the process that carries the identity: `CreateProcessW` with
// the real name of the attribute is `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`
// (value 131081), whose value is the `SECURITY_CAPABILITIES` block
// `AppContainerProfileGuard::security_capabilities` builds. The child created that way is a member
// of the AppContainer whose SID the policy granted, and so is everything below it.
//
// Three details decide whether such a launch works at all, and all three are visible below:
//
// * the attribute list has to be sized by the API itself, in two passes, and deleted on every exit
//   path — `AttributeListGuard`;
// * the profile and the applied ACEs must still be alive when `CreateProcessW` runs, and must not
//   be released before the child has exited — that is the `WindowsPolicySetup` hand-off, and
//   `take_windows_policy_setup` is how the launch layer picks it up;
// * the std handles of this process are console handles on the interactive (ConPTY) path and pipe
//   or file handles on the one-shot path, and the two need different wiring — `stdio_state`.

/// The command the enforcement probe runs: the shell's own `exit`, so the probe creates a real
/// confined process, asks it for nothing, and knows exactly which status a healthy launch reports.
const PROBE_COMMAND: &str = "exit 0";

/// The crash statuses that say the confined process could not be created *inside* the container
/// rather than ran a command that happened to fail: an AppContainer that cannot map its own image,
/// reach its DLLs or enter its own directory dies with one of these before the shell runs anything.
///
/// Spelled as the `NTSTATUS` values `Win32::Foundation` gives them; `STATUS_IN_PAGE_ERROR` belongs
/// on the list because a failed image page-in is an access failure wearing a different mask.
const CONFINEMENT_FAILURE_STATUSES: &[u32] = &[
    STATUS_ACCESS_VIOLATION as u32,
    STATUS_ACCESS_DENIED as u32,
    STATUS_IN_PAGE_ERROR as u32,
    STATUS_DLL_NOT_FOUND as u32,
    STATUS_ENTRYPOINT_NOT_FOUND as u32,
];

/// The shell program the confined launch resolves, from inputs a test can pin.
///
/// The first half mirrors `pty.rs::windows_cmd_program`: a trimmed `%COMSPEC%` when it names
/// something, and an empty or blank value never wins. The difference is the fallback. There, the
/// bare token `cmd.exe` is resolved through `search_path` by portable-pty; here the program token
/// is the first token of `lpCommandLine` with `lpApplicationName` left null, and a confined child
/// is exactly the case where a PATH search can fail, because the search itself needs
/// read-and-execute on every directory it searches. The fallback therefore spells the absolute
/// path (`%SystemRoot%\System32\cmd.exe`, falling back to `C:\Windows\System32\cmd.exe` when
/// `SystemRoot` is unset), which needs no search at all.
#[must_use]
fn shell_program(comspec: Option<&str>, system_root: Option<&str>) -> String {
    if let Some(value) = comspec.map(str::trim).filter(|value| !value.is_empty()) {
        return value.to_owned();
    }
    let root = system_root
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("C:\\Windows");
    let root = root.trim_end_matches(['\\', '/']);
    format!("{root}\\System32\\cmd.exe")
}

/// Quote one program token for `lpCommandLine` with the Windows `QUOTE_AND_ESCAPE` rules: the token
/// is wrapped in quotes when it needs them, a quote inside it is escaped with a backslash after
/// doubling the backslash run that precedes it, and a trailing backslash run is doubled so that it
/// does not eat the closing quote.
///
/// A path with a space in it — a `%COMSPEC%` stored under `C:\Program Files…`, say — reaches
/// `CreateProcessW` as one token only when it is quoted this way. Unquoted it splits, and the
/// launch either fails with `ERROR_FILE_NOT_FOUND` or starts a different program altogether.
#[must_use]
fn quote_command_token(value: &str) -> String {
    let needs_quotes = value
        .chars()
        .any(|ch| matches!(ch, '"' | '\\') || ch.is_whitespace());
    if !needs_quotes {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len() + 8);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            // Escape the quote, and double the backslash run in front of it so that the escape is
            // not mistaken for part of that run.
            '"' => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push('\\');
                out.push('"');
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// The complete `lpCommandLine` of one confined launch: `<program> /C <command>`, or — when the
/// payload *is* an interactive-shell request — the bare shell on its own.
///
/// HOW THE INTERACTIVE SHELL SURVIVES THE RE-ENTRY (the decision, in one paragraph). The unix
/// re-entry child runs its payload through `sh -c <payload>` and a payload that is just `sh`
/// therefore ends up as a nested shell reading the stdin it inherited: the confinement applies and
/// the session stays interactive; the unix path neither refuses nor skips such a request. The
/// Windows re-entry has to land on the same rule, so rule R1 of `pty::windows_shell_argv` is
/// carried through rather than lost: `pty::reentry_handoff` classifies the payload, an interactive
/// shell request is launched as the bare shell (no `/C` and no payload), and a normal payload
/// keeps `<program> /C <payload>`. The alternative — adding `/C` back — is what the gap was: confined
/// `cmd.exe /C cmd` starts `cmd.exe` looking for a program named `cmd`, ends the session, and
/// leaves an interactive request looking like a failure. The one-shot path keeps refusing a
/// payload-less shell for every decision, so only a PTY run can reach this branch.
#[must_use]
fn confined_command_line(program: &str, payload: &str) -> String {
    // The program token is quoted per [`quote_command_token`]; the payload is appended
    // **verbatim**, which is what `windows_shell_argv` rule R5 promises: what follows `/C` is
    // `cmd.exe`'s own command line, and `cmd.exe` re-parses it with its own metacharacters
    // (`&`, `|`, `>`, `^`, `"`). Passing that payload through the MSVCRT escaping a real argv
    // would need (what portable-pty's `append_quoted` does) would change what the confined shell
    // reads — `echo "hi there"` included — so the command reaches it byte for byte.
    if crate::harness::pty::reentry_handoff(payload, crate::harness::pty::SpawnFamily::Windows)
        == crate::harness::pty::ReentryHandoff::InteractiveShell
    {
        // The bare form decides *which* shell runs by the same two rules the spawn side uses:
        // R2 (`cmd`/`cmd.exe`) takes the program token the launch already resolved, which is an
        // absolute path because a confined child cannot count on a PATH search; R3
        // (`powershell`/`powershell.exe`) is the caller's own token, trimmed, because
        // substituting `%COMSPEC%` here would run a different shell than the one asked for.
        let token = if crate::harness::pty::is_windows_cmd_shell(payload) {
            program.to_owned()
        } else {
            payload.trim().to_owned()
        };
        return quote_command_token(&token);
    }
    let quoted = quote_command_token(program);
    format!("{quoted} /C {payload}")
}

/// The `PROC_THREAD_ATTRIBUTE_LIST` of one launch, sized by the API and released on drop.
///
/// The two-pass size query is what the API requires: the first call, with a null list, is the
/// documented way to ask how many bytes the list needs, and it reports that number while failing.
/// The second call initialises a buffer allocated from that number. The guard is not decoration:
/// everything between the second call and `CreateProcessW` is a `?`, and an attribute list that is
/// never deleted is a leak the process carries until it exits.
struct AttributeListGuard {
    /// The backing bytes, spelled as `u64` so that the 8-byte alignment a list holding pointers
    /// needs on x64 is guaranteed by the element type rather than by luck.
    buffer: Vec<u64>,
    /// The byte count the API reported; zero means the list was never initialised.
    size: usize,
    /// Non-`Sync` marker: the list is written and read on the launching thread only.
    _single_thread: PhantomData<*mut ()>,
}

impl AttributeListGuard {
    /// Query the size, then allocate and initialise a list with room for `count` attributes.
    fn new(count: u32) -> Result<Self, WindowsSandboxError> {
        let mut size = 0usize;
        // SAFETY: the size-query pass hands a null list and a valid out-parameter; the documented
        // shape of that call is a failure that writes the required size into `size`.
        let queried =
            unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        let _ = queried;
        if size == 0 {
            let (code, text) = last_win32_error_detail();
            return Err(WindowsSandboxError::AttributeList {
                reason: format!(
                    "the size query for a {count}-attribute list reported 0 bytes (query pass: {text})"
                ),
                code,
            });
        }
        let mut buffer = vec![0u64; size.div_ceil(std::mem::size_of::<u64>())];
        // SAFETY: `buffer` owns at least `size` bytes with the alignment the list needs, and the
        // out-parameter is a valid pointer.
        let initialised = unsafe {
            InitializeProcThreadAttributeList(
                buffer.as_mut_ptr().cast::<c_void>(),
                count,
                0,
                &mut size,
            )
        };
        if initialised == 0 {
            let (code, text) = last_win32_error_detail();
            return Err(WindowsSandboxError::AttributeList {
                reason: format!(
                    "InitializeProcThreadAttributeList({count} attributes, {size} bytes) failed: {text}"
                ),
                code,
            });
        }
        Ok(Self {
            buffer,
            size,
            _single_thread: PhantomData,
        })
    }

    /// Attach one attribute value, sized by the caller.
    ///
    /// The value is copied into the list, so the caller only has to keep it alive for this call,
    /// while everything the value *points at* — the SIDs behind a `SECURITY_CAPABILITIES` block,
    /// owned here by the profile guard — has to stay valid until `CreateProcessW` has returned.
    fn set_raw(
        &mut self,
        attribute: u32,
        value: *const c_void,
        size: usize,
    ) -> Result<(), WindowsSandboxError> {
        let list = self.buffer.as_mut_ptr().cast::<c_void>();
        // SAFETY: the list was initialised by `Self::new` into this buffer, and the value names a
        // readable object of exactly `size` bytes.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                attribute as usize,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if updated == 0 {
            let (code, text) = last_win32_error_detail();
            return Err(WindowsSandboxError::AttributeList {
                reason: format!("UpdateProcThreadAttribute({attribute}) failed: {text}"),
                code,
            });
        }
        Ok(())
    }

    /// Hand the confinement declaration to `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` (131081).
    fn set_security_capabilities(
        &mut self,
        capabilities: &SECURITY_CAPABILITIES,
    ) -> Result<(), WindowsSandboxError> {
        self.set_raw(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            std::ptr::from_ref(capabilities).cast::<c_void>(),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        )
    }

    /// Whitelist the handles the child may inherit, through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
    /// (131074, the value verified in `.marmel/research/windows_appcontainer_api.md`).
    fn set_handle_list(&mut self, handles: &[HANDLE]) -> Result<(), WindowsSandboxError> {
        self.set_raw(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            handles.as_ptr().cast::<c_void>(),
            std::mem::size_of_val(handles),
        )
    }

    /// The pointer `STARTUPINFOEXW::lpAttributeList` wants, valid while `self` lives.
    fn as_list(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast::<c_void>()
    }
}

impl Drop for AttributeListGuard {
    fn drop(&mut self) {
        if self.size == 0 {
            return;
        }
        // SAFETY: the list was initialised into this buffer, and the heap block a `Vec` owns is
        // never moved by moving the `Vec`, so this is the same pointer `Initialize` wrote.
        unsafe { DeleteProcThreadAttributeList(self.buffer.as_mut_ptr().cast::<c_void>()) };
    }
}

/// The process and thread handles of a confined child, closed as a unit on drop.
///
/// The two handles belong to one launch, and a launch that fails halfway is the path that leaks
/// exactly one of them. Neither is ever closed twice, and a handle the launch never produced
/// (`null`) is not closed at all.
struct ChildHandles {
    process: HANDLE,
    thread: HANDLE,
}

impl ChildHandles {
    /// Adopt the handles `CreateProcessW` reported.
    const fn new(info: &PROCESS_INFORMATION) -> Self {
        Self {
            process: info.hProcess,
            thread: info.hThread,
        }
    }

    const fn process(&self) -> HANDLE {
        self.process
    }
}

impl Drop for ChildHandles {
    fn drop(&mut self) {
        for handle in [self.process, self.thread] {
            if handle.is_null() {
                continue;
            }
            // SAFETY: these are the two handles the launch produced and nothing else references.
            let closed = unsafe { CloseHandle(handle) };
            if closed == 0 {
                tracing::warn!(
                    "failed to close a handle of the confined child process: {}",
                    last_win32_error()
                );
            }
        }
    }
}

/// The std handles of this process, and what kind they are.
///
/// The kind matters more than the values: the two spawn pipelines of `pty.rs` hand their child a
/// different *kind* of standard stream, and the confined launch has to follow whichever one this
/// process is actually in.
struct StdioState {
    /// The std handles of this process, in `stdin`, `stdout`, `stderr` order.
    handles: [HANDLE; 3],
    /// Whether every one of them is a **console** handle, which is the shape of the interactive
    /// re-entry child: `portable-pty` hands the ConPTY child its standard streams as the
    /// pseudo-console and spawns it with `bInheritHandles = FALSE`, so those handles are console
    /// handles all the way down.
    all_console: bool,
}

/// Whether `handle` names a console input or output buffer rather than a pipe or a file.
fn is_console_handle(handle: HANDLE) -> bool {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let mut mode = 0u32;
    // SAFETY: `GetConsoleMode` writes one mode value into a valid pointer, and reports failure for
    // a handle that is not a console at all rather than answering wrongly about it.
    unsafe { GetConsoleMode(handle, &mut mode) != 0 }
}

/// Read the three std handles once, and ask each one what it is.
///
/// This is the decision the interactive path lives or dies by, so it is worth stating plainly: the
/// re-entry child of an interactive run is itself inside a ConPTY, so its standard streams are the
/// console. The confined launch must then **not** set `STARTF_USESTDHANDLES` and must not inherit
/// handles at all, because the confined grandchild is meant to inherit that same console the way
/// its parent did — which is how its output reaches the PTY reader in the first place. Only when a
/// std handle is *not* a console handle (the one-shot path, where standard streams are pipes or
/// files, and a null or invalid handle counts as not-a-console here) does this module hand handles
/// to the child.
fn stdio_state() -> StdioState {
    let mut handles = [std::ptr::null_mut(); 3];
    for (slot, which) in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        .enumerate()
    {
        // SAFETY: `GetStdHandle` is given one of the documented sentinels and returns the handle of
        // that stream, allocating nothing.
        handles[slot] = unsafe { GetStdHandle(which) };
    }
    let all_console = handles.iter().all(|handle| is_console_handle(*handle));
    StdioState {
        handles,
        all_console,
    }
}

// ---------------------------------------------------------------------------
// handle capability: an ACE on the object the child inherits, not just on the directories
// ---------------------------------------------------------------------------

/// The rights a confined child asks of a standard stream it inherits: read and write on the
/// object itself, its attributes, and the right a reader or writer waits on.
const STD_HANDLE_ACCESS: u32 =
    FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE;

/// The rights the handle that reads and rewrites a std object's DACL has to carry.
///
/// `GetSecurityInfo` requires `READ_CONTROL` and `SetSecurityInfo` requires `WRITE_DAC` — on the
/// **calling handle**, not on the object behind it. The std handles of this process are created
/// outside this crate (the one-shot stdio of a re-entry run is the pair of pipes the plumbing behind
/// `std::process::Command`'s `Stdio::piped()` / `Stdio::null()` builds), and their access mask names
/// only what the stream itself needs. Asking the security calls to work through such a handle
/// answers `ERROR_ACCESS_DENIED` (`os error 5`), which is what this grant path used to report.
const STD_HANDLE_SECURITY_ACCESS: u32 = READ_CONTROL | WRITE_DAC;

/// Copy one std handle into this process with [`STD_HANDLE_SECURITY_ACCESS`] requested, so the DACL
/// read and the DACL rewrite can happen at all.
///
/// `DuplicateHandle` re-runs the access check against the object's own descriptor, so the copy
/// carries the standard security rights the caller already has on that object. The object is the
/// same object: the grant the copy writes is the grant the **original** std handle is later checked
/// against, which is the handle the confined child reads and writes through.
///
/// That same rule is why this copy is not a way to *obtain* rights the object never granted: for a
/// stream object whose descriptor concedes only the rights of a stream, the duplicate is judged by
/// that descriptor too, and the security calls go on answering `ERROR_ACCESS_DENIED`. No
/// duplication flag turns a foreign handle into a security-capable one — the object's descriptor is
/// the only lever — which is what `test_a_foreign_handle_refuses_the_security_read_and_duplication_cannot_repair_it`
/// pins, and why the grant below is warn-only in production and test-owned in the tests.
///
/// The original is deliberately *not* replaced for the launch: the access mask the child inherits is
/// the source handle's, and only the source names `FILE_READ_DATA` and `FILE_WRITE_DATA` — a
/// duplicate that asked for the security rights alone would hand the confined child a std handle it
/// could neither read from nor write to. The SQOS options are left out on purpose: this copy is made
/// inside the same process for the security calls only, and nothing here impersonates anything.
fn duplicate_for_security(handle: HANDLE) -> Result<HANDLE, WIN32_ERROR> {
    let mut duplicated: HANDLE = std::ptr::null_mut();
    // SAFETY: the out-parameter is a valid pointer, and both process arguments are this process'
    // `-1` pseudo-handle, which `DuplicateHandle` accepts for a copy into the current process.
    let made = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut duplicated,
            STD_HANDLE_SECURITY_ACCESS,
            0,
            0,
        )
    };
    if made == 0 {
        return Err(unsafe { GetLastError() });
    }
    Ok(duplicated)
}

/// One trustee naming a SID, built by the API rather than spelled by hand:
/// `BuildTrusteeWithSidW` fills the SID form (`TRUSTEE_IS_SID`, name = the SID pointer) that an
/// AppContainer identity needs, because the name lookup a `TRUSTEE_IS_NAME` trustee would run
/// cannot resolve one.
fn trustee_for_sid(sid: PSID) -> TRUSTEE_W {
    let mut trustee = TRUSTEE_W {
        pMultipleTrustee: std::ptr::null_mut(),
        MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
        TrusteeForm: TRUSTEE_IS_SID,
        TrusteeType: TRUSTEE_IS_UNKNOWN,
        ptstrName: std::ptr::null_mut(),
    };
    // SAFETY: the trustee block is owned by this call and the SID is an allocated identity; the
    // API writes the name/form fields of the block and allocates nothing else.
    unsafe { BuildTrusteeWithSidW(&mut trustee, sid) };
    trustee
}

/// The principals a std object has to name before a confined child may use it.
///
/// The AppContainer SID of this run's profile is the precise identity of the confined child, and
/// is what the file half of the policy grants too. The built-in `ALL APPLICATION PACKAGES`
/// collection SID (`S-1-15-2-2`, the same group the read-only file coverage check accepts as
/// already covering a toolchain directory) is added alongside it because a std handle is inherited
/// by the **whole** confined tree rather than by one process, and any derived AppContainer identity
/// below it — a restricted AppContainer child, or a nested re-entry — belongs to that group rather
/// to this profile. That is the one coarse principal this path accepts, and it is bounded to
/// handles this process already owns: unlike a directory grant, it does not widen what any other
/// process on the machine may open.
#[must_use]
fn std_handle_trustees(app_sid: PSID, all_packages_sid: PSID) -> Vec<EXPLICIT_ACCESS_W> {
    [app_sid, all_packages_sid]
        .into_iter()
        .map(|sid| EXPLICIT_ACCESS_W {
            grfAccessPermissions: STD_HANDLE_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            // A standard stream is an object, not a container: an ACE names what it gives this
            // object, and nothing below a handle can inherit it.
            grfInheritance: NO_INHERITANCE,
            Trustee: trustee_for_sid(sid),
        })
        .collect()
}

/// One inherited std object, the DACL this run added to it, and the handle to put that back.
struct StdHandleGrant {
    /// The handle this guard works through: the [`duplicate_for_security`] copy made for the
    /// security calls, which the guard closes once the DACL is back where it was. The std handle the
    /// process was given is not closed here — it was never ours to close, and it is the handle the
    /// confined launch hands to the child.
    handle: HANDLE,
    /// The security descriptor `GetSecurityInfo` handed back; its DACL is what the restore writes
    /// back, so the descriptor is kept alive until then and released by the same API the read
    /// documented (`LocalFree`).
    saved_sd: PSECURITY_DESCRIPTOR,
    saved_dacl: *mut ACL,
    /// The ACL `SetEntriesInAclW` built for this grant, released on drop.
    applied: *mut ACL,
    /// Non-`Sync` marker: the saved descriptor is read and written on one thread only.
    _single_thread: PhantomData<*mut ()>,
}

impl Drop for StdHandleGrant {
    fn drop(&mut self) {
        // Restore the DACL exactly as it was read. `SetSecurityInfo` treats a null `pdacl` as "do
        // not touch the DACL", which is why the no-DACL case never produces a guard at all (see
        // [`grant_std_handle`]): a NULL DACL already grants everything, and a guard would then
        // have nothing to write.
        //
        // The child may well have exited before this runs — that is expected and harmless. The
        // confined launch waits for its child, and a std handle the child left behind is read by
        // the parent, which never needed the AppContainer ACE at all.
        let code = unsafe {
            SetSecurityInfo(
                self.handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.saved_dacl as *const ACL,
                std::ptr::null(),
            )
        };
        if code != ERROR_SUCCESS {
            tracing::warn!(
                win32_error = code,
                "failed to restore the DACL of an inherited standard stream: the AppContainer ACE may still be set"
            );
        }
        // SAFETY: `SetEntriesInAclW` allocated the applied ACL, and the saved descriptor came from
        // `GetSecurityInfo`; `LocalFree` is the release both document. The handle closed last is the
        // one this guard made for itself; the std handle of the process is untouched.
        unsafe {
            LocalFree(self.applied as *mut c_void);
            LocalFree(self.saved_sd);
            CloseHandle(self.handle);
        }
    }
}

/// Grant one inherited std handle the rights the confined child needs, and hand back the guard
/// that puts the original DACL back.
///
/// Three cases produce no guard, and none of them is a failure: a handle that is not a real object
/// (`null` or `INVALID_HANDLE_VALUE`, i.e. a std stream this process does not have), an object with
/// **no DACL at all** (a NULL DACL already grants every principal everything, so adding an ACE
/// would change nothing — and the restore would have nothing to write), and an object with no
/// security at all, which answers `ERROR_NO_SECURITY_ON_OBJECT` and is likewise reported as "no
/// grant was possible, and none is needed" rather than as an error.
///
/// The work is done through [`duplicate_for_security`], not through the std handle itself: the std
/// handles of this process are created with the rights of a stream and carry neither `READ_CONTROL`
/// nor `WRITE_DAC`, and `SetSecurityInfo` without the latter answers `ERROR_ACCESS_DENIED` on an
/// object the process owns outright.
fn grant_std_handle(
    handle: HANDLE,
    app_sid: PSID,
) -> Result<Option<StdHandleGrant>, WindowsSandboxError> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let label = format!("standard stream object {handle:p}");
    // The read and the rewrite run through a copy of the std handle that carries the rights the two
    // calls require; the object behind it, and the std handle the launch hands to the child, stay
    // exactly as the process had them. See [`STD_HANDLE_SECURITY_ACCESS`].
    let secured = match duplicate_for_security(handle) {
        Ok(duplicated) => duplicated,
        Err(code) => {
            return Err(WindowsSandboxError::AclMutate {
                path: label,
                reason: format!(
                    "DuplicateHandle(READ_CONTROL | WRITE_DAC on the std handle) failed: {}",
                    last_win32_error()
                ),
                code,
            });
        }
    };
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle names one live object handle, and every out-parameter is a valid pointer
    // the API writes into.
    let read = unsafe {
        GetSecurityInfo(
            secured,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            &mut sacl,
            &mut sd,
        )
    };
    let _ = sacl; // the SACL is not part of this policy
    if read == ERROR_NO_SECURITY_ON_OBJECT {
        // SAFETY: the duplicate is this call's to close, and nothing else was allocated.
        unsafe { CloseHandle(secured) };
        return Ok(None);
    }
    if read != ERROR_SUCCESS {
        // SAFETY: the read allocated nothing this call still needs.
        unsafe { CloseHandle(secured) };
        return Err(WindowsSandboxError::AclRead {
            path: label,
            reason: format!(
                "GetSecurityInfo failed (win32 error {read}): {}",
                status_text(read)
            ),
            code: read,
        });
    }
    if dacl.is_null() {
        // SAFETY: the descriptor came from `GetSecurityInfo`, whose documented release is
        // `LocalFree`; nothing references it, because no grant is applied to a NULL DACL. The
        // duplicate is closed with it: no guard exists to hand it to.
        unsafe {
            LocalFree(sd);
            CloseHandle(secured);
        };
        return Ok(None);
    }
    let all_packages = ApplicationPackagesSid::all()?;
    let entries = std_handle_trustees(app_sid, all_packages.sid());
    let mut applied: *mut ACL = std::ptr::null_mut();
    // SAFETY: `entries` is owned by this call for its whole duration, and the old ACL belongs to
    // the descriptor the guard keeps alive.
    let built =
        unsafe { SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut applied) };
    if built != ERROR_SUCCESS {
        // SAFETY: the descriptor is the only object read here, and it is released unchanged. The
        // handle made for this call is released with it, because no guard takes it over.
        unsafe {
            LocalFree(sd);
            CloseHandle(secured);
        };
        return Err(WindowsSandboxError::AclMutate {
            path: label,
            reason: format!("SetEntriesInAclW failed: {}", last_win32_error()),
            code: built,
        });
    }
    // SAFETY: the handle names the object, and `applied` is the freshly built ACL.
    let set = unsafe {
        SetSecurityInfo(
            secured,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            applied,
            std::ptr::null(),
        )
    };
    if set != ERROR_SUCCESS {
        // SAFETY: neither the ACL that was built nor the descriptor reached the object, so both
        // are released here rather than handed to a guard. The duplicate outlives neither.
        unsafe {
            LocalFree(applied as *mut c_void);
            LocalFree(sd);
            CloseHandle(secured);
        };
        return Err(WindowsSandboxError::AclMutate {
            path: label,
            reason: format!("SetSecurityInfo failed: {}", last_win32_error()),
            code: set,
        });
    }
    Ok(Some(StdHandleGrant {
        handle: secured,
        saved_sd: sd,
        saved_dacl: dacl,
        applied,
        _single_thread: PhantomData,
    }))
}

/// Grant every inherited std handle, keeping only the warnings.
///
/// A std stream whose grant was refused stays unusable by the confined child, and that is reported
/// the way every other capability this backend cannot have is reported: a `tracing::warn!` plus the
/// note on stderr, in the shape of the `extra_path_skip_note` precedent, because the re-entry child
/// has no log subscriber yet. The launch is not failed over it, and it is not treated as a success
/// either: the confined command still runs under the policy it was given, and the lost stream is
/// what the operator is told about.
fn grant_std_handles(handles: &[HANDLE; 3], app_sid: PSID) -> Vec<StdHandleGrant> {
    let mut grants = Vec::new();
    for (index, handle) in handles.iter().enumerate() {
        match grant_std_handle(*handle, app_sid) {
            Ok(Some(guard)) => grants.push(guard),
            Ok(None) => {}
            Err(err) => {
                let note = format!(
                    "warning: could not give the confined child access to standard stream {}: {err}",
                    index + 1
                );
                tracing::warn!("{note}");
                eprintln!("{note}");
            }
        }
    }
    grants
}

/// Create the confined process, wait for it, and report the exit code it left behind, as the raw
/// `u32` `GetExitCodeProcess` wrote.
///
/// The two helpers of the PTY path are reused rather than reimplemented: `pty.rs::windows_shell_argv`
/// yields `(COMSPEC-or-cmd.exe, ["/C", command-verbatim])`, and `pty.rs::windows_child_cwd` strips
/// the `\\?\` / `\\?\UNC\` namespace prefix `canonicalize()` produces, because `cmd.exe` reads a
/// leading `\\` as a UNC location and then quietly runs in a different directory. The confined
/// launch uses that same cwd spelling for that same reason.
fn launch_confined(
    setup: &WindowsPolicySetup,
    cwd: &Path,
    command: &str,
) -> Result<u32, WindowsSandboxError> {
    if command.trim().is_empty() {
        return Err(WindowsSandboxError::RuleSetup {
            path: path_text(cwd),
            reason: "the command to run inside the AppContainer is empty".to_string(),
        });
    }

    // The child's cwd, spelled the way the PTY path spells it, for the reason just documented.
    let child_cwd = crate::harness::pty::windows_child_cwd(cwd);
    if !child_cwd.is_dir() {
        return Err(WindowsSandboxError::RuleSetup {
            path: path_text(&child_cwd),
            reason: "the working directory of the confined command does not exist".to_string(),
        });
    }
    let cwd_wide = WideString::from_path(&child_cwd);

    let program = shell_program(
        std::env::var("COMSPEC").ok().as_deref(),
        std::env::var("SystemRoot").ok().as_deref(),
    );
    let line = confined_command_line(&program, command);
    // A NUL anywhere in the line is a truncation waiting to happen: `lpCommandLine` is a UTF-16
    // buffer, and the API stops reading at the first NUL it meets.
    if line.contains('\0') {
        return Err(WindowsSandboxError::RuleSetup {
            path: path_text(&child_cwd),
            reason: "the launch command line contains a null character".to_string(),
        });
    }
    let mut line_wide = WideString::new(&line);

    let stdio = stdio_state();
    // The std wiring, decided once and applied consistently with that decision:
    //
    // * every std handle is a console handle (the ConPTY path): no `STARTF_USESTDHANDLES`, no handle
    //   list, `bInheritHandles = FALSE`, and the confined child inherits the console itself;
    // * the std handles are pipes or files (the one-shot path): `STARTF_USESTDHANDLES` with those
    //   handles, `bInheritHandles = TRUE`, and `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` naming exactly
    //   them, which also keeps every *other* inheritable handle of this process out of the sandbox.
    //
    // Making a handle visible to the AppContainer child is not the same as letting the child use
    // the object behind it: the access check for an inherited kernel object is re-run in the
    // child's token context, and the anonymous pipes the one-shot path creates carry the default
    // DACL of this process's token, which names the user and not the AppContainer identity. So the
    // std objects are granted that identity before the launch (see [`grant_std_handles`]), and the
    // guards returned here are what put the original DACLs back — only after the child has exited,
    // which is the whole point of binding them here rather than in the call.
    let inherited: Option<[HANDLE; 3]> = if stdio.all_console {
        None
    } else {
        Some(stdio.handles)
    };
    let std_grants = match &inherited {
        Some(handles) => grant_std_handles(handles, setup.profile.sid()),
        None => Vec::new(),
    };

    let capabilities = setup.profile.security_capabilities();
    let mut attrlist = AttributeListGuard::new(1 + u32::from(inherited.is_some()))?;
    attrlist.set_security_capabilities(&capabilities)?;
    if let Some(handles) = &inherited {
        attrlist.set_handle_list(handles)?;
    }

    let (std_input, std_output, std_error) = match inherited {
        Some(handles) => (handles[0], handles[1], handles[2]),
        None => (
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ),
    };
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            // `cb` has to name the extended block: with `EXTENDED_STARTUPINFO_PRESENT` set,
            // `CreateProcessW` reads the attribute list only when this is its size.
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            lpReserved: std::ptr::null_mut(),
            lpDesktop: std::ptr::null_mut(),
            lpTitle: std::ptr::null_mut(),
            dwX: 0,
            dwY: 0,
            dwXSize: 0,
            dwYSize: 0,
            dwXCountChars: 0,
            dwYCountChars: 0,
            dwFillAttribute: 0,
            dwFlags: if inherited.is_some() {
                STARTF_USESTDHANDLES
            } else {
                0
            },
            wShowWindow: 0,
            cbReserved2: 0,
            lpReserved2: std::ptr::null_mut(),
            hStdInput: std_input,
            hStdOutput: std_output,
            hStdError: std_error,
        },
        lpAttributeList: attrlist.as_list(),
    };

    let mut info = PROCESS_INFORMATION {
        hProcess: std::ptr::null_mut(),
        hThread: std::ptr::null_mut(),
        dwProcessId: 0,
        dwThreadId: 0,
    };
    // `lpApplicationName` stays null: the program is the first token of the command line, which is
    // a mutable `PWSTR` the API is allowed to rewrite. `lpenvironment` stays null and
    // `CREATE_UNICODE_ENVIRONMENT` stays unset: the child gets a copy of the environment of this
    // process, which is what keeps `COMSPEC`, `PATH` and the rest usable by a confined `cmd.exe`.
    let spawned = unsafe {
        CreateProcessW(
            std::ptr::null(),
            line_wide.buf.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            if inherited.is_some() { 1 } else { 0 },
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            cwd_wide.pcwstr(),
            &startup.StartupInfo,
            &mut info,
        )
    };
    // The two handles exist the moment the call returns, whether or not it succeeded; taking them
    // over here is what stops every `?` below from leaking one of them.
    let handles = ChildHandles::new(&info);
    if spawned == 0 {
        let code = unsafe { GetLastError() };
        return Err(WindowsSandboxError::Launch {
            reason: format!(
                "CreateProcessW({line}) in {} failed: {}",
                path_text(&child_cwd),
                io::Error::from_raw_os_error(code as i32)
            ),
            code,
        });
    }

    let waited = unsafe { WaitForSingleObject(handles.process(), INFINITE) };
    if waited != WAIT_OBJECT_0 {
        let code = unsafe { GetLastError() };
        return Err(WindowsSandboxError::Launch {
            reason: format!(
                "waiting for the confined process returned {waited} instead of WAIT_OBJECT_0: {}",
                io::Error::from_raw_os_error(code as i32)
            ),
            code,
        });
    }
    let mut raw = 0u32;
    let ok = unsafe { GetExitCodeProcess(handles.process(), &mut raw) };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        return Err(WindowsSandboxError::Launch {
            reason: format!(
                "GetExitCodeProcess of the confined child failed: {}",
                io::Error::from_raw_os_error(code as i32)
            ),
            code,
        });
    }
    // `STILL_ACTIVE` (259) is what `GetExitCodeProcess` reports for a process that is still
    // running. After an infinite wait that is a contradiction about the child, not an exit code, so
    // it is reported as a failure rather than handed back as 259.
    if raw == STILL_ACTIVE as u32 {
        return Err(WindowsSandboxError::Launch {
            reason: "the confined process reported STILL_ACTIVE after the wait".to_string(),
            code: 0,
        });
    }
    // The std-handle grants go back only now, after the wait and the read of the exit code: a DACL
    // restored while the child was still writing would take its write access away mid-stream. Once
    // the wait has ended the child is gone, so releasing here is late enough to be safe and early
    // enough that a launch that never happened still restores what it changed.
    drop(std_grants);
    Ok(raw)
}

/// The exit-code surface: the raw `u32` rendered as the `i32` an exit code is expected to be.
/// NTSTATUS-shaped statuses (`0x8000_0000` and above) wrap to negative numbers, which is exactly how
/// a shell reports them.
#[must_use]
const fn confined_exit_code(raw: u32) -> i32 {
    raw as i32
}

/// Launch `command` confined in the AppContainer of `setup`, wait for it, and return its exit code.
///
/// The setup has to stay alive for the whole call: it owns the profile whose SID is being declared
/// and the ACEs the child is running under.
pub fn spawn_in_appcontainer(
    setup: &WindowsPolicySetup,
    cwd: &Path,
    command: &str,
) -> Result<i32, WindowsSandboxError> {
    launch_confined(setup, cwd, command).map(confined_exit_code)
}

/// Run one confined probe process, and report whether the confinement is real enough to trust.
///
/// This is what keeps the attempt surface from claiming `FullyEnforced` on the strength of a profile
/// and a set of ACEs that nothing has ever used: an AppContainer whose identity Win32 refuses to put
/// on a process, or whose ACEs are on objects the child cannot actually open, either cannot be
/// launched at all or dies with one of the statuses in `CONFINEMENT_FAILURE_STATUSES`. The probe is
/// one process, created once, asking for nothing but `exit 0`.
fn probe_confined_launch(
    setup: &WindowsPolicySetup,
    cwd: &Path,
) -> Result<(), WindowsSandboxError> {
    let raw = match launch_confined(setup, cwd, PROBE_COMMAND) {
        Ok(raw) => raw,
        Err(err) => {
            // The code of the failure *behind* the probe: a probe that never started says what
            // the launch reported, so the log field of a rank-3 refusal names a number and not 0.
            let code = err.win32_code();
            return Err(WindowsSandboxError::Probe {
                reason: format!("the confined probe process could not be launched: {err}"),
                code,
            });
        }
    };
    if CONFINEMENT_FAILURE_STATUSES.contains(&raw) {
        return Err(WindowsSandboxError::Probe {
            reason: format!(
                "the confined probe process died with the status {}, which is a confinement failure rather than a command failure",
                exit_status_text(raw)
            ),
            code: raw,
        });
    }
    if raw != 0 {
        return Err(WindowsSandboxError::Probe {
            reason: format!(
                "the confined probe process ({PROBE_COMMAND}) exited with {}, not the 0 it asked for",
                exit_status_text(raw)
            ),
            code: raw,
        });
    }
    Ok(())
}

/// The exit status a confined child reported, spelled so an `NTSTATUS`-shaped one cannot be read
/// as a small command exit code: from `0x8000_0000` up it is named in hex with its decimal beside
/// it, and below that as the plain decimal number a shell hands back.
#[must_use]
fn exit_status_text(raw: u32) -> String {
    if raw & 0x8000_0000 == 0 {
        raw.to_string()
    } else {
        format!("{raw:#010x} (decimal {})", raw as i32)
    }
}

/// The reason text of a failure that carries a Win32 error, with the code spelled the way the
/// operator and the log grep it: `win32 code = 5`.
///
/// The code survives into the verdict text (rather than only into a log record) because the
/// reason string is the one channel a refusal has to the operator: the re-entry child prints
/// it to stderr, and the harness folds that stderr into the `run_command` result. `0` means no
/// Win32 error is involved, and is deliberately not spelled as a code.
///
/// A status with the high bit set — an `HRESULT` from `CreateAppContainerProfile`, or an
/// `NTSTATUS` a confined child died with — is **not** a small Win32 code, and its decimal
/// rendering (`-2147024891`, `3221225781`) reads like one and is not recognisable in a log or a
/// search. Those are spelled in hex first, with the decimal kept beside them so the two renderings
/// of the same number still line up with the decimal `code` field of the log record.
#[must_use]
fn append_win32_code(reason: String, code: WIN32_ERROR) -> String {
    if code == 0 {
        reason
    } else if code & 0x8000_0000 == 0 {
        format!("{reason} (win32 code = {code})")
    } else {
        format!(
            "{reason} (win32 code = {code:#010x}, decimal {})",
            code as i32
        )
    }
}

/// The two spellings one machine status has to reach the log with: the decimal `code` field
/// tracing records for a number, and the `code_hex` field that makes an `HRESULT` or `NTSTATUS`
/// recognisable without arithmetic (`0x80070005`, never `2147942405` on its own).
#[must_use]
fn log_code_fields(err: &WindowsSandboxError) -> (WIN32_ERROR, String) {
    let code = err.win32_code();
    (code, format!("{code:#010x}"))
}

/// One log record per backend failure, with the fields a diagnosis needs: where in the chain it
/// happened (`step`), what stage failed (`kind`), which object it is about (`path`) and the
/// Win32 error the failing call reported (`code`, plus `code_hex` for the same number).
///
/// The `code` field is the raw status the call reported: a `WIN32_ERROR` for a DACL, SID or launch
/// call, the `HRESULT` of `CreateAppContainerProfile` for a profile failure, and the status the
/// confined child answered with for a probe failure. A numeric field is recorded as decimal, which
/// is why the same record carries `code_hex`: `0x80070005` and `-2147024891` are one status, and an
/// operator reading `code=2147942405` needs to know which of the two to look up. The prose of the
/// failure spells it the same way, so the two views cannot disagree.
///
/// Every refusal class has to appear in the log with its causal chain, because the operator can
/// only be told to look there; the prose of the failure is kept in the message so the two views
/// cannot disagree.
fn log_windows_sandbox_failure(step: &str, err: &WindowsSandboxError) {
    let (code, code_hex) = log_code_fields(err);
    tracing::error!(
        step = step,
        kind = err.kind(),
        path = err.path(),
        code,
        code_hex = code_hex.as_str(),
        "sandbox refusal: {err}"
    );
}

/// Map one backend failure onto the `SandboxAttempt` variant that describes it.
#[must_use]
fn attempt_from_windows_error(err: WindowsSandboxError) -> SandboxAttempt {
    match err {
        WindowsSandboxError::Profile { name, reason, .. } => SandboxAttempt::Unavailable {
            reason: format!("AppContainer profile {name} could not be created: {reason}"),
        },
        WindowsSandboxError::Sid { reason, .. } => SandboxAttempt::Unavailable { reason },
        WindowsSandboxError::RuleSetup { path, reason }
        | WindowsSandboxError::AclRead { path, reason, .. } => {
            SandboxAttempt::RuleFailed { path, reason }
        }
        WindowsSandboxError::AclMutate { path, reason, code } => SandboxAttempt::RuleFailed {
            path,
            reason: append_win32_code(reason, code),
        },
        WindowsSandboxError::AclVerification { path, reason } => {
            SandboxAttempt::EnforcementFailed {
                reason: format!("{path}: {reason}"),
            }
        }
        // Launch-side failures say the same thing about the attempt: the policy was built, but
        // confining something with it did not work, which is an enforcement failure. A probe
        // failure is not given a second copy of its code, because its prose already names the
        // status the child (or the launch behind it) reported — the `AclRead` precedent.
        WindowsSandboxError::AttributeList { reason, .. }
        | WindowsSandboxError::Probe { reason, .. } => SandboxAttempt::EnforcementFailed { reason },
        WindowsSandboxError::Launch { reason, code } => SandboxAttempt::EnforcementFailed {
            reason: append_win32_code(reason, code),
        },
    }
}

/// Convert a `WindowsSandboxError` into the backend-neutral error the re-entry contract speaks:
/// `Unavailable` when no AppContainer identity could be made, `RuleSetup` when the policy that
/// was asked for is not the policy that exists.
impl From<WindowsSandboxError> for SandboxError {
    fn from(value: WindowsSandboxError) -> Self {
        match value {
            WindowsSandboxError::Profile { name, reason, .. } => SandboxError::Unavailable {
                reason: format!("AppContainer profile {name} could not be created: {reason}"),
            },
            WindowsSandboxError::Sid { reason, .. } => SandboxError::Unavailable {
                reason: format!("sid: {reason}"),
            },
            WindowsSandboxError::RuleSetup { path, reason } => {
                SandboxError::RuleSetup { path, reason }
            }
            // `AclRead` now carries the code structurally as well, but its prose already names it
            // (`GetNamedSecurityInfoW failed (win32 error 5) …`), so the verdict text is not
            // inflated with a second copy of the same number. `AclMutate` is the arm that does
            // append, because its prose does not name the code.
            WindowsSandboxError::AclRead {
                path,
                reason,
                code: _,
            } => SandboxError::RuleSetup { path, reason },
            WindowsSandboxError::AclMutate { path, reason, code } => SandboxError::RuleSetup {
                path,
                reason: append_win32_code(reason, code),
            },
            // A verification failure is not a rule that could not be built: the rule is there, and
            // the object says the grant is not. The confinement in force is therefore not the one
            // the policy promised — which is what `Enforcement` spells, and what
            // `attempt_from_windows_error` already says for the same failure. The two mappings have
            // to agree because the attempt surface and this error are one verdict rendered twice.
            WindowsSandboxError::AclVerification { path, reason } => SandboxError::Enforcement {
                reason: format!("{path}: {reason}"),
            },
            WindowsSandboxError::AttributeList { reason, .. }
            | WindowsSandboxError::Probe { reason, .. } => SandboxError::Enforcement { reason },
            WindowsSandboxError::Launch { reason, code } => SandboxError::Enforcement {
                reason: append_win32_code(reason, code),
            },
        }
    }
}

/// The reason a partially enforced run carries: the read-write entries the policy asked for and
/// did not get, each named by path. `None` when nothing was declined, i.e. when the confinement
/// that exists is the whole policy that was asked for.
///
/// The paths are named because the operator has to be able to act on the verdict: an unnamed
/// missing write target is a warning nobody can trace back to a directory.
#[must_use]
fn partial_reason_for(declined_readwrite: &[String]) -> Option<String> {
    if declined_readwrite.is_empty() {
        return None;
    }
    Some(format!(
        "the AppContainer policy granted every required entry but not every optional read-write \
         entry: {} — that write target is missing from the confinement",
        declined_readwrite.join(", ")
    ))
}

/// The reason a run is degraded because a grant stopped at the object it was written on: the
/// entries whose propagation onto pre-existing descendants was refused, each with the clause
/// [`propagation_note`] built for it. `None` when every grant reached the whole tree.
///
/// This is a degraded verdict, never a refusal: the confined command runs, writes what it creates,
/// and inherits from now on. What it cannot touch is only what already existed and keeps its own
/// DACL, which is precisely the thing the operator — not this run — has to reset.
#[must_use]
fn propagation_reason_for(notes: &[String]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    Some(format!(
        "the AppContainer policy wrote every grant it asked for, but {} of them did not reach the \
         objects that already existed under the entry: {} — pre-existing files or directories \
         there may stay out of reach for the confined command until their own DACL is reset \
         (`icacls \"<path>\\<name>\" /reset /c /q`); everything created from now on inherits",
        notes.len(),
        notes.join(" | ")
    ))
}

/// Everything the policy layer has established for one run.
///
/// `t-008` hands [`WindowsPolicySetup::profile`] (and the capability list it owns) to
/// `CreateProcessW`, and has to keep it — together with [`WindowsPolicySetup::grants`] — alive
/// across the launch: deleting the profile or restoring a DACL early takes the confinement away
/// from a command that is still running.
pub struct WindowsPolicySetup {
    /// The fresh AppContainer profile created for this run.
    pub profile: AppContainerProfileGuard,
    /// The ACEs applied for it, in application order; restoring them is how the run ends.
    pub grants: Vec<AclGrantGuard>,
    /// The read-write entries this policy asked for and did not get, named by path in the order
    /// they were declined: a temporary directory whose DACL mutation was refused, or whose applied
    /// grant is not visible in the object it names.
    ///
    /// Empty means the policy is the policy that was asked for. Non-`empty` means the confinement
    /// in force is that policy **minus one write target**, which the attempt surface reports as a
    /// partially enforced run rather than quietly calling full, or failing every command run on
    /// that host.
    pub declined_readwrite: Vec<String>,
    /// The entries whose grant was written on the object itself but could not be propagated to the
    /// objects that already existed under it, each carrying the clause [`propagation_note`] built
    /// for it, in application order.
    ///
    /// Empty means every grant covers its whole subtree. Non-empty means the confinement is the
    /// policy that was asked for **except** for pre-existing objects behind one of these entries,
    /// which is a degraded run (`PartiallyEnforced`) and never a refused one: the entry itself is
    /// granted, so everything the confined command creates is reachable.
    pub propagation_notes: Vec<String>,
}

impl WindowsPolicySetup {
    /// The AppContainer SID, for the probe and for the launch.
    #[must_use]
    pub fn sid(&self) -> PSID {
        self.profile.sid()
    }

    /// The reason of a partially enforced run: the optional read-write entries that are missing
    /// from the confinement that was asked for, and the grants that stopped at the object they were
    /// written on. `None` when every entry landed and reached its tree, i.e. when the confinement is
    /// the whole policy.
    ///
    /// Every entry it names is a path the operator can act on, which is the point of spelling the
    /// reason out rather than keeping the verdict quiet.
    #[must_use]
    pub fn partial_reason(&self) -> Option<String> {
        // Both degradation classes belong in the one verdict, so an operator reading the attempt
        // sees a declined write target and a stalled propagation as one honest sentence instead of
        // the first masking the second.
        let reasons = [
            partial_reason_for(&self.declined_readwrite),
            propagation_reason_for(&self.propagation_notes),
        ];
        let reasons: Vec<String> = reasons.into_iter().flatten().collect();
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }
}

/// Create the profile and apply the policy, keeping every guard alive.
pub fn prepare_windows_policy(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> Result<WindowsPolicySetup, WindowsSandboxError> {
    let root = workspace_root
        .canonicalize()
        .map_err(|err| WindowsSandboxError::RuleSetup {
            path: path_text(workspace_root),
            reason: format!("the workspace root could not be resolved: {err}"),
        })?;
    let table = windows_policy_entries(&root, extra_readonly_paths)?;

    // The notes are printed here rather than where they were built: this is the point at which
    // the attempt is known, and the re-entry child still has no log subscriber.
    for note in &table.notes {
        tracing::warn!("sandbox policy note: {note}");
        eprintln!("warning: {note}");
    }

    let capabilities = CapabilitySids::internet_client()?;
    let profile = AppContainerProfileGuard::create(&root, capabilities)?;
    tracing::info!(
        profile = %profile.name(),
        "created the AppContainer profile for this run"
    );

    let application = apply_policy_entries(profile.sid(), &table)?;
    let mut declined_readwrite = application.declined_readwrite;

    // Every **required** grant must be visible on the object it names, or the confinement the
    // policy promises is not the confinement the child is about to experience. That is the one
    // entry the confinement cannot lose, so a grant missing from it is an enforcement failure
    // rather than a note.
    for entry in table.confinement_critical() {
        if !grant_is_visible(profile.sid(), entry)? {
            return Err(WindowsSandboxError::AclVerification {
                path: path_text(&entry.path),
                reason: "the grant this policy applied is not visible in the object's DACL"
                    .to_string(),
            });
        }
    }

    // The optional read-write entries — the temporary directories — are re-read with the same
    // snapshot logic, and a grant that is not visible there is the same decline as a refused
    // mutation: one write target is missing, the confinement stands. An entry already declined
    // above is not reported a second time.
    for entry in table.optional_read_write_entries() {
        let name = path_text(&entry.path);
        if declined_readwrite.contains(&name) {
            continue;
        }
        match grant_is_visible(profile.sid(), entry) {
            Ok(true) => {}
            Ok(false) => decline_read_write_entry(
                &mut declined_readwrite,
                entry,
                "the grant this policy applied is not visible in the object's DACL",
            ),
            // The re-read itself failed: the entry cannot be confirmed, which is a decline, not a
            // verdict on the confinement.
            Err(err) => decline_read_write_entry(&mut declined_readwrite, entry, &err),
        }
    }

    Ok(WindowsPolicySetup {
        profile,
        grants: application.guards,
        declined_readwrite,
        propagation_notes: application.propagation_notes,
    })
}

thread_local! {
    /// The setup prepared by the policy half of this layer, parked for the launch half.
    ///
    /// `thread_local!` rather than a `static`, because [`WindowsPolicySetup`] is neither `Send` nor
    /// `Sync`: the profile guard and the ACL guards own objects created on the thread that applied
    /// them, and a DACL restored from another thread would race the thread that changed it. That is
    /// not a limitation but the shape of the hand-off — the re-entry child runs its sandbox
    /// handshake on one thread, so exactly one setup is prepared there, parked once, and picked up
    /// once by [`take_windows_policy_setup`].
    static PREPARED_SETUP: RefCell<Option<WindowsPolicySetup>> = const { RefCell::new(None) };
}

/// Park a prepared setup for the launch half, releasing whatever was parked before.
///
/// A second preparation on the same thread means two AppContainer profiles over the same objects:
/// the older profile has to be deleted, and its ACEs restored, before the newer one is used — which
/// is what dropping the replaced setup does. The note is printed on the console, in the style of the
/// `extra_path_skip_note` precedent, because the re-entry child has no log subscriber yet.
fn put_prepared_setup(setup: WindowsPolicySetup) {
    let replaced = PREPARED_SETUP.with(|cell| cell.replace(Some(setup)));
    if replaced.is_some() {
        tracing::warn!(
            "an AppContainer setup was already parked for this run: releasing the older one first"
        );
        eprintln!(
            "warning: an AppContainer setup was already parked for this run; releasing the older one first"
        );
        drop(replaced);
    }
}

/// Pick up the setup prepared by the policy half, in the launch half.
///
/// Returns `None` when nothing was parked: the caller has either prepared a setup (via
/// [`attempt_windows_sandbox_with_setup`]) or given up on confinement. Exactly one of the two per
/// run, which is what makes the hand-off single-shot rather than a race between two profiles.
#[must_use]
pub fn take_windows_policy_setup() -> Option<WindowsPolicySetup> {
    PREPARED_SETUP.with(|cell| cell.take())
}

/// Release a parked setup without running anything confined in it, and report whether one was
/// parked at all.
///
/// The path that needs this is a caller which decided, after the attempt said `FullyEnforced`, not
/// to run a confined command: leaving the setup parked would keep this process's AppContainer SID
/// written into the DACLs of the workspace until exit, instead of restoring them now.
pub fn discard_windows_policy_setup() -> bool {
    let taken = PREPARED_SETUP.with(|cell| cell.take());
    let released = taken.is_some();
    drop(taken);
    released
}

/// The policy judgement and the guard hand-off in one call.
///
/// Returns what the attempt achieved **and** the [`WindowsPolicySetup`] it built: the profile and
/// the applied ACEs have to outlive the judgement, because [`CreateProcessW`] confines the child by
/// the SID in that profile and the ACEs on those objects. Nothing is launched or reverted between
/// the two, so the caller of this function owns the guards (and the drop that restores the
/// DACLs and deletes the profile) from here on.
///
/// The setup is parked in the `PREPARED_SETUP` thread-local and handed straight back, which is
/// what keeps preparation single-shot per run: whoever prepared it once is the only one who can
/// pick it up. The returned attempt is `FullyEnforced` only when the enforcement probe accepted
/// the confinement **and** every entry of the policy it built; it is `PartiallyEnforced` when the
/// probe accepted a policy that had to decline one of its optional read-write entries. Every
/// failure comes back with `None` — no half-applied policy is parked.
#[must_use]
pub fn attempt_windows_sandbox_with_setup(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> (SandboxAttempt, Option<WindowsPolicySetup>) {
    let setup = match prepare_windows_policy(workspace_root, extra_readonly_paths) {
        Ok(setup) => setup,
        Err(err) => {
            // One record per failure stage, with the class, the object and the Win32 code the
            // failing call reported: this is the only place the whole causal chain is written
            // down, because the operator-facing reason line is one line of prose.
            log_windows_sandbox_failure("sandbox attempt", &err);
            return (attempt_from_windows_error(err), None);
        }
    };
    // The probe is what turns "the profile exists and the ACEs are set" into "confinement is
    // enforceable": one confined process, launched under exactly the SECURITY_CAPABILITIES block
    // the launch layer will use, asked for nothing but `exit 0`. Without it a backend could report
    // FullyEnforced while CreateProcessW would fail, or while the confined child cannot even reach
    // the workspace it was confined to.
    if let Err(err) = probe_confined_launch(&setup, workspace_root) {
        log_windows_sandbox_failure("sandbox probe", &err);
        return (attempt_from_windows_error(err), None);
    }
    put_prepared_setup(setup);
    let setup = take_windows_policy_setup();
    // The verdict describes the policy that exists, not the one that was intended. A declined
    // read-write entry — a temporary directory this host would not let the policy grant — means
    // the confinement in force is the confinement that was asked for, minus one write target.
    // That is what `PartiallyEnforced` spells (and what the shared dispatch warns about), rather
    // than a `FullyEnforced` claim the object state contradicts or a rule failure that costs the
    // whole host every command it runs.
    let attempt = match setup.as_ref().and_then(|setup| setup.partial_reason()) {
        Some(reason) => SandboxAttempt::PartiallyEnforced { reason },
        None => SandboxAttempt::FullyEnforced,
    };
    (attempt, setup)
}

/// What one Windows sandbox attempt achieved, with the policy it built parked for the launch
/// half rather than released.
///
/// This is the judgement surface the dispatch (`sandbox::attempt_backend`) calls: it prepares the
/// policy, checks it with the probe and reports. The [`WindowsPolicySetup`] it built is parked in
/// the `PREPARED_SETUP` hand-off, because the confinement it judged is the confinement the pending
/// command has to be launched inside: the ACEs and the profile must survive until that command has
/// exited, which is what [`run_in_appcontainer`] does when it picks the setup up. A run that
/// decides **not** to launch anything confined — a refusal, for instance — must release the parked
/// setup itself with [`discard_windows_policy_setup`], or the AppContainer SID stays written into
/// the DACLs of the workspace until exit.
#[must_use]
pub fn attempt_windows_sandbox(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> SandboxAttempt {
    let (attempt, setup) = attempt_windows_sandbox_with_setup(workspace_root, extra_readonly_paths);
    // Park what the judgement accepted for the launch half. Nothing is parked behind a failed
    // attempt, which is the documented shape of the hand-off ("every failure comes back with
    // `None`"), so a `Some` here is exactly the policy that was accepted.
    if let Some(setup) = setup {
        put_prepared_setup(setup);
    }
    attempt
}

/// Map one failed attempt onto the error the re-entry contract carries.
///
/// `Ok(())` from [`sandbox::sandbox_outcome`] means the attempt was not a failure at all, which in
/// this module's flow means the setup went missing rather than the policy failing: spelled out as
/// `Unavailable` rather than reported as a success.
fn attempt_error(attempt: SandboxAttempt) -> SandboxError {
    match sandbox::sandbox_outcome(&attempt) {
        Ok(()) => SandboxError::Unavailable {
            reason: "no prepared AppContainer setup is parked for this run".to_string(),
        },
        Err(err) => err,
    }
}

/// Run `command` inside the AppContainer this run prepared, and return its exit code.
///
/// The composite of the two layers: it picks up the setup prepared by
/// [`attempt_windows_sandbox_with_setup`] (preparing one only when nothing was parked — so a re-entry
/// run that has already been through the attempt surface does not create a second profile over the
/// same objects), launches the command confined in it, and releases the guards only once the child
/// has exited. The confined child of a re-entry process is a grandchild: everything below the
/// re-entry process inherits its AppContainer membership whether or not it was launched with the
/// attribute list, so confining the command this way is what the re-entry's own confinement means.
///
/// Called from the re-entry child in `main.rs` as the Windows launch half: the policy pass parks
/// the setup, this call runs the pending command inside it, and the exit code it returns is the
/// exit code of that child process.
pub fn run_in_appcontainer(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
    cwd: &Path,
    command: &str,
) -> Result<i32, SandboxError> {
    let setup = match take_windows_policy_setup() {
        Some(setup) => setup,
        None => {
            let (attempt, setup) =
                attempt_windows_sandbox_with_setup(workspace_root, extra_readonly_paths);
            match setup {
                Some(setup) => setup,
                None => {
                    let err = attempt_error(attempt);
                    // Nothing parked: the launch is refused without an attempt failure to point
                    // at, and that has to be said out loud rather than only exits with it.
                    tracing::warn!(
                        step = "confined launch",
                        kind = err.kind(),
                        "sandbox refusal: no confined setup is available for this run: {err}"
                    );
                    return Err(err);
                }
            }
        }
    };
    let outcome = spawn_in_appcontainer(&setup, cwd, command).map_err(|err| {
        // The second refusal site of a run: the attempt said the confinement was real, and
        // launching the pending command inside it still failed. Same fields, so the two refusals
        // are distinguishable in the log by their `step`.
        log_windows_sandbox_failure("confined launch", &err);
        SandboxError::from(err)
    });
    // Release only now: the child has exited, so the ACEs and the profile are no longer in use, and
    // dropping the guards here is what puts the workspace DACLs back.
    drop(setup);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Security::Authorization::DENY_ACCESS;
    use windows_sys::Win32::Security::{
        CopySid, GetLengthSid, GetTokenInformation, InitializeSecurityDescriptor,
        SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetShortPathNameW, ReadFile, WRITE_OWNER, WriteFile,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::OpenProcessToken;

    /// The fixture host: one temporary directory playing the role of `%USERPROFILE%`, with the
    /// sensitive directories, the toolchain directories, a temporary directory and a
    /// program-files directory inside it. Nothing outside that temporary directory is created,
    /// read or granted, so these tests never touch a real user's home.
    struct FakeHost {
        _root: tempfile::TempDir,
        host: WindowsHostPaths,
        /// The stand-in for `%USERPROFILE%`.
        profile: PathBuf,
        temp_dir: PathBuf,
        /// The stand-in for a program-files directory that does not exist on this host.
        missing: PathBuf,
        workspace: PathBuf,
    }

    impl FakeHost {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let root = tempfile::tempdir()?;
            let profile = root.path().join("User");
            for dir in [
                ".ssh",
                ".gnupg",
                "AppData/Local/Temp",
                ".cargo/bin",
                ".rustup",
                "Windows/System32",
                "Program Files",
            ] {
                std::fs::create_dir_all(profile.join(dir))?;
            }
            let temp_dir = profile.join("AppData").join("Local").join("Temp");
            let missing = profile.join("no-such-program-files-here");
            let workspace = root.path().join("workspace");
            std::fs::create_dir_all(&workspace)?;
            let host = WindowsHostPaths {
                user_profile: Some(profile.clone()),
                temp_dirs: vec![temp_dir.clone()],
                cargo_home: Some(profile.join(".cargo")),
                rustup_home: Some(profile.join(".rustup")),
                system_root: Some(profile.join("Windows")),
                program_files: vec![profile.join("Program Files"), missing.clone()],
            };
            Ok(Self {
                _root: root,
                host,
                profile,
                temp_dir,
                missing,
                workspace,
            })
        }

        /// The entries the fixture host would deny: the sensitive directories it stands in for.
        fn sensitive(&self) -> Vec<PathBuf> {
            self.host
                .user_profile
                .iter()
                .flat_map(|home| {
                    sbpl::SENSITIVE_DENIED_BASENAMES
                        .iter()
                        .map(|name| home.join(name))
                        .collect::<Vec<PathBuf>>()
                })
                .collect()
        }
    }

    /// The verbatim (`\\?\` + the rest) spelling of `path`: the shape `canonicalize()` produces on
    /// Windows, and therefore the shape a real workspace root and a real extra path arrive in.
    /// Used to hand the fixture paths to the table in the spelling production actually uses.
    fn verbatim_spelling(path: &Path) -> PathBuf {
        let text = path.display().to_string();
        PathBuf::from(format!("\\\\?\\{}", text.trim_start_matches(['/', '\\'])))
    }

    /// The 8.3 short spelling of `path`, as this volume stores it — the spelling a runner's `%TEMP%`
    /// holds for its own directory. `GetShortPathNameW` answers a too-small buffer with the size the
    /// answer needs (including the terminating NUL), the same sizing protocol [`GetLongPathNameW`]
    /// documents; a host that declines, or a volume that stores no short names, yields the input.
    fn short_path_spelling(path: &Path) -> PathBuf {
        let wide = WideString::from_path(path);
        let mut probe = [0u16; 1];
        // SAFETY: `wide` owns the NUL-terminated input for the duration of the call, and `probe` is
        // a writable buffer of exactly the length stated.
        let needed =
            unsafe { GetShortPathNameW(wide.pcwstr(), probe.as_mut_ptr(), probe.len() as u32) };
        let Ok(needed) = usize::try_from(needed) else {
            return path.to_path_buf();
        };
        if needed == 0 || needed > VERBATIM_PATH_CEILING {
            return path.to_path_buf();
        }
        let mut buffer: Vec<u16> = vec![0; needed];
        // SAFETY: `buffer` holds `needed` code units, which is the size handed to the call, and the
        // answer it writes stays inside it.
        let written = unsafe {
            GetShortPathNameW(
                wide.pcwstr(),
                buffer.as_mut_ptr(),
                u32::try_from(needed).unwrap_or(0),
            )
        };
        if written == 0
            || usize::try_from(written)
                .ok()
                .is_none_or(|written| written >= buffer.len())
        {
            return path.to_path_buf();
        }
        PathBuf::from(wide_cstr_to_string(buffer.as_ptr()))
    }

    /// The marker half of the policy domain, on its own: every marker spelling Windows accepts is
    /// removed — in either separator, in either case — a verbatim share is rewritten onto the plain
    /// share spelling, a device marker goes the same way, and a `?` that is a real segment rather
    /// than a marker is left exactly where it was.
    #[test]
    fn test_the_marker_strip_recognises_every_marker_spelling_and_only_a_marker() {
        for (spelling, expected) in [
            (r"\\?\C:\Users\fredrik", r"C:\Users\fredrik"),
            (r"//?/C:/Users/fredrik", r"C:\Users\fredrik"),
            (r"//?\C:/Users/fredrik", r"C:\Users\fredrik"),
            (r"\\?\c:\users\FREDRIK", r"c:\users\FREDRIK"),
            (r"\\?\UNC\server\share\work", r"\\server\share\work"),
            (r"//?/UNC/server/share/work", r"\\server\share\work"),
            (r"\\?\unc\SERVER\Share", r"\\SERVER\Share"),
            (r"\\.\COM1", "COM1"),
            (r"//./COM1", "COM1"),
            (r"C:/Users/fredrik", r"C:\Users\fredrik"),
            (r"repos\alpha", r"repos\alpha"),
            (r"C:\Users\?\dir", r"C:\Users\?\dir"),
        ] {
            assert_eq!(strip_windows_path_markers(spelling), expected, "{spelling}");
        }

        // Which of them was removed is a fact the storage step needs (a share rewrites differently
        // from a volume), and the answer does not depend on how the host typed the marker.
        assert_eq!(path_marker(r"\\?\UNC\SERVER\share"), PathMarker::Unc);
        assert_eq!(path_marker(r"//?/unc/SERVER/share"), PathMarker::Unc);
        assert_eq!(path_marker(r"\\?\C:\a"), PathMarker::Verbatim);
        assert_eq!(path_marker(r"//?/C:/a"), PathMarker::Verbatim);
        assert_eq!(path_marker(r"\\.\COM1"), PathMarker::Device);
        assert_eq!(path_marker(r"C:\a"), PathMarker::None);
        assert_eq!(path_marker(r"C:\a\?\b"), PathMarker::None);
    }

    /// The domain a [`PolicyPath`] is stored in is one spelling per location: the `\\?\C:\…` form
    /// `canonicalize()` produces, the same marker written `//?/C:/…` by a channel that flattens
    /// separators on the way in, and the plain `C:\…` all normalise onto one path, and a verbatim
    /// share keeps its server and its share instead of hiding them behind the marker.
    #[test]
    fn test_the_policy_domain_collapses_the_marker_spellings() {
        let plain = Path::new(r"C:\Users\fredrik\source\marmel");
        let spellings = [
            plain,
            Path::new(r"\\?\C:\Users\fredrik\source\marmel"),
            Path::new(r"//?/C:/Users/fredrik/source/marmel"),
            Path::new(r"//?\C:\Users\fredrik\source\marmel"),
            Path::new(r"C:/Users/fredrik/source/marmel"),
        ];
        let normalised: Vec<PathBuf> = spellings
            .iter()
            .map(|path| normalise_policy_path(path))
            .collect();
        let first = normalised
            .first()
            .expect("the fixture list is not empty")
            .clone();
        for (spelling, path) in spellings.iter().zip(&normalised) {
            assert!(
                paths_match(path, plain),
                "{} normalises to a path that is not the location it names: {}",
                spelling.display(),
                path_text(path)
            );
            assert_eq!(
                *path, first,
                "every marker spelling reaches one stored path"
            );
            assert_eq!(
                normalise_policy_path(path),
                *path,
                "the domain is stable under itself"
            );
        }
        let text = path_text(&first);
        assert!(
            !text.starts_with(VERBATIM_MARKER) && !text.starts_with("//?/"),
            "a path below the ceiling is stored without its marker: {text}"
        );

        for spelling in [
            r"\\?\UNC\server\share\work",
            r"//?/UNC/server/share/work",
            r"\\?\unc\SERVER\share\work",
        ] {
            let normalised = normalise_policy_path(Path::new(spelling));
            assert!(
                paths_match(&normalised, Path::new(r"\\server\share\work")),
                "{spelling} is one share: {}",
                path_text(&normalised)
            );
            // The identity of the share survives: the server and the share are still the first two
            // components of the stored path, not a `?` and a `UNC` segment in front of them.
            let components = comparable_components(&normalised);
            assert!(
                components.starts_with(&["server".to_string(), "share".to_string()]),
                "{spelling} loses the share identity: {components:?}"
            );
            assert!(
                !path_text(&normalised).starts_with(VERBATIM_MARKER),
                "{spelling} is below the ceiling and so stored without its marker: {}",
                path_text(&normalised)
            );
        }
    }

    /// The marker is noise only while the path can be named without it: at or beyond `MAX_PATH` it
    /// is the spelling the file and security APIs accept and nothing else is, so the domain keeps it
    /// there — and never invents one for a spelling that did not carry it.
    #[test]
    fn test_a_path_at_the_max_path_ceiling_keeps_the_marker_it_needed_to_be_named() {
        let tail = format!(
            "\\{}",
            "a_directory_name_that_is_far_too_long_for_eight\\".repeat(6)
        );
        let long = format!("C:{tail}");
        assert!(
            long.encode_utf16().count() >= MAX_PATH_CEILING,
            "the fixture is at or beyond the ceiling: {}",
            long.encode_utf16().count()
        );

        let verbatim = PathBuf::from(format!("{VERBATIM_MARKER}{long}"));
        let kept = normalise_policy_path(&verbatim);
        assert!(
            paths_match(&kept, &verbatim),
            "one location, spelled two ways: {}",
            path_text(&kept)
        );
        assert!(
            path_text(&kept).starts_with(VERBATIM_MARKER),
            "a path at the ceiling is stored with the marker that makes it nameable: {}",
            path_text(&kept)
        );
        assert_eq!(
            normalise_policy_path(&kept),
            kept,
            "and normalising it again changes nothing"
        );

        let verbatim_share = PathBuf::from(format!("{VERBATIM_UNC_MARKER}server\\share{tail}"));
        let kept = normalise_policy_path(&verbatim_share);
        assert!(
            paths_match(&kept, &verbatim_share),
            "one share, spelled two ways: {}",
            path_text(&kept)
        );
        assert!(
            path_text(&kept).starts_with(VERBATIM_UNC_PREFIX),
            "the share form of the ceiling keeps the share marker: {}",
            path_text(&kept)
        );

        assert!(
            !path_text(&normalise_policy_path(Path::new(r"\\?\C:\Users\fredrik")))
                .starts_with(VERBATIM_MARKER),
            "below the ceiling, the same marker is noise and goes"
        );
        assert!(
            !path_text(&normalise_policy_path(Path::new(&long))).starts_with(VERBATIM_MARKER),
            "a plain spelling is not handed a marker it never had: the policy does not reinterpret \
             how Windows is to parse the path"
        );
    }

    /// The half of the domain no synthetic fixture can stand in for: the 8.3 short name a volume
    /// really stores for a real directory, and the long name `canonicalize()` answers with for that
    /// same directory. A runner's `%TEMP%` is spelled the first way and a canonicalised root the
    /// second, which is how one profile directory reached the CI policy table as both
    /// `C:\Users\RUNNER~1\…` and `C:\Users\runneradmin\…`.
    #[test]
    fn test_a_short_name_and_its_long_name_are_one_location_in_the_policy_domain()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        // A component with no 8.3 form of its own, so the volume has to *make* one for it.
        let component = "a directory name far too long for eight dot three";
        let long = root.path().join(component).join(".ssh").join("repo");
        std::fs::create_dir_all(&long)?;
        let short = short_path_spelling(&long);
        let canonical = std::fs::canonicalize(&long)?;

        for (left, right) in [
            (short.clone(), long.clone()),
            (long.clone(), short.clone()),
            (short.clone(), canonical.clone()),
            (canonical.clone(), long.clone()),
        ] {
            assert!(
                paths_match(&left, &right),
                "{} and {} are one directory: {:?} vs {:?}",
                left.display(),
                right.display(),
                comparable_components(&left),
                comparable_components(&right)
            );
        }

        // The containment the deny-list is built from is taken in the same domain, in both
        // directions and across spellings — this is the check that silently missed the root sitting
        // inside `.ssh` while the profile was spelled `RUNNER~1`.
        let profile = root.path();
        let short_profile = short_path_spelling(profile);
        assert!(
            is_ancestor_or_self(profile, &short),
            "the profile is the ancestor of the repository spelled short: {}",
            short.display()
        );
        assert!(
            is_ancestor_or_self(&short_profile, &long),
            "the profile spelled short is still the ancestor of the repository spelled long: {}",
            short_profile.display()
        );
        assert!(
            !is_ancestor_or_self(&short, profile),
            "and the relation still discriminates: a descendant is not an ancestor"
        );

        // An entry made from the short spelling is stored as the location, not as the spelling.
        let entry = normalise_policy_path(&short);
        assert!(
            paths_match(&entry, &long),
            "the entry a short spelling becomes still names the directory: {}",
            path_text(&entry)
        );
        assert_eq!(
            normalise_policy_path(&entry),
            entry,
            "and normalising that again changes nothing"
        );
        if !path_text(&short).contains(component) {
            // This volume stores 8.3 names and shortened a component of the fixture, so this is the
            // CI case in full: the short spelling is a different string from the long one, and the
            // entry made from it names the location in its long form. Where the volume stores no
            // short names, `GetShortPathNameW` hands the long path straight back, and the comparisons
            // above are the whole of what that host can be shown.
            let short_text = path_text(&short);
            assert!(
                short_text.contains("~1"),
                "{short_text} shortened a component without naming an 8.3 short name"
            );
            assert!(
                path_text(&entry).contains(component),
                "the stored entry restores the long name of the shortened component: {}",
                path_text(&entry)
            );
            assert_ne!(
                entry, short,
                "the short spelling is not already the domain spelling"
            );
            assert_ne!(
                short, long,
                "a short spelling is not the spelling it was made from"
            );
        }
        Ok(())
    }

    /// The security property a mixed-spelling table broke: a protected directory stays inaccessible
    /// when the two sides of the comparison arrive in **different** spellings. The host publishes the
    /// profile in the 8.3 short form its `%TEMP%` holds, while the workspace root inside
    /// `%USERPROFILE%\.ssh` arrives in the long form `canonicalize()` produces — the CI host, both
    /// ways round. The deny-list has to fire, and name the directory it sits inside.
    #[test]
    fn test_the_deny_list_fires_across_spellings_of_the_protected_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let inside = host.profile.join(".ssh").join("repo");
        std::fs::create_dir_all(&inside)?;
        let short_profile = short_path_spelling(&host.profile);
        let canonical_profile = std::fs::canonicalize(&host.profile)?;
        let canonical_inside = canonical_profile.join(".ssh").join("repo");

        let cases = [
            (
                "plain host, plain root",
                host.profile.clone(),
                inside.clone(),
            ),
            (
                "short host, canonicalised root",
                short_profile.clone(),
                canonical_inside.clone(),
            ),
            (
                "short host, plain root",
                short_profile.clone(),
                inside.clone(),
            ),
            (
                "canonicalised host, plain root",
                canonical_profile.clone(),
                inside.clone(),
            ),
            (
                "plain host, canonicalised root",
                host.profile.clone(),
                canonical_inside,
            ),
        ];
        for (label, profile, root) in cases {
            let mut host_paths = host.host.clone();
            host_paths.user_profile = Some(profile.clone());
            match build_policy_table(&root, &[], &host_paths) {
                Ok(table) => panic!(
                    "{label}: {} inside a denied directory names no honest policy, got {:?}",
                    root.display(),
                    table
                        .entries
                        .iter()
                        .map(|entry| path_text(&entry.path))
                        .collect::<Vec<String>>()
                ),
                Err(err @ WindowsSandboxError::RuleSetup { .. }) => {
                    let rendered = err.to_string();
                    assert!(
                        rendered.contains("sits inside the sensitive directory"),
                        "{label}: the refusal has to name the denied directory it sits inside: {rendered}"
                    );
                }
                Err(other) => panic!("{label}: expected a rule setup failure, got {other:?}"),
            }
        }

        // The same property one step later in the table, where it is a suppression rather than a
        // refusal: the profile root offered as an extra in one spelling, while the host publishes it
        // in another, is still the profile root and never becomes a grant.
        let suppressions = [
            (
                "host short, extra long",
                short_profile.clone(),
                canonical_profile.clone(),
            ),
            ("host long, extra short", canonical_profile, short_profile),
        ];
        for (label, profile, extra) in suppressions {
            let mut host_paths = host.host.clone();
            host_paths.user_profile = Some(profile);
            let table = build_policy_table(&host.workspace, &[path_text(&extra)], &host_paths)?;
            assert!(
                table.notes.iter().any(|note| {
                    note.contains("the user profile directory itself is never granted")
                }),
                "{label}: the profile root is rejected whichever side of the comparison is spelled \
                 short: {:?}",
                table.notes
            );
            assert!(
                table
                    .entries
                    .iter()
                    .all(|entry| !paths_match(&entry.path, &extra)),
                "{label}: the profile root never becomes an entry, however it is spelled: {:?}",
                table
                    .entries
                    .iter()
                    .map(|entry| path_text(&entry.path))
                    .collect::<Vec<String>>()
            );
        }
        Ok(())
    }

    /// Every entry a table produces is stored in the one domain — which is the property whose
    /// absence let a single CI table name one profile directory three ways at once, the required
    /// entry verbatim and every derived entry in short form.
    #[test]
    fn test_every_entry_of_a_table_is_stored_in_one_spelling_domain()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let extra = host.workspace.join("extra");
        std::fs::create_dir_all(&extra)?;
        let extras = vec![
            verbatim_spelling(&extra).to_string_lossy().into_owned(),
            path_text(&short_path_spelling(&extra)),
        ];
        // The root reaches the table in the verbatim spelling, as it does in production.
        let root = verbatim_spelling(&host.workspace);
        let table = build_policy_table(&root, &extras, &host.host)?;
        assert!(
            !table.entries.is_empty(),
            "the fixture host produces a policy: {:?}",
            table.notes
        );

        for entry in &table.entries {
            let text = path_text(&entry.path);
            assert_eq!(
                *entry.path,
                normalise_policy_path(&entry.path),
                "{text} is not stored in the domain"
            );
            if text.encode_utf16().count() < MAX_PATH_CEILING {
                assert!(
                    !text.starts_with(r"\\?\") && !text.starts_with("//?/"),
                    "{text} is below the ceiling and so stored without a marker"
                );
            }
        }

        assert_eq!(
            table
                .entries
                .iter()
                .filter(|entry| paths_match(&entry.path, &extra))
                .count(),
            1,
            "the verbatim and the short spelling of one extra are the one grant they name: {:?}",
            table
                .entries
                .iter()
                .map(|entry| path_text(&entry.path))
                .collect::<Vec<String>>()
        );
        assert_eq!(
            table
                .entries
                .iter()
                .filter(|entry| paths_match(&entry.path, &host.temp_dir))
                .count(),
            1,
            "one entry per directory, in whichever spelling the host published it: {:?}",
            table
                .entries
                .iter()
                .map(|entry| path_text(&entry.path))
                .collect::<Vec<String>>()
        );
        assert_eq!(
            table.entries.iter().filter(|entry| entry.required).count(),
            1,
            "and the root is still the one required entry"
        );
        Ok(())
    }

    /// The profile name is derived from the workspace root, survives sanitisation, and stays
    /// inside the name ceiling Windows enforces for an AppContainer profile.
    #[test]
    fn test_profile_name_is_derived_and_stays_within_the_name_limit() {
        let root_a = Path::new("C:\\repos\\alpha");
        let root_b = Path::new("C:\\repos\\beta");
        let salt = "550e8400-e29b-41d4-a716-446655440000";

        let a = appcontainer_profile_name(root_a, salt);
        let a2 = appcontainer_profile_name(root_a, salt);
        let b = appcontainer_profile_name(root_b, salt);

        assert_eq!(a, a2, "the same root and salt give the same name");
        assert_ne!(a, b, "two workspace roots must not share one profile name");
        assert!(a.starts_with(APPCONTAINER_PROFILE_PREFIX));
        assert!(a.len() <= APPCONTAINER_PROFILE_MAX_LEN);
        assert!(a.chars().all(is_profile_name_char), "{a}");
        assert!(is_valid_profile_name(&a), "{a}");

        // Junk in the salt is dropped rather than smuggled into the profile name.
        let junky = appcontainer_profile_name(root_a, "a*b:c?d e/f\\g|h");
        assert!(
            junky.chars().all(is_profile_name_char),
            "sanitised name must hold only the accepted character set: {junky}"
        );

        // A long salt is shortened to the ceiling, and the prefix plus root hash survive.
        let long = appcontainer_profile_name(root_a, &"z".repeat(400));
        assert!(long.len() <= APPCONTAINER_PROFILE_MAX_LEN, "{long}");
        assert!(long.starts_with(APPCONTAINER_PROFILE_PREFIX));

        assert!(!is_valid_profile_name(""), "an empty name is never valid");
        assert!(
            !is_valid_profile_name(APPCONTAINER_PROFILE_PREFIX),
            "the bare prefix is not a profile name"
        );
        assert!(
            !is_valid_profile_name("some-other-prefix-1"),
            "a name this backend would not have generated is rejected"
        );
    }

    /// The table hands the workspace root and the temporary directories the read-write grants,
    /// and marks only the workspace root as required.
    #[test]
    fn test_policy_table_grants_workspace_and_temp_readwrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let table = build_policy_table(&host.workspace, &[], &host.host)?;

        let required: Vec<&PolicyPath> = table
            .entries
            .iter()
            .filter(|entry| entry.required)
            .collect();
        assert_eq!(
            required.len(),
            1,
            "exactly one entry is required, the table holds {}",
            table.entries.len()
        );
        assert!(
            paths_match(&required[0].path, &host.workspace),
            "the required entry is the workspace root the table was asked for: {}",
            path_text(&required[0].path)
        );
        assert_eq!(
            required[0].path,
            normalise_policy_path(&host.workspace),
            "and it is stored in the table's one spelling domain, not in the spelling it arrived in"
        );
        assert_eq!(required[0].grant, Grant::ReadWrite);

        let read_write: Vec<PathBuf> = table
            .entries
            .iter()
            .filter(|entry| entry.grant == Grant::ReadWrite)
            .map(|entry| entry.path.clone())
            .collect();
        assert!(
            read_write
                .iter()
                .any(|path| paths_match(path, &host.temp_dir)),
            "the temporary directory is read-write: {read_write:?}"
        );
        assert!(
            read_write
                .iter()
                .any(|path| paths_match(path, &host.workspace)),
            "the workspace root is the read-write entry it was asked to be: {read_write:?}"
        );
        Ok(())
    }

    /// The confinement may only be strict about **one** entry: the workspace root, the entry the
    /// shared policy list marks `required`. Every temporary directory is an *optional* read-write
    /// entry instead, so a refusal there costs one write target and warns rather than failing the
    /// policy — on a host whose temp DACL cannot be mutated, that failure used to be every command
    /// on that host exiting with the refusal code.
    #[test]
    fn test_confinement_critical_is_the_workspace_root_and_nothing_else()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let table = build_policy_table(&host.workspace, &[], &host.host)?;

        let critical: Vec<PathBuf> = table
            .confinement_critical()
            .map(|entry| entry.path.clone())
            .collect();
        assert_eq!(
            critical.len(),
            1,
            "exactly one entry may be confinement critical, got {critical:?}"
        );
        assert!(
            paths_match(&critical[0], &host.workspace),
            "and it is the workspace root: {}",
            path_text(&critical[0])
        );
        assert_eq!(
            critical[0],
            normalise_policy_path(&host.workspace),
            "stored in the one domain, whatever spelling the root arrived in"
        );

        let optional: Vec<PathBuf> = table
            .optional_read_write_entries()
            .map(|entry| entry.path.clone())
            .collect();
        assert!(
            optional
                .iter()
                .any(|path| paths_match(path, &host.temp_dir)),
            "the temporary directory is an optional read-write entry: {optional:?}"
        );
        assert!(
            !optional
                .iter()
                .any(|path| paths_match(path, &host.workspace)),
            "the required entry is never part of the optional half: {optional:?}"
        );
        Ok(())
    }

    /// A refused read-write grant is reported in the one-line note vocabulary the other backends
    /// use, with the polarity that was declined; and it is what turns the verdict from
    /// `FullyEnforced` into `PartiallyEnforced` — a reason naming the path the operator can act
    /// on, never a bare failure.
    #[test]
    fn test_a_declined_read_write_grant_is_noted_and_named_in_the_partial_reason() {
        let temp = PolicyPath {
            path: PathBuf::from(r"C:\Windows\Temp"),
            grant: Grant::ReadWrite,
            required: false,
        };
        let note = declined_grant_note(&temp, "SetNamedSecurityInfoW failed: Access is denied.");
        assert!(
            note.starts_with("warning: ignoring sandbox read-write path "),
            "the note must name the polarity that was declined: {note}"
        );
        assert!(
            note.contains(r"C:\Windows\Temp") && note.contains("Access is denied."),
            "the note must name the entry and the reason behind it: {note}"
        );
        assert_eq!(
            note.lines().count(),
            1,
            "one line per declined entry: {note}"
        );

        let read_only = PolicyPath {
            path: PathBuf::from(r"C:\Windows"),
            grant: Grant::ReadOnly,
            required: false,
        };
        assert!(
            declined_grant_note(&read_only, "no WRITE_DAC on this object")
                .starts_with("warning: ignoring sandbox read-only path "),
            "a read-only decline keeps the wording the read-only skips already had"
        );

        assert_eq!(
            partial_reason_for(&[]),
            None,
            "a policy that declined nothing is the whole policy"
        );
        let reason = partial_reason_for(std::slice::from_ref(&path_text(&temp.path)))
            .expect("one declined read-write entry makes the attempt partially enforced");
        assert!(
            reason.contains(r"C:\Windows\Temp"),
            "the reason must name the write target that is missing: {reason}"
        );
    }

    /// The toolchain and system directories get the read-and-execute grant, and a path that
    /// does not exist on this host is skipped with a note instead of granted or treated as a
    /// failure.
    #[test]
    fn test_policy_table_is_readonly_for_toolchains_and_skips_missing_paths()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let table = build_policy_table(&host.workspace, &[], &host.host)?;

        for granted in [
            host.host.cargo_bin(),
            host.host.cargo_home.clone(),
            host.host.rustup_home.clone(),
            host.host.system32(),
            host.host.system_root.clone(),
            Some(host.profile.join("Program Files")),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                table
                    .entries
                    .iter()
                    .any(|entry| entry.grant == Grant::ReadOnly
                        && paths_match(&entry.path, &granted)),
                "the toolchain entry {} must be granted read-and-execute",
                granted.display()
            );
        }

        assert!(
            !table
                .entries
                .iter()
                .any(|entry| paths_match(&entry.path, &host.missing)),
            "a program-files spelling that does not exist must not become an entry"
        );
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.contains(&path_text(&host.missing))
                    && note.contains("does not exist")),
            "the missing entry is reported with the shared skip wording: {:?}",
            table.notes
        );
        Ok(())
    }

    /// The sensitive-directory invariant, and the note each kind of entry really earns: the
    /// user-profile root is never granted and says so in its own wording, an entry that is — or
    /// contains — one of the denied directories is rejected with the inheritance wording, an entry
    /// that does not exist on this host is skipped with the shared skip wording, and a genuine
    /// extra directory survives every check above and becomes a grant.
    #[test]
    fn test_policy_table_never_exposes_the_sensitive_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let extras = vec![
            // The user-profile root: the directory that makes `.ssh` and `.gnupg` reachable by
            // inheritance, so it is rejected even though nothing asked for anything narrower.
            host.profile.to_string_lossy().into_owned(),
            // The denied directory itself, granted explicitly by an over-eager operator.
            host.profile
                .join(sbpl::SENSITIVE_DENIED_BASENAMES[0])
                .to_string_lossy()
                .into_owned(),
            // One level above the profile root: an entry that contains the denied directories
            // without being the profile root, so the inheritance wording is the note it earns.
            host.profile
                .parent()
                .unwrap_or(host.profile.as_path())
                .to_string_lossy()
                .into_owned(),
            // A real extra directory, which must survive the checks above.
            host.workspace.join("extra").to_string_lossy().into_owned(),
            // A program-files spelling that does not exist here, published through the extras
            // channel: the shared skip wording decides that one, not a rejection and not a grant.
            host.missing.to_string_lossy().into_owned(),
        ];
        std::fs::create_dir_all(host.workspace.join("extra"))?;

        let table = build_policy_table(&host.workspace, &extras, &host.host)?;

        for denied in host.sensitive() {
            for entry in &table.entries {
                assert!(
                    !is_ancestor_or_self(&entry.path, &denied),
                    "the entry {} would expose the denied directory {}",
                    entry.path.display(),
                    denied.display()
                );
            }
        }
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.contains("the user profile directory itself is never granted")),
            "the profile-root rejection must be reported: {:?}",
            table.notes
        );
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.contains("contains the sensitive directory")),
            "the denied-directory rejection must be reported: {:?}",
            table.notes
        );
        // The two rejections are two notes, and each names the entry it was written for: the
        // profile root because it is the profile root, and the entry above it because a denied
        // directory sits inside that one.
        let profile_note = path_text(&host.profile);
        let ancestor_note = path_text(host.profile.parent().unwrap_or(host.profile.as_path()));
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.starts_with(&profile_note) && note.contains("never granted")),
            "the profile root is rejected by name, with the profile-root wording: {:?}",
            table.notes
        );
        let denied_note = path_text(&host.sensitive()[0]);
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.starts_with(&ancestor_note)
                    && note.contains("contains the sensitive directory")
                    && note.contains(&denied_note)),
            "the entry above the profile root is rejected with the inheritance wording, naming the \
             denied directory it contains: {:?}",
            table.notes
        );
        // An entry that *is* one of the denied directories is rejected too, by name.
        let denied_itself = path_text(&host.profile.join(sbpl::SENSITIVE_DENIED_BASENAMES[0]));
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.starts_with(&denied_itself)
                    && note.contains("contains the sensitive directory")),
            "an entry that is itself a denied directory is rejected as such: {:?}",
            table.notes
        );
        // The missing extra entry is the shared skip wording, neither a rejection nor a grant.
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.contains(&path_text(&host.missing))
                    && note.contains("does not exist")),
            "an extra path absent on this host is skipped with the shared skip wording: {:?}",
            table.notes
        );
        assert!(
            table
                .entries
                .iter()
                .any(|entry| paths_match(&entry.path, &host.workspace.join("extra"))),
            "a genuine extra path is still granted"
        );
        Ok(())
    }

    /// A workspace root that is itself inside a denied directory has no honest policy, which is
    /// reported as a rule setup failure rather than silently widened.
    #[test]
    fn test_policy_table_rejects_a_workspace_root_inside_a_denied_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let inside = host.profile.join(".ssh").join("workspace");
        std::fs::create_dir_all(&inside)?;
        match build_policy_table(&inside, &[], &host.host) {
            Ok(table) => panic!("no policy is possible here, got {:?}", table.entries),
            Err(err) => {
                assert!(
                    matches!(err, WindowsSandboxError::RuleSetup { .. }),
                    "a workspace inside a denied directory must be reported as a rule setup failure, got {err:?}"
                );
            }
        }
        Ok(())
    }

    /// The capability list this backend declares renders as the documented `internetClient`
    /// string, and the application-package groups it verifies against as their documented
    /// strings: those are exactly the SIDs the launch layer hands to Win32.
    #[test]
    fn test_capability_and_application_package_sids_have_the_documented_forms()
    -> Result<(), Box<dyn std::error::Error>> {
        let capabilities = CapabilitySids::internet_client()?;
        assert_eq!(capabilities.count(), 1);
        assert_eq!(capabilities.strings()?, vec!["S-1-15-3-1".to_string()]);

        let all = ApplicationPackagesSid::all()?;
        let restricted = ApplicationPackagesSid::restricted()?;
        // SAFETY: both arguments are SIDs this module allocated immediately above.
        assert_eq!(unsafe { sid_to_string(all.sid())? }, "S-1-15-2-2");
        assert_eq!(unsafe { sid_to_string(restricted.sid())? }, "S-1-15-2-3");

        // The identity is the object Win32 allocated, never a re-parsed name: the string form is
        // diagnostics only, so the check that matters is that the capability SID the profile
        // declares and the one the production construction path allocates are the same identity.
        let internet_client = allocate_app_package_sid(&[3, 1])?;
        // SAFETY: `internet_client` is the SID `AllocateAndInitializeSid` just allocated.
        assert_eq!(
            unsafe { sid_to_string(internet_client)? },
            "S-1-15-3-1",
            "allocating the capability identity again gives the same SID"
        );
        // SAFETY: the SID was allocated by `AllocateAndInitializeSid`, whose documented release is
        // `FreeSid`.
        unsafe { FreeSid(internet_client) };
        Ok(())
    }

    /// A workspace root that does not exist can never be confined, and the attempt surface must
    /// say so as a rule failure rather than claim a policy that does not exist. This needs no
    /// Win32 object at all: the root alone decides it.
    #[test]
    fn test_attempt_reports_a_missing_workspace_root_as_a_rule_failure() {
        let missing = if cfg!(target_os = "windows") {
            "Q:\\marmel-does-not-exist"
        } else {
            "/marmel-does-not-exist"
        };
        let attempt = attempt_windows_sandbox(Path::new(missing), &[]);
        match attempt {
            SandboxAttempt::RuleFailed { path, reason } => {
                assert!(
                    path.contains("marmel-does-not-exist"),
                    "the failing path must be named: {path}"
                );
                assert!(!reason.is_empty());
            }
            other => panic!("expected RuleFailed, got {other:?}"),
        }
    }

    /// This module compiles only for Windows, and `src/harness/mod.rs` must declare it behind
    /// the matching gate: a module that exists but is never declared is dead weight, and one
    /// declared for the wrong host is a compile error there.
    #[test]
    fn test_windows_backend_is_registered_behind_its_cfg_gate() {
        let quote = char::from(34);
        let raw =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/harness/mod.rs"))
                .expect("mod.rs must be readable from the crate manifest directory");
        // The checkout's line endings are not part of this assertion. A Windows CI checkout can be
        // written with CRLF (`core.autocrlf`/`core.eol`), and the needle below spans a newline, so
        // an unnormalised read would search for one spelling of the declaration and find another.
        let source = raw.replace("\r\n", "\n");
        // Spelled from parts, so this file never carries the needles it is checking for.
        let gate = format!("#[cfg(target_os = {quote}windows{quote})]");
        let module = format!("pub mod sandbox_{backend};", backend = "windows");
        assert!(
            source.contains(&format!("{gate}\n{module}")),
            "the Windows backend must be declared behind its own platform gate in mod.rs"
        );
    }

    // -------------------------------------------------------------------
    // launch layer: command line, attribute list, exit codes, hand-off
    // -------------------------------------------------------------------

    /// The command line the confined launch hands to `CreateProcessW`: the program token quoted so
    /// that it stays one token, and the payload appended verbatim because `cmd.exe` re-parses what
    /// follows `/C` with its own metacharacters.
    #[test]
    fn test_confined_command_line_quotes_the_program_token_and_keeps_the_payload_verbatim() {
        assert_eq!(
            confined_command_line("C:\\Windows\\System32\\cmd.exe", "exit 0"),
            "\"C:\\Windows\\System32\\cmd.exe\" /C exit 0"
        );
        // A program path with a space in it is quoted, and only quoted: nothing else changes.
        assert_eq!(
            confined_command_line("C:\\Program Files\\spec.exe", "echo hi"),
            "\"C:\\Program Files\\spec.exe\" /C echo hi"
        );
        // A token with no backslash, quote or space is passed through unquoted.
        assert_eq!(
            confined_command_line("cmd.exe", "echo hi"),
            "cmd.exe /C echo hi"
        );
        // The MSVCRT escaping rules: a trailing backslash run is doubled so that it cannot eat the
        // closing quote, and a quote inside the token is escaped with a backslash.
        assert_eq!(
            quote_command_token("C:\\dir test\\"),
            "\"C:\\dir test\\\\\""
        );
        assert_eq!(
            quote_command_token("C:\\a b\\c\" d"),
            "\"C:\\a b\\c\\\" d\""
        );
        // The payload is not escaped at all: `cmd.exe` sees the command exactly as it was given,
        // quoting and shell metacharacters included.
        let payload = r#"echo "hi there" && dir"#;
        assert_eq!(
            confined_command_line("cmd.exe", payload),
            format!("cmd.exe /C {payload}")
        );
        assert_eq!(
            confined_command_line("C:\\sh\"ell.exe", "exit 0"),
            "\"C:\\sh\\\"ell.exe\" /C exit 0"
        );
    }

    /// `%COMSPEC%` wins when it names something; an empty or blank value never does, and the
    /// fallback spells the absolute path because a confined shell cannot rely on a PATH search.
    #[test]
    fn test_shell_program_prefers_comspec_and_falls_back_to_the_absolute_path() {
        assert_eq!(
            shell_program(Some(r#"C:\Windows\System32\cmd.exe"#), None),
            r"C:\Windows\System32\cmd.exe"
        );
        // A blank value is not a program: the fallback takes over.
        assert_eq!(
            shell_program(Some("   "), Some(r#"C:\toolchain"#)),
            r"C:\toolchain\System32\cmd.exe"
        );
        // Whitespace around a real value is dropped, the value is not.
        assert_eq!(
            shell_program(Some(" C:\\tmp\\shell.exe "), Some(r#"Q:\Windows"#)),
            r"C:\tmp\shell.exe"
        );
        // A trailing separator in the value is kept: it is part of what `%COMSPEC%` named.
        assert_eq!(shell_program(Some("C:\\tool\\"), None), "C:\\tool\\");
        // A `SystemRoot` with a trailing separator does not become `System32\\cmd.exe`.
        assert_eq!(
            shell_program(None, Some(r#"D:\win\"#)),
            r"D:\win\System32\cmd.exe"
        );
        // Nothing usable set at all: the documented default, which needs no search.
        assert_eq!(
            shell_program(None, Some("   ")),
            r"C:\Windows\System32\cmd.exe"
        );
        assert_eq!(shell_program(None, None), r"C:\Windows\System32\cmd.exe");
    }

    /// The interactive-shell rule as the confined launch applies it: an R1 request runs the bare
    /// shell (no `/C`, no payload), and every other payload still goes behind `/C`.
    ///
    /// This is the Windows half of what the unix re-entry already does — confine the request and
    /// leave it interactive — pinned at the seam that used to turn `cmd` into `cmd.exe /C cmd`.
    #[test]
    fn test_confined_command_line_keeps_an_interactive_shell_bare() {
        let comspec = r"C:\Windows\System32\cmd.exe";
        // R2: the `cmd` family takes the program token the launch resolved (absolute, so the
        // confined child needs no PATH search), and nothing else.
        for shell in ["cmd", "CMD", "cmd.exe", "  CMD.EXE  "] {
            assert_eq!(
                confined_command_line(comspec, shell),
                "\"C:\\Windows\\System32\\cmd.exe\"",
                "`{shell:?}` is an interactive-shell request: the bare shell, no payload"
            );
        }
        // R3: `powershell` is used as the caller wrote it, so the bare form names that token and
        // never substitutes the `cmd` program.
        assert_eq!(
            confined_command_line(comspec, "powershell"),
            "powershell",
            "the requested shell stays the requested shell"
        );
        assert_eq!(
            confined_command_line(comspec, "  powershell.exe "),
            "powershell.exe"
        );
        // A payload with an argument attached is not an R1 request, and keeps `/C`.
        assert_eq!(
            confined_command_line(comspec, "cmd /C echo hi"),
            "\"C:\\Windows\\System32\\cmd.exe\" /C cmd /C echo hi"
        );
        assert_eq!(
            confined_command_line("cmd.exe", "echo hi"),
            "cmd.exe /C echo hi",
            "a normal command is unaffected by the rule"
        );
    }

    /// The std-handle grant, asserted through the value the launch builds rather than through a
    /// live launch: the two AppContainer principals, the rights they are handed, the SID form the
    /// trustee has to take, and the absence of inheritance on an object that is not a container.
    #[test]
    fn test_std_handle_trustees_name_the_appcontainer_identity_readwrite_and_without_inheritance()
    -> Result<(), WindowsSandboxError> {
        // A capability SID of the app-package authority stands in for the profile SID the launch
        // passes in production; the collection SID is the real `S-1-15-2-2`.
        let stand_in_app_sid = allocate_app_package_sid(&[3, 1])?;
        let all_packages = ApplicationPackagesSid::all()?;
        let entries = std_handle_trustees(stand_in_app_sid, all_packages.sid());
        assert_eq!(
            entries.len(),
            2,
            "the precise profile identity first, the collection SID second"
        );
        for entry in &entries {
            assert_eq!(
                entry.grfAccessPermissions, STD_HANDLE_ACCESS,
                "one mask for every trustee: read and write on the object"
            );
            assert_eq!(entry.grfAccessMode, GRANT_ACCESS);
            assert_eq!(
                entry.grfInheritance, NO_INHERITANCE,
                "a standard stream is an object, not a container: nothing inherits from a handle"
            );
            assert_eq!(
                entry.Trustee.TrusteeForm, TRUSTEE_IS_SID,
                "an AppContainer SID is a name the lookup of a `TRUSTEE_IS_NAME` trustee would miss"
            );
            assert_eq!(entry.Trustee.TrusteeType, TRUSTEE_IS_UNKNOWN);
            assert_eq!(
                entry.Trustee.MultipleTrusteeOperation, NO_MULTIPLE_TRUSTEE,
                "one principal per entry"
            );
        }
        assert_eq!(
            entries[0].Trustee.ptstrName, stand_in_app_sid as *mut u16,
            "the first trustee names the precise AppContainer identity, by pointer"
        );
        assert_eq!(
            entries[1].Trustee.ptstrName,
            all_packages.sid() as *mut u16,
            "the second trustee is `ALL APPLICATION PACKAGES`"
        );
        // The mask is read **and** write, not one of them: a confined child has to be able to read
        // its own stdin and write its own stdout.
        assert_eq!(
            STD_HANDLE_ACCESS & (FILE_READ_DATA | FILE_WRITE_DATA),
            FILE_READ_DATA | FILE_WRITE_DATA,
            "the mask must cover both directions"
        );
        // SAFETY: the SID was allocated by `AllocateAndInitializeSid`, whose documented release is
        // `FreeSid`, and the trustee entries only ever pointed at it.
        unsafe { FreeSid(stand_in_app_sid) };
        Ok(())
    }

    /// Whether `handle` names an object whose DACL already carries an allow ACE for `sid` covering
    /// `mask` — the read side of the handle grant, spelled once for the tests below.
    ///
    /// The read runs through [`duplicate_for_security`] for the same reason the grant itself does, and
    /// it works here only because the fixture objects are created by this test with a descriptor that
    /// concedes `READ_CONTROL` to this process ([`TestDescriptor`]): duplication re-runs the access
    /// check against the object's own DACL, so it can never add a right that DACL refuses. What that
    /// means for a handle this process did not create is pinned by
    /// [`test_a_foreign_handle_refuses_the_security_read_and_duplication_cannot_repair_it`]. A refused
    /// read is answered here by a panic, because "the DACL could not be read" and "the DACL holds no
    /// such ACE" are different answers, and the grant test has to tell them apart.
    fn handle_names_sid(handle: HANDLE, sid: PSID, mask: u32) -> bool {
        let secured = match duplicate_for_security(handle) {
            Ok(duplicated) => duplicated,
            Err(code) => panic!(
                "DuplicateHandle(READ_CONTROL | WRITE_DAC) on the test handle {handle:p} was \
                 refused (win32 error {code}): the object's DACL has to concede READ_CONTROL for \
                 this DACL read and WRITE_DAC for the grant under test; the refusal said: {} — \
                 duplication can only narrow what that DACL already concedes, never add to it",
                last_win32_error()
            ),
        };
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the handle names one live object, and each out-parameter is a valid pointer.
        let code = unsafe {
            GetSecurityInfo(
                secured,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                &mut sacl,
                &mut sd,
            )
        };
        let _ = sacl;
        if code != ERROR_SUCCESS {
            // SAFETY: the read allocated nothing this call still needs, and the handle it copied is
            // this helper's to close.
            unsafe {
                LocalFree(sd);
                CloseHandle(secured);
            }
            panic!(
                "GetSecurityInfo(DACL_SECURITY_INFORMATION) through the READ_CONTROL duplicate \
                 {secured:p} of the test handle {handle:p} could not read the DACL (win32 error \
                 {code}): {}",
                last_win32_error()
            );
        }
        if dacl.is_null() {
            // SAFETY: the descriptor came from `GetSecurityInfo`, and the copy made for the read is
            // this helper's to close.
            unsafe {
                LocalFree(sd);
                CloseHandle(secured);
            }
            // A NULL DACL carries no list to inspect: nothing names this SID.
            return false;
        }
        let mut size = ACL_SIZE_INFORMATION {
            AceCount: 0,
            AclBytesInUse: 0,
            AclBytesFree: 0,
        };
        // SAFETY: the ACL and the information buffer belong to this call.
        let sized = unsafe {
            GetAclInformation(
                dacl,
                std::ptr::from_mut(&mut size).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        };
        let mut found = false;
        if sized != 0 {
            for index in 0..size.AceCount {
                if found {
                    break;
                }
                let mut ace: *mut c_void = std::ptr::null_mut();
                // SAFETY: `GetAce` reads the ACL it is given and writes one pointer.
                if unsafe { GetAce(dacl, index, &mut ace) } == 0 || ace.is_null() {
                    continue;
                }
                let typed: *const ACCESS_ALLOWED_ACE = ace.cast();
                // SAFETY: an access-allowed ACE begins with its header, and `GetAce` guarantees
                // the pointer for the ACE's own size.
                let allowed = unsafe { &*typed };
                if allowed.Header.AceType != ACCESS_ALLOWED_ACE_TYPE
                    || u32::from(allowed.Header.AceFlags) & INHERIT_ONLY_ACE != 0
                {
                    continue;
                }
                let ace_sid = std::ptr::from_ref(&allowed.SidStart)
                    .cast::<c_void>()
                    .cast_mut();
                // SAFETY: both SID arguments are valid: the ACE's SID starts at `SidStart`, and
                // the compared SID is one this test allocated.
                found = unsafe { EqualSid(sid, ace_sid) } != 0 && (allowed.Mask & mask) == mask;
            }
        }
        // SAFETY: the descriptor was allocated by `GetSecurityInfo`, and the copy made for this read
        // is this helper's to close.
        unsafe {
            LocalFree(sd);
            CloseHandle(secured);
        };
        found
    }

    // -----------------------------------------------------------------------
    // fixture: one object whose security descriptor this test writes itself
    // -----------------------------------------------------------------------

    /// `SECURITY_DESCRIPTOR_REVISION` spelled locally, for the same reason `ACCESS_ALLOWED_ACE_TYPE`
    /// is: the constant in `windows_sys::Win32::System::SystemServices` lives in a module this crate
    /// does not enable.
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

    /// What a fixture object concedes to the process that created it, and why every one of these
    /// bits is there, which is one Windows rule: a caller-supplied `SECURITY_ATTRIBUTES` is not a
    /// suggestion. `CreatePipe` creates the pipe **with** the descriptor this test wrote, and the
    /// object manager then runs the create-time access check **against that DACL**, for the access
    /// the creating process asks of the two ends it is handed. A DACL that is present but does not
    /// concede one of those rights answers `ERROR_ACCESS_DENIED` (5) and no object comes into being
    /// at all: present-but-insufficient is a refusal, and only a **NULL** DACL means "grant
    /// everything". The mask therefore has to concede the creator's own request — in the spelling
    /// that request is made in — next to the rights the calls under test ask for afterwards.
    ///
    /// * `GENERIC_READ | GENERIC_WRITE` — the data rights of a stream in the generic spelling the
    ///   stream APIs ask of the read end and the write end with. Drop them and the **create** is
    ///   what fails, one call before the code under test is reached.
    /// * `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES |
    ///   SYNCHRONIZE` — the same data rights spelled specifically, which is how a later access
    ///   check against the object names them: the byte the companion test pushes through the pipe,
    ///   and the read and write of the child that inherits the handle.
    /// * `READ_CONTROL | WRITE_DAC` — the two rights the grant path is built on. `GetSecurityInfo`
    ///   asks `READ_CONTROL` and `SetSecurityInfo` asks `WRITE_DAC` of the handle they work
    ///   through, and [`duplicate_for_security`] can only carry rights this DACL concedes, because
    ///   `DuplicateHandle` with an explicit `dwDesiredAccess` re-runs this same access check and
    ///   can narrow access, never invent it.
    /// * `WRITE_OWNER` — the ownership right of the process that rewrote the descriptor.
    const FIXTURE_CALLER_ACCESS: u32 = GENERIC_READ
        | GENERIC_WRITE
        | READ_CONTROL
        | WRITE_DAC
        | WRITE_OWNER
        | SYNCHRONIZE
        | FILE_READ_DATA
        | FILE_WRITE_DATA
        | FILE_READ_ATTRIBUTES
        | FILE_WRITE_ATTRIBUTES;

    /// The contrast object's mask: everything a stream handle really carries — the data rights in
    /// both spellings, so that what the companion test measures is the refused security read and
    /// not the create that had to succeed before it — and none of what the security calls need.
    /// That is the shape of the std handles this process is *given*.
    const FIXTURE_DATA_ONLY_ACCESS: u32 = GENERIC_READ
        | GENERIC_WRITE
        | SYNCHRONIZE
        | FILE_READ_DATA
        | FILE_WRITE_DATA
        | FILE_READ_ATTRIBUTES
        | FILE_WRITE_ATTRIBUTES;

    /// The two rights that decide whether a DACL can be read and rewritten at all.
    const FIXTURE_SECURITY_ACCESS: u32 = READ_CONTROL | WRITE_DAC;

    /// The Win32 code `GetSecurityInfo` answers when asked for the DACL of `handle` and nothing else,
    /// releasing whatever the read allocated either way.
    ///
    /// This is the question the foreign-handle test has to ask: the answer is `ERROR_ACCESS_DENIED`
    /// whenever the object's DACL refuses `READ_CONTROL` to the caller, and no amount of handle
    /// juggling changes the answer, because the object's DACL is what the access check reads.
    fn security_read_code(handle: HANDLE) -> WIN32_ERROR {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the handle names one live object, and every out-parameter is a valid pointer.
        let code = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                &mut sacl,
                &mut sd,
            )
        };
        let _ = dacl;
        let _ = sacl;
        // SAFETY: `GetSecurityInfo` documents `LocalFree` as the release of the descriptor it hands
        // back, and `LocalFree(NULL)` is defined to do nothing.
        unsafe { LocalFree(sd) };
        code
    }

    /// Create the fixture's anonymous pipe with `attrs`, and answer a refusal with the precondition
    /// it broke rather than with a bare Win32 code.
    ///
    /// `conceded` is the mask the fixture's own DACL grants this process, and it belongs in the
    /// message because a create refused by the descriptor the test wrote can only mean one thing:
    /// that mask withholds a right the **creator** asked for. The create runs the same access check
    /// as everything else here, against the supplied descriptor, for the data rights of a stream
    /// (`GENERIC_READ` for the read end, `GENERIC_WRITE` for the write end) — so
    /// `ERROR_ACCESS_DENIED` on this call names a fixture whose mask forgot its own creator, not a
    /// Windows the grant path cannot work on.
    ///
    /// `Err` carries the code so that a test which needs the object can report the exact refusal: a
    /// create the fixture's own DACL defeats is a mask that withheld a right the creator asked for,
    /// which is a failure to surface and never a reason to skip the measurement that follows.
    fn create_fixture_pipe(
        attrs: &SECURITY_ATTRIBUTES,
        conceded: u32,
    ) -> Result<(HANDLE, HANDLE), (WIN32_ERROR, String)> {
        let mut read: HANDLE = std::ptr::null_mut();
        let mut write: HANDLE = std::ptr::null_mut();
        // SAFETY: two out-parameters, plus a security-attributes block naming a live descriptor the
        // caller owns and keeps alive across the call.
        let made = unsafe { CreatePipe(&mut read, &mut write, attrs, 0) };
        if made == 0 {
            let code = unsafe { GetLastError() };
            return Err((
                code,
                format!(
                    "CreatePipe refused the object it was creating (win32 error {code}): {detail}; \
                     the create-time access check runs against the descriptor the fixture wrote, \
                     whose DACL concedes 0x{conceded:08X} to this process — the data rights of the \
                     two ends it hands back (GENERIC_READ on the read end, GENERIC_WRITE on the \
                     write end) are part of that request",
                    detail = last_win32_error(),
                ),
            ));
        }
        Ok((read, write))
    }

    /// One SID owned by this test: `CopySid` wrote it into a `u32`-aligned buffer, which is the
    /// alignment every `PSID` user demands, and that buffer lives exactly as long as this value.
    struct OwnedSid {
        buffer: Vec<u32>,
    }

    impl OwnedSid {
        fn copy(sid: PSID) -> Result<Self, WindowsSandboxError> {
            // SAFETY: `GetLengthSid` only reads the header of a valid SID.
            let bytes = unsafe { GetLengthSid(sid) };
            if bytes == 0 {
                return Err(WindowsSandboxError::Sid {
                    reason: format!("GetLengthSid failed: {}", last_win32_error()),
                    code: unsafe { GetLastError() },
                });
            }
            let mut buffer = vec![0u32; bytes.div_ceil(4) as usize];
            let destination = buffer.as_mut_ptr().cast::<c_void>();
            // SAFETY: the destination is `bytes` bytes of live, DWORD-aligned memory owned by
            // `buffer`, and `sid` is a valid SID of the length `GetLengthSid` just reported.
            if unsafe { CopySid(bytes, destination, sid) } == 0 {
                return Err(WindowsSandboxError::Sid {
                    reason: format!("CopySid failed: {}", last_win32_error()),
                    code: unsafe { GetLastError() },
                });
            }
            Ok(Self { buffer })
        }

        /// The copied SID. `Vec` never moves the block it allocated once its length is fixed, so the
        /// address `copy` wrote into is the address this returns.
        fn sid(&self) -> PSID {
            self.buffer.as_ptr().cast_mut().cast::<c_void>()
        }
    }

    /// The SID this process runs as: the principal a fixture descriptor has to name, because it is
    /// the caller the access check behind `GetSecurityInfo`, `SetSecurityInfo` and `DuplicateHandle`
    /// is run against.
    fn caller_sid() -> Result<OwnedSid, WindowsSandboxError> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `GetCurrentProcess()` is the pseudo-handle `OpenProcessToken` documents, and the
        // out-parameter is a valid pointer.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return Err(WindowsSandboxError::Sid {
                reason: format!("OpenProcessToken failed: {}", last_win32_error()),
                code: unsafe { GetLastError() },
            });
        }
        // Two passes: the size of a `TOKEN_USER` header plus the SID it points at is only known by
        // asking, and the size probe is documented to answer `ERROR_INSUFFICIENT_BUFFER`.
        let mut needed = 0u32;
        // SAFETY: a zero-length buffer with a null pointer is that documented probe.
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            let code = unsafe { GetLastError() };
            // SAFETY: the token handle is this call's to close, and nothing else was opened.
            unsafe { CloseHandle(token) };
            return Err(WindowsSandboxError::Sid {
                reason: format!(
                    "GetTokenInformation(TokenUser) reported no size: {}",
                    last_win32_error()
                ),
                code,
            });
        }
        // `u64` elements, because a `TOKEN_USER` holds a pointer and therefore wants 8-byte alignment
        // that a byte buffer is not promised.
        let mut buffer = vec![0u64; needed.div_ceil(8) as usize];
        // SAFETY: the buffer is at least `needed` bytes long, which is what the probe asked for.
        let read = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        };
        let read_error = if read == 0 {
            unsafe { GetLastError() }
        } else {
            ERROR_SUCCESS
        };
        // SAFETY: the user SID lives in the buffer, not in the token handle, so the handle can go.
        unsafe { CloseHandle(token) };
        if read_error != ERROR_SUCCESS {
            return Err(WindowsSandboxError::Sid {
                reason: format!(
                    "GetTokenInformation(TokenUser) failed: {}",
                    last_win32_error()
                ),
                code: read_error,
            });
        }
        // SAFETY: the buffer holds the `TOKEN_USER` header followed by the SID its `Sid` field points
        // at, both written by `GetTokenInformation` into this one allocation.
        let sid = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() }.User.Sid;
        OwnedSid::copy(sid)
    }

    /// An absolute security descriptor this test builds itself, naming exactly one principal: the
    /// process running the test.
    ///
    /// This is the fixture's whole point. A descriptor handed to `SECURITY_ATTRIBUTES::lpSecurityDescriptor`
    /// is honoured only for an object being **created**, so the object behind the fixture handle is
    /// governed by a DACL this test wrote, and the two security rights the grant path needs are inside
    /// it. That is what makes the fixture an object this test may read, rewrite and put back; a handle
    /// to anything else in the machine is at the mercy of a DACL somebody else wrote (see
    /// [`test_a_foreign_handle_refuses_the_security_read_and_duplication_cannot_repair_it`]).
    struct TestDescriptor {
        /// Boxed so its address stays put while `SECURITY_ATTRIBUTES` points at it.
        sd: Box<SECURITY_DESCRIPTOR>,
        /// The DACL `SetEntriesInAclW` allocated for the descriptor, released on drop — the object is
        /// created from a copy of the descriptor, so the ACL outlives every use here and nothing else.
        dacl: *mut ACL,
    }

    impl TestDescriptor {
        /// Grant `allow` to this process, and — when `deny` is not `0` — refuse it `deny` as well, with
        /// nobody else on the list. `SetEntriesInAclW` orders a deny ACE ahead of the allow ACEs, so the
        /// refused rights stay refused.
        ///
        /// The ACE has the shape a non-container object needs, which is also the shape the production
        /// grant uses: `GRANT_ACCESS` — access is conceded, never implicit; a trustee in SID form
        /// (`TRUSTEE_IS_SID` with `TRUSTEE_IS_UNKNOWN`) naming the **token's user**, because that is
        /// the principal both the create-time check and the handle-based checks run for (an
        /// AppContainer SID here would name a principal this process is not); and `NO_INHERITANCE`,
        /// which on an object that supports no inheritance leaves a plain `ACCESS_ALLOWED_ACE` with no
        /// `INHERIT_ONLY_ACE` bit — so the ACE counts for the object it is written on, which is the
        /// only object this fixture has.
        fn for_caller(
            allow: u32,
            deny: u32,
            caller: &OwnedSid,
        ) -> Result<Self, WindowsSandboxError> {
            let trustee = trustee_for_sid(caller.sid());
            let mut entries = vec![EXPLICIT_ACCESS_W {
                grfAccessPermissions: allow,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: NO_INHERITANCE,
                Trustee: trustee,
            }];
            if deny != 0 {
                entries.push(EXPLICIT_ACCESS_W {
                    grfAccessPermissions: deny,
                    grfAccessMode: DENY_ACCESS,
                    grfInheritance: NO_INHERITANCE,
                    Trustee: trustee,
                });
            }
            let mut dacl: *mut ACL = std::ptr::null_mut();
            // SAFETY: `entries` outlives the call, and a null old ACL means "start from these entries
            // alone" — which is exactly a DACL that names no one but this process.
            let built = unsafe {
                SetEntriesInAclW(
                    entries.len() as u32,
                    entries.as_ptr(),
                    std::ptr::null(),
                    &mut dacl,
                )
            };
            if built != ERROR_SUCCESS {
                return Err(WindowsSandboxError::AclMutate {
                    path: "the fixture descriptor".to_string(),
                    reason: format!("SetEntriesInAclW failed: {}", last_win32_error()),
                    code: built,
                });
            }
            let mut sd = Box::new(SECURITY_DESCRIPTOR {
                Revision: 0,
                Sbz1: 0,
                Control: 0,
                Owner: std::ptr::null_mut(),
                Group: std::ptr::null_mut(),
                Sacl: std::ptr::null_mut(),
                Dacl: std::ptr::null_mut(),
            });
            // `Box` has no stable `as_mut_ptr`, so the address is taken through a mutable borrow of
            // the boxed value, which keeps the box as the owner; `PSECURITY_DESCRIPTOR` is a type alias
            // for `*mut c_void`, hence the cast.
            let descriptor_ptr = std::ptr::from_mut(&mut *sd).cast::<c_void>();
            // SAFETY: the pointer names the live, boxed `SECURITY_DESCRIPTOR` this call initialises,
            // and revision 1 is the only revision the API accepts.
            let initialised = unsafe {
                InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION)
            };
            let initialised_error = if initialised == 0 {
                unsafe { GetLastError() }
            } else {
                ERROR_SUCCESS
            };
            if initialised_error != ERROR_SUCCESS {
                // SAFETY: the ACL reached no descriptor, so this call releases it itself.
                unsafe { LocalFree(dacl as *mut c_void) };
                return Err(WindowsSandboxError::AclMutate {
                    path: "the fixture descriptor".to_string(),
                    reason: format!(
                        "InitializeSecurityDescriptor failed: {}",
                        last_win32_error()
                    ),
                    code: initialised_error,
                });
            }
            // SAFETY: the descriptor is initialised, `descriptor_ptr` still names the boxed
            // descriptor (the box is not moved), and the ACL handed to it is the one this struct keeps
            // alive until drop — past every object created from it.
            let attached =
                unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, dacl as *const ACL, 0) };
            let attached_error = if attached == 0 {
                unsafe { GetLastError() }
            } else {
                ERROR_SUCCESS
            };
            if attached_error != ERROR_SUCCESS {
                // SAFETY: as above — no descriptor outlives this function to reference the ACL.
                unsafe { LocalFree(dacl as *mut c_void) };
                return Err(WindowsSandboxError::AclMutate {
                    path: "the fixture descriptor".to_string(),
                    reason: format!("SetSecurityDescriptorDacl failed: {}", last_win32_error()),
                    code: attached_error,
                });
            }
            Ok(Self { sd, dacl })
        }

        /// The attributes block to hand to the create call: this descriptor, and no inheritance — the
        /// fixture handle stays inside this test, so nothing else can stumble into it.
        fn attributes(&mut self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: std::ptr::from_mut(&mut *self.sd).cast::<c_void>(),
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for TestDescriptor {
        fn drop(&mut self) {
            // SAFETY: `dacl` is the ACL `SetEntriesInAclW` allocated, and `LocalFree` is its documented
            // release; the object created from the descriptor holds its own copy.
            unsafe { LocalFree(self.dacl as *mut c_void) };
        }
    }

    /// A real handle, a real DACL, a real grant — on an object this test created with a descriptor it
    /// wrote itself: `CreatePipe` with a [`TestDescriptor`] whose DACL concedes this process the data
    /// rights of the two ends (in both spellings, see [`FIXTURE_CALLER_ACCESS`], because the create is
    /// checked against that very descriptor) and the two security rights the grant works through. The
    /// grant puts the AppContainer ACE on that object, and dropping the guard puts the original DACL
    /// back.
    ///
    /// Why the fixture has to own the descriptor: `grant_std_handle` works through
    /// [`duplicate_for_security`], and `DuplicateHandle` with an explicit desired-access mask re-runs
    /// the access check against the **object's** security descriptor for the calling token. A duplicate
    /// therefore carries only rights that descriptor already concedes — for a handle this process did
    /// not create (cargo's captured stdio, an anonymous pipe carrying the default DACL of the token)
    /// there is no duplication flag combination that produces `READ_CONTROL` or `WRITE_DAC`, and the
    /// security calls answer `ERROR_ACCESS_DENIED` anyway. That case is pinned as its own test,
    /// [`test_a_foreign_handle_refuses_the_security_read_and_duplication_cannot_repair_it`]; this one
    /// proves the grant-and-restore path itself, on the only kind of object it can ever succeed on.
    ///
    /// Of the two candidate fixtures, (A) `CreatePipe` with explicit `SECURITY_ATTRIBUTES` is chosen
    /// over (B) `CreateFileW("\\\\.\\NUL", READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES |
    /// SYNCHRONIZE, …)` for two reasons. `lpSecurityAttributes` is consulted only when the object is
    /// **created**: opening the existing `NUL` device would leave its own DACL in charge, which is a
    /// foreign object wearing a different costume, and its rights are the device's, not the caller's
    /// choice. And the production grant is about a stream object — the anonymous pipes the one-shot
    /// path hands to the confined child — so a pipe is the object class the test should exercise.
    ///
    /// This test is only compiled where the Windows backend is compiled — the module is behind
    /// `#[cfg(target_os = "windows")]` in `mod.rs`, so it runs on the native Windows CI job.
    #[test]
    fn test_a_real_std_handle_grant_is_applied_and_then_restored() -> Result<(), WindowsSandboxError>
    {
        let caller = caller_sid()?;
        let mut descriptor = TestDescriptor::for_caller(FIXTURE_CALLER_ACCESS, 0, &caller)?;
        let attrs = descriptor.attributes();
        let (read, write) =
            create_fixture_pipe(&attrs, FIXTURE_CALLER_ACCESS).map_err(|(code, detail)| {
                WindowsSandboxError::Launch {
                    reason: format!(
                        "the fixture could not create the object its own descriptor is meant to \
                         make possible: {detail}"
                    ),
                    code,
                }
            })?;
        let app_sid = allocate_app_package_sid(&[2, 2])?;
        assert!(
            !handle_names_sid(write, app_sid, STD_HANDLE_ACCESS),
            "before the grant runs, the DACL of the write end {write:p} must not yet name the \
             AppContainer SID (mask 0x{:08X}) — it is the one this test wrote, which names only \
             this process",
            STD_HANDLE_ACCESS
        );
        let grant = grant_std_handle(write, app_sid)?;
        let Some(grant) = grant else {
            // SAFETY: the two pipe ends and the SID are this test's to release.
            unsafe {
                FreeSid(app_sid);
                CloseHandle(read);
                CloseHandle(write);
            }
            return Err(WindowsSandboxError::AclMutate {
                path: "the test pipe".to_string(),
                reason: format!(
                    "the write end {write:p} of the fixture pipe concedes this process \
                     READ_CONTROL | WRITE_DAC, so the grant had to add the AppContainer ACE (mask \
                     0x{:08X}) to its DACL and reported none instead",
                    STD_HANDLE_ACCESS
                ),
                code: 0,
            });
        };
        assert!(
            handle_names_sid(write, app_sid, STD_HANDLE_ACCESS),
            "the DACL read through the READ_CONTROL duplicate of the write end {write:p} must show \
             the ACE this run applied (AppContainer SID, mask 0x{:08X})",
            STD_HANDLE_ACCESS
        );
        drop(grant);
        assert!(
            !handle_names_sid(write, app_sid, STD_HANDLE_ACCESS),
            "the guard restores the DACL it read: the AppContainer ACE (mask 0x{:08X}) must be \
             gone from the write end {write:p}",
            STD_HANDLE_ACCESS
        );
        assert!(
            handle_names_sid(write, caller.sid(), FIXTURE_CALLER_ACCESS),
            "the restored DACL is the one this test wrote: the ACE granting this process the \
             fixture mask 0x{:08X} must survive the restore on the write end {write:p}",
            FIXTURE_CALLER_ACCESS
        );
        // SAFETY: the SID was allocated by `AllocateAndInitializeSid`, and the two pipe ends were
        // created by this test.
        unsafe {
            FreeSid(app_sid);
            CloseHandle(read);
            CloseHandle(write);
        }
        Ok(())
    }

    /// The reality a duplication trick cannot move: an object whose DACL refuses `READ_CONTROL` and
    /// `WRITE_DAC` to this process refuses them to **every** handle that names it, because
    /// `DuplicateHandle` with an explicit desired-access mask re-runs the access check against the
    /// object's own security descriptor for the calling token. Concretely: the fixture below is a pipe
    /// created with a descriptor that allows the data rights and explicitly denies
    /// `READ_CONTROL | WRITE_DAC`, and its DACL is out of reach both through the handle the create
    /// returned and through a duplicate asking for exactly the rights the security calls want.
    ///
    /// This is why the **production** stdio grant is warn-only and stays warn-only. The streams the
    /// one-shot path hands over are pipes this process did not create: the re-entry plumbing's own
    /// pipes and the ones behind `Stdio::piped()` / `Stdio::null()`, whose descriptors concede the
    /// rights of a stream and not the security rights. So `grant_std_handle` may legitimately answer
    /// `ERROR_ACCESS_DENIED`, and `grant_std_handles` — the `tracing::warn!` and `eprintln!` pair that
    /// closes its `Err(err)` arm, at `:2462`-`:2463` of this commit, the same two lines at
    /// `:2455`-`:2456` of `494ec78` and `:2328`-`:2329` of `697d7b2` — downgrades exactly that to a
    /// warning plus the operator note on stderr, instead of failing the run or pretending the stream
    /// works. The confined child does not
    /// depend on that grant to have a terminal: commit `0ff93f5` introduced the inherited-handle
    /// capability — the `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` that hands the child precisely these
    /// std handles (`launch_confined`, `stdio_state`) together with the best-effort ACE above — and the
    /// behaviour is proven end to end by `test_windows_confined_child_prints_on_stdout_and_stderr` and
    /// `test_windows_runner_confined_child_writes_on_the_inherited_streams` in
    /// `tests/windows_sandbox.rs`, which read back what a confined child wrote on stdout and stderr.
    ///
    /// Nobody should "fix" this by re-adding a duplication flag: the object's descriptor is the only
    /// lever, which is what the companion test above exercises with a descriptor of its own.
    #[test]
    fn test_a_foreign_handle_refuses_the_security_read_and_duplication_cannot_repair_it()
    -> Result<(), WindowsSandboxError> {
        let caller = caller_sid()?;
        // One right of difference from the fixture of the grant test: the data rights stay — in both
        // spellings, so that the create is not what fails here — and the two security rights are
        // explicitly refused.
        let mut descriptor =
            TestDescriptor::for_caller(FIXTURE_DATA_ONLY_ACCESS, FIXTURE_SECURITY_ACCESS, &caller)?;
        let attrs = descriptor.attributes();
        // The contrast object has to exist, or the two assertions below are never executed and the
        // test goes green having measured nothing. The DACL this test wrote concedes the generic
        // data rights `CreatePipe` asks for at create time and refuses only the two security rights,
        // so the create is expected to succeed; a refusal here is a broken fixture — a mask that
        // forgot the rights its own creator asked for — and that is a regression to report, not a
        // condition to skip past. The failure names the object, the conceded mask in hex and the
        // Win32 code, all three of which `create_fixture_pipe` already puts into `detail`.
        let (read, write) =
            create_fixture_pipe(&attrs, FIXTURE_DATA_ONLY_ACCESS).map_err(|(code, detail)| {
                WindowsSandboxError::Launch {
                    reason: format!(
                        "the foreign-handle test could not create the contrast pipe its own \
                         descriptor is meant to make possible, so its refused security read was \
                         never measured: {detail}"
                    ),
                    code,
                }
            })?;
        // The object is perfectly usable for data: what is out of reach is its descriptor, not the
        // stream. One byte proves the pipe works, so the refusals below cannot be blamed on an object
        // that never came into being.
        let mut written = 0u32;
        // SAFETY: a one-byte buffer, a live write end, and one out-parameter.
        let wrote =
            unsafe { WriteFile(write, b"x".as_ptr(), 1, &mut written, std::ptr::null_mut()) };
        assert!(
            wrote != 0 && written == 1,
            "WriteFile on the write end {write:p} must still work: the fixture DACL concedes it the \
             data rights of a stream, {}",
            last_win32_error()
        );
        let mut buffer = [0u8; 1];
        let mut got = 0u32;
        // SAFETY: a one-byte buffer, a live read end, and one out-parameter.
        let read_ok =
            unsafe { ReadFile(read, buffer.as_mut_ptr(), 1, &mut got, std::ptr::null_mut()) };
        assert!(
            read_ok != 0 && got == 1 && buffer == *b"x",
            "ReadFile on the read end {read:p} must return the byte the data rights wrote: {}",
            last_win32_error()
        );
        // (i) the security read through the handle the create returned is refused: that handle carries
        // the rights of a stream, and the object's DACL does not add the security ones.
        assert_eq!(
            security_read_code(write),
            ERROR_ACCESS_DENIED,
            "GetSecurityInfo(DACL_SECURITY_INFORMATION) through the data-rights handle {write:p} \
             must be refused: the fixture DACL denies this process READ_CONTROL"
        );
        // (ii) and duplicating the handle asking for exactly the rights the security calls need does
        // not repair it: the copy is granted what the object's descriptor concedes, and nothing more.
        match duplicate_for_security(write) {
            Err(code) => assert_eq!(
                code, ERROR_ACCESS_DENIED,
                "DuplicateHandle(READ_CONTROL | WRITE_DAC) on the data-rights handle {write:p} is \
                 judged against the object's DACL, which denies both of those rights to this \
                 process"
            ),
            Ok(duplicated) => {
                // Even where the copy itself is granted, the read through it still answers the same
                // way, because the object's DACL is the thing that decides.
                assert_eq!(
                    security_read_code(duplicated),
                    ERROR_ACCESS_DENIED,
                    "the READ_CONTROL | WRITE_DAC duplicate {duplicated:p} of the data-rights handle \
                     {write:p} reads no more of the object's DACL than the handle did"
                );
                // SAFETY: this test created the duplicate, so this test closes it.
                unsafe { CloseHandle(duplicated) };
            }
        }
        // SAFETY: the two pipe ends were created by this test.
        unsafe {
            CloseHandle(read);
            CloseHandle(write);
        }
        Ok(())
    }

    /// The attribute list is sized by the API, in two passes; the extended startup block is the
    /// plain block plus the list pointer, which is what `STARTUPINFOEXW::cb` has to name.
    #[test]
    fn test_the_attribute_list_is_sized_by_the_api_and_holds_the_two_attributes()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            std::mem::size_of::<STARTUPINFOEXW>(),
            std::mem::size_of::<STARTUPINFOW>()
                + std::mem::size_of::<LPPROC_THREAD_ATTRIBUTE_LIST>(),
            "the extended block is the plain one plus the list pointer"
        );

        let mut guard = AttributeListGuard::new(2)?;
        // The API reported a size, and the buffer is that many bytes rounded up to `u64` elements
        // (which is also what guarantees the 8-byte alignment a list of pointers needs).
        assert!(guard.size > 0, "the two-pass size query must report a size");
        assert!(guard.buffer.len() * std::mem::size_of::<u64>() >= guard.size);
        assert_eq!(
            guard.buffer.as_ptr() as usize % std::mem::align_of::<u64>(),
            0
        );
        assert!(!guard.as_list().is_null());

        // A real `SECURITY_CAPABILITIES` block, built the way the profile guard builds it: the
        // list accepts it, which is what proves the attribute constant and the size argument
        // agree. The SID is allocated the way the capability list allocates its own.
        let sid = allocate_app_package_sid(&[3, 1])?;
        let capabilities = CapabilitySids::internet_client()?;
        let block = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: capabilities.as_sid_attributes(),
            CapabilityCount: capabilities.count(),
            Reserved: 0,
        };
        guard.set_security_capabilities(&block)?;

        // And the handle list, with the std handles of this process as the confined launch hands
        // them over.
        let handles = stdio_state().handles;
        guard.set_handle_list(&handles)?;

        // SAFETY: the SID was allocated by `AllocateAndInitializeSid`, whose documented release is
        // `FreeSid`; the capability array belongs to `capabilities`, which outlives this call.
        unsafe { FreeSid(sid) };
        Ok(())
    }

    /// The exit-code surface and the probe constants: a status stays a status, and the probe asks
    /// the shell for nothing but `exit 0`.
    #[test]
    fn test_raw_statuses_reach_the_caller_as_exit_codes() {
        assert_eq!(confined_exit_code(0), 0);
        assert_eq!(confined_exit_code(3), 3);
        // NTSTATUS-shaped statuses wrap to the negative number a shell reports.
        assert_eq!(
            confined_exit_code(STATUS_ACCESS_VIOLATION as u32),
            -1073741819
        );
        assert!(CONFINEMENT_FAILURE_STATUSES.contains(&(STATUS_ACCESS_VIOLATION as u32)));
        assert!(CONFINEMENT_FAILURE_STATUSES.contains(&(STATUS_ACCESS_DENIED as u32)));
        assert_ne!(
            confined_exit_code(STILL_ACTIVE as u32),
            confined_exit_code(0),
            "the STILL_ACTIVE guard has something to guard against"
        );
        assert_eq!(PROBE_COMMAND, "exit 0");
        assert!(!PROBE_COMMAND.contains('\0'));
    }

    /// Every launch-side failure says the same thing about the attempt: the policy was built, and
    /// confining something with it did not work. That is enforcement, never a warning.
    #[test]
    fn test_launch_side_failures_map_onto_enforcement() {
        for err in [
            WindowsSandboxError::AttributeList {
                reason: "the attribute list refused the capability block".to_string(),
                code: 87,
            },
            WindowsSandboxError::Launch {
                reason: "CreateProcessW failed".to_string(),
                code: 2,
            },
            WindowsSandboxError::Probe {
                reason: "the confined probe process could not be launched".to_string(),
                code: 2,
            },
        ] {
            let attempt = attempt_from_windows_error(err.clone());
            match attempt {
                SandboxAttempt::EnforcementFailed { reason } => {
                    assert!(!reason.is_empty(), "the reason has to say what failed");
                }
                other => panic!("expected EnforcementFailed, got {other:?}"),
            }
            let mapped = SandboxError::from(err.clone());
            assert_eq!(
                mapped.kind(),
                "enforcement",
                "a launch-side failure must reach the caller as an enforcement failure"
            );
            assert!(!mapped.reason().is_empty());
        }
    }

    /// The rest of the mapping stays the way the policy layer left it: no identity is an
    /// availability failure, a refused rule is a rule failure, and each reaches the matching
    /// `SandboxError` class.
    #[test]
    fn test_availability_and_rule_failures_keep_their_classes() {
        let no_identity = WindowsSandboxError::Sid {
            reason: "no SID could be allocated".to_string(),
            code: 1337,
        };
        assert!(
            matches!(
                attempt_from_windows_error(no_identity.clone()),
                SandboxAttempt::Unavailable { .. }
            ),
            "no confinement identity is an availability failure"
        );
        assert_eq!(SandboxError::from(no_identity).kind(), "unavailable");

        let refused = WindowsSandboxError::AclMutate {
            path: r"C:\Windows\System32".to_string(),
            reason: "no WRITE_DAC on that object".to_string(),
            code: 5,
        };
        assert!(
            matches!(
                attempt_from_windows_error(refused.clone()),
                SandboxAttempt::RuleFailed { .. }
            ),
            "a refused rule is a rule failure"
        );
        assert_eq!(SandboxError::from(refused).kind(), "rule-setup");

        let broken_verification = WindowsSandboxError::AclVerification {
            path: r"C:\workspace".to_string(),
            reason: "the applied ACE is not visible in the DACL".to_string(),
        };
        assert!(
            matches!(
                attempt_from_windows_error(broken_verification.clone()),
                SandboxAttempt::EnforcementFailed { .. }
            ),
            "a policy that does not match the object is an enforcement failure"
        );
        assert_eq!(
            SandboxError::from(broken_verification).kind(),
            "enforcement"
        );
    }

    /// The Win32 code a failing call reported has to survive **both** renderings of the same
    /// verdict, because the refusal text is the operator's only channel to it: the log exists
    /// only for whoever knows where to look, and `run_command` output is what the operator reads.
    #[test]
    fn test_the_win32_code_survives_both_renderings_of_a_failure() {
        let refused = WindowsSandboxError::AclMutate {
            path: r"C:\work\proj".to_string(),
            reason: "SetNamedSecurityInfoW failed: Access is denied.".to_string(),
            code: 5,
        };
        let from_attempt = match attempt_from_windows_error(refused.clone()) {
            SandboxAttempt::RuleFailed { reason, .. } => reason,
            other => panic!("expected RuleFailed, got {other:?}"),
        };
        let from_error = SandboxError::from(refused.clone()).to_string();
        for rendered in [from_attempt, from_error] {
            assert!(
                rendered.contains("win32 code = 5"),
                "the Win32 code must reach the operator text, got: {rendered}"
            );
        }

        // A launch failure carries its code the same way, and a failure that is not the outcome
        // of a Win32 call must not invent one.
        let launch = WindowsSandboxError::Launch {
            reason: "CreateProcessW of the confined probe failed".to_string(),
            code: 2,
        };
        let rendered = match attempt_from_windows_error(launch.clone()) {
            SandboxAttempt::EnforcementFailed { reason } => reason,
            other => panic!("expected EnforcementFailed, got {other:?}"),
        };
        assert!(rendered.contains("win32 code = 2"), "{rendered}");
        let probe = WindowsSandboxError::Probe {
            reason: "the confined probe died with status 0x80000003".to_string(),
            code: 0x8000_0003,
        };
        let rendered = match attempt_from_windows_error(probe) {
            SandboxAttempt::EnforcementFailed { reason } => reason,
            other => panic!("expected EnforcementFailed, got {other:?}"),
        };
        assert_eq!(
            rendered, "the confined probe died with status 0x80000003",
            "a probe failure names the status in its prose, so the verdict text does not repeat \
             it as a second code: {rendered}"
        );
    }

    /// The structured fields of the log record come from the failure itself: the class names the
    /// stage, the path names the object, and the code is `0` exactly where no Win32 call failed.
    #[test]
    fn test_failure_fields_match_the_variant_that_carries_them() {
        let refused = WindowsSandboxError::AclMutate {
            path: r"C:\work\proj".to_string(),
            reason: "no WRITE_DAC on that object".to_string(),
            code: 5,
        };
        assert_eq!(refused.kind(), "acl-mutate");
        assert_eq!(refused.win32_code(), 5);
        assert_eq!(refused.path(), r"C:\work\proj");

        let probe = WindowsSandboxError::Probe {
            reason: "the confined probe exited with 1".to_string(),
            code: 1,
        };
        assert_eq!(probe.kind(), "probe");
        assert_eq!(
            probe.win32_code(),
            1,
            "the status the confined child answered with is the code the log field names"
        );
        assert_eq!(probe.path(), "", "the probe names no object");

        // The desktop case for a profile refusal: `E_ACCESSDENIED` from
        // `CreateAppContainerProfile`. The HRESULT is the status that call reported, so the log
        // field has to carry it rather than `0`.
        let profile = WindowsSandboxError::Profile {
            name: "marmel-sandbox-profile".to_string(),
            reason: "CreateAppContainerProfile failed: Access is denied. (os error -2147024891) \
                 (HRESULT 0x80070005, decimal -2147024891, wrapping win32 code 5)"
                .to_string(),
            code: 0x8007_0005,
        };
        assert_eq!(profile.kind(), "profile");
        assert_eq!(
            profile.win32_code(),
            0x8007_0005,
            "the status the failing call answered with is the code the log field names"
        );
        assert_eq!(profile.path(), "", "a profile name is not an object path");
    }

    /// Every variant that *is* the outcome of a Win32 call has to report the status that call
    /// answered with, and the class it is reported under: `AclRead` and `Profile` used to answer
    /// `0` while their prose named `5` and an `HRESULT`. Those were two ranks of a desktop
    /// refusal, one log line apart and impossible to tell apart from the `code` field alone
    /// (t-030 §5-§6).
    #[test]
    fn test_each_variant_reports_its_own_kind_and_the_status_of_its_call() {
        let cases: Vec<WindowsSandboxError> = vec![
            // `E_ACCESSDENIED`: the status `CreateAppContainerProfile` answers with when the
            // profile directory cannot be created (a redirected, mandatory or guarded profile).
            WindowsSandboxError::Profile {
                name: "marmel-sandbox-x".to_string(),
                reason: "CreateAppContainerProfile failed".to_string(),
                code: 0x8007_0005,
            },
            WindowsSandboxError::AclRead {
                path: r"C:\work\proj".to_string(),
                reason: "GetNamedSecurityInfoW failed (win32 error 5)".to_string(),
                code: 5,
            },
            WindowsSandboxError::Sid {
                reason: "AllocateAndInitializeSid(S-1-15-3-1) failed".to_string(),
                code: 1337,
            },
            WindowsSandboxError::AttributeList {
                reason: "the size query for an attribute list reported 0 bytes".to_string(),
                code: 24,
            },
            // The two that already reported their code, unchanged.
            WindowsSandboxError::AclMutate {
                path: r"C:\work\proj".to_string(),
                reason: "SetNamedSecurityInfoW failed".to_string(),
                code: 1314,
            },
            WindowsSandboxError::Launch {
                reason: "CreateProcessW failed".to_string(),
                code: 2,
            },
            // `STATUS_DLL_NOT_FOUND`: what a confined child dies with when the container cannot
            // map its own image, which is the probe rank of a desktop refusal.
            WindowsSandboxError::Probe {
                reason: "the confined probe process died with the status 0xc0000135".to_string(),
                code: 0xC000_0135,
            },
        ];
        // The class and the code belong to the variant: this is what the log record of a refusal
        // is built from, so a wrong pairing is a wrong diagnosis.
        let expected: &[(&str, WIN32_ERROR)] = &[
            ("profile", 0x8007_0005),
            ("acl-read", 5),
            ("sid", 1337),
            ("attribute-list", 24),
            ("acl-mutate", 1314),
            ("launch", 2),
            ("probe", 0xC000_0135),
        ];
        for (err, (expected_kind, expected_code)) in cases.into_iter().zip(expected.iter()) {
            assert_eq!(err.kind(), *expected_kind, "the class of {err}");
            assert_eq!(
                err.win32_code(),
                *expected_code,
                "the code the log field would carry for {err}"
            );
        }

        // The two variants decided *before or between* calls still report no code, because
        // reporting one would be a lie: the rule failure is a policy judgement, and the
        // verification failure is a DACL that does not match the policy after every call that read
        // it answered `ERROR_SUCCESS`. A `Probe` failure used to sit here and answer `0`, while its
        // prose named the status the child reported — the same asymmetry as `AclRead` and
        // `Profile`, and one log line away from the launch code it wraps.
        for (err, expected_kind) in [
            (
                WindowsSandboxError::RuleSetup {
                    path: r"C:\work\proj".to_string(),
                    reason: "the root is a denied directory".to_string(),
                },
                "rule-setup",
            ),
            (
                WindowsSandboxError::AclVerification {
                    path: r"C:\work\proj".to_string(),
                    reason: "the grant this policy applied is not visible in the DACL".to_string(),
                },
                "acl-verification",
            ),
        ] {
            assert_eq!(err.kind(), expected_kind, "the class of {err}");
            assert_eq!(err.win32_code(), 0, "no Win32 call failed behind {err}");
        }
    }

    /// An `0x8007…`-class `HRESULT` has to reach the operator as hex **and** decimal, with the
    /// Win32 code it wraps: `io::Error::from_raw_os_error` alone prints one as a bare negative
    /// number that looks like a Win32 code and is not one (t-030 §6).
    #[test]
    fn test_hresult_is_rendered_as_hex_and_decimal_for_the_operator() {
        // `E_ACCESSDENIED` as `CreateAppContainerProfile` reports it: `HRESULT` from
        // `FACILITY_WIN32` and `ERROR_ACCESS_DENIED`.
        let status: windows_sys::core::HRESULT = 0x8007_0005u32 as i32;
        let clauses = hresult_clauses(status);
        for spelling in ["0x80070005", "-2147024891", "win32 code 5"] {
            assert!(
                clauses.contains(spelling),
                "the reason line has to carry the {spelling} spelling: {clauses}"
            );
        }
        // The reason text the operator of a profile refusal actually reads.
        let reason = format!(
            "CreateAppContainerProfile failed: {} {}",
            hresult_text(status),
            hresult_clauses(status)
        );
        assert!(reason.contains("Access is denied"), "{reason}");
        assert!(reason.contains("0x80070005"), "{reason}");
        assert!(reason.contains("-2147024891"), "{reason}");

        // A plain Win32 code is spelled as the one number it is, not as an `HRESULT`.
        assert_eq!(hresult_clauses(5), "(win32 code 5)");
        // And the mapping back to a code the operator can look up has to be exact in both
        // directions: the Win32 facility wraps, anything else does not.
        assert_eq!(win32_code_of_hresult(0x8007_0005), Some(5));
        assert_eq!(win32_code_of_hresult(0x8007_04CF), Some(1231));
        assert_eq!(
            win32_code_of_hresult(5),
            None,
            "a plain Win32 error has no `HRESULT` form to unwrap"
        );
        assert_eq!(
            win32_code_of_hresult(0xC000_0005),
            None,
            "an access-violation status does not wrap a Win32 code"
        );
    }

    /// A probe refusal has to carry the status that made it one — the crash status the confined
    /// child died with, the exit code it returned, or, when it never started, the code of the
    /// launch behind it. All three are real machine statuses, and a `code = 0` in the capture of a
    /// probe-rank refusal reads as "nothing reported" (t-030 §5-§6).
    #[test]
    fn test_a_probe_failure_reports_the_status_behind_it() {
        // `STATUS_ACCESS_DENIED`: the container refused the child its own image.
        let crash = WindowsSandboxError::Probe {
            reason: "the confined probe process died with the status 0xc0000022".to_string(),
            code: 0xC000_0022,
        };
        assert_eq!(crash.kind(), "probe");
        assert_eq!(
            crash.win32_code(),
            0xC000_0022,
            "the crash status is the diagnosis"
        );

        // A plain non-zero exit is a status too, and is the one that says the confinement worked
        // and the command inside it did not.
        let exit = WindowsSandboxError::Probe {
            reason: "the confined probe process (exit 0) exited with 73".to_string(),
            code: 73,
        };
        assert_eq!(exit.win32_code(), 73, "a non-zero exit is a status");
        assert_eq!(exit.path(), "", "the probe names no object");

        // The exit arm spells an `NTSTATUS`-shaped status so it cannot be mistaken for a small
        // command exit code, and leaves a plain one as the decimal a shell hands back.
        assert_eq!(exit_status_text(0), "0");
        assert_eq!(exit_status_text(73), "73");
        assert_eq!(
            exit_status_text(0xC000_0374),
            "0xc0000374 (decimal -1073740940)",
            "a heap-corruption status must not read as a command exit code"
        );

        // The launch-behind case is carried structurally, which is what keeps the code of a probe
        // that never started from being lost in the reason text it is folded into.
        let never_started = WindowsSandboxError::Probe {
            reason: "the confined probe process could not be launched: CreateProcessW failed"
                .to_string(),
            code: 2,
        };
        assert_eq!(never_started.win32_code(), 2);
        assert_eq!(never_started.kind(), "probe");
    }

    /// An `HRESULT`- or `NTSTATUS`-class status has to be recognisable as one wherever it is
    /// read: in the operator's reason line, and as `code_hex` next to the decimal `code` field of
    /// the log record. `2147942405` is not recognisable as `E_ACCESSDENIED`, and the decimal-only
    /// field left the operator to work out which number to look up.
    #[test]
    fn test_the_machine_status_is_spelled_in_hex_for_the_operator_and_the_log() {
        // A plain Win32 code keeps the single spelling the operator and the log grep for.
        assert_eq!(
            append_win32_code("no WRITE_DAC on that object".to_string(), 5),
            "no WRITE_DAC on that object (win32 code = 5)"
        );
        // `0` is not a code, and is deliberately not spelled as one.
        assert_eq!(append_win32_code("quiet".to_string(), 0), "quiet");

        // `E_ACCESSDENIED` from `CreateAppContainerProfile`: hex first, with the decimal the log
        // field carries kept beside it so the two spellings cannot be confused for two numbers.
        let profile_line = append_win32_code(
            "the AppContainer profile could not be created".to_string(),
            0x8007_0005,
        );
        for spelling in ["0x80070005", "-2147024891"] {
            assert!(
                profile_line.contains(spelling),
                "the reason line has to carry the {spelling} spelling: {profile_line}"
            );
        }
        // The crash status of a confined child, the same class of number.
        let probe_line = append_win32_code("the confined probe failed".to_string(), 0xC000_0135);
        let probe_decimal = (0xC000_0135u32 as i32).to_string();
        for spelling in ["0xc0000135", probe_decimal.as_str()] {
            assert!(
                probe_line.contains(spelling),
                "the reason line has to carry the {spelling} spelling: {probe_line}"
            );
        }

        // The record of a refusal names the same number twice, once per spelling, for whichever
        // variant it describes — including the ones with no code, which say so in both.
        let profile = WindowsSandboxError::Profile {
            name: "marmel-sandbox-x".to_string(),
            reason: "CreateAppContainerProfile failed".to_string(),
            code: 0x8007_0005,
        };
        assert_eq!(
            log_code_fields(&profile),
            (0x8007_0005, "0x80070005".to_string())
        );
        let refused = WindowsSandboxError::AclRead {
            path: r"C:\work\proj".to_string(),
            reason: "GetNamedSecurityInfoW failed (win32 error 5)".to_string(),
            code: 5,
        };
        assert_eq!(log_code_fields(&refused), (5, "0x00000005".to_string()));
        let probe = WindowsSandboxError::Probe {
            reason: "the confined probe process died with the status 0xc0000135".to_string(),
            code: 0xC000_0135,
        };
        assert_eq!(
            log_code_fields(&probe),
            (0xC000_0135, "0xc0000135".to_string())
        );
        let judgement = WindowsSandboxError::RuleSetup {
            path: r"C:\work\proj".to_string(),
            reason: "the root is a denied directory".to_string(),
        };
        assert_eq!(log_code_fields(&judgement), (0, "0x00000000".to_string()));
    }

    /// The two spellings one location can be written in have to compare as one location: the
    /// workspace root reaches the policy canonicalised (`\\?\C:\…`, the shape `canonicalize()`
    /// produces and that the name-based ACL APIs are handed), while the denied directories come
    /// from the environment in their plain spelling (t-030 §3.4).
    #[test]
    fn test_verbatim_and_plain_spellings_of_one_location_compare_equal() {
        let plain = Path::new(r"C:\Users\fredrik\source\marmel");
        let verbatim = Path::new(r"\\?\C:\Users\fredrik\source\marmel");
        assert_eq!(
            comparable_components(plain),
            comparable_components(verbatim),
            "one volume, spelled two ways, must compare as one location"
        );
        assert!(
            paths_match(plain, verbatim),
            "{} vs {}",
            plain.display(),
            verbatim.display()
        );
        assert!(
            paths_match(verbatim, plain),
            "{} vs {}",
            verbatim.display(),
            plain.display()
        );
        // The marker is a marker, not a segment: it leaves nothing behind in the comparison.
        assert_eq!(
            comparable_components(Path::new(r"\\?\C:\Users\fredrik")),
            vec!["c:".to_string(), "users".to_string(), "fredrik".to_string()],
            "the verbatim marker and the separators must not leave a segment behind"
        );

        // The relation `sensitive_conflict` asks about, between a plain denied directory and a
        // canonicalised root: impossible before the fix, because the drive prefix differed.
        assert!(
            is_ancestor_or_self(
                Path::new(r"C:\Users\fredrik"),
                Path::new(r"\\?\C:\Users\fredrik\.ssh")
            ),
            "a denied directory under the profile root has to be reachable from it"
        );

        // A UNC root: `canonicalize()` spells a mapped, redirected or WSL share `\\?\UNC\…`.
        let plain_unc = Path::new(r"\\server\share\work");
        let verbatim_unc = Path::new(r"\\?\UNC\server\share\work");
        assert!(
            paths_match(plain_unc, verbatim_unc),
            "{} vs {}",
            plain_unc.display(),
            verbatim_unc.display()
        );
        assert!(
            is_ancestor_or_self(Path::new(r"\\?\UNC\server\share"), plain_unc),
            "the share root has to be an ancestor of the directories inside it"
        );

        // Case stays irrelevant, and two different locations do not collapse onto each other just
        // because one of them carries a marker.
        assert!(paths_match(
            Path::new(r"\\?\C:\Users\Fredrik\.ssh"),
            Path::new(r"c:\users\fredrik\.ssh")
        ));
        assert!(
            !paths_match(Path::new(r"repos\alpha"), Path::new(r"C:\repos\alpha")),
            "a relative spelling is not the absolute one"
        );
    }

    /// The verbatim marker is itself spelled with separators, so the *other* separator spelling of
    /// one location is still that location — and used to be a different one, because the marker was
    /// only recognised in its backslash form and `//?/C:\repos` left a `?` segment behind (t-034:
    /// the same `\\?\c:` versus `c:` mismatch the task names, surviving in the spelling the strip
    /// could not see).
    #[test]
    fn test_a_verbatim_marker_written_with_either_separator_names_one_location() {
        let plain = Path::new(r"C:\Users\fredrik\source\marmel");
        for spelling in [
            r"\\?\C:\Users\fredrik\source\marmel",
            r"//?/C:/Users/fredrik/source/marmel",
            r"//?\C:\Users\fredrik\source\marmel",
            r"\\?\c:\users\FREDRIK\Source\MARMEL",
        ] {
            assert!(
                paths_match(plain, Path::new(spelling)),
                "{spelling} is one directory on one volume: {:?}",
                comparable_components(Path::new(spelling))
            );
            // The relation is symmetric, because it is one comparison used both ways.
            assert!(paths_match(Path::new(spelling), plain), "{spelling}");
        }

        // The drive root in every spelling the policy meets: this is the comparison the required
        // workspace-root entry turns on, and a bare `c:` is one location with `c:\` and with the
        // verbatim spelling of either.
        let drive_root = Path::new(r"c:\");
        for spelling in [r"c:", r"C:", r"\\?\c:", r"\\?\C:\", r"//?/c:", r"//?/c:/"] {
            assert!(
                paths_match(drive_root, Path::new(spelling)),
                "the drive root {spelling} has to compare as the drive root: {:?}",
                comparable_components(Path::new(spelling))
            );
        }

        // A verbatim **share** written with slashes is the same share as the plain spelling, and the
        // device marker loses its marker whichever separator writes it.
        let plain_share = Path::new(r"\\server\share\work");
        for spelling in [r"\\?\UNC\server\share\work", r"//?/UNC/server/share/work"] {
            assert!(
                paths_match(plain_share, Path::new(spelling)),
                "{spelling} is one share: {:?}",
                comparable_components(Path::new(spelling))
            );
        }
        assert_eq!(
            comparable_components(Path::new(r"//./COM1")),
            comparable_components(Path::new(r"\\.\COM1")),
            "the device marker is a marker in either separator"
        );

        // What is not one location stays two: the flattening must not collapse different directories
        // onto each other, however they are spelled.
        for (left, right) in [
            (r"//?/C:\Users\alpha", r"C:\Users\beta"),
            (r"//?/C:\repos", r"\\.\repos"),
            (r"//?/C:/repos/proj", r"\\?\UNC\server\share\repos\proj"),
        ] {
            assert!(
                !paths_match(Path::new(left), Path::new(right)),
                "{left} and {right} are different locations"
            );
        }
    }

    /// Every spelling of the workspace root resolves to the **one** required entry it names. Before
    /// the fix a `//?/C:\…` extra became a second grant on the object the table already carries — and
    /// on the required entry itself, the duplicate, profile-root and sensitive-directory checks all
    /// silently missed it (t-034).
    #[test]
    fn test_every_spelling_of_the_workspace_root_resolves_to_one_entry()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let root = host.workspace.clone();
        // The spellings come from the paths themselves rather than from `path_text`, so the leg sees
        // the verbatim marker whether or not the display helper keeps it.
        let plain = root.to_string_lossy().into_owned();
        let verbatim = verbatim_spelling(&root).to_string_lossy().into_owned();
        let spellings = [
            plain.clone(),
            verbatim.clone(),
            plain.replace('\\', "/"),
            verbatim.replace('\\', "/"),
        ];
        let base = build_policy_table(&root, &[], &host.host)?;

        for spelling in &spellings {
            let table = build_policy_table(&root, std::slice::from_ref(spelling), &host.host)?;
            let matching = table
                .entries
                .iter()
                .filter(|entry| paths_match(&entry.path, &root))
                .count();
            assert_eq!(
                matching,
                1,
                "{spelling} is the directory the table already grants read-write, and may not \
                 become a second entry: {:?}",
                table
                    .entries
                    .iter()
                    .map(|entry| path_text(&entry.path))
                    .collect::<Vec<String>>()
            );
            assert_eq!(
                table.notes.len(),
                base.notes.len() + 1,
                "{spelling} has to be reported as the non-entry it is, not dropped: {:?}",
                table.notes
            );
        }

        // The backslash verbatim spelling is unambiguously absolute, so it earns the *duplicate*
        // note: the skip vocabulary the shared layer publishes, not a bespoke Windows one.
        let table = build_policy_table(&root, std::slice::from_ref(&verbatim), &host.host)?;
        assert!(
            table
                .notes
                .iter()
                .any(|note| note.contains("already covered by the built-in allow-list")),
            "the verbatim spelling of an entry is a duplicate of it: {:?}",
            table.notes
        );
        Ok(())
    }

    /// Propagation is best effort. A grant that is written on the object but could not be pushed
    /// onto what already existed under it becomes a warning, a named clause of the partially
    /// enforced reason, and nothing else: it never reaches the fatal rule-setup arm that used to
    /// make every command on such a host exit with the refusal code (t-034).
    #[test]
    fn test_a_stalled_propagation_degrades_to_a_note_and_never_to_a_rule_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let snapshot = DaclSnapshot::read(dir.path())?;
        let missing = dir.path().join("no-such-descendant-here");

        // A name that does not resolve cannot be propagated, and the refusal is a note.
        let note = propagate_grant(&missing, snapshot.dacl())
            .expect("propagating onto a name that does not resolve is refused");
        for expected in [
            path_text(&missing).as_str(),
            "written on the object itself",
            "SetNamedSecurityInfoW failed",
            "icacls",
            "/reset",
        ] {
            assert!(
                note.contains(expected),
                "the propagation note has to name {expected}: {note}"
            );
        }
        assert_eq!(note.lines().count(), 1, "one line per entry: {note}");

        // Propagating an object's own DACL back onto that object is a success, and a success is not
        // a note: the degraded verdict is reserved for a real decline.
        assert!(
            propagate_grant(dir.path(), snapshot.dacl()).is_none(),
            "propagating onto the object that was just granted cannot be refused"
        );

        // The verdict half: nothing stalled is nothing to report, and a stalled grant degrades the
        // run without refusing it.
        assert_eq!(
            propagation_reason_for(&[]),
            None,
            "a policy whose grants all reached their tree is the whole policy"
        );
        let reason = propagation_reason_for(std::slice::from_ref(&note))
            .expect("one stalled propagation degrades the verdict");
        for expected in [
            "1 of them",
            "did not reach the objects that already existed",
            path_text(&missing).as_str(),
            "/reset",
        ] {
            assert!(
                reason.contains(expected),
                "the partial reason has to name {expected}: {reason}"
            );
        }
        let attempt = SandboxAttempt::PartiallyEnforced { reason };
        assert!(
            sandbox::sandbox_outcome(&attempt).is_ok(),
            "a degraded run still runs the confined command"
        );
        assert!(
            !matches!(
                sandbox::sandbox_outcome(&attempt),
                Err(SandboxError::RuleSetup { .. })
            ),
            "a stalled propagation must never reach the fatal rule-setup arm"
        );
        Ok(())
    }

    /// A genuine access refusal on the **required** workspace-root grant stays fatal — the mission is
    /// fail-closed unless the operator opts out — but it is fatal *and* actionable: the right that
    /// was refused, the object it was refused on, where in the sequence it happened, and the
    /// commands that show and fix it (t-034).
    #[test]
    fn test_a_refused_required_grant_stays_fatal_and_names_the_right_it_needed() {
        let entry = PolicyPath {
            path: PathBuf::from(r"C:\work\proj"),
            grant: Grant::ReadWrite,
            required: true,
        };
        let object = path_text(&entry.path);
        let refused = WindowsSandboxError::AclMutate {
            path: object.clone(),
            reason: "SetSecurityInfo failed: Access is denied.".to_string(),
            code: ERROR_ACCESS_DENIED,
        };
        assert!(
            refused.is_access_refusal(),
            "an access refusal on the DACL write is exactly the class the classification exists for"
        );

        let actionable = required_grant_refusal(&entry, refused);
        let WindowsSandboxError::AclMutate { path, reason, code } = &actionable else {
            panic!("a refused required grant stays the same class of failure, got {actionable:?}");
        };
        assert_eq!(
            *path, object,
            "and it stays attached to the object that refused"
        );
        assert_eq!(
            *code, ERROR_ACCESS_DENIED,
            "the win32 code survives the rewrite"
        );
        for expected in [
            "WRITE_DAC",
            "READ_CONTROL",
            object.as_str(),
            "workspace-root grant itself",
            "best effort",
            &format!("icacls \"{object}\""),
            "/grant",
            "/reset",
            crate::harness::pty::SANDBOX_OPTOUT_ENV,
        ] {
            assert!(
                reason.contains(expected),
                "the refusal has to name {expected}: {reason}"
            );
        }
        // The original diagnosis is kept, and the guidance appended to it, so the number the call
        // reported is not edited out of the operator's line of sight.
        assert!(
            reason.starts_with("SetSecurityInfo failed:"),
            "the refusal keeps the call that failed: {reason}"
        );
        assert!(reason.contains(" — "), "the guidance is appended: {reason}");

        // The class survives the mapping into the public verdict: still terminal, still `rule-setup`
        // (which is the exit code every command on that host carries), never a downgraded warning.
        assert_eq!(
            SandboxError::from(actionable.clone()).kind(),
            "rule-setup",
            "the required grant stays a rule failure"
        );
        assert!(
            matches!(
                attempt_from_windows_error(actionable),
                SandboxAttempt::RuleFailed { .. }
            ),
            "and the attempt surface says the same thing"
        );

        // The read half is classified too: the same token limitation is reached one call earlier,
        // and the operator has to be told the same thing about it.
        let read_refused = WindowsSandboxError::AclRead {
            path: object.clone(),
            reason: "GetNamedSecurityInfoW failed (win32 error 5)".to_string(),
            code: ERROR_ACCESS_DENIED,
        };
        assert!(
            read_refused.is_access_refusal(),
            "the DACL read is refused by the same missing right"
        );
        let actionable_read = required_grant_refusal(&entry, read_refused);
        assert!(
            matches!(&actionable_read, WindowsSandboxError::AclRead { code, .. } if *code == ERROR_ACCESS_DENIED),
            "a refused read stays a refused read: {actionable_read:?}"
        );
        let rendered = actionable_read.to_string();
        assert!(
            rendered.contains("WRITE_DAC")
                && rendered.contains(&object)
                && rendered.contains("icacls")
                && rendered.contains(crate::harness::pty::SANDBOX_OPTOUT_ENV),
            "and it says what to do about it: {rendered}"
        );

        // Everything that is not an access refusal is handed back untouched: a name that does not
        // resolve or an object with no security is not fixed by `icacls`, and dressing those up in
        // WRITE_DAC advice would send the operator to a lever that cannot move them.
        for code in [2u32, 1350] {
            // `ERROR_FILE_NOT_FOUND` and `ERROR_CANT_ACCESS_FILE`: neither is a right this token lacks.
            let other = WindowsSandboxError::AclMutate {
                path: object.clone(),
                reason: "SetSecurityInfo failed".to_string(),
                code,
            };
            assert!(
                !other.is_access_refusal(),
                "win32 error {code} is not an access refusal"
            );
            let handed_back = required_grant_refusal(&entry, other);
            assert!(
                matches!(&handed_back, WindowsSandboxError::AclMutate { reason, code: reported, path: _ }
                    if !reason.contains("icacls") && *reported == code),
                "a non-refusal comes back exactly as it went in: {handed_back:?}"
            );
        }
    }

    /// The sensitive-directory invariant has to fire whichever way the paths are spelled, and keep
    /// telling the two kinds apart: an entry that exposes a denied directory through inheritance,
    /// and an entry that sits inside one.
    #[test]
    fn test_sensitive_conflict_reports_both_kinds_for_either_spelling() {
        let denied: Vec<PathBuf> = vec![PathBuf::from(r"C:\Users\fredrik\.ssh")];
        // The root that *contains* a denied directory: plain, and canonicalised.
        for root in [
            Path::new(r"C:\Users\fredrik"),
            Path::new(r"\\?\C:\Users\fredrik"),
        ] {
            let conflict = sensitive_conflict(root, &denied)
                .unwrap_or_else(|| panic!("{} must conflict", root.display()));
            assert_eq!(
                conflict.kind,
                SensitiveConflictKind::Contains,
                "for {}",
                root.display()
            );
            assert!(
                conflict.note().contains("contains the sensitive directory"),
                "{}",
                conflict.note()
            );
        }
        // The root that *sits inside* a denied directory: plain, and canonicalised.
        for root in [
            Path::new(r"C:\Users\fredrik\.ssh\repos"),
            Path::new(r"\\?\C:\Users\fredrik\.ssh\repos"),
        ] {
            let conflict = sensitive_conflict(root, &denied)
                .unwrap_or_else(|| panic!("{} must conflict", root.display()));
            assert_eq!(
                conflict.kind,
                SensitiveConflictKind::Inside,
                "for {}",
                root.display()
            );
            assert!(
                conflict
                    .note()
                    .contains("sits inside the sensitive directory"),
                "{}",
                conflict.note()
            );
        }
        // The same two directions for a UNC root, the shape a mapped, redirected or WSL workspace
        // canonicalises to.
        let unc_denied: Vec<PathBuf> = vec![PathBuf::from(r"\\server\share\.ssh")];
        for root in [
            Path::new(r"\\server\share"),
            Path::new(r"\\?\UNC\server\share"),
        ] {
            let conflict = sensitive_conflict(root, &unc_denied)
                .unwrap_or_else(|| panic!("{} must conflict", root.display()));
            assert_eq!(
                conflict.kind,
                SensitiveConflictKind::Contains,
                "for {}",
                root.display()
            );
        }
        for root in [
            Path::new(r"\\server\share\.ssh\repos"),
            Path::new(r"\\?\UNC\server\share\.ssh\repos"),
        ] {
            let conflict = sensitive_conflict(root, &unc_denied)
                .unwrap_or_else(|| panic!("{} must conflict", root.display()));
            assert_eq!(
                conflict.kind,
                SensitiveConflictKind::Inside,
                "for {}",
                root.display()
            );
        }
        // A sibling of a denied directory is not a conflict in either spelling: the checks above
        // discriminate, they do not reject everything.
        for root in [
            Path::new(r"C:\Users\fredrik\source\marmel"),
            Path::new(r"\\?\C:\Users\fredrik\source\marmel"),
        ] {
            assert!(
                sensitive_conflict(root, &denied).is_none(),
                "{} is neither inside nor an ancestor of the denied directory",
                root.display()
            );
        }
    }

    /// The hard failure for a workspace root with no honest policy has to fire for the
    /// canonicalised spelling, not only for the plain fixture path the tests used: the production
    /// path canonicalises the root before the table is built (t-030 §0.5 and §3.4).
    #[test]
    fn test_policy_table_fails_closed_for_a_canonicalised_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let inside = host.profile.join(".ssh").join("repo");
        std::fs::create_dir_all(&inside)?;
        // Both spellings the policy layer can be handed: the plain one, and the verbatim one
        // `canonicalize()` produces for it on Windows.
        let canonical = std::fs::canonicalize(&inside)?;
        assert_ne!(
            canonical, inside,
            "the fixture has to reach the table in both spellings for the check to mean anything"
        );
        for root in [inside.clone(), canonical] {
            match build_policy_table(&root, &[], &host.host) {
                Ok(table) => panic!(
                    "{} names no honest policy, got {:?}",
                    root.display(),
                    table.entries
                ),
                Err(err @ WindowsSandboxError::RuleSetup { .. }) => {
                    let rendered = err.to_string();
                    assert!(
                        rendered.contains("sits inside the sensitive directory"),
                        "the refusal has to name the denied directory it sits inside: {rendered}"
                    );
                }
                Err(other) => panic!("expected a rule setup failure, got {other:?}"),
            }
        }
        Ok(())
    }

    /// The notes a non-entry earns are chosen by comparing paths, so the verbatim spelling of the
    /// profile root — or of a directory already on the list — still earns the note that belongs to
    /// it: the comparison has to see through the marker for that to hold.
    #[test]
    fn test_a_verbatim_spelling_of_an_entry_earned_its_own_note()
    -> Result<(), Box<dyn std::error::Error>> {
        let host = FakeHost::new()?;
        let extra = host.workspace.join("extra");
        std::fs::create_dir_all(&extra)?;
        let verbatim_profile = verbatim_spelling(&host.profile);
        let verbatim_extra = verbatim_spelling(&extra);
        let extras = vec![
            // The profile root, spelled the way `canonicalize()` spells it.
            verbatim_profile.display().to_string(),
            // One real directory, in its plain spelling and its verbatim spelling.
            extra.display().to_string(),
            verbatim_extra.display().to_string(),
        ];

        let table = build_policy_table(&host.workspace, &extras, &host.host)?;

        assert!(
            table.notes.iter().any(|note| {
                note.contains("the user profile directory itself is never granted")
                    && note.starts_with(&path_text(verbatim_profile.as_path()))
            }),
            "the verbatim spelling of the profile root is rejected as the profile root: {:?}",
            table.notes
        );
        assert!(
            table.notes.iter().any(|note| {
                note.contains(&path_text(verbatim_extra.as_path()))
                    && note.contains("already covered by the built-in allow-list")
            }),
            "the verbatim spelling of a directory already on the list is the duplicate skip, not \
             a second rejection or a second grant: {:?}",
            table.notes
        );
        assert!(
            table
                .entries
                .iter()
                .any(|entry| paths_match(entry.path.as_path(), extra.as_path())),
            "exactly one spelling of the real directory becomes the grant"
        );
        Ok(())
    }

    /// The hand-off is single-shot and per-thread: a thread that never prepared a policy has
    /// nothing parked, and releasing what is not there says so rather than pretending.
    ///
    /// The value the check returns is *whether something was parked*, so the fresh thread answers
    /// `false` — an inverted expectation here reads a reported leak as a pass, which is how this
    /// test used to agree with a hand-off that had nothing in it.
    #[test]
    fn test_a_fresh_thread_holds_no_parked_setup() {
        let handled = std::thread::spawn(|| {
            assert!(
                take_windows_policy_setup().is_none(),
                "a thread that prepared nothing has no parked setup"
            );
            discard_windows_policy_setup()
        })
        .join();
        assert!(
            matches!(handled, Ok(false)),
            "the fresh-thread hand-off check must report that nothing was parked"
        );
        // The other half of the guard: the setup a run prepared belongs to the thread that prepared
        // it, and nothing is left parked on the thread the next command is launched from.
        assert!(
            !discard_windows_policy_setup(),
            "this thread holds no parked setup either, so nothing is left for the next command"
        );
    }

    /// The preparation half of the hand-off returns no setup when it failed, and leaves nothing
    /// parked behind: a failed attempt must not strand a half-applied policy.
    #[test]
    fn test_with_setup_returns_no_setup_when_preparation_fails() {
        let missing = if cfg!(target_os = "windows") {
            "Q:\\marmel-does-not-exist"
        } else {
            "/marmel-does-not-exist"
        };
        let (attempt, setup) = attempt_windows_sandbox_with_setup(Path::new(missing), &[]);
        assert!(
            setup.is_none(),
            "a failed preparation cannot hand over a setup"
        );
        match attempt {
            SandboxAttempt::RuleFailed { path, .. } => {
                assert!(path.contains("marmel-does-not-exist"), "{path}");
            }
            other => panic!("expected RuleFailed, got {other:?}"),
        }
        assert!(
            !discard_windows_policy_setup(),
            "the failed attempt must have left nothing parked"
        );
    }

    /// The judgement surface must park the setup it accepted rather than release it: the confined
    /// launch has to happen inside that very policy, so the profile and the verified ACEs have to
    /// survive the judgement. Dropping the setup here would leave [`run_in_appcontainer`] to
    /// prepare a **second** AppContainer profile over the same objects while the first one's ACEs
    /// are restored in the middle of the run — the hand-off is what makes it one policy.
    ///
    /// Exactly one answer is expected per verdict: a `FullyEnforced` judgement parked a setup, and
    /// every other verdict parked nothing (a failed preparation cannot hand over what it never
    /// built).
    #[test]
    fn test_the_judgement_surface_parks_its_setup_for_the_launch_layer()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;

        let attempt = attempt_windows_sandbox(&workspace, &[]);
        let parked = take_windows_policy_setup().is_some();
        assert_eq!(
            parked,
            matches!(attempt, SandboxAttempt::FullyEnforced),
            "the policy the judgement accepted must reach the launch half, got {attempt:?} with \
             parked={parked}"
        );
        assert!(
            !discard_windows_policy_setup(),
            "picking the setup up leaves nothing parked: one profile per run"
        );
        Ok(())
    }

    /// A confined command in a temporary workspace: what the composite entry point answers.
    ///
    /// The workspace is a `tempfile` directory, so nothing outside it is created, and the command is
    /// the shell's own `exit 0` shape: either the confined child ran and left 0, or the run stopped
    /// at a stage that names itself. What must not happen is a hang, a leaked profile or a parked
    /// setup left behind.
    #[test]
    fn test_run_in_appcontainer_answers_for_a_confined_command_in_a_temp_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace)?;

        let outcome = run_in_appcontainer(&workspace, &[], &workspace, "echo hi");
        match outcome {
            Ok(code) => assert_eq!(code, 0, "the confined command left the wrong exit code"),
            Err(err) => {
                // Not a success: this host could not confine the command, either because the
                // policy could not be applied to the real objects (unavailable, rule-setup) or
                // because the confined launch itself was refused (enforcement). Both answer with a
                // reason the operator can act on, and neither is a silent pass.
                assert!(!err.reason().is_empty(), "the refusal must name a reason");
                assert!(
                    matches!(err.kind(), "unavailable" | "rule-setup" | "enforcement"),
                    "unexpected error class: {}",
                    err.kind()
                );
            }
        }
        assert!(
            !discard_windows_policy_setup(),
            "the run must not leave a setup parked"
        );
        assert!(workspace.is_dir(), "the confined run left the workspace");
        Ok(())
    }
}
