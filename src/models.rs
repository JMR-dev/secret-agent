use crate::types::ToolKind;

const CLAUDE_MODELS: &[&str] = &[
    "default",
    "sonnet",
    "opus",
    "haiku",
    "sonnet[1m]",
    "opus[1m]",
    "opusplan",
    "claude-sonnet-4-6",
    "claude-sonnet-4-6[1m]",
    "claude-opus-4-6",
    "claude-opus-4-6[1m]",
    "claude-haiku-4-5",
];

const AGY_MODELS: &[&str] = &[
    "auto",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-3-pro",
    "gemini-3-pro-preview",
    "gemini-3.1-pro",
    "gemini-3-flash",
];

const CODEX_MODELS: &[&str] = &[
    "gpt-5",
    "gpt-5.1",
    "gpt-5.2",
    "gpt-5.2-pro",
    "gpt-5.4",
    "gpt-5.4-pro",
    "gpt-5-codex",
    "gpt-5.1-codex",
    "gpt-5.1-codex-mini",
    "gpt-5.1-codex-max",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "codex-mini-latest",
];

const COPILOT_MODELS: &[&str] = &[
    "gpt-4.1",
    "gpt-5",
    "gpt-5-mini",
    "gpt-5-nano",
    "gpt-5-pro",
    "gpt-5.1",
    "gpt-5.1-codex",
    "gpt-5.1-codex-mini",
    "gpt-5.1-codex-max",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "gpt-5.4",
    "claude-haiku-4.5",
    "claude-opus-4.5",
    "claude-opus-4.6",
    "claude-sonnet-4",
    "claude-sonnet-4.5",
    "claude-sonnet-4.6",
    "gemini-2.5-pro",
    "gemini-3-flash",
    "gemini-3-pro",
    "gemini-3.1-pro",
    "grok-code-fast-1",
    "raptor-mini",
    "goldeneye",
];

pub fn validate_model_for_tool(tool: ToolKind, model: Option<&str>) -> Result<(), String> {
    let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
        return Ok(());
    };

    if is_model_available(tool, model) {
        return Ok(());
    }

    Err(format!(
        "Model {model} not available in this tool. Please re-enter and try again."
    ))
}

fn is_model_available(tool: ToolKind, model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    match tool {
        ToolKind::Claude => {
            CLAUDE_MODELS.contains(&normalized.as_str()) || matches_claude_model_name(&normalized)
        }
        ToolKind::Agy => AGY_MODELS.contains(&normalized.as_str()),
        ToolKind::Codex => CODEX_MODELS.contains(&normalized.as_str()),
        ToolKind::Copilot => COPILOT_MODELS.contains(&normalized.as_str()),
    }
}

fn matches_claude_model_name(model: &str) -> bool {
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    let Some(rest) = base.strip_prefix("claude-") else {
        return false;
    };

    rest.starts_with("sonnet-4-") || rest.starts_with("opus-4-") || rest.starts_with("haiku-4-")
}

#[cfg(test)]
mod tests {
    use super::validate_model_for_tool;
    use crate::types::ToolKind;

    #[test]
    fn accepts_known_claude_aliases() {
        assert!(validate_model_for_tool(ToolKind::Claude, Some("sonnet")).is_ok());
        assert!(validate_model_for_tool(ToolKind::Claude, Some("claude-opus-4-6[1m]")).is_ok());
    }

    #[test]
    fn rejects_unknown_claude_model() {
        let error = validate_model_for_tool(ToolKind::Claude, Some("claude-random"))
            .expect_err("unknown model should fail");
        assert_eq!(
            error,
            "Model claude-random not available in this tool. Please re-enter and try again."
        );
    }

    #[test]
    fn accepts_known_codex_model() {
        assert!(validate_model_for_tool(ToolKind::Codex, Some("gpt-5.4")).is_ok());
    }

    #[test]
    fn rejects_unknown_codex_model() {
        assert!(validate_model_for_tool(ToolKind::Codex, Some("sonnet")).is_err());
    }

    #[test]
    fn accepts_empty_model() {
        assert!(validate_model_for_tool(ToolKind::Copilot, None).is_ok());
        assert!(validate_model_for_tool(ToolKind::Copilot, Some("   ")).is_ok());
    }
}
