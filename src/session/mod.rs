mod agent;

use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use self::agent::{AgentEvent, AgentProcess};
use crate::accounts::{AccountProvider, AccountStore, ONBOARDING_MODEL, SESSION_PROFILE_NAME};
use crate::config::ClaudexConfig;

const DEFAULT_EFFORT: &str = "high";
const MAX_TRANSCRIPT_ITEMS: usize = 800;
const MAX_TRANSCRIPT_ITEM_BYTES: usize = 512 * 1024;
const MAX_TRANSCRIPT_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptKind {
    User,
    Assistant,
    Tool,
    Status,
    Error,
}

#[derive(Debug, Clone)]
struct TranscriptItem {
    kind: TranscriptKind,
    text: String,
}

#[derive(Debug, Clone)]
enum PickerItem {
    Model {
        id: String,
        provider: String,
        is_default: bool,
    },
    Authenticate,
}

#[derive(Debug, Clone)]
struct PermissionPrompt {
    request_id: String,
    tool_use_id: String,
    tool_name: String,
    description: String,
    input: Value,
    selected: usize,
}

#[derive(Debug, Clone)]
struct QuestionPrompt {
    request_id: String,
    tool_use_id: String,
    input: Value,
    question_index: usize,
    selected: usize,
    checked: Vec<Vec<bool>>,
    answers: Map<String, Value>,
    custom_input: Option<String>,
}

#[derive(Debug, Clone)]
enum Popup {
    Models { selected: usize },
    Providers { selected: usize },
    Effort { selected: usize },
    Permission(PermissionPrompt),
    Question(QuestionPrompt),
}

#[derive(Debug)]
enum SessionAction {
    None,
    Quit,
    RestartHarness,
    FetchUsage,
}

struct SessionApp {
    input: String,
    cursor: usize,
    transcript: Vec<TranscriptItem>,
    stream_item: Option<usize>,
    seen_tools: HashSet<String>,
    popup: Option<Popup>,
    store: AccountStore,
    current_model: String,
    effort: String,
    permission_mode: String,
    session_id: Option<String>,
    initialized: bool,
    busy: bool,
    should_quit: bool,
    notice: Option<String>,
    manual_scroll: u16,
}

impl SessionApp {
    fn new(store: AccountStore) -> Self {
        let current_model = store
            .default_model
            .clone()
            .unwrap_or_else(|| ONBOARDING_MODEL.to_string());
        Self {
            input: String::new(),
            cursor: 0,
            transcript: Vec::new(),
            stream_item: None,
            seen_tools: HashSet::new(),
            popup: None,
            store,
            current_model,
            effort: DEFAULT_EFFORT.to_string(),
            permission_mode: "default".to_string(),
            session_id: None,
            initialized: false,
            busy: false,
            should_quit: false,
            notice: None,
            manual_scroll: 0,
        }
    }

    fn header_status(&self) -> String {
        if self.store.accounts.is_empty() {
            "/model to authenticate".to_string()
        } else {
            format!(
                "{} · {} effort",
                self.current_model,
                effort_display_name(&self.effort)
            )
        }
    }

    fn append(&mut self, kind: TranscriptKind, text: impl Into<String>) {
        self.transcript.push(TranscriptItem {
            kind,
            text: bounded_text(text.into(), MAX_TRANSCRIPT_ITEM_BYTES),
        });
        self.trim_transcript();
        self.manual_scroll = 0;
    }

    fn append_stream(&mut self, text: &str) {
        let index = match self.stream_item {
            Some(index) if index < self.transcript.len() => index,
            _ => {
                self.transcript.push(TranscriptItem {
                    kind: TranscriptKind::Assistant,
                    text: String::new(),
                });
                let index = self.transcript.len() - 1;
                self.stream_item = Some(index);
                index
            }
        };
        let current_len = self.transcript[index].text.len();
        if current_len < MAX_TRANSCRIPT_ITEM_BYTES {
            let remaining = MAX_TRANSCRIPT_ITEM_BYTES - current_len;
            let prefix = utf8_prefix(text, remaining);
            self.transcript[index].text.push_str(prefix);
            if prefix.len() < text.len() {
                self.transcript[index]
                    .text
                    .push_str("\n… [terminal display truncated; full session remains on disk]");
            }
        }
        self.manual_scroll = 0;
    }

    fn trim_transcript(&mut self) {
        let mut total_bytes: usize = self.transcript.iter().map(|item| item.text.len()).sum();
        while self.transcript.len() > MAX_TRANSCRIPT_ITEMS
            || total_bytes > MAX_TRANSCRIPT_TOTAL_BYTES
        {
            let removed = self.transcript.remove(0);
            total_bytes = total_bytes.saturating_sub(removed.text.len());
            self.stream_item = self.stream_item.and_then(|index| index.checked_sub(1));
        }
    }

    fn model_items(&self) -> Vec<PickerItem> {
        let mut items = Vec::new();
        for account in &self.store.accounts {
            for model in &account.models {
                items.push(PickerItem::Model {
                    id: model.clone(),
                    provider: account.provider.label().to_string(),
                    is_default: self.store.default_model.as_deref() == Some(model.as_str()),
                });
            }
        }
        items.push(PickerItem::Authenticate);
        items
    }

    fn open_model_picker(&mut self) {
        self.popup = if self.store.accounts.is_empty() {
            Some(Popup::Providers { selected: 0 })
        } else {
            let selected = self
                .model_items()
                .iter()
                .position(|item| {
                    matches!(item, PickerItem::Model { id, .. } if id == &self.current_model)
                })
                .unwrap_or(0);
            Some(Popup::Models { selected })
        };
    }

    fn refresh_store(&mut self, config: &mut ClaudexConfig, agent: &AgentProcess) -> Result<bool> {
        let latest = AccountStore::load()?;
        if latest == self.store {
            return Ok(false);
        }

        let previously_empty = self.store.accounts.is_empty();
        self.store = latest;
        crate::accounts::apply_store_to_config(config, &self.store);
        crate::integration::sync_account_skills(&self.store)?;

        if !self.store.has_model(&self.current_model) {
            self.current_model = self
                .store
                .default_model
                .clone()
                .unwrap_or_else(|| ONBOARDING_MODEL.to_string());
            agent.set_model(&self.current_model)?;
        } else if previously_empty {
            if let Some(default) = self.store.default_model.clone() {
                self.current_model = default;
                agent.set_model(&self.current_model)?;
            }
        }

        if previously_empty && !self.store.accounts.is_empty() {
            self.notice = Some(format!("Connected. Default model: {}", self.current_model));
            self.popup = Some(Popup::Models { selected: 0 });
        }
        Ok(true)
    }

    fn handle_agent_event(&mut self, event: AgentEvent) -> SessionAction {
        match event {
            AgentEvent::Message(message) => self.handle_protocol_message(message),
            AgentEvent::Stderr(line) => {
                // Stderr is intentionally captured so configuration warnings
                // cannot corrupt the terminal renderer. Surface it inside the
                // transcript where it remains readable and bounded.
                self.append(TranscriptKind::Error, format!("Harness: {line}"));
                SessionAction::None
            }
            AgentEvent::ProtocolError(error) => {
                self.append(TranscriptKind::Error, error);
                SessionAction::None
            }
            AgentEvent::Exited(status) => {
                self.initialized = false;
                self.busy = false;
                self.append(
                    TranscriptKind::Status,
                    format!("Agent harness exited ({status}); restarting locally…"),
                );
                SessionAction::RestartHarness
            }
        }
    }

    fn handle_protocol_message(&mut self, message: Value) -> SessionAction {
        match message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "system" => self.handle_system_message(&message),
            "stream_event" => {
                let event = &message["event"];
                if event.get("type").and_then(Value::as_str) == Some("content_block_delta") {
                    if let Some(text) = event
                        .get("delta")
                        .and_then(|delta| delta.get("text"))
                        .and_then(Value::as_str)
                    {
                        self.append_stream(text);
                    }
                }
                SessionAction::None
            }
            "assistant" => {
                self.handle_assistant_message(&message);
                SessionAction::None
            }
            "control_request" => {
                self.handle_control_request(&message);
                SessionAction::None
            }
            "result" => {
                self.busy = false;
                self.stream_item = None;
                self.seen_tools.clear();
                if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                    let error = message
                        .get("errors")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "The agent turn failed.".to_string());
                    self.append(TranscriptKind::Error, error);
                }
                SessionAction::None
            }
            _ => SessionAction::None,
        }
    }

    fn handle_system_message(&mut self, message: &Value) -> SessionAction {
        match message
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "init" => {
                self.initialized = true;
                self.session_id = message
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| self.session_id.clone());
                if let Some(mode) = message.get("permissionMode").and_then(Value::as_str) {
                    self.permission_mode = mode.to_string();
                }
            }
            "task_started" => {
                let description = message
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("Subagent started");
                self.append(TranscriptKind::Tool, format!("Subagent: {description}"));
            }
            "task_progress" => {
                if let Some(summary) = message.get("summary").and_then(Value::as_str) {
                    self.notice = Some(format!("Subagent: {summary}"));
                }
            }
            "task_notification" => {
                let summary = message
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("Subagent finished");
                self.append(TranscriptKind::Tool, summary);
            }
            "api_retry" => {
                let attempt = message.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                self.notice = Some(format!("Provider retry {attempt}…"));
            }
            "compact_boundary" => {
                self.append(TranscriptKind::Status, "Context compacted.");
            }
            "permission_denied" => {
                if let Some(text) = message.get("message").and_then(Value::as_str) {
                    self.append(TranscriptKind::Error, text);
                }
            }
            _ => {}
        }
        SessionAction::None
    }

    fn handle_assistant_message(&mut self, message: &Value) {
        let parent = message.get("parent_tool_use_id").and_then(Value::as_str);
        let Some(content) = message
            .pointer("/message/content")
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    if !id.is_empty() && !self.seen_tools.insert(id.to_string()) {
                        continue;
                    }
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
                    let description = tool_description(name, block.get("input"));
                    self.append(TranscriptKind::Tool, description);
                }
                Some("text") if parent.is_some() => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.append(TranscriptKind::Tool, format!("Subagent: {text}"));
                    }
                }
                Some("text") if self.stream_item.is_none() => {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        self.append(TranscriptKind::Assistant, text);
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_control_request(&mut self, message: &Value) {
        let request_id = message
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let request = &message["request"];
        if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
            return;
        }
        let tool_use_id = request
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let tool_name = request
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("Tool")
            .to_string();
        let input = request.get("input").cloned().unwrap_or_else(|| json!({}));

        if tool_name == "AskUserQuestion" {
            let question_count = input
                .get("questions")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if question_count > 0 {
                let checked = (0..question_count)
                    .map(|index| {
                        input["questions"][index]["options"]
                            .as_array()
                            .map_or_else(Vec::new, |options| vec![false; options.len()])
                    })
                    .collect();
                self.popup = Some(Popup::Question(QuestionPrompt {
                    request_id,
                    tool_use_id,
                    input,
                    question_index: 0,
                    selected: 0,
                    checked,
                    answers: Map::new(),
                    custom_input: None,
                }));
                return;
            }
        }

        self.popup = Some(Popup::Permission(PermissionPrompt {
            request_id,
            tool_use_id,
            tool_name,
            description: request
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input,
            selected: 0,
        }));
    }
}

pub async fn run_session(config: &mut ClaudexConfig) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("the Joey's Claudex session UI requires an interactive terminal");
    }

    let store = AccountStore::load()?;
    crate::accounts::apply_store_to_config(config, &store);
    let fast_session = crate::fast::FastSession::create()?;
    let mut app = SessionApp::new(store);
    let (mut agent, mut agent_events) = spawn_agent(config, &app, &fast_session, None).await?;
    let (async_tx, mut async_rx) = mpsc::unbounded_channel::<Result<String, String>>();

    let mut terminal = enter_terminal()?;
    let _guard = TerminalGuard;
    let mut events = EventStream::new();
    let mut render_tick = tokio::time::interval(Duration::from_millis(33));
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut account_tick = tokio::time::interval(Duration::from_millis(500));
    account_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    terminal.draw(|frame| draw(frame, &mut app))?;
    let mut needs_render = false;

    loop {
        if app.should_quit {
            break;
        }

        let action = tokio::select! {
            event = events.next() => {
                needs_render = true;
                match event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key, &agent, config, &fast_session)?
                    }
                    Some(Ok(Event::Paste(text))) => {
                        insert_text(&mut app.input, &mut app.cursor, &text);
                        SessionAction::None
                    }
                    Some(Err(error)) => return Err(error.into()),
                    _ => SessionAction::None,
                }
            }
            event = agent_events.recv() => {
                needs_render = true;
                event.map_or(SessionAction::RestartHarness, |event| app.handle_agent_event(event))
            }
            result = async_rx.recv() => {
                needs_render = true;
                if let Some(result) = result {
                    match result {
                        Ok(text) => app.append(TranscriptKind::Status, text),
                        Err(error) => app.append(TranscriptKind::Error, error),
                    }
                }
                SessionAction::None
            }
            _ = account_tick.tick() => {
                match app.refresh_store(config, &agent) {
                    Ok(changed) => needs_render |= changed,
                    Err(error) => {
                        app.notice = Some(format!("Account refresh failed: {error}"));
                        needs_render = true;
                    }
                }
                SessionAction::None
            }
            _ = render_tick.tick() => {
                if needs_render {
                    terminal.draw(|frame| draw(frame, &mut app))?;
                    needs_render = false;
                }
                SessionAction::None
            }
        };

        match action {
            SessionAction::None => {}
            SessionAction::Quit => app.should_quit = true,
            SessionAction::RestartHarness => {
                let resume = app.session_id.clone();
                app.initialized = false;
                agent.terminate();
                tokio::time::sleep(Duration::from_millis(75)).await;
                let replacement = spawn_agent(config, &app, &fast_session, resume.as_deref()).await;
                match replacement {
                    Ok((new_agent, new_events)) => {
                        agent = new_agent;
                        agent_events = new_events;
                    }
                    Err(error) => {
                        return Err(error).context("failed to restart the local agent harness");
                    }
                }
            }
            SessionAction::FetchUsage => {
                let mut usage_config = config.clone();
                let sender = async_tx.clone();
                tokio::spawn(async move {
                    let result = crate::openai::subscription_usage(&mut usage_config)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = sender.send(result);
                });
            }
        }
    }

    drop(agent);
    terminal.show_cursor()?;
    Ok(())
}

async fn spawn_agent(
    config: &ClaudexConfig,
    app: &SessionApp,
    fast_session: &crate::fast::FastSession,
    resume: Option<&str>,
) -> Result<(AgentProcess, mpsc::Receiver<AgentEvent>)> {
    let profile = config
        .find_profile(SESSION_PROFILE_NAME)
        .cloned()
        .context("runtime Claudex session profile is missing")?;
    AgentProcess::spawn(
        config,
        &profile,
        &app.current_model,
        &app.effort,
        fast_session.id(),
        resume,
    )
    .await
}

fn handle_key(
    app: &mut SessionApp,
    key: KeyEvent,
    agent: &AgentProcess,
    config: &mut ClaudexConfig,
    fast_session: &crate::fast::FastSession,
) -> Result<SessionAction> {
    if app.popup.is_some() {
        return handle_popup_key(app, key, agent, config);
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.busy {
            agent.interrupt()?;
            app.notice = Some("Interrupt requested…".to_string());
            return Ok(SessionAction::None);
        }
        return Ok(SessionAction::Quit);
    }

    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            insert_text(&mut app.input, &mut app.cursor, "\n");
        }
        KeyCode::Enter => {
            let text = app.input.trim().to_string();
            app.input.clear();
            app.cursor = 0;
            if text.is_empty() {
                return Ok(SessionAction::None);
            }
            if text.starts_with('/') {
                return handle_command(app, &text, agent, config, fast_session);
            }
            if app.store.accounts.is_empty() {
                app.append(TranscriptKind::User, text);
                app.append(
                    TranscriptKind::Status,
                    "No model is connected. Use /model to authenticate an LLM provider.",
                );
                app.open_model_picker();
                return Ok(SessionAction::None);
            }
            app.append(TranscriptKind::User, text.clone());
            app.stream_item = None;
            app.busy = true;
            agent.send_user_message(&text)?;
        }
        KeyCode::Backspace => delete_previous(&mut app.input, &mut app.cursor),
        KeyCode::Delete => delete_next(&mut app.input, &mut app.cursor),
        KeyCode::Left => move_left(&app.input, &mut app.cursor),
        KeyCode::Right => move_right(&app.input, &mut app.cursor),
        KeyCode::Home => app.cursor = 0,
        KeyCode::End => app.cursor = app.input.len(),
        KeyCode::PageUp => app.manual_scroll = app.manual_scroll.saturating_add(8),
        KeyCode::PageDown => app.manual_scroll = app.manual_scroll.saturating_sub(8),
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            insert_text(&mut app.input, &mut app.cursor, &character.to_string());
        }
        _ => {}
    }
    Ok(SessionAction::None)
}

fn handle_command(
    app: &mut SessionApp,
    command: &str,
    agent: &AgentProcess,
    _config: &mut ClaudexConfig,
    fast_session: &crate::fast::FastSession,
) -> Result<SessionAction> {
    let mut parts = command.split_whitespace();
    let name = parts.next().unwrap_or_default().to_ascii_lowercase();
    match name.as_str() {
        "/model" | "/models" => {
            if let Some(model) = parts.next() {
                select_model(app, model, agent)?;
            } else {
                app.open_model_picker();
            }
        }
        "/effort" => {
            let levels = effort_levels();
            let selected = levels
                .iter()
                .position(|(_, value)| value == &app.effort)
                .unwrap_or(2);
            app.popup = Some(Popup::Effort { selected });
        }
        "/permissions" | "/permission" => {
            app.append(
                TranscriptKind::Status,
                format!("Permission mode: {}", app.permission_mode),
            );
        }
        "/fast" => {
            let availability = crate::fast::FastAvailability::from_store(&app.store);
            if !availability.any() {
                app.append(
                    TranscriptKind::Error,
                    "/fast requires a connected OpenAI subscription or supported Anthropic model.",
                );
            } else {
                let enabled = fast_session.toggle()?;
                app.append(
                    TranscriptKind::Status,
                    if enabled {
                        "Fast mode ON — the active provider route chooses its supported priority path."
                    } else {
                        "Fast mode OFF — provider routes use standard service."
                    },
                );
            }
        }
        "/usage" => {
            if app.store.has_provider(AccountProvider::Openai) {
                app.append(TranscriptKind::Status, "Fetching live OpenAI usage…");
                return Ok(SessionAction::FetchUsage);
            }
            app.append(
                TranscriptKind::Error,
                "/usage appears only for a connected OpenAI subscription.",
            );
        }
        "/clear" => {
            app.transcript.clear();
            app.stream_item = None;
        }
        "/exit" | "/quit" => return Ok(SessionAction::Quit),
        "/help" => {
            let help = command_help(&app.store);
            app.append(TranscriptKind::Status, help);
        }
        _ => {
            if app.store.accounts.is_empty() {
                app.open_model_picker();
            } else {
                app.append(TranscriptKind::User, command);
                app.busy = true;
                agent.send_user_message(command)?;
            }
        }
    }
    Ok(SessionAction::None)
}

fn handle_popup_key(
    app: &mut SessionApp,
    key: KeyEvent,
    agent: &AgentProcess,
    config: &mut ClaudexConfig,
) -> Result<SessionAction> {
    let Some(mut popup) = app.popup.take() else {
        return Ok(SessionAction::None);
    };
    match &mut popup {
        Popup::Models { selected } => {
            let count = app.model_items().len();
            match key.code {
                KeyCode::Esc => return Ok(SessionAction::None),
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(count.saturating_sub(1)),
                KeyCode::Enter => {
                    if let Some(item) = app.model_items().get(*selected).cloned() {
                        match item {
                            PickerItem::Model { id, .. } => {
                                select_model(app, &id, agent)?;
                                return Ok(SessionAction::None);
                            }
                            PickerItem::Authenticate => {
                                app.popup = Some(Popup::Providers { selected: 0 });
                                return Ok(SessionAction::None);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Popup::Providers { selected } => match key.code {
            KeyCode::Esc => return Ok(SessionAction::None),
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(1),
            KeyCode::Enter => {
                let provider = if *selected == 0 {
                    "openai"
                } else {
                    "anthropic"
                };
                crate::integration::open_provider_manager(config, provider)?;
                app.notice = Some(format!(
                    "{} authentication opened in your browser. This list updates automatically.",
                    if provider == "openai" {
                        "OpenAI"
                    } else {
                        "Anthropic"
                    }
                ));
                app.popup = Some(Popup::Models { selected: 0 });
                return Ok(SessionAction::None);
            }
            _ => {}
        },
        Popup::Effort { selected } => {
            let levels = effort_levels();
            match key.code {
                KeyCode::Esc => return Ok(SessionAction::None),
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(levels.len() - 1),
                KeyCode::Enter => {
                    app.effort = levels[*selected].1.to_string();
                    app.notice = Some(format!(
                        "Effort set to {}; restarting the local harness without losing the session…",
                        levels[*selected].0
                    ));
                    return Ok(SessionAction::RestartHarness);
                }
                _ => {}
            }
        }
        Popup::Permission(prompt) => match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                agent.respond_to_permission(
                    &prompt.request_id,
                    &prompt.tool_use_id,
                    "deny",
                    &prompt.input,
                    None,
                )?;
                return Ok(SessionAction::None);
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Tab => {
                prompt.selected = 1 - prompt.selected;
            }
            KeyCode::Char('y') => {
                agent.respond_to_permission(
                    &prompt.request_id,
                    &prompt.tool_use_id,
                    "allow",
                    &prompt.input,
                    None,
                )?;
                return Ok(SessionAction::None);
            }
            KeyCode::Enter => {
                let allow = prompt.selected == 0;
                agent.respond_to_permission(
                    &prompt.request_id,
                    &prompt.tool_use_id,
                    if allow { "allow" } else { "deny" },
                    &prompt.input,
                    None,
                )?;
                return Ok(SessionAction::None);
            }
            _ => {}
        },
        Popup::Question(prompt) => {
            if handle_question_key(prompt, key, agent)? {
                return Ok(SessionAction::None);
            }
        }
    }
    app.popup = Some(popup);
    Ok(SessionAction::None)
}

fn handle_question_key(
    prompt: &mut QuestionPrompt,
    key: KeyEvent,
    agent: &AgentProcess,
) -> Result<bool> {
    if let Some(custom) = &mut prompt.custom_input {
        match key.code {
            KeyCode::Esc => prompt.custom_input = None,
            KeyCode::Backspace => {
                custom.pop();
            }
            KeyCode::Enter => {
                let answer = custom.trim().to_string();
                prompt.custom_input = None;
                if !answer.is_empty() {
                    return finish_question(prompt, answer, agent);
                }
            }
            KeyCode::Char(character) => custom.push(character),
            _ => {}
        }
        return Ok(false);
    }

    let options = question_options(prompt);
    let count = options.len();
    match key.code {
        KeyCode::Esc => {
            agent.respond_to_permission(
                &prompt.request_id,
                &prompt.tool_use_id,
                "deny",
                &prompt.input,
                Some("User cancelled the question"),
            )?;
            return Ok(true);
        }
        KeyCode::Up => prompt.selected = prompt.selected.saturating_sub(1),
        KeyCode::Down => prompt.selected = (prompt.selected + 1).min(count.saturating_sub(1)),
        KeyCode::Char('o') | KeyCode::Char('O') => {
            prompt.custom_input = Some(String::new());
        }
        KeyCode::Char(' ') if question_multi_select(prompt) => {
            if let Some(checked) = prompt.checked.get_mut(prompt.question_index) {
                if let Some(value) = checked.get_mut(prompt.selected) {
                    *value = !*value;
                }
            }
        }
        KeyCode::Enter => {
            let answer = if question_multi_select(prompt) {
                prompt
                    .checked
                    .get(prompt.question_index)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, checked)| **checked)
                    .filter_map(|(index, _)| options.get(index).cloned())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                options.get(prompt.selected).cloned().unwrap_or_default()
            };
            if !answer.is_empty() {
                return finish_question(prompt, answer, agent);
            }
        }
        _ => {}
    }
    Ok(false)
}

fn finish_question(
    prompt: &mut QuestionPrompt,
    answer: String,
    agent: &AgentProcess,
) -> Result<bool> {
    let question = prompt.input["questions"][prompt.question_index]["question"]
        .as_str()
        .unwrap_or("Question")
        .to_string();
    prompt.answers.insert(question, Value::String(answer));
    prompt.question_index += 1;
    prompt.selected = 0;
    let count = prompt.input["questions"].as_array().map_or(0, Vec::len);
    if prompt.question_index < count {
        return Ok(false);
    }

    let mut updated = prompt.input.clone();
    updated["answers"] = Value::Object(prompt.answers.clone());
    agent.respond_to_permission(
        &prompt.request_id,
        &prompt.tool_use_id,
        "allow",
        &updated,
        None,
    )?;
    Ok(true)
}

fn select_model(app: &mut SessionApp, model: &str, agent: &AgentProcess) -> Result<()> {
    if !app.store.has_model(model) {
        anyhow::bail!("model '{model}' is not connected; use /model to authenticate a provider");
    }
    app.store.set_default_model(model)?;
    app.store.save()?;
    app.current_model = model.to_string();
    agent.set_model(model)?;
    app.notice = Some(format!("Using {model}"));
    Ok(())
}

fn question_options(prompt: &QuestionPrompt) -> Vec<String> {
    prompt.input["questions"][prompt.question_index]["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|option| option.get("label").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn question_multi_select(prompt: &QuestionPrompt) -> bool {
    prompt.input["questions"][prompt.question_index]["multiSelect"]
        .as_bool()
        .unwrap_or(false)
}

fn draw(frame: &mut Frame<'_>, app: &mut SessionApp) {
    let area = frame.area();
    let input_height = (app.input.lines().count().min(5) as u16 + 2).clamp(3, 7);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(frame, app, layout[0]);
    draw_transcript(frame, app, layout[1]);
    draw_input(frame, app, layout[2]);
    draw_footer(frame, app, layout[3]);
    if let Some(popup) = &app.popup {
        draw_popup(frame, app, popup, area);
    } else {
        let (column, row) = input_cursor_position(app, layout[2]);
        frame.set_cursor_position(Position::new(column, row));
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &SessionApp, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            " Joey's Claudex ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("v{} ", env!("CARGO_PKG_VERSION")),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    let status = app.header_status();
    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled("  ", Style::default()),
        Span::styled(
            status,
            Style::default().fg(if app.store.accounts.is_empty() {
                Color::Yellow
            } else {
                Color::White
            }),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(paragraph, area);
}

fn draw_transcript(frame: &mut Frame<'_>, app: &SessionApp, area: Rect) {
    let mut lines = Vec::new();
    for item in &app.transcript {
        let (prefix, color) = match item.kind {
            TranscriptKind::User => ("You", Color::Cyan),
            TranscriptKind::Assistant => ("Claudex", Color::Green),
            TranscriptKind::Tool => ("Agent", Color::Magenta),
            TranscriptKind::Status => ("Status", Color::Yellow),
            TranscriptKind::Error => ("Error", Color::Red),
        };
        lines.push(Line::from(Span::styled(
            prefix,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        if item.text.is_empty() {
            lines.push(Line::from(Span::styled("…", Style::default().fg(color))));
        } else {
            for line in item.text.lines() {
                lines.push(Line::from(Span::raw(line)));
            }
        }
        lines.push(Line::default());
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            if app.store.accounts.is_empty() {
                "Type /model to connect your first LLM provider."
            } else {
                "Ask anything about this workspace."
            },
            Style::default().fg(Color::DarkGray),
        )));
    }

    let visible = area.height.saturating_sub(2) as usize;
    let total = approximate_wrapped_lines(&lines, area.width.saturating_sub(2) as usize);
    let bottom = total.saturating_sub(visible).min(u16::MAX as usize) as u16;
    let scroll = bottom.saturating_sub(app.manual_scroll);
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn draw_input(frame: &mut Frame<'_>, app: &SessionApp, area: Rect) {
    let title = if app.busy { " Working " } else { " Message " };
    let paragraph = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame<'_>, app: &SessionApp, area: Rect) {
    let text = app.notice.as_deref().unwrap_or(if app.initialized {
        "Enter send · Shift+Enter newline · /model models · Ctrl+C interrupt/exit"
    } else {
        "Starting local agent harness…"
    });
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center),
        area,
    );
}

fn draw_popup(frame: &mut Frame<'_>, app: &SessionApp, popup: &Popup, area: Rect) {
    match popup {
        Popup::Models { selected } => {
            let items = app.model_items();
            let rows = items
                .iter()
                .map(|item| match item {
                    PickerItem::Model {
                        id,
                        provider,
                        is_default,
                    } => ListItem::new(Line::from(vec![
                        Span::raw(id.clone()),
                        Span::styled(
                            format!(
                                "  · {provider}{}",
                                if *is_default { "  [default]" } else { "" }
                            ),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])),
                    PickerItem::Authenticate => ListItem::new(Line::from(Span::styled(
                        "Authenticate another LLM provider",
                        Style::default().fg(Color::Cyan),
                    ))),
                })
                .collect::<Vec<_>>();
            draw_list_popup(frame, area, " Models ", rows, *selected);
        }
        Popup::Providers { selected } => {
            let rows = vec![
                ListItem::new("OpenAI  · ChatGPT browser authentication"),
                ListItem::new("Anthropic  · Console credential authentication"),
            ];
            draw_list_popup(
                frame,
                area,
                " Authenticate an LLM provider ",
                rows,
                *selected,
            );
        }
        Popup::Effort { selected } => {
            let rows = effort_levels()
                .iter()
                .map(|(label, _)| ListItem::new(*label))
                .collect();
            draw_list_popup(frame, area, " Effort ", rows, *selected);
        }
        Popup::Permission(prompt) => draw_permission_popup(frame, area, prompt),
        Popup::Question(prompt) => draw_question_popup(frame, area, prompt),
    }
}

fn draw_list_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'_>>,
    selected: usize,
) {
    let popup = centered_rect(74, (items.len() as u16 + 4).clamp(7, 22), area);
    frame.render_widget(Clear, popup);
    let mut state = ListState::default().with_selected(Some(selected));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_bottom(Line::from(" ↑↓ choose · Enter select · Esc close ").centered()),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(35, 55, 75))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, popup, &mut state);
}

fn draw_permission_popup(frame: &mut Frame<'_>, area: Rect, prompt: &PermissionPrompt) {
    let popup = centered_rect(78, 20, area);
    frame.render_widget(Clear, popup);
    let input = serde_json::to_string_pretty(&prompt.input).unwrap_or_default();
    let input = truncate_chars(&input, 1600);
    let choices = if prompt.selected == 0 {
        "\n  › Allow once       Deny"
    } else {
        "\n    Allow once     › Deny"
    };
    let body = format!(
        "{}\n{}{}\n\n{}",
        prompt.tool_name,
        if prompt.description.is_empty() {
            ""
        } else {
            prompt.description.as_str()
        },
        if prompt.description.is_empty() {
            ""
        } else {
            "\n"
        },
        input
    ) + choices;
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Permission request ")
                    .title_bottom(Line::from(" Y allow · N deny · Enter choose ").centered()),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_question_popup(frame: &mut Frame<'_>, area: Rect, prompt: &QuestionPrompt) {
    let popup = centered_rect(76, 18, area);
    frame.render_widget(Clear, popup);
    let question = prompt.input["questions"][prompt.question_index]["question"]
        .as_str()
        .unwrap_or("Claude needs your input");
    let options = question_options(prompt);
    let mut lines = vec![
        Line::from(Span::styled(
            question,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    if let Some(custom) = &prompt.custom_input {
        lines.push(Line::from("Other answer:"));
        lines.push(Line::from(Span::styled(
            format!("> {custom}"),
            Style::default().fg(Color::Cyan),
        )));
    } else {
        for (index, option) in options.iter().enumerate() {
            let checked = prompt
                .checked
                .get(prompt.question_index)
                .and_then(|values| values.get(index))
                .copied()
                .unwrap_or(false);
            lines.push(Line::from(format!(
                "{} {} {}",
                if index == prompt.selected { "›" } else { " " },
                if question_multi_select(prompt) {
                    if checked {
                        "[x]"
                    } else {
                        "[ ]"
                    }
                } else {
                    ""
                },
                option
            )));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Question ")
                    .title_bottom(
                        Line::from(" ↑↓ choose · Space toggle · O other · Enter confirm ")
                            .centered(),
                    ),
            )
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = area.width.saturating_mul(percent_x).saturating_div(100);
    let width = width.clamp(30, area.width.saturating_sub(2).max(1));
    let height = height.min(area.height.saturating_sub(2).max(1));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
            Err(error).context("failed to initialize terminal UI")
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
    }
}

fn input_cursor_position(app: &SessionApp, area: Rect) -> (u16, u16) {
    let before = &app.input[..app.cursor];
    let row_offset = before
        .chars()
        .filter(|character| *character == '\n')
        .count()
        .min(u16::MAX as usize) as u16;
    let column_offset = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        .min(u16::MAX as usize) as u16;
    (
        (area.x + 1 + column_offset).min(area.right().saturating_sub(2)),
        (area.y + 1 + row_offset).min(area.bottom().saturating_sub(2)),
    )
}

fn insert_text(value: &mut String, cursor: &mut usize, text: &str) {
    let remaining = MAX_INPUT_BYTES.saturating_sub(value.len());
    let prefix = utf8_prefix(text, remaining);
    value.insert_str(*cursor, prefix);
    *cursor += prefix.len();
}

fn delete_previous(value: &mut String, cursor: &mut usize) {
    if let Some((previous, _)) = value[..*cursor].char_indices().last() {
        value.replace_range(previous..*cursor, "");
        *cursor = previous;
    }
}

fn delete_next(value: &mut String, cursor: &mut usize) {
    if *cursor >= value.len() {
        return;
    }
    if let Some(character) = value[*cursor..].chars().next() {
        value.replace_range(*cursor..*cursor + character.len_utf8(), "");
    }
}

fn move_left(value: &str, cursor: &mut usize) {
    if let Some((previous, _)) = value[..*cursor].char_indices().last() {
        *cursor = previous;
    }
}

fn move_right(value: &str, cursor: &mut usize) {
    if let Some(character) = value[*cursor..].chars().next() {
        *cursor += character.len_utf8();
    }
}

fn tool_description(name: &str, input: Option<&Value>) -> String {
    let detail = match name {
        "Bash" => input
            .and_then(|value| value.get("command"))
            .and_then(Value::as_str),
        "Read" | "Write" | "Edit" => input
            .and_then(|value| value.get("file_path"))
            .and_then(Value::as_str),
        "Agent" | "Task" => input
            .and_then(|value| value.get("description"))
            .and_then(Value::as_str),
        _ => None,
    };
    detail.map_or_else(|| name.to_string(), |detail| format!("{name}: {detail}"))
}

fn effort_levels() -> &'static [(&'static str, &'static str)] {
    &[
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("ultracode", "xhigh"),
        ("max", "max"),
    ]
}

fn command_help(store: &AccountStore) -> String {
    let mut commands = vec!["/model", "/effort"];
    if crate::fast::FastAvailability::from_store(store).any() {
        commands.push("/fast");
    }
    if store.has_provider(AccountProvider::Openai) {
        commands.push("/usage");
    }
    commands.extend(["/clear", "/exit"]);
    format!(
        "{}\nOther slash commands are passed to the underlying agent harness.",
        commands.join("  ")
    )
}

fn effort_display_name(effort: &str) -> &str {
    if effort == "xhigh" {
        "ultracode"
    } else {
        effort
    }
}

fn approximate_wrapped_lines(lines: &[Line<'_>], width: usize) -> usize {
    let width = width.max(1);
    lines
        .iter()
        .map(|line| {
            let length: usize = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum();
            length.max(1).div_ceil(width)
        })
        .sum()
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut output: String = value.chars().take(limit).collect();
    if value.chars().count() > limit {
        output.push_str("\n…");
    }
    output
}

fn bounded_text(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let end = utf8_prefix(&value, max_bytes).len();
    value.truncate(end);
    value.push_str("\n… [terminal display truncated; full session remains on disk]");
    value
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn rendered_screen(app: &mut SessionApp) -> String {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn first_run_header_is_provider_neutral() {
        let mut app = SessionApp::new(AccountStore::default());
        assert_eq!(app.header_status(), "/model to authenticate");
        assert!(!app.header_status().contains("Billing"));
        assert!(!app.header_status().contains("Claude"));

        let screen = rendered_screen(&mut app);
        assert!(screen.contains("Joey's Claudex"));
        assert!(screen.contains(env!("CARGO_PKG_VERSION")));
        assert!(screen.contains("/model to authenticate"));
        assert!(!screen.contains("Claude Code"));
        assert!(!screen.contains("API Usage Billing"));
        assert!(!screen.contains("Fable"));
    }

    #[test]
    fn connected_picker_stacks_provider_models_and_auth_action() {
        let mut store = AccountStore::default();
        store.upsert_with_models(AccountProvider::Openai, vec!["gpt-5.6".to_string()]);
        store.upsert_with_models(
            AccountProvider::Anthropic,
            vec!["claude-sonnet-5".to_string()],
        );
        let mut app = SessionApp::new(store);
        let items = app.model_items();
        assert!(matches!(items[0], PickerItem::Model { ref provider, .. } if provider == "OpenAI"));
        assert!(
            matches!(items[1], PickerItem::Model { ref provider, .. } if provider == "Anthropic")
        );
        assert!(matches!(items[2], PickerItem::Authenticate));

        app.open_model_picker();
        let screen = rendered_screen(&mut app);
        assert!(screen.contains("gpt-5.6"));
        assert!(screen.contains("claude-sonnet-5"));
        assert!(screen.contains("Authenticate another LLM provider"));
        assert!(!screen.contains("custom Opus"));
        assert!(!screen.contains("Fable"));
    }

    #[test]
    fn xhigh_is_presented_as_ultracode() {
        assert_eq!(effort_display_name("xhigh"), "ultracode");
    }

    #[test]
    fn provider_commands_only_appear_in_help_when_available() {
        let empty = AccountStore::default();
        assert!(!command_help(&empty).contains("/fast"));
        assert!(!command_help(&empty).contains("/usage"));

        let mut openai = AccountStore::default();
        openai.upsert(AccountProvider::Openai);
        assert!(command_help(&openai).contains("/fast"));
        assert!(command_help(&openai).contains("/usage"));
    }
}
