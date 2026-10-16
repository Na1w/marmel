//! Agent core: turn-budget helpers, mission phase state, plan management.
//!
//! The live turn loops are `src/ui/session.rs` (Manager) and
//! `src/agents/runner/execution.rs` (specialists); this module holds the
//! budget, plan and marker-facing helpers they share. The only success signals
//! in that path are the structured tool-error flag at the live call sites and
//! the deliverable-marker verdict owned by [`crate::markers`].

pub mod context;
pub mod r#loop;
pub mod phase;

pub use context::{ContextEngine, ContextEngineFactory};
pub use r#loop::{MAX_TURNS, is_read_tool};
// `Plan` is the single authority for the on-disk plan and its pending/complete
// state, so it is the only plan type re-exported here.
pub use phase::Plan;
