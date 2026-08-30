//! Agent core: turn loop state machine, mission phase gating, plan management.

pub mod context;
pub mod r#loop;
pub mod phase;

pub use context::{ContextEngine, ContextEngineFactory};
pub use r#loop::{
    AgentLoop, MAX_TURNS, ManagerLoop, Signal, TURN_WATCHDOG_SECS, TurnOutcome, TurnPhase,
};
pub use phase::{MissionPhase, Plan, output_is_success};
