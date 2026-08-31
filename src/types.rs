//! Wire types for the OpenAI-compatible chat completions API and tool calls.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A message in the chat transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default)]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn role(&self) -> &'static str {
        match self {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::Tool { .. } => "tool",
        }
    }

    pub fn content(&self) -> Option<&str> {
        match self {
            Message::System { content } => Some(content.as_str()),
            Message::User { content } => Some(content.as_str()),
            Message::Assistant { content, .. } => content.as_deref(),
            Message::Tool { content, .. } => Some(content.as_str()),
        }
    }
}

/// A single tool invocation requested by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

/// Request body sent to `/chat/completions`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// `enable_thinking=false` forces the backend to suppress reasoning tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolDef {
    pub fn delegate_task(roles: &[&str]) -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "delegate_task".to_string(),
                description: "Dispatch a bounded unit of domain work to a specialist subagent. The subagent receives only the brief and the supplied snippets, never the full conversation history."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_name": {
                            "type": "string",
                            "description": "Specialist role to dispatch the work to.",
                            "enum": roles
                        },
                        "prompt": {
                            "type": "string",
                            "description": "Self-contained task brief in English, scoped to the selected specialist's domain."
                        },
                        "snippets": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Bounded list of relevant excerpts or file paths passed to the subagent as its isolated context."
                        },
                        "task_id": {
                            "type": "string",
                            "description": "Optional execution_plan.md task id (e.g. t-042) enabling automatic check-off on success."
                        },
                        "image_urls": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional image references for multimodal specialists."
                        },
                        "audio_urls": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional audio references for audio specialists."
                        }
                    },
                    "required": ["agent_name", "prompt"]
                }),
            },
        }
    }

    pub fn create_plan() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "create_plan".to_string(),
                description:
                    "Write or overwrite the workspace execution plan in .marmel/execution_plan.md."
                        .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "plan_markdown": {
                            "type": "string",
                            "description": "Markdown formatted plan with - [ ] [t-xxx] tasks."
                        }
                    },
                    "required": ["plan_markdown"]
                }),
            },
        }
    }

    pub fn read_file() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "read_file".to_string(),
                description: "Read paginated UTF-8 text from a file by character offset and limit."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file." },
                        "offset": { "type": "integer", "description": "Starting character index (0-based, default: 0)." },
                        "limit": { "type": "integer", "description": "Maximum number of characters to read (default: 8000, max: 8000)." }
                    },
                    "required": ["path"]
                }),
            },
        }
    }

    pub fn write_file() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "write_file".to_string(),
                description: "Create a new file or completely overwrite an existing file."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file." },
                        "content": { "type": "string", "description": "Full file content to write." }
                    },
                    "required": ["path", "content"]
                }),
            },
        }
    }

    pub fn replace() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "replace".to_string(),
                description: "Replace an exact, unique block of text within a file. Fails if old_str matches 0 or >1 times."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the file." },
                        "old_str": { "type": "string", "description": "Exact text block to replace." },
                        "new_str": { "type": "string", "description": "New text block to insert." }
                    },
                    "required": ["path", "old_str", "new_str"]
                }),
            },
        }
    }

    pub fn run_command() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "run_command".to_string(),
                description: "Execute a command line inside a dedicated PTY with timeout and process-group isolation."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute." }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    pub fn grep_search() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "grep_search".to_string(),
                description:
                    "Search for a regex pattern across workspace files honoring .gitignore."
                        .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern to search." },
                        "path": { "type": "string", "description": "Subdirectory to restrict search." },
                        "max_results": { "type": "integer", "description": "Maximum match count (default: 100)." }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    pub fn glob() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "glob".to_string(),
                description: "Find files matching a glob pattern.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Glob pattern (e.g. '**/*.rs')." }
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    pub fn rebirth() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "rebirth".to_string(),
                description: "Compact conversation history into a structured checkpoint summary to preserve context budget."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string", "description": "Structured summary of accomplishments and current state." }
                    },
                    "required": ["summary"]
                }),
            },
        }
    }

    pub fn archive_current_plan() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "archive_current_plan".to_string(),
                description: "Archive the current execution plan to `.marmel/archive/` once all tasks have been completed."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        }
    }

    pub fn pty_spawn() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "pty_spawn".to_string(),
                description: "Spawn an interactive persistent PTY terminal session (e.g. for gdb, interactive shells, or long-running commands)."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Unique session identifier (e.g. 'dbg-1')." },
                        "command": { "type": "string", "description": "Command to run interactively (e.g. 'gdb ./target/debug/app')." },
                        "rows": { "type": "integer", "description": "Terminal rows (default: 24)." },
                        "cols": { "type": "integer", "description": "Terminal columns (default: 80)." }
                    },
                    "required": ["id", "command"]
                }),
            },
        }
    }

    pub fn pty_write() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "pty_write".to_string(),
                description: "Send input text or commands to an active interactive PTY session."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Session identifier." },
                        "input": { "type": "string", "description": "Text or command string to write." },
                        "wait_ms": { "type": "integer", "description": "Milliseconds to wait for output (default: 300)." }
                    },
                    "required": ["id", "input"]
                }),
            },
        }
    }

    pub fn pty_read() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "pty_read".to_string(),
                description: "Read unread buffer output from an active interactive PTY session."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Session identifier." },
                        "wait_ms": { "type": "integer", "description": "Milliseconds to wait before reading (default: 0)." }
                    },
                    "required": ["id"]
                }),
            },
        }
    }

    pub fn pty_close() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "pty_close".to_string(),
                description: "Close an active interactive PTY session and kill its process group."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Session identifier." }
                    },
                    "required": ["id"]
                }),
            },
        }
    }

    pub fn pty_list() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "pty_list".to_string(),
                description: "List all active interactive PTY sessions.".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
        }
    }

    pub fn leave_verdict() -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: "leave_verdict".to_string(),
                description: "Record the final verification verdict for this task. You must call this tool to finish validation."
                    .to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "verdict": {
                            "type": "string",
                            "enum": ["APPROVED", "REJECTED"],
                            "description": "The validation verdict."
                        },
                        "comments": {
                            "type": "string",
                            "description": "Mandatory feedback/critique."
                        }
                    },
                    "required": ["verdict", "comments"]
                }),
            },
        }
    }

    /// Minify and sanitize a JSON Schema by removing non-semantic metadata keys
    /// (`$schema`, `title`, `$id`, empty `$defs`/`definitions`) that bloat LLM tool definitions
    /// and degrade KV cache efficiency on local and small models.
    pub fn minify_json_schema(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut cleaned = serde_json::Map::new();
                for (k, v) in map {
                    // Strip unnecessary schema metadata
                    if k == "$schema" || k == "$id" || k == "title" {
                        continue;
                    }
                    // Strip empty $defs / definitions
                    if (k == "$defs" || k == "definitions")
                        && v.as_object().map_or(false, |o| o.is_empty())
                    {
                        continue;
                    }
                    cleaned.insert(k.clone(), Self::minify_json_schema(v));
                }
                if cleaned.is_empty() {
                    serde_json::json!({"type": "object"})
                } else {
                    serde_json::Value::Object(cleaned)
                }
            }
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(Self::minify_json_schema).collect())
            }
            other => other.clone(),
        }
    }

    /// Build a tool definition dynamically from an MCP tool, applying schema minification.
    pub fn from_mcp(tool: &crate::mcp::McpTool) -> ToolDef {
        ToolDef {
            kind: "function".to_string(),
            function: ToolFunctionDef {
                name: tool.qualified_name(),
                description: tool.description.clone().unwrap_or_default(),
                parameters: Self::minify_json_schema(&tool.input_schema),
            },
        }
    }

    pub fn default_tools() -> Vec<ToolDef> {
        let roles = &["coder", "researcher", "debugger", "validator", "generalist"];
        vec![
            Self::delegate_task(roles),
            Self::create_plan(),
            Self::archive_current_plan(),
            Self::read_file(),
            Self::write_file(),
            Self::replace(),
            Self::run_command(),
            Self::grep_search(),
            Self::glob(),
            Self::pty_spawn(),
            Self::pty_write(),
            Self::pty_read(),
            Self::pty_close(),
            Self::pty_list(),
            Self::rebirth(),
            Self::leave_verdict(),
        ]
    }
}

/// A single streaming chunk from the SSE response.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    pub id: Option<String>,
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    pub delta: ChunkDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, alias = "reasoning")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolCall {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkToolFunction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolFunction {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpTool;

    fn sample_tool(server_name: &str, name: &str) -> McpTool {
        McpTool {
            name: name.to_string(),
            description: Some(format!("{name} from {server_name}")),
            input_schema: serde_json::json!({"type": "object"}),
            server_name: server_name.to_string(),
        }
    }

    #[test]
    fn from_mcp_uses_qualified_name() {
        let tool = sample_tool("alpha", "get_weather");
        let def = ToolDef::from_mcp(&tool);
        assert_eq!(def.function.name, "alpha__get_weather");
    }

    #[test]
    fn from_mcp_preserves_description_and_schema() {
        let tool = sample_tool("alpha", "get_weather");
        let def = ToolDef::from_mcp(&tool);
        assert_eq!(def.function.description, "get_weather from alpha");
        assert_eq!(
            def.function.parameters,
            serde_json::json!({"type": "object"})
        );
        assert_eq!(def.kind, "function");
    }

    #[test]
    fn from_mcp_minifies_schema_metadata() {
        let tool = McpTool {
            name: "read_file".to_string(),
            description: Some("Reads a file".to_string()),
            input_schema: serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "title": "ReadFileArgs",
                "$id": "https://example.com/read_file.json",
                "$defs": {},
                "type": "object",
                "properties": {
                    "path": {
                        "title": "Path",
                        "type": "string",
                        "description": "File path"
                    }
                },
                "required": ["path"]
            }),
            server_name: "fs".to_string(),
        };
        let def = ToolDef::from_mcp(&tool);
        assert_eq!(
            def.function.parameters,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path"
                    }
                },
                "required": ["path"]
            })
        );
    }
}
