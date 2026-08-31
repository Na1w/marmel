//! Landlock LSM sandbox for Linux process isolation.
//!
//! Restricts child processes (such as spawned PTY shells) to the workspace
//! root, /tmp, and standard build caches (~/.cargo, ~/.cache), while keeping
//! system toolchains (/usr, /bin, /lib, ~/.rustup) strictly read-only and
//! blocking all access to sensitive user directories (~/.ssh, ~/.gnupg, other projects).

use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::Context;
#[cfg(target_os = "linux")]
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// Apply Landlock sandbox restrictions to the current process on Linux.
///
/// On non-Linux platforms or when Landlock is not supported by the kernel,
/// this logs a warning and returns `Ok(())` gracefully.
pub fn apply_sandbox(workspace_root: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        apply_landlock_linux(workspace_root)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = workspace_root;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply_landlock_linux(workspace_root: &Path) -> Result<()> {
    let abi = ABI::V1;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .context("configuring Landlock access rights")?
        .create()
        .context("creating Landlock ruleset");

    let mut ruleset = match status {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Landlock not supported by current kernel: {e:#}");
            return Ok(());
        }
    };

    // 1. Full Read/Write/Execute/Create/Delete rights for workspace
    if let Ok(fd) = PathFd::new(workspace_root) {
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .context("adding workspace rule to Landlock")?;
    }

    // 2. Full Read/Write for /tmp
    if let Ok(fd) = PathFd::new("/tmp") {
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .context("adding /tmp rule to Landlock")?;
    }

    // 3. User build caches: ~/.cargo and ~/.cache (so cargo/pip/npm can download & build)
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let cargo_dir = home.join(".cargo");
        if cargo_dir.exists()
            && let Ok(fd) = PathFd::new(&cargo_dir)
        {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                .context("adding ~/.cargo rule to Landlock")?;
        }
        let cache_dir = home.join(".cache");
        if cache_dir.exists()
            && let Ok(fd) = PathFd::new(&cache_dir)
        {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                .context("adding ~/.cache rule to Landlock")?;
        }
        // ~/.rustup read-only
        let rustup_dir = home.join(".rustup");
        if rustup_dir.exists()
            && let Ok(fd) = PathFd::new(&rustup_dir)
        {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                .context("adding ~/.rustup read rule to Landlock")?;
        }
    }

    // 4. System toolchains, device nodes, and binaries (Read-Only + Execute)
    let ro_paths = [
        "/usr", "/bin", "/lib", "/lib64", "/opt", "/etc", "/dev", "/proc", "/sys",
    ];
    for p in ro_paths {
        if Path::new(p).exists()
            && let Ok(fd) = PathFd::new(p)
        {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                .context("adding system read rule to Landlock")?;
        }
    }

    // 5. Restrict process
    let res = ruleset
        .restrict_self()
        .context("restricting process with Landlock");
    match res {
        Ok(_) => {
            tracing::debug!(
                "Landlock sandbox successfully applied for {}",
                workspace_root.display()
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!("Failed to enforce Landlock restrictions: {e:#}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_sandbox_does_not_panic() {
        let cur = std::env::current_dir().unwrap();
        // apply_sandbox should gracefully succeed or log warning without panicking
        let res = apply_sandbox(&cur);
        assert!(res.is_ok());
    }
}
