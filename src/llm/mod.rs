//! LLM client: reqwest SSE streaming, retry logic, thinking demuxer, and the
//! shared stream channel that routes both Manager and specialist turns.

pub mod client;
pub mod stream;
pub mod thinking;

pub use client::{
    ChatClient, StreamedReply, get_global_token_counts, record_tokens_in, record_tokens_out,
};
pub use stream::{
    NullSink, StreamConfig, StreamEvent, StreamSink, StreamTarget, TurnStreamHandler, VecSink,
    chat_client_turn, drive_streamed_turn,
};
pub use thinking::{
    DeltaKind, NudgePolicy, RecoveryAdjustment, ThinkingDemuxer, apply_recovery, demux_stream,
};
