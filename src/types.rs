use std::fmt;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Claude,
    Gemini,
    Codex,
    Copilot,
}

impl ToolKind {
    pub const ALL: [ToolKind; 4] = [
        ToolKind::Claude,
        ToolKind::Gemini,
        ToolKind::Codex,
        ToolKind::Copilot,
    ];

    pub fn command_name(self) -> &'static str {
        match self {
            ToolKind::Claude => "claude",
            ToolKind::Gemini => "gemini",
            ToolKind::Codex => "codex",
            ToolKind::Copilot => "copilot",
        }
    }

    pub fn supports_system_prompt(self) -> bool {
        matches!(self, ToolKind::Claude)
    }
}

impl fmt::Display for ToolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            ToolKind::Claude => "Claude",
            ToolKind::Gemini => "Gemini",
            ToolKind::Codex => "Codex",
            ToolKind::Copilot => "Copilot",
        };
        f.write_str(label)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSource {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageAlertKind {
    QuotaExhausted,
    RateLimited,
}

impl fmt::Display for UsageAlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UsageAlertKind::QuotaExhausted => f.write_str("quota exhausted"),
            UsageAlertKind::RateLimited => f.write_str("rate limited"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageAlert {
    pub kind: UsageAlertKind,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl fmt::Display for ConversationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConversationStatus::Running => f.write_str("running"),
            ConversationStatus::Completed => f.write_str("completed"),
            ConversationStatus::Failed => f.write_str("failed"),
            ConversationStatus::Cancelled => f.write_str("cancelled"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleApplyMode {
    Auto,
    System,
    Prepend,
}

impl Default for RoleApplyMode {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone)]
pub struct RoleSpec {
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub default_extra_args: Vec<String>,
    pub apply_mode: RoleApplyMode,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CurrentSettings {
    pub tool: ToolKind,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Default for CurrentSettings {
    fn default() -> Self {
        Self {
            tool: ToolKind::Claude,
            model: None,
            effort: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentRequest {
    pub tool: ToolKind,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub extra_args: Vec<String>,
    pub role: Option<RoleSpec>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub id: i64,
    pub secretagent_conversation_id: String,
    pub tool_conversation_id: Option<String>,
    pub tool: ToolKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub role_name: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: ConversationStatus,
    pub title: String,
    pub usage_alert: Option<UsageAlert>,
}

#[derive(Debug, Clone)]
pub struct ConversationRecord {
    pub id: i64,
    pub secretagent_conversation_id: String,
    pub tool_conversation_id: Option<String>,
    pub tool: ToolKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub role_name: Option<String>,
    pub role_path: Option<PathBuf>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: ConversationStatus,
    pub prompt: String,
    pub response: String,
    pub stderr_output: String,
    pub extra_args: Vec<String>,
    pub usage_alert: Option<UsageAlert>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct ConversationDraft {
    pub secretagent_conversation_id: String,
    pub tool_conversation_id: Option<String>,
    pub tool: ToolKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub role_name: Option<String>,
    pub role_path: Option<PathBuf>,
    pub prompt: String,
    pub extra_args: Vec<String>,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ConversationUpdate {
    pub id: i64,
    pub tool_conversation_id: Option<String>,
    pub response: String,
    pub stderr_output: String,
    pub exit_code: Option<i32>,
    pub ended_at: DateTime<Utc>,
    pub status: ConversationStatus,
    pub usage_alert: Option<UsageAlert>,
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub assistant_response: String,
    pub tool_conversation_id: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub cancelled: bool,
    pub usage_alert: Option<UsageAlert>,
}

#[derive(Debug, Clone)]
pub enum RunnerEvent {
    Started(DateTime<Utc>),
    ToolConversationId(String),
    Chunk { source: StreamSource, text: String },
    Alert(UsageAlert),
    Finished(RunResult),
    Failed(String),
}
