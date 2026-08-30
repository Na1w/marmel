//! LLM client: reqwest SSE streaming, retry logic, thinking demuxer, and the
//! shared stream channel that routes both Manager and specialist turns.

pub mod client;
pub mod stream;
pub mod thinking;

pub use client::ChatClient;
pub use stream::{
    NullSink, StreamConfig, StreamEvent, StreamSink, VecSink, chat_client_turn, drive_streamed_turn,
};
pub use thinking::{
    NudgePolicy, RecoveryAdjustment, ThinkingDemuxer, apply_recovery, demux_stream,
};
