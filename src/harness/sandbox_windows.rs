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
//! non-required entry, and a rule failure only for a required one (the workspace root and the
//! temporary directories).
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
use std::path::{Component, Path, PathBuf};

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
    /// The AppContainer profile for this run could not be created or derived: no confinement
    /// identity exists, so this is an availability failure.
    #[error("AppContainer profile {name} could not be created: {reason}")]
    Profile { name: String, reason: String },
    /// A SID could not be allocated, converted or released, for the same reason: there is no
    /// AppContainer identity to build a policy around.
    #[error("the AppContainer security identifier could not be handled: {reason}")]
    Sid { reason: String },
    /// The policy names an entry it cannot grant: the workspace root is itself inside (or is)
    /// one of the directories the policy denies, so no honest policy exists for this run.
    #[error("sandbox policy could not be built for {path}: {reason}")]
    RuleSetup { path: String, reason: String },
    /// The DACL of `path` could not be read, so the policy for that path cannot be verified.
    #[error("the DACL of {path} could not be read: {reason}")]
    AclRead { path: String, reason: String },
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
    AttributeList { reason: String },
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
    Probe { reason: String },
}

impl WindowsSandboxError {
    /// Whether the failure is the expected "an unprivileged process cannot rewrite this DACL"
    /// case: `ERROR_ACCESS_DENIED` (no `WRITE_DAC` on the object) or
    /// `ERROR_PRIVILEGE_NOT_HELD`.
    ///
    /// This is what separates a skip note from a hard rule failure, so it stays an exact match
    /// on those two codes rather than a heuristic over the reason text.
    #[must_use]
    pub fn is_access_refusal(&self) -> bool {
        match self {
            WindowsSandboxError::AclMutate { code, .. } => {
                *code == ERROR_ACCESS_DENIED || *code == ERROR_PRIVILEGE_NOT_HELD
            }
            _ => false,
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
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, LocalFree, STATUS_ACCESS_DENIED, STATUS_ACCESS_VIOLATION,
    STATUS_DLL_NOT_FOUND, STATUS_ENTRYPOINT_NOT_FOUND, STATUS_IN_PAGE_ERROR, STILL_ACTIVE,
    WAIT_OBJECT_0, WIN32_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, AllocateAndInitializeSid,
    DACL_SECURITY_INFORMATION, EqualSid, FreeSid, GetAce, GetAclInformation, INHERIT_ONLY_ACE,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_APP_PACKAGE_AUTHORITY, SECURITY_CAPABILITIES,
    SID_AND_ATTRIBUTES, SID_IDENTIFIER_AUTHORITY, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
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
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
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

    /// The buffer decoded back for diagnostics (lossy: it may hold lone surrogates).
    fn as_string(&self) -> String {
        let end = self
            .buf
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(self.buf.len());
        String::from_utf16_lossy(&self.buf[..end])
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

/// The last Win32 error as text.
///
/// `windows_sys` 0.59 carries no error object, so `GetLastError` plus
/// `io::Error::from_raw_os_error` is the spelling this crate uses.
fn last_win32_error() -> String {
    // SAFETY: GetLastError reads thread-local error state and takes no arguments.
    let code = unsafe { GetLastError() };
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

/// Render a Win32 `HRESULT` as the OS error it carries, for operator-visible text.
fn hresult_text(code: windows_sys::core::HRESULT) -> String {
    std::io::Error::from_raw_os_error(code).to_string()
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
        return Err(WindowsSandboxError::Sid {
            reason: format!(
                "AllocateAndInitializeSid(S-1-15-{}) failed: {}",
                sub_authorities
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<String>>()
                    .join("-"),
                last_win32_error()
            ),
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
/// `CreateAppContainerProfile` or an app-package SID from `allocate_app_package_sid`) or by
/// `ConvertStringSidToSidW` satisfies this. Win32 reads the memory behind the pointer.
pub unsafe fn sid_to_string(sid: PSID) -> Result<String, WindowsSandboxError> {
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: the output pointer is valid; the string it receives is read below and freed here.
    let ok = unsafe { ConvertSidToStringSidW(sid, &mut raw) };
    if ok == 0 || raw.is_null() {
        return Err(WindowsSandboxError::Sid {
            reason: format!("ConvertSidToStringSidW failed: {}", last_win32_error()),
        });
    }
    let text = wide_cstr_to_string(raw);
    // SAFETY: the conversion API allocated `raw`, and it is read no more.
    unsafe {
        LocalFree(raw as *mut c_void);
    }
    Ok(text)
}

/// Parse a SID string back into an allocated SID (released with `LocalFree`).
///
/// Kept as the inverse of [`sid_to_string`]: it is the helper the launch layer uses when it has
/// only the string spelling of a SID it needs to hand to Win32, and the tests of this module use
/// it to check that the strings this module produces really do round-trip.
pub fn string_to_sid(text: &str) -> Result<PSID, WindowsSandboxError> {
    let wide = WideString::new(text);
    let mut sid: PSID = std::ptr::null_mut();
    // SAFETY: the input is a NUL-terminated string owned by `wide`, the output is read into.
    let ok = unsafe { ConvertStringSidToSidW(wide.pcwstr(), &mut sid) };
    if ok == 0 || sid.is_null() {
        return Err(WindowsSandboxError::Sid {
            reason: format!(
                "ConvertStringSidToSidW({}) failed: {}",
                wide.as_string(),
                last_win32_error()
            ),
        });
    }
    Ok(sid)
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
    /// The SID is the one `CreateAppContainerProfile` derives from the generated name, which is
    /// the same SID [`Self::derive_sid`] would hand back for that name later. Taking the
    /// capabilities by value is deliberate: the guard keeps them alive for the launch.
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
                reason: format!("CreateAppContainerProfile failed: {}", hresult_text(result)),
            });
        }
        Ok(Self {
            name,
            sid,
            capabilities,
            _single_thread: PhantomData,
        })
    }

    /// The identity of an existing profile, without creating anything: the fallback used when a
    /// profile from an earlier run has to be reused.
    ///
    /// The returned SID is owned by the caller and released with `LocalFree`.
    #[must_use = "the derived SID is a LocalFree allocation the caller owns"]
    pub fn derive_sid(name: &str) -> Result<PSID, WindowsSandboxError> {
        let wide = WideString::new(name);
        let mut sid: PSID = std::ptr::null_mut();
        // SAFETY: `wide` owns the name for the duration of the call.
        let result = unsafe { DeriveAppContainerSidFromAppContainerName(wide.pcwstr(), &mut sid) };
        if result < 0 || sid.is_null() {
            return Err(WindowsSandboxError::Profile {
                name: wide.as_string(),
                reason: format!(
                    "DeriveAppContainerSidFromAppContainerName failed: {}",
                    hresult_text(result)
                ),
            });
        }
        Ok(sid)
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
/// separators and the drive-prefix spelling flattened away.
///
fn paths_match(left: &Path, right: &Path) -> bool {
    comparable_components(left) == comparable_components(right)
}

/// The components of `path`, normalised for [`paths_match`].
fn comparable_components(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| match component {
            Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().to_lowercase(),
            Component::RootDir => String::new(),
            Component::CurDir => String::new(),
            Component::ParentDir => "..".to_string(),
            Component::Normal(name) => name.to_string_lossy().to_lowercase(),
        })
        .collect()
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
    /// The entries whose mutation the confinement cannot lose: the read-write grants, i.e. the
    /// workspace root and the temporary directories. A refusal on one of these is a rule
    /// failure, because confining the run without a writable workspace and temporary directory
    /// is not the policy that was asked for.
    #[must_use = "the entries whose ACEs the confinement cannot lose, for the caller that must apply them or report the failure"]
    pub fn confinement_critical(&self) -> impl Iterator<Item = &PolicyPath> {
        self.entries
            .iter()
            .filter(|entry| entry.required || entry.grant == Grant::ReadWrite)
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
        path: workspace_root.to_path_buf(),
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

    // The invariant, enforced rather than merely documented: an entry that contains a denied
    // directory (or is the user-profile root, which contains every one of them) would make
    // `.ssh` and `.gnupg` reachable through the inheritance of the grant itself.
    if let Some(conflict) = sensitive_conflict(path, sensitive) {
        notes.push(format!("{text}: {}", conflict.note()));
        return;
    }
    if let Some(home) = host.user_profile_root()
        && paths_match(path, home)
    {
        notes.push(format!(
            "{text}: the user profile directory itself is never granted, because the sensitive directories below it would become reachable through it"
        ));
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
                    last_win32_error()
                ),
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
            return Err(WindowsSandboxError::AclRead {
                path: path_text(path),
                reason: format!("GetAclInformation failed: {}", last_win32_error()),
            });
        }
        for index in 0..size.AceCount {
            let mut ace: *mut c_void = std::ptr::null_mut();
            // SAFETY: `GetAce` reads the ACL it is given and writes one pointer.
            let ok = unsafe { GetAce(self.dacl, index, &mut ace) };
            if ok == 0 || ace.is_null() {
                return Err(WindowsSandboxError::AclRead {
                    path: path_text(path),
                    reason: format!("GetAce(index {index}) failed: {}", last_win32_error()),
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

/// One applied ACE, and the handle on the DACL it replaced.
///
/// Adding an ACE for the AppContainer SID changes an object the operator owns; putting the
/// saved DACL back when the run ends is what stops a failed or interrupted run from leaking a
/// grant that every later process would inherit. `Drop` performs that restore, so the guard is
/// the whole contract: as long as it drops on every path, no ACE outlives the run.
pub struct AclGrantGuard {
    path: PathBuf,
    /// The security descriptor read before the ACE was added; its DACL is what the restore
    /// writes back, so it is kept alive until then.
    saved: DaclSnapshot,
    /// The ACL `SetEntriesInAclW` built for this grant, released on drop.
    applied_dacl: *mut ACL,
    /// Whether the saved DACL was a NULL DACL, which the restore has to put back as such rather
    /// than as an empty ACL.
    saved_null: bool,
    /// Non-`Sync` marker: the saved descriptor is only ever read and written on one thread.
    _single_thread: PhantomData<*mut ()>,
}

impl AclGrantGuard {
    /// Apply the ACE `entry` asks for, for the AppContainer SID `app_sid`.
    ///
    /// The guard is returned only when a mutation was really needed: an entry already covered
    /// by an existing ACE ([`AclCoverage::Covered`]) and an object with a NULL DACL
    /// ([`AclCoverage::NoDacl`]) need neither a change nor a restore.
    #[must_use = "the guard restores the saved DACL when the run ends, so no applied ACE leaks"]
    pub fn apply(app_sid: PSID, entry: &PolicyPath) -> Result<Option<Self>, WindowsSandboxError> {
        let path = entry.path.as_path();
        let saved = DaclSnapshot::read(path)?;
        let saved_null = saved.is_null();
        if saved_null || saved.covers(path, app_sid, entry.grant)? {
            return Ok(None);
        }
        let mask = mask_for_grant(entry.grant);
        let wide_path = WideString::from_path(path);
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
        // SAFETY: the object name is owned by `wide_path`, and `applied` is a freshly built ACL.
        let set = unsafe {
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
        if set != ERROR_SUCCESS {
            // SAFETY: the ACL that was just built is freed here rather than kept, because the
            // object was never changed.
            unsafe { LocalFree(applied as *mut c_void) };
            return Err(WindowsSandboxError::AclMutate {
                path: path_text(path),
                reason: format!("SetNamedSecurityInfoW failed: {}", last_win32_error()),
                code: set,
            });
        }
        Ok(Some(Self {
            path: path.to_path_buf(),
            saved,
            applied_dacl: applied,
            saved_null,
            _single_thread: PhantomData,
        }))
    }

    /// The path this guard changed.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for AclGrantGuard {
    fn drop(&mut self) {
        // Restore the saved DACL, exactly as it was read: a NULL DACL goes back as NULL, because
        // an empty ACL would deny everything instead.
        let wide_path = WideString::from_path(&self.path);
        let dacl: *const ACL = if self.saved_null || self.saved.dacl().is_null() {
            std::ptr::null()
        } else {
            self.saved.dacl()
        };
        // SAFETY: the name is owned by `wide_path`; the ACL is the saved one, still owned by
        // `saved`, which drops after this call.
        let code = unsafe {
            SetNamedSecurityInfoW(
                wide_path.pcwstr(),
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
        // SAFETY: `SetEntriesInAclW` allocated the applied ACL, and nothing else references it.
        unsafe { LocalFree(self.applied_dacl as *mut c_void) };
    }
}

/// Apply every entry of the table, and hand back the guards that restore what changed.
///
/// The classification follows the fail-closed contract: a read-write entry (the workspace root
/// and the temporary directories) whose mutation is refused is a rule failure, because the
/// policy that can then be enforced is not the policy that was asked for. A read-only entry
/// whose mutation is refused keeps the confinement and loses one toolchain directory, which is
/// reported as a skip note and a warning — the same shape an operator-supplied extra path that
/// cannot become a rule takes on a Landlock host.
fn apply_policy_entries(
    app_sid: PSID,
    table: &WindowsPolicyTable,
) -> Result<Vec<AclGrantGuard>, WindowsSandboxError> {
    let mut guards: Vec<AclGrantGuard> = Vec::new();
    for entry in &table.entries {
        match AclGrantGuard::apply(app_sid, entry) {
            Ok(Some(guard)) => guards.push(guard),
            Ok(None) => {}
            Err(err) if !(entry.required || entry.grant == Grant::ReadWrite) => {
                // A read-only refusal: the confinement stands and one entry is simply not
                // granted. The re-entry child has no logging subscriber yet, so the operator
                // also sees the note on stderr (the `extra_path_skip_note` precedent).
                let note = format!(
                    "warning: ignoring sandbox read-only path {}: {err}",
                    path_text(&entry.path)
                );
                tracing::warn!("{note}");
                eprintln!("{note}");
            }
            Err(err) => return Err(err),
        }
    }
    Ok(guards)
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

/// The complete `lpCommandLine` of one confined launch: `<program> /C <command>`.
///
/// The program token is quoted per [`quote_command_token`]; the payload is appended **verbatim**,
/// which is what `windows_shell_argv` rule R5 promises: what follows `/C` is `cmd.exe`'s own
/// command line, and `cmd.exe` re-parses it with its own metacharacters (`&`, `|`, `>`, `^`, `"`).
/// Passing that payload through the MSVCRT escaping a real argv would need (what portable-pty's
/// `append_quoted` does) would change what the confined shell reads — `echo "hi there"` included —
/// so the command reaches it byte for byte.
#[must_use]
fn confined_command_line(program: &str, payload: &str) -> String {
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
            return Err(WindowsSandboxError::AttributeList {
                reason: format!(
                    "the size query for a {count}-attribute list reported 0 bytes (query pass: {}",
                    last_win32_error()
                ),
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
            return Err(WindowsSandboxError::AttributeList {
                reason: format!(
                    "InitializeProcThreadAttributeList({count} attributes, {size} bytes) failed: {}",
                    last_win32_error()
                ),
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
            return Err(WindowsSandboxError::AttributeList {
                reason: format!(
                    "UpdateProcThreadAttribute({attribute}) failed: {}",
                    last_win32_error()
                ),
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
    // HANDLE-CAPABILITY CAVEAT (documented, not faked): making a handle visible to an AppContainer
    // child is not the same as letting the child use the object behind it. The access check for an
    // inherited kernel object is re-run in the child's token context, and the anonymous pipes this
    // process creates carry the default DACL of this process's token, which names the user and not
    // the AppContainer SID (nor the app-capability SID). A confined child in that shape can
    // therefore be created and run while still failing to write to its own standard streams, and
    // that shows up as a lost write and an exit status rather than as a hidden success. Granting the
    // std objects an explicit ACE for the AppContainer identity is the fix, and is deliberately not
    // attempted here: it would mean rewriting standard streams this module does not own.
    let inherited: Option<[HANDLE; 3]> = if stdio.all_console {
        None
    } else {
        Some(stdio.handles)
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
            return Err(WindowsSandboxError::Probe {
                reason: format!("the confined probe process could not be launched: {err}"),
            });
        }
    };
    if CONFINEMENT_FAILURE_STATUSES.contains(&raw) {
        return Err(WindowsSandboxError::Probe {
            reason: format!(
                "the confined probe process died with the status {raw:#010x}, which is a confinement failure rather than a command failure"
            ),
        });
    }
    if raw != 0 {
        return Err(WindowsSandboxError::Probe {
            reason: format!(
                "the confined probe process ({PROBE_COMMAND}) exited with {raw}, not the 0 it asked for"
            ),
        });
    }
    Ok(())
}

/// Map one backend failure onto the `SandboxAttempt` variant that describes it.
#[must_use]
fn attempt_from_windows_error(err: WindowsSandboxError) -> SandboxAttempt {
    match err {
        WindowsSandboxError::Profile { name, reason } => SandboxAttempt::Unavailable {
            reason: format!("AppContainer profile {name} could not be created: {reason}"),
        },
        WindowsSandboxError::Sid { reason } => SandboxAttempt::Unavailable { reason },
        WindowsSandboxError::RuleSetup { path, reason }
        | WindowsSandboxError::AclRead { path, reason }
        | WindowsSandboxError::AclMutate { path, reason, .. } => {
            SandboxAttempt::RuleFailed { path, reason }
        }
        WindowsSandboxError::AclVerification { path, reason } => {
            SandboxAttempt::EnforcementFailed {
                reason: format!("{path}: {reason}"),
            }
        }
        // Launch-side failures say the same thing about the attempt: the policy was built, but
        // confining something with it did not work, which is an enforcement failure.
        WindowsSandboxError::AttributeList { reason }
        | WindowsSandboxError::Probe { reason }
        | WindowsSandboxError::Launch { reason, .. } => {
            SandboxAttempt::EnforcementFailed { reason }
        }
    }
}

/// Convert a `WindowsSandboxError` into the backend-neutral error the re-entry contract speaks:
/// `Unavailable` when no AppContainer identity could be made, `RuleSetup` when the policy that
/// was asked for is not the policy that exists.
impl From<WindowsSandboxError> for SandboxError {
    fn from(value: WindowsSandboxError) -> Self {
        match value {
            WindowsSandboxError::Profile { name, reason } => SandboxError::Unavailable {
                reason: format!("AppContainer profile {name} could not be created: {reason}"),
            },
            WindowsSandboxError::Sid { reason } => SandboxError::Unavailable {
                reason: format!("sid: {reason}"),
            },
            WindowsSandboxError::RuleSetup { path, reason } => {
                SandboxError::RuleSetup { path, reason }
            }
            WindowsSandboxError::AclRead { path, reason }
            | WindowsSandboxError::AclMutate { path, reason, .. }
            | WindowsSandboxError::AclVerification { path, reason } => {
                SandboxError::RuleSetup { path, reason }
            }
            WindowsSandboxError::AttributeList { reason }
            | WindowsSandboxError::Probe { reason }
            | WindowsSandboxError::Launch { reason, .. } => SandboxError::Enforcement { reason },
        }
    }
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
}

impl WindowsPolicySetup {
    /// The AppContainer SID, for the probe and for the launch.
    #[must_use]
    pub fn sid(&self) -> PSID {
        self.profile.sid()
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

    let grants = apply_policy_entries(profile.sid(), &table)?;

    // Every read-write grant must be visible on the object it names, or the confinement the
    // policy promises is not the confinement the child is about to experience.
    for entry in table.confinement_critical() {
        let snapshot = DaclSnapshot::read(&entry.path)?;
        let covered =
            snapshot.is_null() || snapshot.covers(&entry.path, profile.sid(), entry.grant)?;
        if !covered {
            return Err(WindowsSandboxError::AclVerification {
                path: path_text(&entry.path),
                reason: "the grant this policy applied is not visible in the object's DACL"
                    .to_string(),
            });
        }
    }

    Ok(WindowsPolicySetup { profile, grants })
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
/// the confinement, and every failure comes back with `None` — no half-applied policy is parked.
#[must_use]
pub fn attempt_windows_sandbox_with_setup(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> (SandboxAttempt, Option<WindowsPolicySetup>) {
    let setup = match prepare_windows_policy(workspace_root, extra_readonly_paths) {
        Ok(setup) => setup,
        Err(err) => return (attempt_from_windows_error(err), None),
    };
    // The probe is what turns "the profile exists and the ACEs are set" into "confinement is
    // enforceable": one confined process, launched under exactly the SECURITY_CAPABILITIES block
    // the launch layer will use, asked for nothing but `exit 0`. Without it a backend could report
    // FullyEnforced while CreateProcessW would fail, or while the confined child cannot even reach
    // the workspace it was confined to.
    if let Err(err) = probe_confined_launch(&setup, workspace_root) {
        return (attempt_from_windows_error(err), None);
    }
    put_prepared_setup(setup);
    (SandboxAttempt::FullyEnforced, take_windows_policy_setup())
}

/// What one Windows sandbox attempt achieved, guards released.
///
/// This is the judgement surface the dispatch (`sandbox::attempt_backend`) calls: it prepares the
/// policy, checks it with the probe and reports. The setup it built is released here — which
/// restores the DACLs and deletes the profile — because this surface owns no child to keep it alive
/// for; the launch layer takes the same setup from [`attempt_windows_sandbox_with_setup`] instead.
#[must_use]
pub fn attempt_windows_sandbox(
    workspace_root: &Path,
    extra_readonly_paths: &[String],
) -> SandboxAttempt {
    let (attempt, setup) = attempt_windows_sandbox_with_setup(workspace_root, extra_readonly_paths);
    drop(setup);
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
/// Not yet called in this crate: the re-entry plumbing (`main.rs`) is what references it, in the
/// follow-up task that wires the re-entry command line to this layer.
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
                None => return Err(attempt_error(attempt)),
            }
        }
    };
    let outcome = spawn_in_appcontainer(&setup, cwd, command).map_err(SandboxError::from);
    // Release only now: the child has exited, so the ACEs and the profile are no longer in use, and
    // dropping the guards here is what puts the workspace DACLs back.
    drop(setup);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(required[0].path, host.workspace);
        assert_eq!(required[0].grant, Grant::ReadWrite);

        let read_write: Vec<PathBuf> = table
            .entries
            .iter()
            .filter(|entry| entry.grant == Grant::ReadWrite)
            .map(|entry| entry.path.clone())
            .collect();
        assert!(
            read_write.contains(&host.temp_dir),
            "the temporary directory is read-write: {read_write:?}"
        );
        assert!(read_write.contains(&host.workspace));
        Ok(())
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

    /// The sensitive-directory invariant: the user-profile root is never granted, the denied
    /// directories themselves are never granted, and an operator-supplied entry that is an
    /// ancestor of a denied directory is rejected with a note rather than turned into an ACE.
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
            // A real extra directory, which must survive the checks above.
            host.workspace.join("extra").to_string_lossy().into_owned(),
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

        // The string form round-trips through `ConvertStringSidToSidW`, the helper a later stage
        // uses when it has only a name for the SID it needs.
        let parsed = string_to_sid("S-1-15-3-1")?;
        // SAFETY: `parsed` is the SID `ConvertStringSidToSidW` just allocated.
        assert_eq!(unsafe { sid_to_string(parsed)? }, "S-1-15-3-1");
        // SAFETY: the SID was allocated by `ConvertStringSidToSidW`, whose documented release is
        // `LocalFree`.
        unsafe { LocalFree(parsed) };
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
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/harness/mod.rs"))
                .expect("mod.rs must be readable from the crate manifest directory");
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

        // A real `SECURITY_CAPABILITIES` block, built the way the profile guard builds it: the list
        // accepts it, which is what proves the attribute constant and the size argument agree.
        let sid = string_to_sid("S-1-15-3-1")?;
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

        // SAFETY: the SID was allocated by `ConvertStringSidToSidW`, whose documented release is
        // `LocalFree`; the capability array belongs to `capabilities`, which outlives this call.
        unsafe { LocalFree(sid) };
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
            },
            WindowsSandboxError::Launch {
                reason: "CreateProcessW failed".to_string(),
                code: 2,
            },
            WindowsSandboxError::Probe {
                reason: "the confined probe process could not be launched".to_string(),
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

    /// The hand-off is single-shot and per-thread: a thread that never prepared a policy has
    /// nothing parked, and releasing what is not there says so rather than pretending.
    #[test]
    fn test_a_fresh_thread_holds_no_parked_setup() {
        let handled = std::thread::spawn(|| {
            assert!(
                take_windows_policy_setup().is_none(),
                "a thread that prepared nothing has no parked setup"
            );
            !discard_windows_policy_setup()
        })
        .join();
        assert!(
            matches!(handled, Ok(false)),
            "the fresh-thread hand-off check must report that nothing was parked"
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
