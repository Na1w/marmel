//! Bridge between LLM streaming turns, interactive renderer, and steer arbitration.

mod drain;
mod sink;
mod steer;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

pub(crate) use drain::drain_steer_arbitration_events_with_transcript;
pub use sink::RendererSink;
pub use steer::{SharedSteeringHistory, SteerArbEvent, spawn_steer_arbitration};
