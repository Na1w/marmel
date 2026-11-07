//! Integration tests for cleaning up .marmel on plan completion.
//!
//! When an execution plan finishes (`is_complete()`), runtime files (plan, prompts,
//! session bundle, archive, research notes, temporary files, logs) are swept at session end
//! unless `--keep-session` is set. User-authored files are preserved, and empty `.marmel`
//! directories are removed completely.

use std::fs;
use std::sync::Mutex;

use marmennill::manager::phase::{Plan, clear_plan_start, is_keep_session, set_keep_session};

static TEST_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn integration_cleanup_on_completion_removes_marmel_dir_when_empty() {
    let _serial = TEST_MUTEX.lock().unwrap();
    clear_plan_start();
    set_keep_session(false);

    let tmp = tempfile::tempdir().expect("tempdir");
    let marmel_dir = tmp.path().join(".marmel");
    let plan = Plan::at(&marmel_dir);

    // Initial plan with one task
    plan.create("# Execution Plan\n- [ ] [t-001] Complete feature\n")
        .expect("plan create");
    assert!(!plan.is_complete());

    // Incomplete plan does not clean up
    let cleaned = plan.cleanup_completed().expect("cleanup_completed");
    assert!(!cleaned);
    assert!(marmel_dir.exists());

    // Mark task complete
    plan.check_off("t-001").expect("check off");
    assert!(plan.is_complete());

    // Add generated runtime files
    fs::create_dir_all(marmel_dir.join("prompts")).expect("prompts dir");
    fs::write(marmel_dir.join("prompts/t-001.md"), "prompt").expect("prompt file");
    fs::create_dir_all(marmel_dir.join("session")).expect("session dir");
    fs::write(marmel_dir.join("session/manifest.json"), "{}").expect("manifest");
    fs::create_dir_all(marmel_dir.join("archive")).expect("archive dir");
    fs::write(marmel_dir.join("archive/plan_1.md"), "archive").expect("archive");
    fs::create_dir_all(marmel_dir.join("research")).expect("research dir");
    fs::write(marmel_dir.join("research/topic.md"), "research notes").expect("research notes");
    fs::create_dir_all(marmel_dir.join("tmp")).expect("tmp dir");
    fs::write(marmel_dir.join("tmp/overflow.txt"), "overflow").expect("tmp overflow");
    fs::write(marmel_dir.join("marmel.log"), "log").expect("log");
    fs::write(marmel_dir.join("debug.log"), "debug").expect("debug log");

    // Clean up
    let cleaned = plan.cleanup_completed().expect("cleanup_completed");
    assert!(cleaned, "completed plan must be cleaned up");
    assert!(
        !marmel_dir.exists(),
        ".marmel directory must be completely removed when only runtime files were present"
    );
}

#[test]
fn integration_cleanup_preserves_user_files_and_keeps_marmel_dir() {
    let _serial = TEST_MUTEX.lock().unwrap();
    clear_plan_start();
    set_keep_session(false);

    let tmp = tempfile::tempdir().expect("tempdir");
    let marmel_dir = tmp.path().join(".marmel");
    let plan = Plan::at(&marmel_dir);

    plan.create("# Execution Plan\n- [ ] [t-001] Complete feature\n")
        .expect("plan create");
    plan.check_off("t-001").expect("check off");
    assert!(plan.is_complete());

    // Add runtime files
    fs::create_dir_all(marmel_dir.join("prompts")).expect("prompts dir");
    fs::write(marmel_dir.join("prompts/t-001.md"), "prompt").expect("prompt file");
    fs::create_dir_all(marmel_dir.join("research")).expect("research dir");
    fs::write(marmel_dir.join("research/notes.md"), "research notes").expect("notes");
    fs::write(marmel_dir.join("marmel.log"), "log").expect("log");

    // Add user files
    fs::write(marmel_dir.join("marmel.toml"), "model = \"test\"").expect("config file");
    fs::create_dir_all(marmel_dir.join("skills")).expect("skills dir");
    fs::write(marmel_dir.join("skills/custom.md"), "my skill").expect("skill");

    let cleaned = plan.cleanup_completed().expect("cleanup_completed");
    assert!(cleaned, "completed plan must trigger cleanup");
    assert!(
        marmel_dir.exists(),
        ".marmel must remain because user files exist"
    );
    assert!(
        marmel_dir.join("marmel.toml").exists(),
        "marmel.toml must be preserved"
    );
    assert!(
        marmel_dir.join("skills/custom.md").exists(),
        "skills/custom.md must be preserved"
    );
    assert!(
        !marmel_dir.join("prompts").exists(),
        "prompts directory must be deleted"
    );
    assert!(
        !marmel_dir.join("research").exists(),
        "research directory must be deleted"
    );
    assert!(
        !marmel_dir.join("marmel.log").exists(),
        "marmel.log must be deleted"
    );
}

#[test]
fn integration_cleanup_respects_keep_session_flag() {
    let _serial = TEST_MUTEX.lock().unwrap();
    clear_plan_start();
    set_keep_session(false);

    let tmp = tempfile::tempdir().expect("tempdir");
    let marmel_dir = tmp.path().join(".marmel");
    let plan = Plan::at(&marmel_dir);

    plan.create("# Execution Plan\n- [ ] [t-001] Complete feature\n")
        .expect("plan create");
    plan.check_off("t-001").expect("check off");
    assert!(plan.is_complete());

    set_keep_session(true);
    assert!(is_keep_session());

    let cleaned = plan.cleanup_completed().expect("cleanup_completed");
    set_keep_session(false);

    assert!(!cleaned, "must not clean up when keep_session is active");
    assert!(marmel_dir.exists());
    assert!(plan.exists());
}
