use std::fs;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;

use crate::adapters::{OutputMode, build_command};
use crate::types::{
    AgentRequest, RunResult, RunnerEvent, StreamSource, UsageAlert, UsageAlertKind,
};

#[derive(Debug)]
enum InternalEvent {
    ToolConversationId(String),
    Chunk(StreamSource, String),
    StreamClosed,
}

pub struct RunningHandle {
    pub receiver: mpsc::UnboundedReceiver<RunnerEvent>,
    cancel: Option<oneshot::Sender<()>>,
}

impl RunningHandle {
    pub fn cancel(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

pub fn spawn(request: AgentRequest) -> RunningHandle {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    tokio::spawn(async move {
        if let Err(err) = run(request, event_tx.clone(), cancel_rx).await {
            let _ = event_tx.send(RunnerEvent::Failed(err.to_string()));
        }
    });

    RunningHandle {
        receiver: event_rx,
        cancel: Some(cancel_tx),
    }
}

async fn run(
    request: AgentRequest,
    event_tx: mpsc::UnboundedSender<RunnerEvent>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let started_at = Utc::now();
    let _ = event_tx.send(RunnerEvent::Started(started_at));

    let command_spec = build_command(&request)?;
    if let Some(tool_conversation_id) = &command_spec.tool_conversation_id_hint {
        let _ = event_tx.send(RunnerEvent::ToolConversationId(
            tool_conversation_id.clone(),
        ));
    }

    let mut child = Command::new(&command_spec.program)
        .args(&command_spec.args)
        .current_dir(&request.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", request.tool.command_name()))?;

    let stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr")?;

    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    match command_spec.output_mode {
        OutputMode::PlainText => {
            tokio::spawn(read_plain_stream(
                stdout,
                StreamSource::Stdout,
                internal_tx.clone(),
            ));
        }
        OutputMode::GeminiJson | OutputMode::CodexJson => {
            tokio::spawn(read_structured_stream(
                stdout,
                command_spec.output_mode,
                internal_tx.clone(),
            ));
        }
    }
    tokio::spawn(read_plain_stream(
        stderr,
        StreamSource::Stderr,
        internal_tx.clone(),
    ));

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut closed_streams = 0usize;
    let mut exit_code = None;
    let mut cancelled = false;
    let mut usage_alert = None;
    let mut tool_conversation_id = command_spec.tool_conversation_id_hint.clone();

    loop {
        tokio::select! {
            maybe_event = internal_rx.recv() => {
                match maybe_event {
                    Some(InternalEvent::ToolConversationId(id)) => {
                        if tool_conversation_id.as_deref() != Some(id.as_str()) {
                            tool_conversation_id = Some(id.clone());
                            let _ = event_tx.send(RunnerEvent::ToolConversationId(id));
                        }
                    }
                    Some(InternalEvent::Chunk(source, text)) => {
                        if let Some(alert) = detect_usage_issue(&text)
                            && usage_alert.is_none()
                        {
                            usage_alert = Some(alert.clone());
                            let _ = event_tx.send(RunnerEvent::Alert(alert));
                        }

                        match source {
                            StreamSource::Stdout => stdout_buf.push_str(&text),
                            StreamSource::Stderr => stderr_buf.push_str(&text),
                        }
                        let _ = event_tx.send(RunnerEvent::Chunk { source, text });
                    }
                    Some(InternalEvent::StreamClosed) => {
                        closed_streams += 1;
                    }
                    None => break,
                }
            }
            _ = &mut cancel_rx, if !cancelled => {
                cancelled = true;
                let _ = child.kill().await;
            }
            _ = sleep(Duration::from_millis(50)) => {
                if exit_code.is_none() {
                    if let Some(status) = child.try_wait().context("failed waiting on child process")? {
                        exit_code = status.code();
                    }
                }
            }
        }

        if exit_code.is_some() && closed_streams >= 2 {
            break;
        }
    }

    if exit_code.is_none() {
        let status = child
            .wait()
            .await
            .context("failed waiting on child process")?;
        exit_code = status.code();
    }

    let ended_at = Utc::now();
    let assistant_response = if let Some(path) = &command_spec.response_capture_file {
        let response = fs::read_to_string(path).unwrap_or_else(|_| stdout_buf.clone());
        let _ = fs::remove_file(path);
        response
    } else {
        stdout_buf.clone()
    };

    let result = RunResult {
        stdout: stdout_buf,
        stderr: stderr_buf,
        assistant_response,
        tool_conversation_id,
        exit_code,
        started_at,
        ended_at,
        cancelled,
        usage_alert,
    };

    let _ = event_tx.send(RunnerEvent::Finished(result));
    Ok(())
}

async fn read_plain_stream<R>(
    mut reader: R,
    source: StreamSource,
    tx: mpsc::UnboundedSender<InternalEvent>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut buf = vec![0_u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => {
                let _ = tx.send(InternalEvent::StreamClosed);
                break;
            }
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = tx.send(InternalEvent::Chunk(source, text));
            }
            Err(err) => {
                let _ = tx.send(InternalEvent::Chunk(
                    StreamSource::Stderr,
                    format!("stream read error: {err}\n"),
                ));
                let _ = tx.send(InternalEvent::StreamClosed);
                break;
            }
        }
    }
}

async fn read_structured_stream<R>(
    reader: R,
    output_mode: OutputMode,
    tx: mpsc::UnboundedSender<InternalEvent>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match parse_structured_line(output_mode, &line) {
                Ok(parsed) => {
                    if let Some(tool_conversation_id) = parsed.tool_conversation_id {
                        let _ = tx.send(InternalEvent::ToolConversationId(tool_conversation_id));
                    }
                    if let Some(display_text) = parsed.display_text
                        && !display_text.is_empty()
                    {
                        let _ = tx.send(InternalEvent::Chunk(StreamSource::Stdout, display_text));
                    }
                }
                Err(_) => {
                    let _ = tx.send(InternalEvent::Chunk(
                        StreamSource::Stdout,
                        format!("{line}\n"),
                    ));
                }
            },
            Ok(None) => {
                let _ = tx.send(InternalEvent::StreamClosed);
                break;
            }
            Err(err) => {
                let _ = tx.send(InternalEvent::Chunk(
                    StreamSource::Stderr,
                    format!("stream read error: {err}\n"),
                ));
                let _ = tx.send(InternalEvent::StreamClosed);
                break;
            }
        }
    }
}

#[derive(Debug)]
struct ParsedStructuredLine {
    tool_conversation_id: Option<String>,
    display_text: Option<String>,
}

fn parse_structured_line(output_mode: OutputMode, line: &str) -> Result<ParsedStructuredLine> {
    let value: Value = serde_json::from_str(line)?;
    let tool_conversation_id = find_tool_conversation_id(output_mode, &value);
    let display_text = match output_mode {
        OutputMode::GeminiJson => extract_gemini_display_text(&value),
        OutputMode::CodexJson => extract_codex_display_text(&value),
        OutputMode::PlainText => None,
    };

    Ok(ParsedStructuredLine {
        tool_conversation_id,
        display_text,
    })
}

fn extract_gemini_display_text(value: &Value) -> Option<String> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if event_type == "message" && role == "assistant" {
        return value
            .get("content")
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }

    if event_type == "error" {
        return value
            .get("message")
            .and_then(Value::as_str)
            .map(|message| format!("{message}\n"));
    }

    None
}

fn extract_codex_display_text(value: &Value) -> Option<String> {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if event_type.starts_with("response.output_text.delta") {
        return value
            .get("delta")
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }

    if event_type.starts_with("response.output_text.done") {
        return value
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| value.get("content").and_then(Value::as_str))
            .map(ToString::to_string);
    }

    if event_type.contains("error") {
        return value
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| value.get("content").and_then(Value::as_str))
            .map(|message| format!("{message}\n"));
    }

    if value.get("role").and_then(Value::as_str) == Some("assistant") {
        return value
            .get("content")
            .and_then(Value::as_str)
            .map(ToString::to_string);
    }

    None
}

fn find_tool_conversation_id(output_mode: OutputMode, value: &Value) -> Option<String> {
    let keys = match output_mode {
        OutputMode::GeminiJson => &["session_id", "sessionId"][..],
        OutputMode::CodexJson => &["thread_id", "threadId", "session_id", "sessionId"][..],
        OutputMode::PlainText => return None,
    };
    find_string_recursive(value, keys)
}

fn find_string_recursive(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(candidate) = map.get(*key).and_then(Value::as_str)
                    && !candidate.trim().is_empty()
                {
                    return Some(candidate.to_string());
                }
            }

            for nested in map.values() {
                if let Some(found) = find_string_recursive(nested, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|nested| find_string_recursive(nested, keys)),
        _ => None,
    }
}

fn detect_usage_issue(text: &str) -> Option<UsageAlert> {
    let lower = text.to_ascii_lowercase();
    let lower = lower.trim();

    let quota_patterns = [
        "insufficient_quota",
        "usage limit reached",
        "quota exceeded",
        "exceeded your current quota",
        "out of premium requests",
        "premium requests quota",
        "request limit reached",
        "out of usage tokens",
        "out of tokens",
        "quota exhausted",
        "monthly quota",
    ];

    let rate_patterns = [
        "rate limit",
        "too many requests",
        "please try again later",
        "temporarily unavailable",
        "server overloaded",
        "429",
    ];

    if quota_patterns.iter().any(|pattern| lower.contains(pattern)) {
        return Some(UsageAlert {
            kind: UsageAlertKind::QuotaExhausted,
            evidence: summarize_evidence(text),
        });
    }

    if rate_patterns.iter().any(|pattern| lower.contains(pattern)) {
        return Some(UsageAlert {
            kind: UsageAlertKind::RateLimited,
            evidence: summarize_evidence(text),
        });
    }

    None
}

fn summarize_evidence(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(text.trim())
        .chars()
        .take(160)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{OutputMode, detect_usage_issue, parse_structured_line, summarize_evidence};
    use crate::types::UsageAlertKind;

    #[test]
    fn detects_quota_exhaustion() {
        let alert = detect_usage_issue("Error: usage limit reached for this account")
            .expect("alert should be detected");
        assert_eq!(alert.kind, UsageAlertKind::QuotaExhausted);
    }

    #[test]
    fn detects_rate_limits() {
        let alert = detect_usage_issue("429 Too many requests").expect("alert should be detected");
        assert_eq!(alert.kind, UsageAlertKind::RateLimited);
    }

    #[test]
    fn trims_evidence() {
        let summary = summarize_evidence(" \n hello world \n second line");
        assert_eq!(summary, "hello world");
    }

    #[test]
    fn parses_gemini_session_id() {
        let parsed = parse_structured_line(
            OutputMode::GeminiJson,
            r#"{"type":"init","session_id":"gemini-session-1"}"#,
        )
        .expect("structured line should parse");
        assert_eq!(
            parsed.tool_conversation_id.as_deref(),
            Some("gemini-session-1")
        );
    }

    #[test]
    fn parses_codex_thread_id() {
        let parsed = parse_structured_line(
            OutputMode::CodexJson,
            r#"{"type":"response.output_text.delta","delta":"hi","thread_id":"thread-1"}"#,
        )
        .expect("structured line should parse");
        assert_eq!(parsed.tool_conversation_id.as_deref(), Some("thread-1"));
        assert_eq!(parsed.display_text.as_deref(), Some("hi"));
    }
}
