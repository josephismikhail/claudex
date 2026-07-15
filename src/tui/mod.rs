pub mod dashboard;
pub mod input;
pub mod widgets;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{Event, EventStream, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::ListState;
use ratatui::Terminal;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::config::{ClaudexConfig, ProfileConfig, ProviderType};
use crate::oauth::AuthType;
use crate::proxy::health::{HealthMap, HealthStatus};
use crate::proxy::metrics::MetricsStore;

// ── Profile Form Field Indices ──

const FIELD_NAME: usize = 0;
const FIELD_PROVIDER_TYPE: usize = 1;
const FIELD_BASE_URL: usize = 2;
const FIELD_API_KEY: usize = 3;
const FIELD_MODEL: usize = 4;
const FIELD_ENABLED: usize = 5;
const FIELD_PRIORITY: usize = 6;

// ── State Machine ──

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Search,
    AddProfile,
    EditProfile,
    Confirm,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RightPanel {
    Logs,
    Detail,
}

#[derive(Debug, Clone)]
pub enum AsyncAction {
    SaveProfile(ProfileForm),
    DeleteProfile(String),
    StartProxy,
    StopProxy,
    TestProfile(String),
}

// ── Notification ──

#[derive(Debug, Clone)]
pub enum NotificationLevel {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub message: String,
    pub level: NotificationLevel,
    pub created_at: Instant,
}

impl Notification {
    pub fn info(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            level: NotificationLevel::Info,
            created_at: Instant::now(),
        }
    }
    pub fn success(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            level: NotificationLevel::Success,
            created_at: Instant::now(),
        }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            level: NotificationLevel::Error,
            created_at: Instant::now(),
        }
    }
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= 3
    }
}

// ── Profile Snapshot ──

#[derive(Debug, Clone)]
pub struct ProfileSnapshot {
    pub name: String,
    pub enabled: bool,
    pub provider_type: String,
    pub base_url: String,
    pub default_model: String,
    pub priority: u32,
    pub auth_type: String,
    pub has_api_key: bool,
}

impl ProfileSnapshot {
    fn from_profile(p: &ProfileConfig) -> Self {
        Self {
            name: p.name.clone(),
            enabled: p.enabled,
            provider_type: match p.provider_type {
                ProviderType::DirectAnthropic => "DirectAnthropic".to_string(),
                ProviderType::OpenAICompatible => "OpenAICompatible".to_string(),
                ProviderType::OpenAIResponses => "OpenAIResponses".to_string(),
            },
            base_url: p.base_url.clone(),
            default_model: p.default_model.clone(),
            priority: p.priority,
            auth_type: match p.auth_type {
                AuthType::ApiKey => "ApiKey".to_string(),
                AuthType::OAuth => "OAuth".to_string(),
            },
            has_api_key: !p.api_key.is_empty() || p.api_key_keyring.is_some(),
        }
    }
}

// ── Form ──

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    Password,
    Select(Vec<String>),
    Bool,
    Number,
}

#[derive(Debug, Clone)]
pub struct FormField {
    pub label: &'static str,
    pub kind: FieldKind,
    pub value: String,
    pub cursor_pos: usize,
}

impl FormField {
    fn new(label: &'static str, kind: FieldKind, value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor_pos = value.len();
        Self {
            label,
            kind,
            value,
            cursor_pos,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        match self.kind {
            FieldKind::Number => {
                if c.is_ascii_digit() {
                    self.value.insert(self.cursor_pos, c);
                    self.cursor_pos += 1;
                }
            }
            FieldKind::Bool => {
                // toggle on any char
                self.value = if self.value == "true" {
                    "false"
                } else {
                    "true"
                }
                .to_string();
            }
            FieldKind::Select(ref options) => {
                // cycle to next
                if let Some(idx) = options.iter().position(|o| o == &self.value) {
                    let next = (idx + 1) % options.len();
                    self.value = options[next].clone();
                }
            }
            _ => {
                self.value.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
            }
        }
    }

    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            if let Some((previous, _)) = self.value[..self.cursor_pos].char_indices().last() {
                self.value.remove(previous);
                self.cursor_pos = previous;
            }
        }
    }

    pub fn move_cursor_left(&mut self) {
        if let Some((previous, _)) = self.value[..self.cursor_pos].char_indices().last() {
            self.cursor_pos = previous;
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.value.len() {
            if let Some(next) = self.value[self.cursor_pos..].chars().next() {
                self.cursor_pos += next.len_utf8();
            }
        }
    }

    pub fn cycle_select(&mut self, forward: bool) {
        if let FieldKind::Select(ref options) = self.kind {
            if let Some(idx) = options.iter().position(|o| o == &self.value) {
                let next = if forward {
                    (idx + 1) % options.len()
                } else {
                    (idx + options.len() - 1) % options.len()
                };
                self.value = options[next].clone();
            }
        } else if self.kind == FieldKind::Bool {
            self.value = if self.value == "true" {
                "false"
            } else {
                "true"
            }
            .to_string();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProfileForm {
    pub fields: Vec<FormField>,
    pub focused_field: usize,
    pub is_edit: bool,
    pub original_name: Option<String>,
}

impl ProfileForm {
    pub fn new_blank() -> Self {
        Self {
            fields: vec![
                FormField::new("Name", FieldKind::Text, ""),
                FormField::new(
                    "Provider Type",
                    FieldKind::Select(vec![
                        "DirectAnthropic".to_string(),
                        "OpenAICompatible".to_string(),
                        "OpenAIResponses".to_string(),
                    ]),
                    "OpenAICompatible",
                ),
                FormField::new("Base URL", FieldKind::Text, ""),
                FormField::new("API Key", FieldKind::Password, ""),
                FormField::new("Default Model", FieldKind::Text, ""),
                FormField::new("Enabled", FieldKind::Bool, "true"),
                FormField::new("Priority", FieldKind::Number, "100"),
            ],
            focused_field: 0,
            is_edit: false,
            original_name: None,
        }
    }

    pub fn from_profile(p: &ProfileConfig) -> Self {
        let provider_type = match p.provider_type {
            ProviderType::DirectAnthropic => "DirectAnthropic",
            ProviderType::OpenAICompatible => "OpenAICompatible",
            ProviderType::OpenAIResponses => "OpenAIResponses",
        };
        Self {
            fields: vec![
                FormField::new("Name", FieldKind::Text, &p.name),
                FormField::new(
                    "Provider Type",
                    FieldKind::Select(vec![
                        "DirectAnthropic".to_string(),
                        "OpenAICompatible".to_string(),
                        "OpenAIResponses".to_string(),
                    ]),
                    provider_type,
                ),
                FormField::new("Base URL", FieldKind::Text, &p.base_url),
                FormField::new("API Key", FieldKind::Password, &p.api_key),
                FormField::new("Default Model", FieldKind::Text, &p.default_model),
                FormField::new(
                    "Enabled",
                    FieldKind::Bool,
                    if p.enabled { "true" } else { "false" },
                ),
                FormField::new("Priority", FieldKind::Number, p.priority.to_string()),
            ],
            focused_field: 0,
            is_edit: true,
            original_name: Some(p.name.clone()),
        }
    }

    pub fn to_profile_config(&self, existing: Option<&ProfileConfig>) -> ProfileConfig {
        let provider_type = match self.fields[FIELD_PROVIDER_TYPE].value.as_str() {
            "DirectAnthropic" => ProviderType::DirectAnthropic,
            "OpenAIResponses" => ProviderType::OpenAIResponses,
            _ => ProviderType::OpenAICompatible,
        };
        let mut profile = existing.cloned().unwrap_or_default();
        profile.name = self.fields[FIELD_NAME].value.clone();
        profile.provider_type = provider_type;
        profile.base_url = self.fields[FIELD_BASE_URL].value.clone();
        // The edit form intentionally does not load a saved secret. A blank
        // password field therefore means "keep it", not "erase it".
        if existing.is_none() || !self.fields[FIELD_API_KEY].value.is_empty() {
            profile.api_key = self.fields[FIELD_API_KEY].value.clone();
        }
        profile.default_model = self.fields[FIELD_MODEL].value.clone();
        profile.priority = self.fields[FIELD_PRIORITY].value.parse().unwrap_or(100);
        profile.enabled = self.fields[FIELD_ENABLED].value == "true";
        profile
    }

    pub fn focus_next(&mut self) {
        if self.focused_field < self.fields.len() - 1 {
            self.focused_field += 1;
        }
    }

    pub fn focus_prev(&mut self) {
        self.focused_field = self.focused_field.saturating_sub(1);
    }
}

// ── App ──

pub struct App {
    pub config: Arc<RwLock<ClaudexConfig>>,
    pub metrics: MetricsStore,
    pub health_status: Arc<RwLock<HealthMap>>,
    pub should_quit: bool,
    pub mode: AppMode,
    pub right_panel: RightPanel,
    pub search_query: String,
    pub proxy_running: bool,
    pub show_help: bool,
    pub launch_profile: Option<String>,
    pub notification: Option<Notification>,
    pub pending_action: Option<AsyncAction>,
    /// Proxy task owned by this dashboard session, if any.
    pub proxy_task: Option<JoinHandle<()>>,
    /// Avoid opening a new health-check connection on every 250 ms render tick.
    pub last_proxy_check: Instant,
    pub confirm_target: Option<String>,
    pub form: ProfileForm,

    /// Cached profile list for sync access
    pub profile_list: Vec<ProfileSnapshot>,
    /// Indices into profile_list filtered by search
    pub filtered_indices: Vec<usize>,
    /// ratatui ListState for profile selection (over filtered_indices)
    pub profile_state: ListState,
    /// tui-logger state
    pub log_state: tui_logger::TuiWidgetState,
}

impl App {
    pub fn new(
        config: Arc<RwLock<ClaudexConfig>>,
        metrics: MetricsStore,
        health_status: Arc<RwLock<HealthMap>>,
    ) -> Self {
        Self {
            config,
            metrics,
            health_status,
            should_quit: false,
            mode: AppMode::Normal,
            right_panel: RightPanel::Logs,
            search_query: String::new(),
            proxy_running: false,
            show_help: false,
            launch_profile: None,
            notification: None,
            pending_action: None,
            proxy_task: None,
            last_proxy_check: Instant::now(),
            confirm_target: None,
            form: ProfileForm::new_blank(),
            profile_list: Vec::new(),
            filtered_indices: Vec::new(),
            profile_state: ListState::default(),
            log_state: tui_logger::TuiWidgetState::new(),
        }
    }

    /// Refresh the cached profile list from config
    pub async fn refresh_profiles(&mut self) {
        let config = self.config.read().await;
        self.profile_list = config
            .profiles
            .iter()
            .map(ProfileSnapshot::from_profile)
            .collect();
        drop(config);
        self.apply_search_filter();
    }

    /// Rebuild filtered_indices from search_query
    pub fn apply_search_filter(&mut self) {
        let query = self.search_query.to_lowercase();
        if query.is_empty() {
            self.filtered_indices = (0..self.profile_list.len()).collect();
        } else {
            self.filtered_indices = self
                .profile_list
                .iter()
                .enumerate()
                .filter(|(_, p)| p.name.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect();
        }
        // Clamp selection
        if self.filtered_indices.is_empty() {
            self.profile_state.select(None);
        } else {
            match self.profile_state.selected() {
                None => self.profile_state.select(Some(0)),
                Some(sel) if sel >= self.filtered_indices.len() => {
                    self.profile_state
                        .select(Some(self.filtered_indices.len() - 1));
                }
                _ => {}
            }
        }
    }

    /// Get the currently selected profile name (from filtered view)
    pub fn selected_profile_name(&self) -> Option<String> {
        let sel = self.profile_state.selected()?;
        let orig_idx = *self.filtered_indices.get(sel)?;
        self.profile_list.get(orig_idx).map(|p| p.name.clone())
    }

    /// Get the currently selected profile snapshot
    pub fn selected_profile(&self) -> Option<&ProfileSnapshot> {
        let sel = self.profile_state.selected()?;
        let orig_idx = *self.filtered_indices.get(sel)?;
        self.profile_list.get(orig_idx)
    }

    pub fn select_next(&mut self) {
        if self.filtered_indices.is_empty() {
            return;
        }
        let i = match self.profile_state.selected() {
            Some(i) => (i + 1).min(self.filtered_indices.len() - 1),
            None => 0,
        };
        self.profile_state.select(Some(i));
    }

    pub fn select_previous(&mut self) {
        let i = match self.profile_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.profile_state.select(Some(i));
    }
}

pub async fn run_tui(
    config: Arc<RwLock<ClaudexConfig>>,
    metrics: MetricsStore,
    health_status: Arc<RwLock<HealthMap>>,
) -> Result<()> {
    // Initialize tui-logger
    tui_logger::init_logger(log::LevelFilter::Info).ok();
    tui_logger::set_default_level(log::LevelFilter::Info);

    let mut terminal_session = TerminalSession::enter()?;
    let stdout = std::io::stdout();

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config, metrics, health_status);
    {
        let config = app.config.read().await;
        app.proxy_running =
            crate::proxy::is_proxy_reachable(&config.proxy_host, config.proxy_port).await;
    }
    app.refresh_profiles().await;

    log::info!("Claudex dashboard started");
    if app.proxy_running {
        log::info!("Proxy is running");
    }

    let mut event_stream = EventStream::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));

    loop {
        // Handle pending async actions first
        handle_async_actions(&mut app).await?;

        if app.should_quit {
            break;
        }

        // Check if we should exit TUI for launch
        if let Some(profile_name) = app.launch_profile.take() {
            terminal_session.restore()?;

            let config = app.config.read().await;
            if let Some(profile) = config.find_profile(&profile_name) {
                let profile = profile.clone();
                let config_snapshot = config.clone();
                drop(config);

                if !crate::proxy::is_proxy_reachable(
                    &config_snapshot.proxy_host,
                    config_snapshot.proxy_port,
                )
                .await
                {
                    println!("Starting proxy in background...");
                    let bg_config = config_snapshot.clone();
                    app.proxy_task = Some(tokio::spawn(async move {
                        if let Err(e) = crate::proxy::start_embedded_proxy(bg_config, None).await {
                            tracing::error!("proxy failed: {e}");
                        }
                    }));
                    if !crate::proxy::wait_for_proxy(
                        &config_snapshot.proxy_host,
                        config_snapshot.proxy_port,
                        std::time::Duration::from_secs(5),
                    )
                    .await
                    {
                        if let Some(task) = app.proxy_task.take() {
                            task.abort();
                        }
                        anyhow::bail!("proxy failed to start within 5 seconds");
                    }
                }

                crate::process::launch::launch_claude(
                    &config_snapshot,
                    &profile,
                    None,
                    &[],
                    false,
                )?;
            }
            return Ok(());
        }

        // Render
        let config_snap = app.config.read().await.clone();
        let health_snap = app.health_status.read().await.clone();
        terminal.draw(|f| {
            dashboard::render(f, &mut app, &config_snap, &health_snap);
            // Overlay layers
            match app.mode {
                AppMode::AddProfile | AppMode::EditProfile => {
                    widgets::render_form_popup(f, &app.form);
                }
                AppMode::Confirm => {
                    if let Some(ref target) = app.confirm_target {
                        widgets::render_confirm_dialog(f, target);
                    }
                }
                _ => {}
            }
            if app.show_help {
                widgets::render_help_popup(f);
            }
            if let Some(ref notif) = app.notification {
                widgets::render_notification(f, notif);
            }
        })?;

        // Async event handling with tokio::select!
        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        input::handle_key_event(&mut app, key);
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        // Terminal will auto-redraw on next loop iteration
                    }
                    Some(Err(_)) => {
                        app.should_quit = true;
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                // Periodic refresh
                app.refresh_profiles().await;
                if app.last_proxy_check.elapsed() >= std::time::Duration::from_secs(2) {
                    let (host, port) = {
                        let config = app.config.read().await;
                        (config.proxy_host.clone(), config.proxy_port)
                    };
                    app.proxy_running = crate::proxy::is_proxy_reachable(&host, port).await;
                    app.last_proxy_check = Instant::now();
                }
                // Clear expired notifications
                if let Some(ref notif) = app.notification {
                    if notif.is_expired() {
                        app.notification = None;
                    }
                }
            }
        }
    }

    if let Some(task) = app.proxy_task.take() {
        task.abort();
    }
    terminal_session.restore()?;

    Ok(())
}

async fn handle_async_actions(app: &mut App) -> Result<()> {
    let action = match app.pending_action.take() {
        Some(a) => a,
        None => return Ok(()),
    };

    match action {
        AsyncAction::TestProfile(profile_name) => {
            let config = app.config.read().await;
            if let Some(profile) = config.find_profile(&profile_name) {
                let profile = profile.clone();
                drop(config);
                log::info!("Testing {}...", profile_name);
                match crate::config::profile::test_connectivity(&profile).await {
                    Ok(latency) => {
                        app.health_status.write().await.insert(
                            profile_name.clone(),
                            HealthStatus {
                                healthy: true,
                                latency_ms: Some(latency),
                                last_check: Some(std::time::Instant::now()),
                                error: None,
                            },
                        );
                        let msg = format!("{}: OK ({latency}ms)", profile_name);
                        log::info!("{msg}");
                        app.notification = Some(Notification::success(msg));
                    }
                    Err(e) => {
                        let error = e.to_string();
                        app.health_status.write().await.insert(
                            profile_name.clone(),
                            HealthStatus {
                                healthy: false,
                                latency_ms: None,
                                last_check: Some(std::time::Instant::now()),
                                error: Some(error.clone()),
                            },
                        );
                        let msg = format!("{}: FAIL - {error}", profile_name);
                        log::error!("{msg}");
                        app.notification = Some(Notification::error(msg));
                    }
                }
            }
        }
        AsyncAction::SaveProfile(form) => {
            let mut config = app.config.write().await;
            let existing = form
                .original_name
                .as_deref()
                .and_then(|name| config.find_profile(name))
                .cloned();
            let profile_config = form.to_profile_config(existing.as_ref());
            let name = profile_config.name.clone();
            let original_profiles = config.profiles.clone();

            if form.is_edit {
                let original_name = form.original_name.as_deref().unwrap_or_default();
                if config
                    .profiles
                    .iter()
                    .any(|profile| profile.name == name && profile.name != original_name)
                {
                    app.notification = Some(Notification::error(format!(
                        "Profile '{name}' already exists"
                    )));
                    return Ok(());
                }
                if let Some(position) = form
                    .original_name
                    .as_deref()
                    .and_then(|original| config.profiles.iter().position(|p| p.name == original))
                {
                    config.profiles[position] = profile_config;
                } else {
                    app.notification =
                        Some(Notification::error("Original profile no longer exists"));
                    return Ok(());
                }
            } else if config.find_profile(&name).is_some() {
                app.notification = Some(Notification::error(format!(
                    "Profile '{name}' already exists"
                )));
                return Ok(());
            } else {
                config.profiles.push(profile_config);
            }
            if let Err(e) = config.save() {
                config.profiles = original_profiles;
                app.notification = Some(Notification::error(format!("Save failed: {e}")));
            } else {
                let verb = if form.is_edit { "Updated" } else { "Added" };
                app.notification = Some(Notification::success(format!("{verb} profile '{name}'")));
                log::info!("{verb} profile '{name}'");
            }
            drop(config);
            app.refresh_profiles().await;
        }
        AsyncAction::DeleteProfile(name) => {
            let mut config = app.config.write().await;
            let original_profiles = config.profiles.clone();
            config.profiles.retain(|p| p.name != name);
            if let Err(e) = config.save() {
                config.profiles = original_profiles;
                app.notification = Some(Notification::error(format!("Delete failed: {e}")));
            } else {
                app.notification = Some(Notification::success(format!("Deleted profile '{name}'")));
                log::info!("Deleted profile '{name}'");
            }
            drop(config);
            app.refresh_profiles().await;
        }
        AsyncAction::StartProxy => {
            let config = app.config.read().await.clone();
            if crate::proxy::is_proxy_reachable(&config.proxy_host, config.proxy_port).await {
                app.proxy_running = true;
                app.notification = Some(Notification::info("Proxy is already running"));
                return Ok(());
            }
            let host = config.proxy_host.clone();
            let port = config.proxy_port;
            app.proxy_task = Some(tokio::spawn(async move {
                if let Err(e) = crate::proxy::start_embedded_proxy(config, None).await {
                    tracing::error!("proxy failed: {e}");
                }
            }));
            app.proxy_running =
                crate::proxy::wait_for_proxy(&host, port, std::time::Duration::from_secs(5)).await;
            if app.proxy_running {
                app.notification = Some(Notification::success("Proxy started"));
                log::info!("Proxy started");
            } else {
                if let Some(task) = app.proxy_task.take() {
                    task.abort();
                }
                app.notification = Some(Notification::error("Proxy failed to start"));
                log::error!("Proxy failed to start");
            }
        }
        AsyncAction::StopProxy => match stop_proxy_for_dashboard(app).await {
            Ok(()) => {
                app.proxy_running = false;
                app.notification = Some(Notification::success("Proxy stopped"));
                log::info!("Proxy stopped");
            }
            Err(e) => {
                app.notification = Some(Notification::error(format!("Stop failed: {e}")));
                log::error!("Stop proxy failed: {e}");
            }
        },
    }

    Ok(())
}

async fn stop_proxy_for_dashboard(app: &mut App) -> Result<()> {
    if let Some(task) = app.proxy_task.take() {
        task.abort();
        let _ = task.await;
        return Ok(());
    }
    crate::process::daemon::stop_proxy()
}

/// Restores the user's terminal on normal exit, errors, and unwinding panics.
/// Without this guard, any `?` inside the event loop can leave raw mode and the
/// alternate screen active, which looks like a terminal crash.
struct TerminalSession {
    active: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self { active: true })
    }

    fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }

        let raw_result = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let screen_result = execute!(stdout, LeaveAlternateScreen, Show);
        self.active = false;
        raw_result?;
        screen_result?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_field_cursor_stays_on_utf8_boundaries() {
        let mut field = FormField::new("Name", FieldKind::Text, "aé中");
        assert_eq!(field.cursor_pos, field.value.len());

        field.move_cursor_left();
        assert_eq!(field.cursor_pos, "aé".len());
        field.move_cursor_left();
        assert_eq!(field.cursor_pos, "a".len());

        field.insert_char('🙂');
        assert_eq!(field.value, "a🙂é中");
        assert_eq!(field.cursor_pos, "a🙂".len());

        field.delete_char();
        assert_eq!(field.value, "aé中");
        assert_eq!(field.cursor_pos, "a".len());

        field.move_cursor_right();
        assert_eq!(field.cursor_pos, "aé".len());
    }

    #[test]
    fn editing_profile_preserves_advanced_and_secret_fields() {
        let existing = ProfileConfig {
            name: "ultracode".to_string(),
            base_url: "https://old.example".to_string(),
            api_key: "saved-secret".to_string(),
            api_key_keyring: Some("saved-keyring".to_string()),
            default_model: "chat-model".to_string(),
            auth_type: AuthType::OAuth,
            oauth_provider: Some(crate::oauth::OAuthProvider::Openai),
            model_routes: std::collections::HashMap::from([(
                "claude-model".to_string(),
                "anthropic".to_string(),
            )]),
            ..Default::default()
        };
        let mut form = ProfileForm::from_profile(&existing);
        form.fields[FIELD_API_KEY].value.clear();
        form.fields[FIELD_BASE_URL].value = "https://new.example".to_string();

        let updated = form.to_profile_config(Some(&existing));

        assert_eq!(updated.base_url, "https://new.example");
        assert_eq!(updated.api_key, "saved-secret");
        assert_eq!(updated.api_key_keyring.as_deref(), Some("saved-keyring"));
        assert_eq!(updated.auth_type, AuthType::OAuth);
        assert_eq!(
            updated.model_routes.get("claude-model").map(String::as_str),
            Some("anthropic")
        );
    }
}
