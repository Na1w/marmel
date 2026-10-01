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
    let abi = ABI::V5;
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

    // 2. Full Read/Write for /tmp and /var/tmp
    for tmp_dir in ["/tmp", "/var/tmp"] {
        if Path::new(tmp_dir).exists()
            && let Ok(fd) = PathFd::new(tmp_dir)
        {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                .context(format!("adding {tmp_dir} rule to Landlock"))?;
        }
    }

    // 3. User build caches and toolchains in HOME
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        // Read/Write caches for build tools (cargo, pip, npm)
        for dir_name in [".cargo", ".cache", ".npm"] {
            let p = home.join(dir_name);
            if p.exists()
                && let Ok(fd) = PathFd::new(&p)
            {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                    .context(format!("adding ~/{dir_name} rule to Landlock"))?;
            }
        }
        // Read-only user toolchains and configurations (rustup, local binaries, gitconfig, config)
        for entry in [".rustup", ".config", ".local", ".gitconfig"] {
            let p = home.join(entry);
            if p.exists()
                && let Ok(fd) = PathFd::new(&p)
            {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                    .context(format!("adding ~/{entry} read rule to Landlock"))?;
            }
        }
    }

    // Custom CARGO_HOME / RUSTUP_HOME if set outside ~/.cargo or ~/.rustup
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME").map(PathBuf::from)
        && cargo_home.exists()
        && let Ok(fd) = PathFd::new(&cargo_home)
    {
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
            .context("adding CARGO_HOME rule to Landlock")?;
    }
    if let Some(rustup_home) = std::env::var_os("RUSTUP_HOME").map(PathBuf::from)
        && rustup_home.exists()
        && let Ok(fd) = PathFd::new(&rustup_home)
    {
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
            .context("adding RUSTUP_HOME rule to Landlock")?;
    }

    // 4. Essential device nodes with read/write access (/dev/null, /dev/zero, /dev/full, /dev/tty, /dev/pts, /dev/shm)
    let rw_devs = [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/urandom",
        "/dev/random",
        "/dev/pts",
        "/dev/shm",
    ];
    for p in rw_devs {
        if Path::new(p).exists()
            && let Ok(fd) = PathFd::new(p)
        {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                .context("adding device rw rule to Landlock")?;
        }
    }

    // 5. System toolchains, device nodes, runtime files (DNS /run/systemd/resolve), and binaries (Read-Only + Execute)
    let ro_paths = [
        "/usr", "/bin", "/lib", "/lib64", "/opt", "/etc", "/dev", "/proc", "/sys", "/run", "/var",
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn test_landlock_ruleset_builds_with_devices() {
        let tmp = tempfile::tempdir().unwrap();
        let abi = ABI::V5;
        let ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .unwrap()
            .create();
        if let Ok(mut r) = ruleset {
            for dev in ["/dev/null", "/dev/zero", "/dev/tty"] {
                if let Ok(fd) = PathFd::new(dev) {
                    r = r
                        .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                        .expect("adding device rule must succeed");
                }
            }
            if let Ok(fd) = PathFd::new(tmp.path()) {
                let _ = r
                    .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                    .expect("adding tmp rule must succeed");
            }
        }
    }

    #[test]
    fn test_internal_sandbox_exec_dev_null_and_dns() {
        // Test running marmel binary with --internal-sandbox-exec writing to /dev/null and reading DNS config
        let exe = std::env::current_exe().expect("current test binary");
        let marmel_bin = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("marmel"));
        if let Some(bin) = marmel_bin
            && bin.exists()
        {
            let tmp = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(bin)
                .arg("--internal-sandbox-exec")
                .arg(tmp.path())
                .arg("echo hello > /dev/null && cat /etc/resolv.conf > /dev/null")
                .status();
            assert!(
                status.is_ok_and(|s| s.success()),
                "marmel --internal-sandbox-exec must succeed writing to /dev/null and reading /etc/resolv.conf"
            );
        }
    }

    #[test]
    fn test_internal_sandbox_cross_directory_rename() {
        let exe = std::env::current_exe().expect("current test binary");
        let marmel_bin = exe
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("marmel"));
        if let Some(bin) = marmel_bin
            && bin.exists()
        {
            let tmp = tempfile::tempdir().unwrap();
            let d1 = tmp.path().join("d1");
            let d2 = tmp.path().join("d2");
            std::fs::create_dir_all(&d1).unwrap();
            std::fs::create_dir_all(&d2).unwrap();
            std::fs::write(d1.join("test.txt"), "rename test payload").unwrap();

            // Direct rename syscall via python3 to ensure kernel rename() succeeds without EXDEV
            let script = format!(
                "import os; os.rename('{}/d1/test.txt', '{}/d2/test.txt')",
                tmp.path().display(),
                tmp.path().display()
            );
            let cmd = if std::process::Command::new("python3")
                .arg("--version")
                .output()
                .is_ok()
            {
                format!("python3 -c \"{script}\"")
            } else {
                format!(
                    "mv '{}/d1/test.txt' '{}/d2/test.txt'",
                    tmp.path().display(),
                    tmp.path().display()
                )
            };

            let status = std::process::Command::new(bin)
                .arg("--internal-sandbox-exec")
                .arg(tmp.path())
                .arg(cmd)
                .status();
            assert!(
                status.is_ok_and(|s| s.success()),
                "Cross-directory rename inside Landlock sandbox must succeed natively without EXDEV"
            );
            assert!(
                d2.join("test.txt").exists(),
                "Renamed file must exist at destination"
            );
            assert!(
                !d1.join("test.txt").exists(),
                "Original file must no longer exist in source"
            );
        }
    }
}
