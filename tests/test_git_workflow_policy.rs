//! Integration coverage for the git-workflow feature: the built-in `git_workflow`
//! skill, the `## Git workspace state` autodetection fragment and the
//! `git_workflow_policy` gate, as they behave **across** the config, catalog and
//! specialist-runner boundaries — not inside any one of them.
//!
//! Seam. Every git-state case follows the idiom established by
//! `tests/test_reasoning_effort.rs` and `tests/test_specialist_stream.rs`: a
//! throwaway `marmel.toml` **on disk** loaded through the production config seam
//! [`marmennill::config::load`], a real specialist round driven by
//! [`marmennill::agents::run_specialist_live`] (the entry point the delegation path
//! itself calls) against a real `wiremock` socket, and assertions against the
//! **system prompt that actually arrived on the wire** — never a re-serialization of
//! the input and never a renderer called by hand. The round runs inside
//! [`marmennill::harness::with_workspace_root`] over a hand-written git fixture, so
//! the real repository's git state (branch, worktrees, `.marmel/`) can never leak
//! into a verdict.
//!
//! Which link of the feature each case covers:
//!
//! * Skill discovery + composition (`Catalog::discover` →
//!   `PromptBuilder::synthesize_offline` → `### Skill: …` in the composed prompt):
//!   [`git_workflow_skill_is_discovered_and_composed_into_git_role_prompts`], on the
//!   same `Active Domain Skills` / `### Skill:` seam
//!   `tests/test_dynamic_prompts.rs` asserts on.
//! * Detected-state injection on the specialist prompt path
//!   (`src/agents/runner/execution.rs::append_git_state_block`):
//!   [`specialists_on_the_wire_get_the_skill_and_the_detected_git_state`] and
//!   [`specialists_are_told_when_the_workspace_is_not_a_git_repository`].
//! * The policy gate as a **config-file** knob, honoured identically by both prompt
//!   paths because both call the single owner `crate::config::git_prompt_block`:
//!   [`policy_off_silences_the_git_state_block_but_not_the_role_git_guidance`],
//!   [`policy_enforce_worktree_states_the_requirement_and_keeps_the_detected_branch`],
//!   [`enforce_worktree_states_the_requirement_even_in_a_plain_directory`],
//!   [`an_absent_policy_key_defaults_to_auto_and_its_preference_wording`].
//! * TOML parsing of the key through the production loader:
//!   [`each_documented_policy_value_is_parsed_from_a_config_file`] and
//!   [`an_unknown_value_in_a_config_file_is_rejected_by_the_loader`]. The value
//!   vocabulary itself (including the `FromStr`/`Display` round trip) is covered by
//!   the unit tests in `src/config.rs`; what is covered **here** is that a value out
//!   of a `marmel.toml` on disk reaches the prompt, and that a file-level typo is
//!   rejected by `load()` instead of quietly defaulting.
//!
//! Deliberately **not** re-covered here: the per-unit renderings
//! (`format_git_prompt_block`, `detect_git_workspace`, `git_prompt_block`, the
//! registry wiring in `src/agents/catalog.rs`) already have unit tests, and the
//! Manager-side prompt path is covered by the unit tests in `src/ui/helpers.rs` —
//! `load_system_prompt_with_plan_and_warning` is `pub(crate)` and therefore not
//! reachable from an integration target.
//!
//! Offline and deterministic: no network beyond the loopback socket, git metadata
//! written by hand (never by running `git`), one specialist turn per round
//! (`enable_validator = false`), no wall-clock sensitive assertions.

use std::path::PathBuf;

use marmennill::agents::{
    Agent, Catalog, DelegationRequest, IsolatedContext, PromptBuilder, run_specialist_live,
};
use marmennill::config::Config;
use marmennill::config::GitWorkflowPolicy;
use marmennill::config::load as load_config;
use marmennill::harness::workspace::detect_git_workspace;
use marmennill::llm::ChatClient;
use marmennill::prompts::CODER_PROMPT;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Branch pinned by the git-metadata fixture's `HEAD`.
const FIXTURE_BRANCH: &str = "feature/git-workflow-integration";
/// Task id carried by every delegated round in this file.
const TASK_ID: &str = "t-gw-01";
/// Model pinned into every throwaway config and mock client.
const MODEL: &str = "gw-manager-model";
/// The literal sentence `auto` phrases worktree isolation with.
const PREFER_SENTENCE: &str = "Prefer an isolated `git worktree`";
/// The in-place rewrite of that sentence `enforce_worktree` applies to the
/// repository fragment.
const REQUIRE_SENTENCE: &str = "Isolated `git worktree` use is REQUIRED";
/// The standalone requirement line `enforce_worktree` appends when the fragment has
/// no preference sentence to reword (a non-repository verdict).
const REQUIRE_LINE: &str = "Isolated `git worktree` use is **REQUIRED**";
/// Marker carried by the body of the built-in `git_workflow` skill.
const SKILL_BODY_MARKER: &str = "WORKTREE ISOLATION (PREFERRED)";
/// Header line of the `## Git workspace state` fragment.
const GIT_BLOCK_HEADER: &str = "## Git workspace state";

// ---------------------------------------------------------------- fixture types

/// One workspace fixture on disk plus the git metadata it carries.
///
/// Written by hand instead of by running `git`, exactly the way
/// [`detect_git_workspace`] reads it: a `.git` entry, a `HEAD` naming a branch and a
/// `worktrees/` directory. The [`TempDir`] is owned by the fixture, so the fixture
/// outlives the round that runs inside it.
struct Fixture {
    /// Owns the temp dir so the fixture outlives the round running inside it.
    _tmp: TempDir,
    root: PathBuf,
}

impl Fixture {
    /// A git-repo fixture: `<tmp>/repo` holding `.git/HEAD` for
    /// [`FIXTURE_BRANCH`] and two entries under `.git/worktrees/`.
    fn git_repo() -> Self {
        let _tmp = tempfile::tempdir().expect("temp dir for the workspace fixture");
        let root = _tmp.path().join("repo");
        std::fs::create_dir_all(&root).expect("fixture workspace must be creatable");
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("worktrees/wk-one")).expect("linked worktree dir 1");
        std::fs::create_dir_all(git.join("worktrees/wk-two")).expect("linked worktree dir 2");
        std::fs::write(
            git.join("HEAD"),
            format!("ref: refs/heads/{FIXTURE_BRANCH}\n"),
        )
        .expect("fixture HEAD must be writable");
        Self { _tmp, root }
    }

    /// A plain-directory fixture: no `.git` entry at the root or anywhere in its
    /// ancestry (temp dir, `/tmp`, `/`), so Marmel's own repository can never be
    /// mistaken for it.
    fn plain_dir() -> Self {
        let _tmp = tempfile::tempdir().expect("temp dir for the plain workspace fixture");
        let root = _tmp.path().join("project");
        std::fs::create_dir_all(&root).expect("plain fixture workspace must be creatable");
        Self { _tmp, root }
    }

    /// The fixture root exactly as a round sees it: [`marmennill::harness::with_workspace_root`]
    /// canonicalizes the path it is given, so that is the spelling the rendered
    /// fragment prints.
    fn root(&self) -> PathBuf {
        std::fs::canonicalize(&self.root).expect("the fixture root must be canonicalizable")
    }

    /// Non-vacuity guard for the fixture: what the production detector itself sees.
    fn detected(&self) -> marmennill::harness::workspace::GitWorkspace {
        detect_git_workspace(&self.root())
    }
}

// --------------------------------------------------------------- mock backend

/// One canned SSE completion frame carrying plain assistant content and the
/// complete marker for [`TASK_ID`] — with no tool call, so the round is one turn.
fn completion_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-gitwf",
            "choices": [{ "delta": { "content": text }, "finish_reason": "stop" }]
        })
    )
}

/// A mock backend that answers every POST with the same canned completion.
async fn single_reply_backend(reply: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(completion_sse(reply)))
        .mount(&server)
        .await;
    server
}

/// The canned reply every round in this file uses (the complete marker of
/// `TASK_ID`).
fn reply() -> String {
    format!("One line of work.\n\nMISSION COMPLETE ({TASK_ID})")
}

// ------------------------------------------------------- config from the file

/// Write a throwaway `marmel.toml` and load it through the production seam.
///
/// `policy_line` is the top-level `git_workflow_policy` line, or `None` to leave the
/// key out of the file entirely (the default-value case). `backend_url`/`model` are
/// pinned **after** `load()` on purpose: `load()` lets `MARMEL_BACKEND_URL` /
/// `MARMEL_MODEL` beat the file, so an ambient value in a developer's or CI shell
/// must never redirect a test off its own mock backend. The
/// `[orchestration.specialists.coder] enable_validator = false` section keeps exactly
/// one specialist turn per round.
fn load_case_config(dir: &TempDir, backend_url: &str, policy_line: Option<&str>) -> Config {
    let path = dir.path().join("marmel.toml");
    let policy = policy_line.unwrap_or("");
    let toml = format!(
        "backend_url = \"{backend_url}\"\nmodel = \"{MODEL}\"\n{policy}\n\
         [orchestration.specialists.coder]\nenable_validator = false\n"
    );
    std::fs::write(&path, toml).expect("throwaway marmel.toml must be writable");

    let mut cfg = load_config(Some(path.to_str().expect("temp path must be utf-8")))
        .expect("the throwaway marmel.toml must load");
    cfg.backend_url = backend_url.to_string();
    cfg.model = MODEL.to_string();
    cfg
}

// ------------------------------------------------------------ prompt capture

/// Every request body the mock backend actually received, in arrival order.
async fn captured_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock exposes the requests it received");
    requests
        .iter()
        .map(|req| {
            serde_json::from_slice(&req.body).expect("every body Marmel sends must be valid JSON")
        })
        .collect()
}

/// The system prompt of one captured body — what the model was actually told.
fn system_prompt_of(body: &serde_json::Value) -> String {
    let messages = body["messages"]
        .as_array()
        .expect("`messages` must be a JSON array");
    let system = messages
        .iter()
        .find(|m| m["role"].as_str() == Some("system"))
        .expect("the specialist must send its system prompt on the wire");
    system["content"]
        .as_str()
        .expect("the system message must serialize as a string")
        .to_string()
}

// ------------------------------------------------------------- round drivers

/// Drive one **coder specialist round** over `fixture`, then hand back every system
/// prompt that reached the wire.
///
/// The role prompt is `role_prompt` — the caller decides whether that is the real
/// built-in role prompt or a synthesized blueprint — the workspace root is the
/// fixture, and the config is `cfg`. The round is the one the delegation path runs,
/// so the git verdict in the prompt is the one the runner computed for this
/// workspace.
async fn run_coder_round_and_capture_prompts(
    server: &MockServer,
    cfg: &Config,
    fixture: &Fixture,
    role_prompt: &str,
) -> Vec<String> {
    let root = fixture.root();
    let detected = fixture.detected();
    assert_eq!(
        detected.inspected_root.as_deref(),
        Some(root.as_path()),
        "the fixture root must be inspectable, so the round reports an observed verdict"
    );

    let client = ChatClient::new(format!("{}/v1", server.uri()), MODEL);
    let ctx = IsolatedContext {
        role_system_prompt: role_prompt.to_string(),
        brief: "Produce a one-line deliverable and conclude.".to_string(),
        snippets: Vec::new(),
        task_id: Some(TASK_ID.to_string()),
        image_urls: Vec::new(),
        audio_urls: Vec::new(),
        blueprint: None,
    };
    let token = CancellationToken::new();

    let deliverable = marmennill::harness::with_workspace_root(root.clone(), async {
        run_specialist_live(&client, Agent::Coder, &ctx, cfg, &token)
            .await
            .expect("the coder round must complete against the mock backend")
    })
    .await;
    assert!(
        deliverable.contains("MISSION COMPLETE"),
        "the round must end in a complete deliverable, got: {deliverable}"
    );

    let bodies = captured_bodies(server).await;
    assert!(
        !bodies.is_empty(),
        "the specialist must have sent at least one request body"
    );
    bodies.iter().map(system_prompt_of).collect()
}

/// A coder round over the **git-repo** fixture with the given policy line.
///
/// Returns the system prompts that reached the wire together with the canonical
/// fixture root, so a case can assert against the root the fragment had to name.
async fn git_repo_prompts(policy_line: Option<&str>, role_prompt: &str) -> (Vec<String>, PathBuf) {
    let server = single_reply_backend(&reply()).await;
    let config_dir = tempfile::tempdir().expect("temp dir for the throwaway marmel.toml");
    let fixture = Fixture::git_repo();
    let cfg = load_case_config(&config_dir, &format!("{}/v1", server.uri()), policy_line);
    let prompts = run_coder_round_and_capture_prompts(&server, &cfg, &fixture, role_prompt).await;
    (prompts, fixture.root())
}

/// A coder round over the **plain-directory** fixture with the given policy line.
async fn plain_dir_prompts(policy_line: Option<&str>, role_prompt: &str) -> (Vec<String>, PathBuf) {
    let server = single_reply_backend(&reply()).await;
    let config_dir = tempfile::tempdir().expect("temp dir for the throwaway marmel.toml");
    let fixture = Fixture::plain_dir();
    let cfg = load_case_config(&config_dir, &format!("{}/v1", server.uri()), policy_line);
    let prompts = run_coder_round_and_capture_prompts(&server, &cfg, &fixture, role_prompt).await;
    (prompts, fixture.root())
}

/// Return the single system prompt of a one-turn round, failing if the round sent
/// more than one (which would mean the mock reply did not end the turn).
fn single_prompt(prompts: &[String]) -> &str {
    assert_eq!(
        prompts.len(),
        1,
        "a one-turn round sends exactly one system prompt"
    );
    prompts[0].as_str()
}

// -------------------------------------------------------------------- cases

/// The built-in `git_workflow` skill is discoverable through the catalog a session
/// discovers, and the offline synthesizer injects it — verbatim, as a `### Skill:`
/// block — into the composed prompt of every git-capable role. Roles that must never
/// be handed git work get no such block.
#[test]
fn git_workflow_skill_is_discovered_and_composed_into_git_role_prompts() {
    let workspace = tempfile::tempdir().expect("temp dir for a skills-free workspace");
    let catalog = Catalog::discover(workspace.path());
    let skill = catalog
        .get_skill("git_workflow")
        .expect("git_workflow must be discoverable as a built-in skill");
    let skill_header = format!("### Skill: {}", skill.name);

    let request = |agent: Agent, prompt: &str| DelegationRequest {
        agent_name: agent,
        prompt: prompt.to_string(),
        snippets: Vec::new(),
        task_id: Some(TASK_ID.to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };

    for role in [
        Agent::Coder,
        Agent::Debugger,
        Agent::Generalist,
        Agent::Validator,
    ] {
        let blueprint = PromptBuilder::synthesize_offline(
            &catalog,
            &request(role, "Implement the parser and cover it with tests."),
        );
        assert!(
            blueprint.system_prompt.contains("## Active Domain Skills"),
            "{role} must get an Active Domain Skills section"
        );
        assert!(
            blueprint.system_prompt.contains(&skill_header),
            "{role} must be composed with the git workflow skill, got:\n{}",
            blueprint.system_prompt
        );
        assert!(
            blueprint
                .selected_skills
                .contains(&"git_workflow".to_string()),
            "{role} must record git_workflow among its selected skills"
        );
        assert!(
            blueprint.system_prompt.contains(skill.content.as_str())
                && blueprint.system_prompt.contains(SKILL_BODY_MARKER),
            "{role} must be handed the registered built-in skill body, not a paraphrase"
        );
    }

    for role in [Agent::Researcher, Agent::Planner] {
        let blueprint = PromptBuilder::synthesize_offline(
            &catalog,
            &request(role, "Audit the vendor API and document its endpoints."),
        );
        assert!(
            !blueprint.system_prompt.contains(&skill_header),
            "{role} must not be composed with the git workflow skill, got:\n{}",
            blueprint.system_prompt
        );
        assert!(
            !blueprint
                .selected_skills
                .contains(&"git_workflow".to_string()),
            "{role} must not select the git workflow skill"
        );
    }
}

/// End to end: a live coder round against a git fixture injects **both** the
/// `### Skill:` block of the synthesized blueprint and the `## Git workspace state`
/// fragment naming the fixture's branch and worktrees, into the one system prompt
/// that reaches the wire.
#[tokio::test]
async fn specialists_on_the_wire_get_the_skill_and_the_detected_git_state() {
    let workspace = tempfile::tempdir().expect("temp dir for a skills-free workspace");
    let catalog = Catalog::discover(workspace.path());
    let skill = catalog
        .get_skill("git_workflow")
        .expect("git_workflow must be discoverable");
    let composed = PromptBuilder::synthesize_offline(
        &catalog,
        &DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Implement the parser and cover it with tests.".to_string(),
            snippets: Vec::new(),
            task_id: Some(TASK_ID.to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        },
    )
    .system_prompt;
    assert!(
        composed.contains(&format!("### Skill: {}", skill.name)),
        "the fixture's composed prompt must carry the git skill"
    );

    let (prompts, root) = git_repo_prompts(None, &composed).await;
    let prompt = single_prompt(&prompts);

    assert!(
        prompt.contains(&format!("### Skill: {}", skill.name)),
        "the prompt the coder was actually run on must carry the git skill:\n{prompt}"
    );
    assert_eq!(
        prompt.matches(GIT_BLOCK_HEADER).count(),
        1,
        "the git state fragment is appended exactly once:\n{prompt}"
    );
    assert!(
        prompt.contains("The workspace IS a git repository"),
        "the fixture is a repository, and the specialist is told so:\n{prompt}"
    );
    assert!(
        prompt.contains(&format!("root `{}`", root.display())),
        "the fragment must name the fixture root {}:\n{prompt}",
        root.display()
    );
    assert!(
        prompt.contains(&format!("branch `{FIXTURE_BRANCH}`")),
        "the fragment must name the branch the fixture is on:\n{prompt}"
    );
    assert!(
        prompt.contains("(linked worktrees: 2)"),
        "the fragment must report the fixture's linked worktrees:\n{prompt}"
    );
    assert!(
        prompt.contains(PREFER_SENTENCE),
        "the default policy phrases worktree isolation as a preference:\n{prompt}"
    );
    assert!(
        prompt.contains("never commit `.marmel/`"),
        "the fragment must keep the commit-hygiene pointer:\n{prompt}"
    );
    assert_eq!(
        prompt.matches(SKILL_BODY_MARKER).count(),
        1,
        "the built-in skill body is injected exactly once"
    );
}

/// The opposite verdict: a plain-directory fixture (no `.git` entry at the root or
/// above it) makes the fragment state **NOT a git repository**, name the inspected
/// root and give the `git init` / `.gitignore` / initial-commit procedure — and the
/// role prompt's git guidance is still on the prompt, because only the
/// detected-state fragment is policy-gated, never the guidance.
#[tokio::test]
async fn specialists_are_told_when_the_workspace_is_not_a_git_repository() {
    let (prompts, root) =
        plain_dir_prompts(Some("git_workflow_policy = \"auto\""), CODER_PROMPT).await;
    let prompt = single_prompt(&prompts);

    assert!(
        !detect_git_workspace(&root).is_git_repo,
        "the plain fixture must not be detected as a repository"
    );
    assert_eq!(
        prompt.matches(GIT_BLOCK_HEADER).count(),
        1,
        "exactly one git state fragment on a plain-directory workspace:\n{prompt}"
    );
    assert!(
        prompt.contains("NOT a git repository"),
        "the specialist must be told the workspace is not a repository:\n{prompt}"
    );
    assert!(
        prompt.contains(&format!("(no `.git` entry at `{}`", root.display())),
        "the fragment must name the inspected root {}:\n{prompt}",
        root.display()
    );
    assert!(
        prompt.contains("git init")
            && prompt.contains(".gitignore")
            && prompt.contains("initial commit"),
        "the fragment must carry the new-project procedure:\n{prompt}"
    );
    assert!(
        !prompt.contains("The workspace IS a git repository"),
        "a plain directory must never be reported as a repository:\n{prompt}"
    );
    assert!(
        prompt.contains("## Git workflow") && prompt.contains("WORKTREES ARE DEFAULT"),
        "the coder role prompt's own git guidance is never gated on the policy:\n{prompt}"
    );
}

/// `git_workflow_policy = "off"` in `marmel.toml`: the detected-state fragment is
/// gone from the specialist's system prompt, while the role prompt's git guidance
/// stays — the gate silences the verdict, not the instruction.
#[tokio::test]
async fn policy_off_silences_the_git_state_block_but_not_the_role_git_guidance() {
    let (prompts, _) = git_repo_prompts(Some("git_workflow_policy = \"off\""), CODER_PROMPT).await;
    let prompt = single_prompt(&prompts);

    assert_eq!(
        prompt.matches(GIT_BLOCK_HEADER).count(),
        0,
        "`off` must append no git state fragment at all:\n{prompt}"
    );
    assert!(
        !prompt.contains("The workspace IS a git repository"),
        "`off` must state no git verdict anywhere:\n{prompt}"
    );
    assert!(
        !prompt.contains("NOT a git repository"),
        "`off` must state no git verdict anywhere:\n{prompt}"
    );
    assert!(
        prompt.contains("## Git workflow") && prompt.contains("WORKTREES ARE DEFAULT"),
        "the coder role prompt's git guidance must survive an `off` policy:\n{prompt}"
    );
}

/// `git_workflow_policy = "enforce_worktree"`: the **same** detected state (branch,
/// worktrees) is rendered, with the worktree sentence reworded as a requirement — and
/// the fragment still appears exactly once, so the policy cannot duplicate it.
#[tokio::test]
async fn policy_enforce_worktree_states_the_requirement_and_keeps_the_detected_branch() {
    let (prompts, _) = git_repo_prompts(
        Some("git_workflow_policy = \"enforce_worktree\""),
        CODER_PROMPT,
    )
    .await;
    let prompt = single_prompt(&prompts);

    assert_eq!(
        prompt.matches(GIT_BLOCK_HEADER).count(),
        1,
        "the detected-state fragment is still rendered once:\n{prompt}"
    );
    assert!(
        prompt.contains(REQUIRE_SENTENCE),
        "`enforce_worktree` must state the requirement:\n{prompt}"
    );
    assert!(
        !prompt.contains(PREFER_SENTENCE),
        "`enforce_worktree` must not leave the preference wording behind:\n{prompt}"
    );
    assert!(
        prompt.contains(&format!("branch `{FIXTURE_BRANCH}`")),
        "the detection itself must be untouched by the policy:\n{prompt}"
    );
    assert!(
        prompt.contains("(linked worktrees: 2)"),
        "and the worktree count must still be reported:\n{prompt}"
    );
}

/// `enforce_worktree` must never go quiet on the other verdict either: a
/// plain-directory fixture still gets its NOT-a-repository statement **plus** an
/// explicit requirement line, so the policy stays legible even where the shared
/// fragment has no preference sentence to reword.
#[tokio::test]
async fn enforce_worktree_states_the_requirement_even_in_a_plain_directory() {
    let (prompts, _) = plain_dir_prompts(
        Some("git_workflow_policy = \"enforce_worktree\""),
        CODER_PROMPT,
    )
    .await;
    let prompt = single_prompt(&prompts);

    assert_eq!(
        prompt.matches(GIT_BLOCK_HEADER).count(),
        1,
        "the plain-directory fragment is rendered exactly once:\n{prompt}"
    );
    assert!(
        prompt.contains("NOT a git repository"),
        "the observed verdict must not be replaced by the requirement:\n{prompt}"
    );
    assert!(
        prompt.contains(REQUIRE_LINE),
        "`enforce_worktree` must state the requirement even with no worktree sentence to \
         reword:\n{prompt}"
    );
    assert!(
        !prompt.contains(PREFER_SENTENCE),
        "a plain directory has no worktree preference to state in the first place:\n{prompt}"
    );
}

/// The gate's default: a `marmel.toml` that never mentions the key leaves
/// `Config::git_workflow_policy` at `Auto`, and the specialist's prompt then carries
/// the shared fragment with the **default preference wording**, never the requirement.
#[tokio::test]
async fn an_absent_policy_key_defaults_to_auto_and_its_preference_wording() {
    let config_dir = tempfile::tempdir().expect("temp dir for the throwaway marmel.toml");
    // The file never mentions the key, so the loader must fall back to `auto`.
    let cfg = load_case_config(&config_dir, "http://127.0.0.1:1/v1", None);
    assert_eq!(
        cfg.git_workflow_policy,
        GitWorkflowPolicy::Auto,
        "a config file without the key must keep the default `auto`"
    );

    let (prompts, _) = git_repo_prompts(None, CODER_PROMPT).await;
    let prompt = single_prompt(&prompts);
    assert!(
        prompt.contains(PREFER_SENTENCE),
        "`auto` must use the default preference wording:\n{prompt}"
    );
    assert!(
        !prompt.contains(REQUIRE_SENTENCE),
        "`auto` must not phrase the worktree as a requirement:\n{prompt}"
    );
    assert!(
        prompt.contains(&format!("branch `{FIXTURE_BRANCH}`")),
        "and `auto` still reports the detected branch:\n{prompt}"
    );
}

/// The documented vocabulary, straight out of a `marmel.toml` on disk through the
/// production loader: every one of `auto` / `off` / `enforce_worktree` parses, and
/// the parsed value is the one the prompt path then acts on.
#[test]
fn each_documented_policy_value_is_parsed_from_a_config_file() {
    for (raw, expected) in [
        ("auto", GitWorkflowPolicy::Auto),
        ("off", GitWorkflowPolicy::Off),
        ("enforce_worktree", GitWorkflowPolicy::EnforceWorktree),
    ] {
        let dir = tempfile::tempdir().expect("temp dir for the throwaway marmel.toml");
        let path = dir.path().join("marmel.toml");
        std::fs::write(
            &path,
            format!("backend_url = \"http://127.0.0.1:1/v1\"\ngit_workflow_policy = \"{raw}\"\n"),
        )
        .expect("throwaway marmel.toml must be writable");

        let cfg = load_config(Some(path.to_str().expect("temp path must be utf-8")))
            .unwrap_or_else(|e| panic!("`git_workflow_policy = \"{raw}\"` must load: {e:#}"));
        assert_eq!(
            cfg.git_workflow_policy, expected,
            "`git_workflow_policy = \"{raw}\"` must reach the prompt path as {expected}"
        );
    }
}

/// Fail-closed on the config-file path: a key whose value is outside the documented
/// vocabulary is a hard load error that **names the offending value**, so a typo can
/// never become an implicit `auto` and silently change what the model is told.
#[test]
fn an_unknown_value_in_a_config_file_is_rejected_by_the_loader() {
    let dir = tempfile::tempdir().expect("temp dir for the throwaway marmel.toml");
    let path = dir.path().join("marmel.toml");
    std::fs::write(
        &path,
        "backend_url = \"http://127.0.0.1:1/v1\"\n\
         git_workflow_policy = \"mostly_auto\"\n",
    )
    .expect("throwaway marmel.toml must be writable");

    let err = load_config(Some(path.to_str().expect("temp path must be utf-8")))
        .expect_err("an undocumented `git_workflow_policy` must not load");
    let message = format!("{err:#}");
    assert!(
        message.contains("mostly_auto"),
        "the rejection must identify the rejected value, got: {message}"
    );
}
