//! Wire types for the OpenAI-compatible chat completions API and tool calls.

mod tools;
mod wire;

pub use tools::{ToolDef, ToolFunctionDef};
pub use wire::{
    ChatChunk, ChatRequest, ChunkChoice, ChunkDelta, ChunkToolCall, ChunkToolFunction, Message,
    ToolCall, ToolFunction, ensure_valid_json_arguments,
};
