use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use uuid::Uuid;
use which::which;

use crate::types::{AgentRequest, RoleApplyMode, RoleSpec, ToolKind};

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub response_capture_file: Option<PathBuf>,
    pub output_mode: OutputMode,
    pub tool_conversation_id_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    PlainText,
    GeminiJson,
    CodexJson,
}

pub fn build_command(request: &AgentRequest) -> Result<CommandSpec> {
    let program = which(request.tool.command_name()).with_context(|| {
        format!(
            "{} is not installed or not in PATH",
            request.tool.command_name()
        )
    })?;

    let role = request.role.as_ref();
    let composed_prompt = compose_prompt(request.prompt.trim(), role, request.tool);
    let mut args = Vec::new();
    let mut response_capture_file = None;
    let mut output_mode = OutputMode::PlainText;
    let mut tool_conversation_id_hint = None;

    match request.tool {
        ToolKind::Claude => {
            let tool_conversation_id = Uuid::now_v7().to_string();
            args.push("-p".to_string());
            args.push("--session-id".to_string());
            args.push(tool_conversation_id.clone());
            if let Some(model) = clean(&request.model) {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(effort) = clean(&request.effort) {
                args.push("--effort".to_string());
                args.push(effort.to_string());
            }
            if let Some(role) = role
                && should_apply_role_as_system(request.tool, role)
            {
                args.push("--system-prompt".to_string());
                args.push(role.body.clone());
            }
            args.extend(collect_extra_args(role, &request.extra_args)?);
            args.push(composed_prompt);
            tool_conversation_id_hint = Some(tool_conversation_id);
        }
        ToolKind::Gemini => {
            args.push("-p".to_string());
            args.push(composed_prompt);
            args.push("--output-format".to_string());
            args.push("stream-json".to_string());
            if let Some(model) = clean(&request.model) {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            args.extend(collect_extra_args(role, &request.extra_args)?);
            output_mode = OutputMode::GeminiJson;
        }
        ToolKind::Codex => {
            args.push("exec".to_string());
            args.push("--json".to_string());
            if let Some(model) = clean(&request.model) {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            let capture_path = env::temp_dir().join(format!(
                "secret-agent-codex-{}.txt",
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            args.push("--output-last-message".to_string());
            args.push(capture_path.to_string_lossy().to_string());
            response_capture_file = Some(capture_path);
            args.extend(collect_extra_args(role, &request.extra_args)?);
            args.push(composed_prompt);
            output_mode = OutputMode::CodexJson;
        }
        ToolKind::Copilot => {
            let tool_conversation_id = Uuid::now_v7().to_string();
            args.push("-p".to_string());
            args.push(composed_prompt);
            args.push(format!("--resume={tool_conversation_id}"));
            args.push("-s".to_string());
            args.push("--stream".to_string());
            args.push("on".to_string());
            args.push("--allow-all-tools".to_string());
            if let Some(model) = clean(&request.model) {
                args.push("--model".to_string());
                args.push(model.to_string());
            }
            if let Some(effort) = clean(&request.effort) {
                args.push("--effort".to_string());
                args.push(effort.to_string());
            }
            args.extend(collect_extra_args(role, &request.extra_args)?);
            tool_conversation_id_hint = Some(tool_conversation_id);
        }
    }

    Ok(CommandSpec {
        program: program.to_string_lossy().to_string(),
        args,
        response_capture_file,
        output_mode,
        tool_conversation_id_hint,
    })
}

pub fn tool_notes(tool: ToolKind) -> &'static str {
    match tool {
        ToolKind::Claude => {
            "supports model and effort directly; role markdown can be injected as a system prompt"
        }
        ToolKind::Gemini => {
            "supports model directly; role markdown is prepended to the task prompt"
        }
        ToolKind::Codex => "supports model directly; role markdown is prepended to the task prompt",
        ToolKind::Copilot => {
            "supports model and reasoning effort directly; non-interactive mode requires --allow-all-tools"
        }
    }
}

fn collect_extra_args(role: Option<&RoleSpec>, request_args: &[String]) -> Result<Vec<String>> {
    let mut merged = Vec::new();
    if let Some(role) = role {
        merged.extend(role.default_extra_args.clone());
    }
    merged.extend(request_args.to_vec());
    Ok(merged)
}

fn compose_prompt(prompt: &str, role: Option<&RoleSpec>, tool: ToolKind) -> String {
    let Some(role) = role else {
        return prompt.to_string();
    };

    if should_apply_role_as_system(tool, role) {
        return prompt.to_string();
    }

    format!(
        "Follow this role definition for the full task.\n\n# Role\n{}\n\n# Task\n{}",
        role.body.trim(),
        prompt
    )
}

fn should_apply_role_as_system(tool: ToolKind, role: &RoleSpec) -> bool {
    match role.apply_mode {
        RoleApplyMode::System => tool.supports_system_prompt(),
        RoleApplyMode::Prepend => false,
        RoleApplyMode::Auto => tool.supports_system_prompt(),
    }
}

fn clean(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn parse_extra_args(raw: &str) -> Result<Vec<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    shell_words::split(trimmed).map_err(|err| anyhow!("failed to parse extra args: {err}"))
}

#[cfg(test)]
mod tests {
    use super::parse_extra_args;

    #[test]
    fn parses_shell_words() {
        let args = parse_extra_args("--model foo --flag \"hello world\"")
            .expect("shell words parsing should succeed");
        assert_eq!(args, vec!["--model", "foo", "--flag", "hello world"]);
    }
}
