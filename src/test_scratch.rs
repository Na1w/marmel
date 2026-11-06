//! Test-only scratch-directory naming.
//!
//! The hand-rolled scratch fixtures in this crate's unit tests
//! (`fs::path_tests`, `search::tests`, `phase::tests`, `ui::session_tests`) are
//! flat directories under either `std::env::temp_dir()` or the package's
//! `target/` scratch, created by one test and removed by that same test while
//! dozens of sibling tests run concurrently in the same process. Two of them
//! must never pick the same name.
//!
//! The historical token was `<pid>-<wall-clock nanos>`, which is **not** unique:
//! the wall clock on macOS advances in ≥1 µs steps (measured: 48 051 of 50 000
//! consecutive `SystemTime::now()` calls returned an identical value in a tight
//! loop), so two tests started within the same microsecond got the identical
//! directory. One test then wrote its fixture into another test's fixture
//! (`harness::search::tests` matched an extra file) and the first test to finish
//! deleted the shared directory, making the sibling's cleanup fail with
//! `NotFound` (`harness::fs::path_tests`, macos-latest CI). A process-wide
//! counter cannot collide within one process, and the retained `pid` keeps
//! concurrent test binaries apart.

use std::sync::atomic::{AtomicU64, Ordering};

/// Fresh scratch-directory name component `<prefix>-<pid>-<seq>`, unique for
/// every call in this process. Callers pick the parent directory themselves:
/// some fixtures must sit outside the `std::env::temp_dir()` allow-list entry
/// (see `harness::fs::path_tests::scoped_root_outside_temp`), so the parent is
/// not this helper's decision.
pub(crate) fn scratch_name(prefix: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{seq}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::scratch_name;
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::mpsc;

    /// Pins the invariant every scratch fixture in this crate rests on: no two
    /// calls in one process name the same directory. The retired `pid + nanos`
    /// token failed this on macOS, where a tight loop hits the wall clock's ≥1 µs
    /// granularity and 96 % of consecutive `SystemTime::now()` calls come back with
    /// an identical value (measured: 48 051 duplicates out of 50 000 iterations).
    #[test]
    fn scratch_names_never_repeat_within_one_process() {
        let mut names = HashSet::new();
        for _ in 0..100_000 {
            assert!(
                names.insert(scratch_name("marmel_scratch_name_check")),
                "two scratch names collided — the name token is not process-unique"
            );
        }

        // The shape that actually broke `harness::fs::path_tests` and
        // `harness::search::tests`: many tests asking for a directory at once, then
        // each creating the one it named. A shared directory shows up here as a
        // shorter set, exactly as the two fixture-mixing failures did.
        let threads = 64;
        let (tx, rx) = mpsc::channel();
        std::thread::scope(|s| {
            for _ in 0..threads {
                let tx = tx.clone();
                s.spawn(move || {
                    let dir = std::env::temp_dir().join(scratch_name("marmel_scratch_name_check"));
                    fs::create_dir_all(&dir).expect("scratch dir");
                    tx.send(dir).expect("send the scratch dir");
                });
            }
        });
        // Exactly one send per thread, so `take(threads)` closes the iterator
        // without waiting on the outer `tx` clone (which is still alive here).
        let dirs: HashSet<PathBuf> = rx.iter().take(threads).collect();
        assert_eq!(
            dirs.len(),
            threads,
            "concurrently created scratch directories collided"
        );
        for dir in &dirs {
            fs::remove_dir_all(dir).expect("cleanup scratch dir");
        }
    }
}
