//! Tolerant serde types for Claude Code NDJSON (`stream-json`).
//!
//! Designed for version drift: unknown object fields are ignored, missing
//! fields default, volatile blobs stay as [`serde_json::Value`], and unknown
//! top-level `type` tags deserialize as [`ClaudeLine::Unknown`] rather than
//! failing the line.

use serde::Deserialize;
use serde_json::Value;

/// One NDJSON line from `claude --output-format stream-json`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ClaudeLine {
    #[serde(rename = "system")]
    System(SystemLine),
    #[serde(rename = "assistant")]
    Assistant(AssistantLine),
    #[serde(rename = "user")]
    User(UserLine),
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEventLine),
    #[serde(rename = "result")]
    Result(ResultLine),
    /// Unknown `type` (rate limits, control requests, future tags).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SystemLine {
    #[serde(default)]
    pub subtype: String,
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Tool list shape has drifted (strings vs objects); keep opaque.
    #[serde(default)]
    pub tools: Value,
    #[serde(default, rename = "permissionMode")]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub mcp_servers: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssistantLine {
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub message: ClaudeMessage,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UserLine {
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub message: ClaudeMessage,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StreamEventLine {
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub event: StreamEventBody,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResultLine {
    #[serde(default)]
    pub subtype: String,
    #[serde(default, alias = "sessionId")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub num_turns: u32,
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    #[serde(default)]
    pub usage: Option<ClaudeUsage>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClaudeMessage {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Assistant/tool-result lines use a block array; echoed user turns may
    /// use a plain string (the same shape we write to stdin).
    #[serde(default)]
    pub content: MessageContent,
    #[serde(default)]
    pub usage: Option<ClaudeUsage>,
}

#[derive(Debug, Clone)]
pub enum MessageContent {
    Blocks(Vec<ContentBlock>),
    Text(String),
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::Blocks(Vec::new())
    }
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(text) => Ok(Self::Text(text)),
            Value::Null => Ok(Self::Blocks(Vec::new())),
            Value::Array(_) => {
                let blocks: Vec<ContentBlock> =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(Self::Blocks(blocks))
            }
            _ => Ok(Self::Blocks(Vec::new())),
        }
    }
}

impl MessageContent {
    pub fn blocks(&self) -> &[ContentBlock] {
        match self {
            Self::Blocks(blocks) => blocks,
            Self::Text(_) => &[],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        input: Value,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        #[serde(default)]
        tool_use_id: String,
        #[serde(default)]
        content: Value,
        #[serde(default)]
        is_error: bool,
    },
    /// Catch-all so a new block type cannot fail the whole assistant/user line.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClaudeUsage {
    #[serde(default, alias = "inputTokens")]
    pub input_tokens: u64,
    #[serde(default, alias = "outputTokens")]
    pub output_tokens: u64,
    #[serde(default, alias = "cacheCreationInputTokens")]
    pub cache_creation_input_tokens: u64,
    #[serde(default, alias = "cacheReadInputTokens")]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StreamEventBody {
    #[serde(default, rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub delta: Option<StreamDelta>,
    /// `content_block_start` payload; shape varies by block type.
    #[serde(default)]
    pub content_block: Value,
    /// `message_start` / `message_delta` leftovers.
    #[serde(default)]
    pub message: Value,
    #[serde(default)]
    pub usage: Option<ClaudeUsage>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct StreamDelta {
    #[serde(default, rename = "type")]
    pub delta_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub partial_json: Option<String>,
}

impl ClaudeLine {
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::System(line) => line.session_id.as_deref(),
            Self::Assistant(line) => line.session_id.as_deref(),
            Self::User(line) => line.session_id.as_deref(),
            Self::StreamEvent(line) => line.session_id.as_deref(),
            Self::Result(line) => line.session_id.as_deref(),
            Self::Unknown => None,
        }
    }

    pub fn parent_tool_use_id(&self) -> Option<&str> {
        match self {
            Self::Assistant(line) => line.parent_tool_use_id.as_deref(),
            Self::User(line) => line.parent_tool_use_id.as_deref(),
            Self::StreamEvent(line) => line.parent_tool_use_id.as_deref(),
            Self::System(_) | Self::Result(_) | Self::Unknown => None,
        }
    }
}

/// Parse one NDJSON line. Empty/whitespace lines return `Ok(None)`.
pub fn parse_line(line: &str) -> Result<Option<ClaudeLine>, serde_json::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed).map(Some)
}

/// Parse a whole NDJSON document, skipping blank lines.
pub fn parse_ndjson(text: &str) -> Result<Vec<ClaudeLine>, serde_json::Error> {
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(parsed) = parse_line(line)? {
            out.push(parsed);
        }
    }
    Ok(out)
}

/// Convert a USD cost to integer micros, rounding to nearest.
pub fn usd_to_micros(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        0
    } else {
        (usd * 1_000_000.0).round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_top_level_type_is_not_an_error() {
        let line = parse_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"ok"},"extra":1}"#,
        )
        .unwrap()
        .unwrap();
        assert!(matches!(line, ClaudeLine::Unknown));
    }

    #[test]
    fn system_init_ignores_unknown_fields() {
        let line = parse_line(
            r#"{"type":"system","subtype":"init","session_id":"ses_1","model":"sonnet","permissionMode":"default","tools":["Bash"],"slash_commands":["compact"],"future":true}"#,
        )
        .unwrap()
        .unwrap();
        match line {
            ClaudeLine::System(sys) => {
                assert_eq!(sys.subtype, "init");
                assert_eq!(sys.session_id.as_deref(), Some("ses_1"));
                assert_eq!(sys.model.as_deref(), Some("sonnet"));
                assert_eq!(sys.permission_mode.as_deref(), Some("default"));
            }
            other => panic!("expected system, got {other:?}"),
        }
    }

    #[test]
    fn unknown_content_block_does_not_fail_assistant() {
        let line = parse_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"image","source":{"data":"x"}}]}}"#,
        )
        .unwrap()
        .unwrap();
        match line {
            ClaudeLine::Assistant(asst) => {
                let blocks = asst.message.content.blocks();
                assert_eq!(blocks.len(), 2);
                assert!(matches!(blocks[0], ContentBlock::Text { .. }));
                assert!(matches!(blocks[1], ContentBlock::Unknown));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn user_content_string_or_blocks() {
        let echoed = parse_line(
            r#"{"type":"user","message":{"role":"user","content":"hello"},"parent_tool_use_id":null}"#,
        )
        .unwrap()
        .unwrap();
        match echoed {
            ClaudeLine::User(user) => {
                assert!(matches!(user.message.content, MessageContent::Text(_)));
            }
            other => panic!("{other:?}"),
        }

        let result = parse_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}"#,
        )
        .unwrap()
        .unwrap();
        match result {
            ClaudeLine::User(user) => {
                assert!(matches!(
                    user.message.content.blocks()[0],
                    ContentBlock::ToolResult { .. }
                ));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usd_to_micros_rounds() {
        assert_eq!(usd_to_micros(0.0025), 2_500);
        assert_eq!(usd_to_micros(0.0123), 12_300);
        assert_eq!(usd_to_micros(f64::NAN), 0);
    }
}
