use std::future::pending;
use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::time::interval;
use uuid::Uuid;
use which::which;

use crate::adapters::{parse_extra_args, tool_notes};
use crate::models;
use crate::paths::AppPaths;
use crate::roles::RoleLibrary;
use crate::runner::{self, RunningHandle};
use crate::storage::Store;
use crate::types::{
    AgentRequest, ConversationDraft, ConversationStatus, ConversationSummary, ConversationUpdate,
    CurrentSettings, RoleSpec, RunnerEvent, StreamSource, ToolKind, UsageAlert,
};

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

const SLASH_COMMANDS: [SlashCommand; 3] = [
    SlashCommand {
        name: "/detect-tools",
        description: "Re-scan the system for supported tool CLIs",
        action: SlashCommandAction::DetectTools,
    },
    SlashCommand {
        name: "/exit",
        description: "Exit the TUI and return to the CLI",
        action: SlashCommandAction::Exit,
    },
    SlashCommand {
        name: "/quit",
        description: "Exit the TUI and return to the CLI",
        action: SlashCommandAction::Exit,
    },
];

pub async fn run(paths: AppPaths, store: Store, roles: RoleLibrary) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    let supports_keyboard_enhancement = matches!(supports_keyboard_enhancement(), Ok(true));
    execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
    if supports_keyboard_enhancement {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )
        .context("failed to enable keyboard enhancement flags")?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;
    terminal.clear().context("failed to clear terminal")?;

    let result = run_loop(&mut terminal, App::new(paths, store, roles)?).await;

    disable_raw_mode().context("failed to disable raw mode")?;
    if supports_keyboard_enhancement {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)
            .context("failed to disable keyboard enhancement flags")?;
    }
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")?;

    result
}

async fn run_loop(terminal: &mut TuiTerminal, mut app: App) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(100));

    loop {
        terminal.draw(|frame| app.draw(frame))?;

        let runner_future = async {
            if let Some(running) = app.running.as_mut() {
                running.receiver.recv().await
            } else {
                pending::<Option<RunnerEvent>>().await
            }
        };

        tokio::select! {
            maybe_event = events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    app.handle_key(key)?;
                    if app.should_quit {
                        break;
                    }
                }
            }
            maybe_runner = runner_future => {
                if let Some(runner_event) = maybe_runner {
                    app.handle_runner_event(runner_event)?;
                }
            }
            _ = tick.tick() => {}
        }
    }

    Ok(())
}

struct App {
    paths: AppPaths,
    store: Store,
    roles: RoleLibrary,
    history: Vec<ConversationSummary>,
    history_state: ListState,
    focus: Focus,
    available_tools: Vec<ToolKind>,
    tool_index: usize,
    role_index: usize,
    model_editor: TextEditor,
    effort_editor: TextEditor,
    args_editor: TextEditor,
    prompt_editor: TextEditor,
    output_chunks: Vec<OutputChunk>,
    output_scroll: usize,
    status: String,
    alert: Option<UsageAlert>,
    running: Option<RunningHandle>,
    active_conversation_id: Option<i64>,
    active_tool_conversation_id: Option<String>,
    active_stdout: String,
    active_stderr: String,
    should_quit: bool,
}

impl App {
    fn new(paths: AppPaths, store: Store, roles: RoleLibrary) -> Result<Self> {
        let history = store.list_conversations(100)?;
        let available_tools = detect_available_tools();
        let mut history_state = ListState::default();
        if !history.is_empty() {
            history_state.select(Some(0));
        }

        let mut app = Self {
            paths,
            store,
            roles,
            history,
            history_state,
            focus: Focus::Prompt,
            available_tools,
            tool_index: 0,
            role_index: 0,
            model_editor: TextEditor::single_line(),
            effort_editor: TextEditor::single_line(),
            args_editor: TextEditor::single_line(),
            prompt_editor: TextEditor::multi_line(),
            output_chunks: Vec::new(),
            output_scroll: 0,
            status: "ready".to_string(),
            alert: None,
            running: None,
            active_conversation_id: None,
            active_tool_conversation_id: None,
            active_stdout: String::new(),
            active_stderr: String::new(),
            should_quit: false,
        };
        app.load_current_settings_for_current_role()?;
        Ok(app)
    }

    fn draw(&mut self, frame: &mut Frame) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(2),
            ])
            .split(frame.area());

        self.draw_header(frame, layout[0]);

        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(layout[1]);

        self.draw_history(frame, main[0]);
        self.draw_workspace(frame, main[1]);
        self.draw_footer(frame, layout[2]);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    "secret-agent",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(format!("cwd: {}", self.paths.cwd.display())),
            ]),
            Line::from(vec![
                Span::raw(format!("tool: {}", self.current_tool_label())),
                Span::raw("  "),
                Span::raw(format!(
                    "detected: {}  roles: {}  db: {}",
                    self.available_tools.len(),
                    self.roles.all().len(),
                    self.paths.db_path.display()
                )),
            ]),
        ];

        if let Some(alert) = &self.alert {
            lines.push(Line::from(Span::styled(
                format!("alert: {} ({})", alert.kind, alert.evidence),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }

        let block = Block::default().borders(Borders::ALL).title("Session");
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_history(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.history.is_empty() {
            vec![ListItem::new("No saved conversations yet")]
        } else {
            self.history
                .iter()
                .map(|conversation| {
                    let mut line = format!("#{:03} {:8}", conversation.id, conversation.tool);
                    if let Some(model) = &conversation.model {
                        line.push_str(&format!(" {}", model));
                    }
                    if let Some(effort) = &conversation.effort {
                        line.push_str(&format!(" [{}]", effort));
                    }
                    line.push_str(&format!(" {}", conversation.title));
                    if let Some(role_name) = &conversation.role_name {
                        line.push_str(&format!(" [{role_name}]"));
                    }
                    if let Some(ended_at) = conversation.ended_at {
                        line.push_str(&format!(" @{}", ended_at.format("%m-%d %H:%M")));
                    }
                    if let Some(alert) = &conversation.usage_alert {
                        line.push_str(&format!(" [{}]", alert.kind));
                    }
                    if let Some(tool_conversation_id) = &conversation.tool_conversation_id {
                        line.push_str(&format!(" <{}>", shorten_id(tool_conversation_id)));
                    }
                    ListItem::new(line)
                })
                .collect()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_style(self.focus == Focus::History))
            .title("History");
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::default().bg(Color::Blue))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut self.history_state);
    }

    fn draw_workspace(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Min(8),
                Constraint::Min(8),
            ])
            .split(area);

        self.draw_controls(frame, chunks[0]);
        self.draw_prompt(frame, chunks[1]);
        self.draw_output(frame, chunks[2]);
    }

    fn draw_controls(&mut self, frame: &mut Frame, area: Rect) {
        let outer = Block::default().borders(Borders::ALL).title("Controls");
        let inner = outer.inner(area);
        frame.render_widget(outer, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(inner);

        let row_one = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(rows[0]);
        let row_two = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(rows[1]);

        render_value_box(
            frame,
            row_one[0],
            "Tool",
            self.current_tool_label(),
            self.focus == Focus::Tool,
        );
        render_value_box(
            frame,
            row_one[1],
            "Role",
            self.current_role()
                .map(|role| role.name)
                .unwrap_or_else(|| "None".to_string()),
            self.focus == Focus::Role,
        );
        render_editor(
            frame,
            row_two[0],
            "Model",
            self.focus == Focus::Model,
            &mut self.model_editor,
        );
        render_editor(
            frame,
            row_two[1],
            "Effort",
            self.focus == Focus::Effort,
            &mut self.effort_editor,
        );
        render_editor(
            frame,
            rows[2],
            "Extra Args",
            self.focus == Focus::ExtraArgs,
            &mut self.args_editor,
        );
    }

    fn draw_prompt(&mut self, frame: &mut Frame, area: Rect) {
        let prompt = self.prompt_editor.text();
        let Some(matches) = slash_command_matches(&prompt) else {
            render_editor(
                frame,
                area,
                "Prompt",
                self.focus == Focus::Prompt,
                &mut self.prompt_editor,
            );
            return;
        };

        let suggestion_height =
            (matches.len().max(1) as u16 + 2).min(area.height.saturating_sub(3).max(3));
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(suggestion_height)])
            .split(area);

        render_editor(
            frame,
            sections[0],
            "Prompt",
            self.focus == Focus::Prompt,
            &mut self.prompt_editor,
        );
        self.draw_slash_commands(frame, sections[1], &matches);
    }

    fn draw_output(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_style(self.focus == Focus::Output))
            .title("Streamed Output");
        let inner = block.inner(area);
        let lines = self.output_lines();
        let height = inner.height.saturating_sub(1) as usize;
        let max_scroll = lines.len().saturating_sub(height);
        let scroll = self.output_scroll.min(max_scroll);

        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        frame.render_widget(paragraph, area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let left = format!("status: {}", self.status);
        let right =
            "Tab move  Ctrl+Enter run  Ctrl+C cancel  Ctrl+N new  Ctrl+R reload  Ctrl+Q quit";
        let block = Block::default().borders(Borders::ALL).title("Keys");
        let paragraph = Paragraph::new(vec![Line::from(format!("{left} | {right}"))]).block(block);
        frame.render_widget(paragraph, area);
    }

    fn draw_slash_commands(&self, frame: &mut Frame, area: Rect, matches: &[SlashCommand]) {
        let items: Vec<ListItem> = if matches.is_empty() {
            vec![ListItem::new("No matching slash commands")]
        } else {
            matches
                .iter()
                .map(|command| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            command.name,
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            command.description,
                            Style::default().add_modifier(Modifier::DIM),
                        ),
                    ]))
                })
                .collect()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_style(self.focus == Focus::Prompt))
            .title("Slash Commands");
        frame.render_widget(List::new(items).block(block), area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Enter | KeyCode::Char('j') | KeyCode::Char('m') => {
                    self.start_run()?;
                    return Ok(());
                }
                KeyCode::Char('q') => {
                    if self.running.is_some() {
                        self.status = "cancel the active run first with Ctrl+C".to_string();
                    } else {
                        self.should_quit = true;
                    }
                    return Ok(());
                }
                KeyCode::Char('c') => {
                    if let Some(running) = self.running.as_mut() {
                        running.cancel();
                        self.status = "cancellation requested".to_string();
                    } else {
                        self.should_quit = true;
                    }
                    return Ok(());
                }
                KeyCode::Char('n') => {
                    self.reset_draft()?;
                    return Ok(());
                }
                KeyCode::Char('r') => {
                    self.reload()?;
                    return Ok(());
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => {
                self.focus = self.focus.next();
            }
            KeyCode::BackTab => {
                self.focus = self.focus.previous();
            }
            KeyCode::Enter if self.focus == Focus::History => {
                self.load_selected_history()?;
            }
            KeyCode::Enter if self.focus == Focus::Prompt => {
                if let Some(action) = slash_command_action(&self.prompt_editor.text()) {
                    self.run_slash_command(action);
                } else {
                    self.handle_focus_input(key)?;
                }
            }
            _ => self.handle_focus_input(key)?,
        }

        Ok(())
    }

    fn handle_focus_input(&mut self, key: KeyEvent) -> Result<()> {
        match self.focus {
            Focus::History => self.handle_history_key(key),
            Focus::Tool => self.handle_tool_key(key)?,
            Focus::Role => self.handle_role_key(key)?,
            Focus::Model => {
                self.model_editor.handle_key(key);
                self.persist_current_settings_for_current_role()?;
            }
            Focus::Effort => {
                self.effort_editor.handle_key(key);
                self.persist_current_settings_for_current_role()?;
            }
            Focus::ExtraArgs => {
                self.args_editor.handle_key(key);
            }
            Focus::Prompt => {
                self.prompt_editor.handle_key(key);
            }
            Focus::Output => self.handle_output_key(key),
        }
        Ok(())
    }

    fn handle_history_key(&mut self, key: KeyEvent) {
        if self.history.is_empty() {
            return;
        }

        let current = self.history_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up => {
                let next = current.saturating_sub(1);
                self.history_state.select(Some(next));
            }
            KeyCode::Down => {
                let next = (current + 1).min(self.history.len().saturating_sub(1));
                self.history_state.select(Some(next));
            }
            _ => {}
        }
    }

    fn handle_tool_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.available_tools.is_empty() {
            self.status = no_tools_detected_message();
            return Ok(());
        }

        let previous_index = self.tool_index;
        match key.code {
            KeyCode::Left => {
                self.tool_index = self.tool_index.saturating_sub(1);
            }
            KeyCode::Right => {
                self.tool_index =
                    (self.tool_index + 1).min(self.available_tools.len().saturating_sub(1));
            }
            _ => {}
        }
        if self.tool_index != previous_index {
            if self.persist_current_settings_for_current_role()? {
                self.status = tool_notes(
                    self.current_tool()
                        .expect("tool selection should exist when available tools are present"),
                )
                .to_string();
            }
            return Ok(());
        }
        self.status = tool_notes(
            self.current_tool()
                .expect("tool selection should exist when available tools are present"),
        )
        .to_string();
        Ok(())
    }

    fn handle_role_key(&mut self, key: KeyEvent) -> Result<()> {
        let max = self.roles.all().len();
        let previous_role = self.current_role();
        let previous_index = self.role_index;
        match key.code {
            KeyCode::Left => {
                self.role_index = self.role_index.saturating_sub(1);
            }
            KeyCode::Right => {
                self.role_index = (self.role_index + 1).min(max);
            }
            _ => {}
        }
        if self.role_index != previous_index {
            if !self.persist_current_settings(previous_role.as_ref())? {
                self.role_index = previous_index;
                return Ok(());
            }
            self.load_current_settings_for_current_role()?;
        }
        Ok(())
    }

    fn handle_output_key(&mut self, key: KeyEvent) {
        let max_scroll = self.output_lines().len().saturating_sub(1);
        match key.code {
            KeyCode::Up => {
                self.output_scroll = self.output_scroll.saturating_sub(1);
            }
            KeyCode::Down => {
                self.output_scroll = (self.output_scroll + 1).min(max_scroll);
            }
            KeyCode::PageUp => {
                self.output_scroll = self.output_scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.output_scroll = (self.output_scroll + 10).min(max_scroll);
            }
            _ => {}
        }
    }

    fn handle_runner_event(&mut self, event: RunnerEvent) -> Result<()> {
        match event {
            RunnerEvent::Started(started_at) => {
                self.status = format!(
                    "running {} at {}",
                    self.current_tool_label(),
                    started_at.format("%Y-%m-%d %H:%M:%S")
                );
            }
            RunnerEvent::ToolConversationId(id) => {
                self.active_tool_conversation_id = Some(id.clone());
                if let Some(conversation_id) = self.active_conversation_id {
                    self.store
                        .update_tool_conversation_id(conversation_id, &id)?;
                }
                self.status = format!("tool conversation id: {id}");
            }
            RunnerEvent::Chunk { source, text } => {
                match source {
                    StreamSource::Stdout => self.active_stdout.push_str(&text),
                    StreamSource::Stderr => self.active_stderr.push_str(&text),
                }
                self.output_chunks.push(OutputChunk { source, text });
                self.output_scroll = self.output_lines().len().saturating_sub(1);
            }
            RunnerEvent::Alert(alert) => {
                self.alert = Some(alert.clone());
                self.status = format!("usage alert: {}", alert.kind);
            }
            RunnerEvent::Finished(result) => {
                let status = if result.cancelled {
                    ConversationStatus::Cancelled
                } else if result.exit_code == Some(0) {
                    ConversationStatus::Completed
                } else {
                    ConversationStatus::Failed
                };

                if let Some(id) = self.active_conversation_id.take() {
                    self.store.finish_conversation(&ConversationUpdate {
                        id,
                        tool_conversation_id: result
                            .tool_conversation_id
                            .clone()
                            .or(self.active_tool_conversation_id.clone()),
                        response: result.assistant_response.clone(),
                        stderr_output: result.stderr.clone(),
                        exit_code: result.exit_code,
                        ended_at: result.ended_at,
                        status,
                        usage_alert: result.usage_alert.clone(),
                    })?;
                }

                self.alert = result.usage_alert.clone();
                self.status = format!(
                    "{} {} ({} -> {}, exit {:?})",
                    self.current_tool_label(),
                    status,
                    result.started_at.format("%H:%M:%S"),
                    result.ended_at.format("%H:%M:%S"),
                    result.exit_code
                );
                self.running = None;
                self.active_tool_conversation_id = result.tool_conversation_id;
                self.active_stdout = result.stdout;
                self.active_stderr = result.stderr;
                self.reload_history()?;
            }
            RunnerEvent::Failed(message) => {
                if let Some(id) = self.active_conversation_id.take() {
                    self.store.finish_conversation(&ConversationUpdate {
                        id,
                        tool_conversation_id: self.active_tool_conversation_id.clone(),
                        response: self.active_stdout.clone(),
                        stderr_output: format!("{}\n{}", self.active_stderr, message),
                        exit_code: None,
                        ended_at: Utc::now(),
                        status: ConversationStatus::Failed,
                        usage_alert: self.alert.clone(),
                    })?;
                }
                self.output_chunks.push(OutputChunk {
                    source: StreamSource::Stderr,
                    text: format!("{message}\n"),
                });
                self.status = format!("run failed: {message}");
                self.running = None;
                self.reload_history()?;
            }
        }

        Ok(())
    }

    fn start_run(&mut self) -> Result<()> {
        if self.running.is_some() {
            self.status = "a run is already active".to_string();
            return Ok(());
        }

        let prompt = self.prompt_editor.text();
        if let Some(action) = slash_command_action(&prompt) {
            self.run_slash_command(action);
            return Ok(());
        }
        if prompt.trim().is_empty() {
            self.status = "prompt is empty".to_string();
            return Ok(());
        }

        let Some(tool) = self.current_tool() else {
            self.status = no_tools_detected_message();
            return Ok(());
        };

        let model = clean_editor(&self.model_editor);
        if let Err(message) = models::validate_model_for_tool(tool, model.as_deref()) {
            self.status = message;
            return Ok(());
        }
        if !self.persist_current_settings_for_current_role()? {
            return Ok(());
        }

        let extra_args = parse_extra_args(&self.args_editor.text())?;
        let role = self.current_role();
        let request = AgentRequest {
            tool,
            prompt: prompt.clone(),
            model,
            effort: clean_editor(&self.effort_editor),
            extra_args: extra_args.clone(),
            role: role.clone(),
            cwd: self.paths.cwd.clone(),
        };

        let started_at = Utc::now();
        let secretagent_conversation_id = Uuid::now_v7().to_string();
        let conversation_id = self.store.insert_conversation(&ConversationDraft {
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

        self.output_chunks.clear();
        self.output_scroll = 0;
        self.alert = None;
        self.active_stdout.clear();
        self.active_stderr.clear();
        self.active_conversation_id = Some(conversation_id);
        self.active_tool_conversation_id = None;
        self.status = format!("starting conversation #{conversation_id}");
        self.running = Some(runner::spawn(request));
        self.reload_history()?;
        Ok(())
    }

    fn run_slash_command(&mut self, action: SlashCommandAction) {
        self.prompt_editor.clear();

        if self.running.is_some() {
            self.status = "cancel the active run first with Ctrl+C".to_string();
            return;
        }

        match action {
            SlashCommandAction::DetectTools => {
                if let Err(err) = self.detect_tools() {
                    let message = format!("failed to detect tools: {err}");
                    self.status = message.clone();
                    self.push_output_message(StreamSource::Stderr, format!("{message}\n"));
                }
            }
            SlashCommandAction::Exit => {
                self.status = "exiting to CLI".to_string();
                self.should_quit = true;
            }
        }
    }

    fn reload(&mut self) -> Result<()> {
        let selected_role = self.current_role();
        self.roles = RoleLibrary::load(&self.paths.role_dirs)?;
        self.role_index = self.resolve_role_index(
            selected_role.as_ref().map(|role| role.name.as_str()),
            selected_role.as_ref().map(|role| role.path.as_path()),
        );
        self.load_current_settings_for_current_role()?;
        self.reload_history()?;
        self.status = "reloaded roles and history".to_string();
        Ok(())
    }

    fn reload_history(&mut self) -> Result<()> {
        self.history = self.store.list_conversations(100)?;
        if self.history.is_empty() {
            self.history_state.select(None);
        } else if self.history_state.selected().is_none() {
            self.history_state.select(Some(0));
        } else {
            let current = self.history_state.selected().unwrap_or(0);
            self.history_state
                .select(Some(current.min(self.history.len().saturating_sub(1))));
        }
        Ok(())
    }

    fn load_selected_history(&mut self) -> Result<()> {
        let Some(index) = self.history_state.selected() else {
            return Ok(());
        };
        let Some(summary) = self.history.get(index) else {
            return Ok(());
        };
        let Some(record) = self.store.get_conversation(summary.id)? else {
            return Ok(());
        };

        self.apply_current_settings(CurrentSettings {
            tool: record.tool,
            model: record.model.clone(),
            effort: record.effort.clone(),
        });
        self.args_editor.set_text(record.extra_args.join(" "));
        self.prompt_editor.set_text(record.prompt.clone());
        self.output_chunks.clear();
        if !record.response.is_empty() {
            self.output_chunks.push(OutputChunk {
                source: StreamSource::Stdout,
                text: record.response,
            });
        }
        if !record.stderr_output.is_empty() {
            self.output_chunks.push(OutputChunk {
                source: StreamSource::Stderr,
                text: record.stderr_output,
            });
        }
        self.alert = record.usage_alert;
        self.status = format!(
            "loaded conversation #{} ({}, {} -> {}, exit {:?}, sa={}, tool={})",
            record.id,
            record.status,
            record.started_at.format("%Y-%m-%d %H:%M"),
            record
                .ended_at
                .map(|ended_at| ended_at.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".to_string()),
            record.exit_code,
            record.secretagent_conversation_id,
            record
                .tool_conversation_id
                .clone()
                .unwrap_or_else(|| "-".to_string())
        );
        self.role_index =
            self.resolve_role_index(record.role_name.as_deref(), record.role_path.as_deref());
        self.active_tool_conversation_id = record.tool_conversation_id;
        self.output_scroll = self.output_lines().len().saturating_sub(1);
        Ok(())
    }

    fn reset_draft(&mut self) -> Result<()> {
        if self.running.is_some() {
            self.status = "wait for the active run to finish before resetting".to_string();
            return Ok(());
        }

        self.args_editor.clear();
        self.prompt_editor.clear();
        self.output_chunks.clear();
        self.output_scroll = 0;
        self.alert = None;
        self.active_stdout.clear();
        self.active_stderr.clear();
        self.active_conversation_id = None;
        self.active_tool_conversation_id = None;
        self.load_current_settings_for_current_role()?;
        self.status = "draft reset".to_string();
        Ok(())
    }

    fn current_tool(&self) -> Option<ToolKind> {
        self.available_tools.get(self.tool_index).copied()
    }

    fn current_tool_label(&self) -> String {
        self.current_tool()
            .map(|tool| tool.to_string())
            .unwrap_or_else(|| "None detected".to_string())
    }

    fn current_role(&self) -> Option<RoleSpec> {
        if self.role_index == 0 {
            None
        } else {
            self.roles.get_index(self.role_index - 1)
        }
    }

    fn load_current_settings_for_current_role(&mut self) -> Result<()> {
        let role = self.current_role();
        let settings = self.store.current_settings(
            role.as_ref().map(|role| role.name.as_str()),
            role.as_ref().map(|role| role.path.as_path()),
        )?;
        self.apply_current_settings(settings);

        if let Some(role) = role {
            if self.args_editor.text().trim().is_empty() && !role.default_extra_args.is_empty() {
                self.args_editor.set_text(role.default_extra_args.join(" "));
            }
            self.status = self.current_settings_status_message();
        } else {
            self.status = self.current_settings_status_message();
        }
        Ok(())
    }

    fn apply_current_settings(&mut self, settings: CurrentSettings) {
        self.tool_index = self
            .available_tools
            .iter()
            .position(|tool| *tool == settings.tool)
            .unwrap_or(0);
        self.model_editor
            .set_text(settings.model.unwrap_or_default());
        self.effort_editor
            .set_text(settings.effort.unwrap_or_default());
    }

    fn persist_current_settings_for_current_role(&mut self) -> Result<bool> {
        let role = self.current_role();
        self.persist_current_settings(role.as_ref())
    }

    fn persist_current_settings(&mut self, role: Option<&RoleSpec>) -> Result<bool> {
        let Some(tool) = self.current_tool() else {
            self.status = no_tools_detected_message();
            return Ok(false);
        };
        let settings = CurrentSettings {
            tool,
            model: clean_editor(&self.model_editor),
            effort: clean_editor(&self.effort_editor),
        };
        if let Err(message) =
            models::validate_model_for_tool(settings.tool, settings.model.as_deref())
        {
            self.status = message;
            return Ok(false);
        }
        self.store.save_current_settings(
            role.map(|role| role.name.as_str()),
            role.map(|role| role.path.as_path()),
            &settings,
        )?;
        self.status = self.current_settings_status_message();
        Ok(true)
    }

    fn current_settings_status_message(&self) -> String {
        if self.available_tools.is_empty() {
            return no_tools_detected_message();
        }

        self.current_role()
            .and_then(|role| {
                role.description
                    .clone()
                    .or_else(|| Some(format!("role {} selected", role.name)))
            })
            .unwrap_or_else(|| {
                tool_notes(
                    self.current_tool()
                        .expect("tool selection should exist when available tools are present"),
                )
                .to_string()
            })
    }

    fn detect_tools(&mut self) -> Result<()> {
        self.available_tools = detect_available_tools();
        if self.tool_index >= self.available_tools.len() {
            self.tool_index = 0;
        }
        self.load_current_settings_for_current_role()?;
        let message = detected_tools_status_message(&self.available_tools);
        self.status = message.clone();
        self.push_output_message(StreamSource::Stdout, format!("{message}\n"));
        Ok(())
    }

    fn push_output_message(&mut self, source: StreamSource, text: String) {
        self.output_chunks.push(OutputChunk { source, text });
        self.output_scroll = self.output_lines().len().saturating_sub(1);
    }

    fn resolve_role_index(
        &self,
        role_name: Option<&str>,
        role_path: Option<&std::path::Path>,
    ) -> usize {
        if let Some(path) = role_path {
            if let Some(index) = self.roles.all().iter().position(|role| role.path == path) {
                return index + 1;
            }
        }
        if let Some(name) = role_name {
            if let Some(index) = self
                .roles
                .all()
                .iter()
                .position(|role| role.name.eq_ignore_ascii_case(name))
            {
                return index + 1;
            }
        }
        0
    }

    fn output_lines(&self) -> Vec<Line<'static>> {
        if self.output_chunks.is_empty() {
            return vec![Line::from("No output yet")];
        }

        let mut lines = Vec::new();
        for chunk in &self.output_chunks {
            let style = match chunk.source {
                StreamSource::Stdout => Style::default().fg(Color::White),
                StreamSource::Stderr => Style::default().fg(Color::Yellow),
            };

            for line in chunk.text.lines() {
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
            if chunk.text.ends_with('\n') {
                lines.push(Line::from(Span::styled(String::new(), style)));
            }
        }
        lines
    }
}

#[derive(Debug, Clone)]
struct OutputChunk {
    source: StreamSource,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
    action: SlashCommandAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlashCommandAction {
    DetectTools,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    History,
    Tool,
    Role,
    Model,
    Effort,
    ExtraArgs,
    Prompt,
    Output,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::History => Focus::Tool,
            Focus::Tool => Focus::Role,
            Focus::Role => Focus::Model,
            Focus::Model => Focus::Effort,
            Focus::Effort => Focus::ExtraArgs,
            Focus::ExtraArgs => Focus::Prompt,
            Focus::Prompt => Focus::Output,
            Focus::Output => Focus::History,
        }
    }

    fn previous(self) -> Self {
        match self {
            Focus::History => Focus::Output,
            Focus::Tool => Focus::History,
            Focus::Role => Focus::Tool,
            Focus::Model => Focus::Role,
            Focus::Effort => Focus::Model,
            Focus::ExtraArgs => Focus::Effort,
            Focus::Prompt => Focus::ExtraArgs,
            Focus::Output => Focus::Prompt,
        }
    }
}

fn render_value_box(frame: &mut Frame, area: Rect, title: &str, value: String, focused: bool) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_style(focused))
        .title(title);
    let paragraph = Paragraph::new(value).block(block);
    frame.render_widget(paragraph, area);
}

fn render_editor(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    focused: bool,
    editor: &mut TextEditor,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_style(focused))
        .title(title);
    let inner = block.inner(area);
    editor.ensure_visible(inner.width as usize, inner.height as usize);
    let paragraph =
        Paragraph::new(editor.visible_text(inner.width as usize, inner.height as usize))
            .block(block)
            .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);

    if focused {
        if let Some((x, y)) = editor.cursor_position(inner) {
            frame.set_cursor_position((x, y));
        }
    }
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

#[derive(Debug, Clone)]
struct TextEditor {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_row: usize,
    scroll_col: usize,
    multiline: bool,
}

impl TextEditor {
    fn single_line() -> Self {
        Self::new(false)
    }

    fn multi_line() -> Self {
        Self::new(true)
    }

    fn new(multiline: bool) -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            scroll_row: 0,
            scroll_col: 0,
            multiline,
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_row = 0;
        self.scroll_col = 0;
    }

    fn set_text(&mut self, text: String) {
        let lines: Vec<String> = text.split('\n').map(ToString::to_string).collect();
        self.lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };
        self.cursor_row = self.lines.len().saturating_sub(1);
        self.cursor_col = self.current_line_len();
        self.scroll_row = 0;
        self.scroll_col = 0;
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char(ch);
            }
            KeyCode::Backspace => {
                self.backspace();
            }
            KeyCode::Delete => {
                self.delete();
            }
            KeyCode::Enter if self.multiline => {
                self.insert_newline();
            }
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up if self.multiline => self.move_up(),
            KeyCode::Down if self.multiline => self.move_down(),
            KeyCode::Home => self.cursor_col = 0,
            KeyCode::End => self.cursor_col = self.current_line_len(),
            _ => {}
        }
    }

    fn ensure_visible(&mut self, width: usize, height: usize) {
        if self.cursor_row < self.scroll_row {
            self.scroll_row = self.cursor_row;
        }
        let view_height = height.max(1);
        if self.cursor_row >= self.scroll_row + view_height {
            self.scroll_row = self.cursor_row + 1 - view_height;
        }

        if self.cursor_col < self.scroll_col {
            self.scroll_col = self.cursor_col;
        }
        let view_width = width.max(1);
        if self.cursor_col >= self.scroll_col + view_width {
            self.scroll_col = self.cursor_col + 1 - view_width;
        }
    }

    fn visible_text(&self, width: usize, height: usize) -> Text<'static> {
        let mut lines = Vec::new();
        for row in self.scroll_row..(self.scroll_row + height.max(1)) {
            let line = self.lines.get(row).cloned().unwrap_or_default();
            lines.push(Line::from(slice_chars(
                &line,
                self.scroll_col,
                width.max(1),
            )));
        }
        Text::from(lines)
    }

    fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        let row = self.cursor_row.saturating_sub(self.scroll_row);
        let col = self.cursor_col.saturating_sub(self.scroll_col);
        if row >= area.height as usize || col >= area.width as usize {
            return None;
        }
        Some((area.x + col as u16, area.y + row as u16))
    }

    fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cursor_row];
        let index = char_to_byte_index(line, self.cursor_col);
        line.insert(index, ch);
        self.cursor_col += 1;
    }

    fn insert_newline(&mut self) {
        let line = &mut self.lines[self.cursor_row];
        let split = char_to_byte_index(line, self.cursor_col);
        let trailing = line.split_off(split);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.lines.insert(self.cursor_row, trailing);
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let end = char_to_byte_index(line, self.cursor_col);
            let start = char_to_byte_index(line, self.cursor_col - 1);
            line.replace_range(start..end, "");
            self.cursor_col -= 1;
        } else if self.multiline && self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        let line_len = self.current_line_len();
        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_row];
            let start = char_to_byte_index(line, self.cursor_col);
            let end = char_to_byte_index(line, self.cursor_col + 1);
            line.replace_range(start..end, "");
        } else if self.multiline && self.cursor_row + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.multiline && self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.current_line_len();
        }
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.current_line_len() {
            self.cursor_col += 1;
        } else if self.multiline && self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = self.cursor_col.min(self.current_line_len());
        }
    }

    fn current_line_len(&self) -> usize {
        self.lines
            .get(self.cursor_row)
            .map(|line| line.chars().count())
            .unwrap_or(0)
    }
}

fn char_to_byte_index(input: &str, char_index: usize) -> usize {
    input
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or_else(|| input.len())
}

fn slice_chars(input: &str, start: usize, width: usize) -> String {
    input.chars().skip(start).take(width).collect()
}

fn clean_editor(editor: &TextEditor) -> Option<String> {
    let text = editor.text();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn slash_command_matches(prompt: &str) -> Option<Vec<SlashCommand>> {
    let query = slash_command_query(prompt)?;
    Some(
        SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|command| command.name.starts_with(query))
            .collect(),
    )
}

fn slash_command_action(prompt: &str) -> Option<SlashCommandAction> {
    let query = prompt.trim();
    SLASH_COMMANDS
        .iter()
        .find(|command| command.name == query)
        .map(|command| command.action)
}

fn slash_command_query(prompt: &str) -> Option<&str> {
    let first_line = prompt.lines().next().unwrap_or(prompt);
    if !first_line.starts_with('/') {
        return None;
    }
    Some(first_line.split_whitespace().next().unwrap_or(first_line))
}

fn detect_available_tools() -> Vec<ToolKind> {
    ToolKind::ALL
        .iter()
        .copied()
        .filter(|tool| which(tool.command_name()).is_ok())
        .collect()
}

fn no_tools_detected_message() -> String {
    "no supported tools detected; install one and run /detect-tools".to_string()
}

fn detected_tools_status_message(tools: &[ToolKind]) -> String {
    if tools.is_empty() {
        return no_tools_detected_message();
    }

    format!(
        "detected tools: {}",
        tools
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn shorten_id(value: &str) -> String {
    value.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::Path;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{
        App, SlashCommandAction, detected_tools_status_message, slash_command_action,
        slash_command_matches,
    };
    use crate::paths::AppPaths;
    use crate::roles::RoleLibrary;
    use crate::storage::Store;

    fn temp_db_path(test_name: &str) -> std::path::PathBuf {
        env::temp_dir().join(format!(
            "secret-agent-tui-{test_name}-{}.sqlite",
            uuid::Uuid::now_v7()
        ))
    }

    fn remove_db(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = fs::remove_file(path.with_extension("sqlite-shm"));
    }

    fn test_app(test_name: &str) -> (App, std::path::PathBuf) {
        let db_path = temp_db_path(test_name);
        let store = Store::open(&db_path).expect("db should open");
        let temp_dir = env::temp_dir();
        let app = App::new(
            AppPaths {
                cwd: temp_dir.clone(),
                config_dir: temp_dir.clone(),
                db_path: db_path.clone(),
                role_dirs: vec![temp_dir],
            },
            store,
            RoleLibrary::default(),
        )
        .expect("app should build");
        (app, db_path)
    }

    #[test]
    fn detects_exit_slash_command() {
        assert_eq!(
            slash_command_action("/exit"),
            Some(SlashCommandAction::Exit)
        );
        assert_eq!(
            slash_command_action("  /exit  "),
            Some(SlashCommandAction::Exit)
        );
    }

    #[test]
    fn detects_quit_slash_command() {
        assert_eq!(
            slash_command_action("/quit"),
            Some(SlashCommandAction::Exit)
        );
        assert_eq!(
            slash_command_action("\n/quit\n"),
            Some(SlashCommandAction::Exit)
        );
    }

    #[test]
    fn detects_detect_tools_slash_command() {
        assert_eq!(
            slash_command_action("/detect-tools"),
            Some(SlashCommandAction::DetectTools)
        );
    }

    #[test]
    fn ignores_other_prompt_text() {
        assert_eq!(slash_command_action(""), None);
        assert_eq!(slash_command_action("/quit now"), None);
        assert_eq!(slash_command_action("please /exit"), None);
    }

    #[test]
    fn lists_slash_commands_for_a_bare_slash() {
        let names: Vec<_> = slash_command_matches("/")
            .unwrap()
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert_eq!(names, vec!["/detect-tools", "/exit", "/quit"]);
    }

    #[test]
    fn narrows_slash_commands_by_prefix() {
        let names: Vec<_> = slash_command_matches("/d")
            .unwrap()
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert_eq!(names, vec!["/detect-tools"]);
    }

    #[test]
    fn keeps_slash_mode_active_when_no_commands_match() {
        let names: Vec<_> = slash_command_matches("/zzz")
            .unwrap()
            .into_iter()
            .map(|command| command.name)
            .collect();
        assert!(names.is_empty());
    }

    #[test]
    fn ignores_prompts_that_do_not_start_with_a_slash() {
        assert_eq!(slash_command_matches("status"), None);
    }

    #[test]
    fn detect_tools_writes_detected_tools_to_output() {
        let (mut app, db_path) = test_app("slash-detect-tools-output");
        app.prompt_editor.set_text("/detect-tools".to_string());

        app.run_slash_command(SlashCommandAction::DetectTools);

        let last = app
            .output_chunks
            .last()
            .expect("detect tools should write output");
        assert_eq!(
            last.text,
            format!("{}\n", detected_tools_status_message(&app.available_tools))
        );
        assert_eq!(app.prompt_editor.text(), "");
        remove_db(&db_path);
    }

    #[test]
    fn pressing_enter_executes_exact_slash_command() {
        let (mut app, db_path) = test_app("slash-enter-exit");
        app.prompt_editor.set_text("/exit".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("enter should be handled");

        assert!(app.should_quit);
        assert_eq!(app.prompt_editor.text(), "");
        remove_db(&db_path);
    }

    #[test]
    fn pressing_ctrl_enter_submits_the_prompt() {
        let (mut app, db_path) = test_app("slash-ctrl-enter-exit");
        app.prompt_editor.set_text("/quit".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
            .expect("ctrl-enter should be handled");

        assert!(app.should_quit);
        assert_eq!(app.prompt_editor.text(), "");
        remove_db(&db_path);
    }

    #[test]
    fn pressing_ctrl_j_submits_the_prompt() {
        let (mut app, db_path) = test_app("slash-ctrl-j-exit");
        app.prompt_editor.set_text("/quit".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .expect("ctrl-j should be handled");

        assert!(app.should_quit);
        assert_eq!(app.prompt_editor.text(), "");
        remove_db(&db_path);
    }

    #[test]
    fn pressing_enter_keeps_normal_prompt_editing_behavior() {
        let (mut app, db_path) = test_app("prompt-enter-newline");
        app.prompt_editor.set_text("hello".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("enter should be handled");

        assert_eq!(app.prompt_editor.text(), "hello\n");
        assert!(!app.should_quit);
        remove_db(&db_path);
    }
}
