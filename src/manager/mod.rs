//! Agent core: turn-budget helpers, mission phase gating, plan management.
//!
//! The duplicated `AgentLoop` / `ManagerLoop` executors were deleted (see
//! `docs/decision_dead_code_manager.md`); the live turn loops are
//! `src/ui/session.rs` (Manager) and `src/agents/runner/execution.rs`
//! (specialists).
//!
//! `phase::output_is_success` and `Plan::check_off_on_success` are no longer
//! re-exported either: they lost their last production caller with those
//! executors (`docs/decision_dead_code_manager.md` §6 hand-off 3) and were
//! deleted by the `phase.rs` owner, so the only success gates left are the
//! structured tool-error flag at the live call sites and the deliverable marker
//! verdict owned by [`crate::markers`] (recon items H6 / M9).

pub mod context;
pub mod r#loop;
pub mod phase;

pub use context::{ContextEngine, ContextEngineFactory};
pub use r#loop::{MAX_TURNS, is_read_tool};
// `MissionPhase` is no longer re-exported: the phase gate it belonged to was
// deleted as unreachable from the live path (recon L8 — see the wire-or-delete
// evidence recorded in `phase.rs`).
pub use phase::Plan;
