mod adapters;
mod models;
mod paths;
mod roles;
mod runner;
mod storage;
mod tui;
mod types;

use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use paths::AppPaths;
use roles::{RoleLibrary, write_sample_roles};
use storage::Store;
use types::{
    AgentRequest, ConversationDraft, ConversationStatus, ConversationUpdate, CurrentSettings,
    RunnerEvent, StreamSource, ToolKind,
};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "secret-agent")]
#[command(
    about = "Rust TUI/CLI wrapper for Claude, Gemini, Codex, and Copilot with SQLite history"
)]
struct Cli {
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[arg(long = "roles-dir", global = true)]
    role_dirs: Vec<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Tui,
    Run(RunArgs),
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Roles {
        #[command(subcommand)]
        command: RolesCommand,
    },
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long, value_enum)]
    tool: Option<ToolKind>,

    #[arg(long)]
    model: Option<String>,

    #[arg(long)]
    effort: Option<String>,

    #[arg(long)]
    role: Option<String>,

    #[arg(long, default_value = "")]
    extra_args: String,

    prompt: Option<String>,
}

#[derive(Debug, Subcommand)]
enum RolesCommand {
    List,
    Init {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::resolve(cli.db, &cli.role_dirs)?;
    let store = Store::open(&paths.db_path)?;
    let roles = RoleLibrary::load(&paths.role_dirs)?;

    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tui::run(paths, store, roles).await,
        Command::Run(args) => run_once(paths, store, roles, args).await,
        Command::History { limit } => show_history(store, limit),
        Command::Roles { command } => handle_roles(paths, roles, command),
    }
}

async fn run_once(paths: AppPaths, store: Store, roles: RoleLibrary, args: RunArgs) -> Result<()> {
    let prompt = match args.prompt {
        Some(prompt) => prompt,
        None => read_prompt_from_stdin()?,
    };

    let role = resolve_role(&roles, args.role.as_deref())?;
    let current_settings = store.current_settings(
        role.as_ref().map(|role| role.name.as_str()),
        role.as_ref().map(|role| role.path.as_path()),
    )?;
    let resolved_settings = CurrentSettings {
        tool: args.tool.unwrap_or(current_settings.tool),
        model: args.model.clone().or(current_settings.model),
        effort: args.effort.clone().or(current_settings.effort),
    };
    models::validate_model_for_tool(resolved_settings.tool, resolved_settings.model.as_deref())
        .map_err(anyhow::Error::msg)?;
    store.save_current_settings(
        role.as_ref().map(|role| role.name.as_str()),
        role.as_ref().map(|role| role.path.as_path()),
        &resolved_settings,
    )?;

    let extra_args = adapters::parse_extra_args(&args.extra_args)?;
    let request = AgentRequest {
        tool: resolved_settings.tool,
        prompt: prompt.clone(),
        model: resolved_settings.model.clone(),
        effort: resolved_settings.effort.clone(),
        extra_args: extra_args.clone(),
        role: role.clone(),
        cwd: paths.cwd.clone(),
    };

    let started_at = chrono::Utc::now();
    let secretagent_conversation_id = Uuid::now_v7().to_string();
    let conversation_id = store.insert_conversation(&ConversationDraft {
        secretagent_conversation_id,
        tool_conversation_id: None,
        tool: request.tool,
        model: request.model.clone(),
        effort: request.effort.clone(),
        role_name: role.as_ref().map(|role| role.name.clone()),
        role_path: role.as_ref().map(|role| role.path.clone()),
        prompt,
        extra_args,
        started_at,
    })?;

    let mut handle = runner::spawn(request);
    let mut stdout_buffer = String::new();
    let mut stderr_buffer = String::new();
    let mut usage_alert = None;
    let mut tool_conversation_id = None;

    while let Some(event) = handle.receiver.recv().await {
        match event {
            RunnerEvent::Started(ts) => {
                eprintln!(
                    "conversation #{conversation_id} started at {}",
                    ts.format("%Y-%m-%d %H:%M:%S")
                );
            }
            RunnerEvent::ToolConversationId(id) => {
                tool_conversation_id = Some(id.clone());
                store.update_tool_conversation_id(conversation_id, &id)?;
                eprintln!("tool conversation id: {id}");
            }
            RunnerEvent::Chunk { source, text } => match source {
                StreamSource::Stdout => {
                    stdout_buffer.push_str(&text);
                    print!("{text}");
                    io::stdout().flush().ok();
                }
                StreamSource::Stderr => {
                    stderr_buffer.push_str(&text);
                    eprint!("{text}");
                    io::stderr().flush().ok();
                }
            },
            RunnerEvent::Alert(alert) => {
                usage_alert = Some(alert.clone());
                eprintln!("usage alert: {} ({})", alert.kind, alert.evidence);
            }
            RunnerEvent::Finished(result) => {
                let status = if result.cancelled {
                    ConversationStatus::Cancelled
                } else if result.exit_code == Some(0) {
                    ConversationStatus::Completed
                } else {
                    ConversationStatus::Failed
                };

                store.finish_conversation(&ConversationUpdate {
                    id: conversation_id,
                    tool_conversation_id: result
                        .tool_conversation_id
                        .clone()
                        .or(tool_conversation_id),
                    response: result.assistant_response,
                    stderr_output: result.stderr,
                    exit_code: result.exit_code,
                    ended_at: result.ended_at,
                    status,
                    usage_alert: result.usage_alert.clone().or(usage_alert.clone()),
                })?;
                return Ok(());
            }
            RunnerEvent::Failed(message) => {
                store.finish_conversation(&ConversationUpdate {
                    id: conversation_id,
                    tool_conversation_id,
                    response: stdout_buffer,
                    stderr_output: format!("{stderr_buffer}\n{message}"),
                    exit_code: None,
                    ended_at: chrono::Utc::now(),
                    status: ConversationStatus::Failed,
                    usage_alert,
                })?;
                return Err(anyhow!(message));
            }
        }
    }

    Err(anyhow!("runner channel closed unexpectedly"))
}

fn show_history(store: Store, limit: usize) -> Result<()> {
    let history = store.list_conversations(limit)?;
    for conversation in history {
        let model = conversation.model.unwrap_or_else(|| "-".to_string());
        let effort = conversation.effort.unwrap_or_else(|| "-".to_string());
        let ended = conversation
            .ended_at
            .map(|ended_at| ended_at.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "#{:03} {:8} {:10} {} -> {} {} {} {} sa={} tool={}",
            conversation.id,
            conversation.tool,
            conversation.status,
            conversation.started_at.format("%Y-%m-%d %H:%M"),
            ended,
            model,
            effort,
            conversation.title,
            conversation.secretagent_conversation_id,
            conversation
                .tool_conversation_id
                .unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(())
}

fn handle_roles(paths: AppPaths, roles: RoleLibrary, command: RolesCommand) -> Result<()> {
    match command {
        RolesCommand::List => {
            if roles.all().is_empty() {
                println!("No roles found in:");
                for dir in &paths.role_dirs {
                    println!("  {}", dir.display());
                }
                return Ok(());
            }

            for role in roles.all() {
                println!("{}\t{}", role.name, role.path.display());
            }
            Ok(())
        }
        RolesCommand::Init { dir, force } => {
            let target = dir.unwrap_or_else(|| paths.config_dir.join("roles"));
            let written = write_sample_roles(&target, force)?;
            if written.is_empty() {
                println!(
                    "No files written. Existing sample roles already exist in {}",
                    target.display()
                );
            } else {
                println!("Wrote sample roles:");
                for path in written {
                    println!("  {}", path.display());
                }
            }
            Ok(())
        }
    }
}

fn resolve_role(roles: &RoleLibrary, requested: Option<&str>) -> Result<Option<types::RoleSpec>> {
    let Some(requested) = requested else {
        return Ok(None);
    };

    if let Some(role) = roles.get_by_name(requested) {
        return Ok(Some(role));
    }

    let path = PathBuf::from(requested);
    if let Some(role) = roles.get_by_path(&path) {
        return Ok(Some(role));
    }

    Err(anyhow!(
        "role '{requested}' was not found in the configured role directories"
    ))
}

fn read_prompt_from_stdin() -> Result<String> {
    if io::stdin().is_terminal() {
        return Err(anyhow!("prompt is required when stdin is a TTY"));
    }

    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .context("failed to read prompt from stdin")?;
    Ok(buffer)
}
