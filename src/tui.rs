use crate::cli::Cli;
use crate::cli::Command;
use crate::cli::DistillArgs;
use crate::cli::SmartArgs;
use crate::distill;
use crate::run_command;
use crate::runtime::load_runtime_config;
use crate::runtime::shell_quote;
use crate::smart;
use crate::summary::build_session_summary;
use crate::types::SessionSummary;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_core::RolloutRecorder;
use codex_core::ThreadItem;
use codex_core::ThreadSortKey;
use codex_core::config::Config;
use codex_core::find_thread_names_by_ids;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use crossterm::cursor::Hide;
use crossterm::cursor::Show;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::poll;
use crossterm::event::read;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::io::Stdout;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

const THREAD_PAGE_SIZE: usize = 200;
const DETAIL_LOAD_DEBOUNCE: Duration = Duration::from_millis(450);

pub(crate) async fn run() -> Result<()> {
    let config = load_runtime_config(None, None, None, None, None, None).await?;
    let mut app = AppState::new(config);
    app.reload(None).await?;

    let mut terminal = TerminalSession::enter()?;

    loop {
        terminal.draw(&mut app)?;

        if !poll(app.poll_timeout())? {
            app.refresh_selected_detail_if_due().await?;
            continue;
        }

        let Event::Key(key) = read()? else {
            continue;
        };
        if matches!(key.kind, KeyEventKind::Release) {
            continue;
        }

        match app.handle_key(key)? {
            UiEffect::None => {}
            UiEffect::Quit => break,
            UiEffect::Reload => {
                let selection = app.selected_identity();
                app.reload(selection).await?;
            }
            UiEffect::Execute(prepared) => {
                let selection = app.selected_identity();
                let status =
                    execute_prepared_command(&mut terminal, *prepared, app.language).await?;
                app.status = Some(status);
                app.reload(selection).await?;
            }
            UiEffect::RunSmart(args) => match smart::prepare_execution((*args).clone()).await {
                Ok(prepared) => {
                    terminal.suspend()?;
                    let selection = smart::pick_selection(&prepared).await;
                    terminal.resume()?;
                    match selection {
                        Ok(Some(selection)) => {
                            app.mode = Mode::Result(smart_processing_result(app.language));
                            terminal.terminal.clear()?;
                            terminal.draw(&mut app)?;
                            match smart::execute_prepared(prepared, selection).await {
                                Ok(result) => {
                                    let selection = preferred_selection(
                                        result.preferred_thread_id.as_deref(),
                                        result.preferred_rollout_path.as_ref(),
                                    );
                                    app.reload(selection).await?;
                                    app.mode = Mode::Result(ResultViewState {
                                        title: localize_known_result_title(
                                            app.language,
                                            result.title.as_str(),
                                        ),
                                        lines: result.lines,
                                        scroll: 0,
                                    });
                                    terminal.terminal.clear()?;
                                }
                                Err(error) => {
                                    app.mode = Mode::Result(error_result_state(
                                        app.language,
                                        "Smart Switch Failed",
                                        "Smart 切换失败",
                                        &error,
                                    ));
                                    terminal.terminal.clear()?;
                                }
                            }
                        }
                        Ok(None) => {
                            app.mode = Mode::Browsing;
                            app.status = Some(match app.language {
                                Language::English => "Smart switch cancelled".to_string(),
                                Language::Chinese => "已取消 smart 切换".to_string(),
                            });
                        }
                        Err(error) => {
                            app.mode = Mode::Result(error_result_state(
                                app.language,
                                "Smart Picker Failed",
                                "Smart 选择器失败",
                                &error,
                            ));
                            terminal.terminal.clear()?;
                        }
                    }
                }
                Err(error) => {
                    app.mode = Mode::Result(error_result_state(
                        app.language,
                        "Smart Setup Failed",
                        "Smart 初始化失败",
                        &error,
                    ));
                    terminal.terminal.clear()?;
                }
            },
            UiEffect::RunDistill(args) => {
                app.mode = Mode::Result(distill_processing_result(app.language));
                terminal.terminal.clear()?;
                terminal.draw(&mut app)?;
                match distill::execute(*args).await {
                    Ok(output) => {
                        let selection = preferred_selection(
                            output.successor_thread_id.as_deref(),
                            output.successor_rollout_path.as_ref(),
                        );
                        app.reload(selection).await?;
                        app.mode = Mode::Result(ResultViewState {
                            title: localized_heading_text(
                                app.language,
                                "Distilled Successor Ready",
                                "提炼结果已生成",
                            ),
                            lines: distill::render_output_lines(&output),
                            scroll: 0,
                        });
                        terminal.terminal.clear()?;
                    }
                    Err(error) => {
                        app.mode = Mode::Result(error_result_state(
                            app.language,
                            "Distillation Failed",
                            "提炼失败",
                            &error,
                        ));
                        terminal.terminal.clear()?;
                    }
                }
            }
        }

        app.refresh_selected_detail_if_due().await?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct ThreadEntry {
    thread_id: Option<ThreadId>,
    rollout_path: PathBuf,
    provider: String,
    archived: bool,
    title: String,
    thread_name: Option<String>,
    preview: Option<String>,
    cwd: Option<PathBuf>,
    source: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

impl ThreadEntry {
    fn target(&self) -> String {
        self.thread_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| self.rollout_path.display().to_string())
    }

    fn updated_label(&self) -> Option<&str> {
        self.updated_at.as_deref().or(self.created_at.as_deref())
    }

    fn sort_key(&self) -> &str {
        self.updated_label().unwrap_or("")
    }

    fn state_label(&self, language: Language) -> &'static str {
        match (self.archived, language) {
            (true, Language::English) => "archived",
            (false, Language::English) => "active",
            (true, Language::Chinese) => "已归档",
            (false, Language::Chinese) => "活跃",
        }
    }

    fn from_thread_item(
        item: ThreadItem,
        archived: bool,
        default_provider: &str,
        names: &HashMap<ThreadId, String>,
    ) -> Self {
        let thread_id = item.thread_id;
        let thread_name = thread_id
            .and_then(|thread_id| names.get(&thread_id).cloned())
            .filter(|name| !name.trim().is_empty());
        let preview = item
            .first_user_message
            .as_deref()
            .map(clean_text)
            .filter(|text| !text.is_empty());
        let title = derive_thread_title(
            thread_name.as_deref(),
            preview.as_deref(),
            thread_id,
            item.path.as_path(),
        );

        Self {
            thread_id,
            rollout_path: item.path,
            provider: item
                .model_provider
                .unwrap_or_else(|| default_provider.to_string()),
            archived,
            title,
            thread_name,
            preview,
            cwd: item.cwd,
            source: item.source.as_ref().map(format_session_source),
            created_at: item.created_at,
            updated_at: item.updated_at,
        }
    }
}

#[derive(Debug, Clone)]
struct ThreadIdentity {
    thread_id: Option<ThreadId>,
    rollout_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    English,
    Chinese,
}

impl Language {
    fn detect() -> Self {
        sys_locale::get_locale()
            .as_deref()
            .map(Self::from_locale_tag)
            .unwrap_or(Self::English)
    }

    fn from_locale_tag(tag: &str) -> Self {
        if tag.to_ascii_lowercase().starts_with("zh") {
            Self::Chinese
        } else {
            Self::English
        }
    }

    fn toggle(self) -> Self {
        match self {
            Self::English => Self::Chinese,
            Self::Chinese => Self::English,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Chinese => "中文",
        }
    }
}

#[derive(Debug)]
enum DetailState {
    Loaded(Box<SessionSummary>),
    Failed(String),
}

#[derive(Debug, Clone)]
enum CatalogRow {
    Header { provider: String, count: usize },
    Thread(usize),
}

#[derive(Debug, Default)]
struct Catalog {
    threads: Vec<ThreadEntry>,
    rows: Vec<CatalogRow>,
    ordered_threads: Vec<usize>,
    row_by_thread: Vec<usize>,
    active_count: usize,
    archived_count: usize,
    provider_count: usize,
}

impl Catalog {
    fn new(threads: Vec<ThreadEntry>) -> Self {
        let active_count = threads.iter().filter(|thread| !thread.archived).count();
        let archived_count = threads.len().saturating_sub(active_count);
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, thread) in threads.iter().enumerate() {
            groups
                .entry(thread.provider.clone())
                .or_default()
                .push(index);
        }

        let provider_count = groups.len();
        let mut rows = Vec::new();
        let mut ordered_threads = Vec::new();
        let mut row_by_thread = vec![0; threads.len()];

        for (provider, mut indices) in groups {
            indices.sort_by(|left, right| {
                threads[*right]
                    .sort_key()
                    .cmp(threads[*left].sort_key())
                    .then_with(|| threads[*left].title.cmp(&threads[*right].title))
            });
            rows.push(CatalogRow::Header {
                provider,
                count: indices.len(),
            });
            for thread_index in indices {
                row_by_thread[thread_index] = rows.len();
                rows.push(CatalogRow::Thread(thread_index));
                ordered_threads.push(thread_index);
            }
        }

        Self {
            threads,
            rows,
            ordered_threads,
            row_by_thread,
            active_count,
            archived_count,
            provider_count,
        }
    }

    fn find_selection(&self, identity: &ThreadIdentity) -> Option<usize> {
        self.ordered_threads.iter().position(|thread_index| {
            let thread = &self.threads[*thread_index];
            if let (Some(left), Some(right)) =
                (thread.thread_id.as_ref(), identity.thread_id.as_ref())
            {
                return left == right;
            }
            thread.rollout_path == identity.rollout_path
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Show,
    Rename,
    Repair,
    RewriteMeta,
    RepairResumeState,
    Fork,
    Archive,
    Unarchive,
    CopySessionId,
    CopyCwd,
    CopyRolloutPath,
    CopyDeeplink,
    Compact,
    Rollback,
    Migrate,
    Smart,
    Distill,
}

#[derive(Debug, Clone, Copy)]
enum ActionInputKind {
    None,
    Text { required: bool },
    Raw { required: bool },
}

#[derive(Debug, Clone)]
struct PromptState {
    action: Action,
    input: String,
}

#[derive(Debug)]
enum Mode {
    Browsing,
    Actions { selected: usize },
    Prompt(PromptState),
    Result(ResultViewState),
}

#[derive(Debug)]
struct PreparedCommand {
    action: Action,
    command: Command,
    preview: String,
}

#[derive(Debug)]
enum UiEffect {
    None,
    Quit,
    Reload,
    Execute(Box<PreparedCommand>),
    RunSmart(Box<SmartArgs>),
    RunDistill(Box<DistillArgs>),
}

#[derive(Debug)]
struct ResultViewState {
    title: String,
    lines: Vec<String>,
    scroll: usize,
}

struct AppState {
    config: Config,
    catalog: Catalog,
    selected_thread: usize,
    scroll: usize,
    mode: Mode,
    status: Option<String>,
    language: Language,
    detail_cache: HashMap<PathBuf, DetailState>,
    detail_dirty: bool,
    detail_load_due_at: Option<Instant>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config,
            catalog: Catalog::default(),
            selected_thread: 0,
            scroll: 0,
            mode: Mode::Browsing,
            status: None,
            language: Language::detect(),
            detail_cache: HashMap::new(),
            detail_dirty: false,
            detail_load_due_at: None,
        }
    }

    async fn reload(&mut self, preferred_selection: Option<ThreadIdentity>) -> Result<()> {
        self.catalog = load_catalog(&self.config).await?;
        self.selected_thread = preferred_selection
            .as_ref()
            .and_then(|identity| self.catalog.find_selection(identity))
            .unwrap_or(0);
        self.scroll = 0;
        self.mode = Mode::Browsing;
        self.detail_cache.clear();
        self.schedule_selected_detail_refresh();
        self.status = Some(self.loaded_status_message());
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<UiEffect> {
        if matches!(key.code, KeyCode::F(2)) {
            self.language = self.language.toggle();
            self.status = Some(self.language_switched_message());
            return Ok(UiEffect::None);
        }
        match &self.mode {
            Mode::Browsing => self.handle_browse_key(key),
            Mode::Actions { .. } => self.handle_action_key(key),
            Mode::Prompt(_) => self.handle_prompt_key(key),
            Mode::Result(_) => self.handle_result_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> Result<UiEffect> {
        match key.code {
            KeyCode::Char('q') => Ok(UiEffect::Quit),
            KeyCode::Char('r') => Ok(UiEffect::Reload),
            KeyCode::Up => {
                self.move_selection(-1);
                Ok(UiEffect::None)
            }
            KeyCode::Down => {
                self.move_selection(1);
                Ok(UiEffect::None)
            }
            KeyCode::Enter => {
                if self.selected_entry().is_some() {
                    self.mode = Mode::Actions { selected: 0 };
                }
                Ok(UiEffect::None)
            }
            _ => Ok(UiEffect::None),
        }
    }

    fn handle_action_key(&mut self, key: KeyEvent) -> Result<UiEffect> {
        let actions = self.available_actions();
        let selected_index = match &self.mode {
            Mode::Actions { selected } => *selected,
            Mode::Browsing | Mode::Prompt(_) | Mode::Result(_) => return Ok(UiEffect::None),
        };
        let selected_thread = self.selected_entry().cloned();

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browsing;
                Ok(UiEffect::None)
            }
            KeyCode::Up => {
                if let Mode::Actions { selected } = &mut self.mode {
                    *selected = selected.saturating_sub(1);
                }
                Ok(UiEffect::None)
            }
            KeyCode::Down => {
                if !actions.is_empty()
                    && let Mode::Actions { selected } = &mut self.mode
                {
                    *selected = (*selected + 1).min(actions.len().saturating_sub(1));
                }
                Ok(UiEffect::None)
            }
            KeyCode::Enter => {
                let Some(action) = actions.get(selected_index).copied() else {
                    return Ok(UiEffect::None);
                };
                match action.input_kind() {
                    ActionInputKind::None => {
                        let Some(thread) = selected_thread else {
                            return Ok(UiEffect::None);
                        };
                        self.mode = Mode::Browsing;
                        let prepared = prepare_command(action, &thread, "")?;
                        match prepared.command {
                            Command::Smart(args) => Ok(UiEffect::RunSmart(Box::new(args))),
                            _ => Ok(UiEffect::Execute(Box::new(prepared))),
                        }
                    }
                    _ => {
                        self.mode = Mode::Prompt(PromptState {
                            action,
                            input: String::new(),
                        });
                        Ok(UiEffect::None)
                    }
                }
            }
            _ => Ok(UiEffect::None),
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Result<UiEffect> {
        let Some(thread) = self.selected_entry().cloned() else {
            self.mode = Mode::Browsing;
            return Ok(UiEffect::None);
        };

        let Mode::Prompt(prompt) = &mut self.mode else {
            return Ok(UiEffect::None);
        };

        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Actions { selected: 0 };
                Ok(UiEffect::None)
            }
            KeyCode::Backspace => {
                prompt.input.pop();
                Ok(UiEffect::None)
            }
            KeyCode::Enter => match prepare_command(prompt.action, &thread, &prompt.input) {
                Ok(prepared) => {
                    self.mode = Mode::Browsing;
                    match prepared.command {
                        Command::Distill(args) => Ok(UiEffect::RunDistill(Box::new(args))),
                        _ => Ok(UiEffect::Execute(Box::new(prepared))),
                    }
                }
                Err(error) => {
                    self.status = Some(self.input_error_message(error.to_string().as_str()));
                    Ok(UiEffect::None)
                }
            },
            KeyCode::Char(character) => {
                prompt.input.push(character);
                Ok(UiEffect::None)
            }
            _ => Ok(UiEffect::None),
        }
    }

    fn handle_result_key(&mut self, key: KeyEvent) -> Result<UiEffect> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.mode = Mode::Browsing;
                Ok(UiEffect::None)
            }
            KeyCode::Up => {
                if let Mode::Result(result) = &mut self.mode {
                    result.scroll = result.scroll.saturating_sub(1);
                }
                Ok(UiEffect::None)
            }
            KeyCode::Down => {
                if let Mode::Result(result) = &mut self.mode {
                    result.scroll = result.scroll.saturating_add(1);
                }
                Ok(UiEffect::None)
            }
            KeyCode::Char('q') => Ok(UiEffect::Quit),
            _ => Ok(UiEffect::None),
        }
    }

    fn available_actions(&self) -> Vec<Action> {
        let Some(thread) = self.selected_entry() else {
            return Vec::new();
        };

        Action::all()
            .iter()
            .copied()
            .filter(|action| action.is_available(thread.archived))
            .collect()
    }

    fn move_selection(&mut self, delta: i32) {
        let original = self.selected_thread;
        if self.catalog.ordered_threads.is_empty() {
            self.selected_thread = 0;
            return;
        }

        if delta < 0 {
            self.selected_thread = self
                .selected_thread
                .saturating_sub(delta.unsigned_abs() as usize);
        } else if delta > 0 {
            self.selected_thread = (self.selected_thread + delta as usize)
                .min(self.catalog.ordered_threads.len().saturating_sub(1));
        }
        if self.selected_thread != original {
            self.schedule_selected_detail_refresh();
        }
    }

    fn selected_entry(&self) -> Option<&ThreadEntry> {
        let thread_index = *self.catalog.ordered_threads.get(self.selected_thread)?;
        self.catalog.threads.get(thread_index)
    }

    fn selected_identity(&self) -> Option<ThreadIdentity> {
        let thread = self.selected_entry()?;
        Some(ThreadIdentity {
            thread_id: thread.thread_id,
            rollout_path: thread.rollout_path.clone(),
        })
    }

    fn selected_row_index(&self) -> Option<usize> {
        let thread_index = *self.catalog.ordered_threads.get(self.selected_thread)?;
        self.catalog.row_by_thread.get(thread_index).copied()
    }

    fn ensure_scroll(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            self.scroll = 0;
            return;
        }
        let Some(selected_row) = self.selected_row_index() else {
            self.scroll = 0;
            return;
        };

        if selected_row < self.scroll {
            self.scroll = selected_row;
        } else if selected_row >= self.scroll + visible_rows {
            self.scroll = selected_row + 1 - visible_rows;
        }
    }

    async fn refresh_selected_detail(&mut self) -> Result<()> {
        let Some(thread) = self.selected_entry() else {
            self.detail_dirty = false;
            self.detail_load_due_at = None;
            return Ok(());
        };

        let path = thread.rollout_path.clone();
        if self.detail_cache.contains_key(&path) {
            self.detail_dirty = false;
            self.detail_load_due_at = None;
            return Ok(());
        }

        let detail_state = match Box::pin(build_session_summary(&self.config, path.as_path())).await
        {
            Ok(summary) => DetailState::Loaded(Box::new(summary)),
            Err(error) => DetailState::Failed(error.to_string()),
        };
        self.detail_cache.insert(path, detail_state);
        self.detail_dirty = false;
        self.detail_load_due_at = None;
        Ok(())
    }

    async fn refresh_selected_detail_if_due(&mut self) -> Result<()> {
        if self
            .detail_load_due_at
            .is_some_and(|deadline| self.detail_dirty && deadline <= Instant::now())
        {
            self.refresh_selected_detail().await?;
        }
        Ok(())
    }

    fn schedule_selected_detail_refresh(&mut self) {
        let Some(thread) = self.selected_entry() else {
            self.detail_dirty = false;
            self.detail_load_due_at = None;
            return;
        };

        if self.detail_cache.contains_key(&thread.rollout_path) {
            self.detail_dirty = false;
            self.detail_load_due_at = None;
            return;
        }

        self.detail_dirty = true;
        self.detail_load_due_at = Some(Instant::now() + DETAIL_LOAD_DEBOUNCE);
    }

    fn poll_timeout(&self) -> Duration {
        let default_timeout = Duration::from_millis(250);
        match self.detail_load_due_at {
            Some(deadline) if self.detail_dirty => deadline
                .saturating_duration_since(Instant::now())
                .min(default_timeout),
            _ => default_timeout,
        }
    }

    fn selected_detail(&self) -> Option<&DetailState> {
        let thread = self.selected_entry()?;
        self.detail_cache.get(&thread.rollout_path)
    }

    fn loaded_status_message(&self) -> String {
        match self.language {
            Language::English => format!(
                "Loaded {} threads across {} providers ({} active, {} archived)",
                self.catalog.threads.len(),
                self.catalog.provider_count,
                self.catalog.active_count,
                self.catalog.archived_count
            ),
            Language::Chinese => format!(
                "已加载 {} 个线程，来自 {} 个 provider（活跃 {}，归档 {}）",
                self.catalog.threads.len(),
                self.catalog.provider_count,
                self.catalog.active_count,
                self.catalog.archived_count
            ),
        }
    }

    fn language_switched_message(&self) -> String {
        match self.language {
            Language::English => "Language switched to English".to_string(),
            Language::Chinese => "界面语言已切换为中文".to_string(),
        }
    }

    fn input_error_message(&self, error: &str) -> String {
        match self.language {
            Language::English => format!("Input error: {error}"),
            Language::Chinese => format!("输入错误：{error}"),
        }
    }
}

impl Action {
    fn all() -> &'static [Action] {
        &[
            Action::Show,
            Action::Rename,
            Action::Repair,
            Action::RewriteMeta,
            Action::RepairResumeState,
            Action::Fork,
            Action::Archive,
            Action::Unarchive,
            Action::CopySessionId,
            Action::CopyCwd,
            Action::CopyRolloutPath,
            Action::CopyDeeplink,
            Action::Compact,
            Action::Rollback,
            Action::Migrate,
            Action::Smart,
            Action::Distill,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Action::Show => "show",
            Action::Rename => "rename",
            Action::Repair => "repair",
            Action::RewriteMeta => "rewrite-meta",
            Action::RepairResumeState => "repair-resume-state",
            Action::Fork => "fork",
            Action::Archive => "archive",
            Action::Unarchive => "unarchive",
            Action::CopySessionId => "copy-session-id",
            Action::CopyCwd => "copy-cwd",
            Action::CopyRolloutPath => "copy-rollout-path",
            Action::CopyDeeplink => "copy-deeplink",
            Action::Compact => "compact",
            Action::Rollback => "rollback",
            Action::Migrate => "migrate",
            Action::Smart => "smart",
            Action::Distill => "distill",
        }
    }

    fn description(self, language: Language) -> &'static str {
        match (self, language) {
            (Action::Show, Language::English) => "Inspect derived metadata for this thread",
            (Action::Rename, Language::English) => {
                "Append a new thread title into session_index.jsonl"
            }
            (Action::Repair, Language::English) => "Reconcile SQLite metadata from rollout history",
            (Action::RewriteMeta, Language::English) => "Rewrite the first SessionMeta record",
            (Action::RepairResumeState, Language::English) => {
                "Rewrite persisted context-window hints in rollout events"
            }
            (Action::Fork, Language::English) => {
                "Fork this thread with provider/model/runtime overrides"
            }
            (Action::Archive, Language::English) => "Move this rollout into archived storage",
            (Action::Unarchive, Language::English) => {
                "Restore this rollout back into active storage"
            }
            (Action::CopySessionId, Language::English) => "Copy the resolved thread id",
            (Action::CopyCwd, Language::English) => "Copy the recorded working directory",
            (Action::CopyRolloutPath, Language::English) => "Copy the resolved rollout path",
            (Action::CopyDeeplink, Language::English) => "Copy the canonical codex resume command",
            (Action::Compact, Language::English) => "Trigger native Codex compaction",
            (Action::Rollback, Language::English) => "Drop the last N user turns",
            (Action::Migrate, Language::English) => {
                "Compact if needed, then fork to a new runtime shape"
            }
            (Action::Smart, Language::English) => {
                "Guided provider/model switch with automatic runtime repair"
            }
            (Action::Distill, Language::English) => {
                "Create a lighter successor session from a heavy source thread"
            }
            (Action::Show, Language::Chinese) => "查看这个线程的派生摘要信息",
            (Action::Rename, Language::Chinese) => "向 session_index.jsonl 追加新的线程标题",
            (Action::Repair, Language::Chinese) => "根据 rollout 历史修复 SQLite 元数据",
            (Action::RewriteMeta, Language::Chinese) => "重写第一条 SessionMeta 记录",
            (Action::RepairResumeState, Language::Chinese) => {
                "重写 rollout 事件中的持久化上下文窗口提示"
            }
            (Action::Fork, Language::Chinese) => "按新的 provider/model/runtime 参数 fork 线程",
            (Action::Archive, Language::Chinese) => "把 rollout 移入 archived 存储",
            (Action::Unarchive, Language::Chinese) => "把 archived rollout 恢复到 active 存储",
            (Action::CopySessionId, Language::Chinese) => "复制解析后的 thread id",
            (Action::CopyCwd, Language::Chinese) => "复制记录的工作目录",
            (Action::CopyRolloutPath, Language::Chinese) => "复制解析后的 rollout 路径",
            (Action::CopyDeeplink, Language::Chinese) => "复制标准的 codex resume 命令",
            (Action::Compact, Language::Chinese) => "触发原生 Codex compact",
            (Action::Rollback, Language::Chinese) => "丢弃最后 N 个用户轮次",
            (Action::Migrate, Language::Chinese) => "按需 compact 后迁移到新的运行时形态",
            (Action::Smart, Language::Chinese) => {
                "通过向导切换 provider/model，并自动修复运行时状态"
            }
            (Action::Distill, Language::Chinese) => "从重会话里提炼出一个更轻的继任会话",
        }
    }

    fn input_kind(self) -> ActionInputKind {
        match self {
            Action::Show
            | Action::Repair
            | Action::Archive
            | Action::Unarchive
            | Action::CopySessionId
            | Action::CopyCwd
            | Action::CopyRolloutPath
            | Action::CopyDeeplink => ActionInputKind::None,
            Action::Rename => ActionInputKind::Text { required: true },
            Action::RewriteMeta => ActionInputKind::Raw { required: true },
            Action::RepairResumeState => ActionInputKind::Raw { required: false },
            Action::Fork => ActionInputKind::Raw { required: false },
            Action::Compact => ActionInputKind::Raw { required: false },
            Action::Rollback => ActionInputKind::Raw { required: true },
            Action::Migrate => ActionInputKind::Raw { required: false },
            Action::Smart => ActionInputKind::None,
            Action::Distill => ActionInputKind::Raw { required: false },
        }
    }

    fn example(self, language: Language) -> Option<&'static str> {
        match (self, language) {
            (Action::Rename, Language::English) => Some("Provider migration / switch to 256k"),
            (Action::Rename, Language::Chinese) => Some("Provider 迁移 / 切到 256k"),
            (Action::RewriteMeta, _) => {
                Some("--provider yunyi --cwd D:\\Dev\\self\\project --clear-memory-mode")
            }
            (Action::RepairResumeState, _) => Some("--context-window 258400"),
            (Action::Fork, _) => Some(
                "--provider yunyi --model gpt-5.2 --context-window 258400 --thread-name \"forked 256k\"",
            ),
            (Action::Compact, _) => Some("--timeout-secs 600"),
            (Action::Rollback, _) => Some("1 --timeout-secs 120"),
            (Action::Migrate, _) => Some(
                "--provider yunyi --model gpt-5.2 --context-window 258400 --write-profile yunyi-256k --archive-source",
            ),
            (Action::Distill, _) => Some("--preview-only"),
            (Action::Show, _)
            | (Action::Repair, _)
            | (Action::Archive, _)
            | (Action::Unarchive, _)
            | (Action::CopySessionId, _)
            | (Action::CopyCwd, _)
            | (Action::CopyRolloutPath, _)
            | (Action::CopyDeeplink, _)
            | (Action::Smart, _) => None,
        }
    }

    fn is_available(self, archived: bool) -> bool {
        match self {
            Action::Archive => !archived,
            Action::Unarchive => archived,
            Action::Compact | Action::Rollback => !archived,
            _ => true,
        }
    }
}

async fn load_catalog(config: &Config) -> Result<Catalog> {
    let active_threads = collect_threads(config, false).await?;
    let archived_threads = collect_threads(config, true).await?;

    let mut ids = HashSet::new();
    for listed in active_threads.iter().chain(archived_threads.iter()) {
        if let Some(thread_id) = listed.item.thread_id.as_ref() {
            ids.insert(*thread_id);
        }
    }
    let names = find_thread_names_by_ids(&config.codex_home, &ids)
        .await
        .context("failed to load thread titles from session_index.jsonl")?;

    let default_provider = config.model_provider_id.as_str();
    let mut threads = Vec::with_capacity(active_threads.len() + archived_threads.len());
    for listed in active_threads.into_iter().chain(archived_threads) {
        threads.push(ThreadEntry::from_thread_item(
            listed.item,
            listed.archived,
            default_provider,
            &names,
        ));
    }

    Ok(Catalog::new(threads))
}

#[derive(Debug)]
struct ListedThread {
    item: ThreadItem,
    archived: bool,
}

async fn collect_threads(config: &Config, archived: bool) -> Result<Vec<ListedThread>> {
    let mut cursor = None;
    let mut threads = Vec::new();
    let default_provider = config.model_provider_id.clone();

    loop {
        let page = if archived {
            RolloutRecorder::list_archived_threads(
                config,
                THREAD_PAGE_SIZE,
                cursor.as_ref(),
                ThreadSortKey::UpdatedAt,
                INTERACTIVE_SESSION_SOURCES,
                None,
                default_provider.as_str(),
                None,
            )
            .await?
        } else {
            RolloutRecorder::list_threads(
                config,
                THREAD_PAGE_SIZE,
                cursor.as_ref(),
                ThreadSortKey::UpdatedAt,
                INTERACTIVE_SESSION_SOURCES,
                None,
                default_provider.as_str(),
                None,
            )
            .await?
        };

        threads.extend(
            page.items
                .into_iter()
                .map(|item| ListedThread { item, archived }),
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(threads)
}

async fn execute_prepared_command(
    terminal: &mut TerminalSession,
    prepared: PreparedCommand,
    language: Language,
) -> Result<String> {
    terminal.suspend()?;

    println!("> {}", prepared.preview);
    let status = match run_command(prepared.command).await {
        Ok(()) => match language {
            Language::English => format!("Completed {}", prepared.action.label()),
            Language::Chinese => format!("已完成 {}", prepared.action.label()),
        },
        Err(error) => {
            eprintln!("{error:?}");
            match language {
                Language::English => format!("Failed {}: {error}", prepared.action.label()),
                Language::Chinese => format!("执行失败 {}：{error}", prepared.action.label()),
            }
        }
    };

    println!();
    println!(
        "{}",
        match language {
            Language::English => "Press Enter to return to the TUI...",
            Language::Chinese => "按 Enter 返回 TUI……",
        }
    );
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    terminal.resume()?;
    Ok(status)
}

fn prepare_command(action: Action, thread: &ThreadEntry, input: &str) -> Result<PreparedCommand> {
    let argv = build_command_argv(action, thread, input)?;
    let cli = Cli::try_parse_from(argv.clone()).context("failed to parse TUI action input")?;
    let command = cli.command.context("missing subcommand from TUI action")?;
    let preview = argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(PreparedCommand {
        action,
        command,
        preview,
    })
}

fn build_command_argv(action: Action, thread: &ThreadEntry, input: &str) -> Result<Vec<String>> {
    let mut argv = vec![
        "codex-session-manager".to_string(),
        action.label().to_string(),
        thread.target(),
    ];
    match action.input_kind() {
        ActionInputKind::None => {}
        ActionInputKind::Text { required } => {
            let value = input.trim();
            if required && value.is_empty() {
                bail!("{} requires text input", action.label());
            }
            if !value.is_empty() {
                argv.push(value.to_string());
            }
        }
        ActionInputKind::Raw { required } => {
            let trimmed = input.trim();
            if required && trimmed.is_empty() {
                bail!("{} requires additional arguments", action.label());
            }
            if !trimmed.is_empty() {
                argv.extend(shell_words::split(trimmed)?);
            }
        }
    }
    Ok(argv)
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    interactive: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self {
            terminal,
            interactive: true,
        })
    }

    fn draw(&mut self, app: &mut AppState) -> Result<()> {
        self.terminal.draw(|frame| draw_app(frame, app))?;
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        if !self.interactive {
            return Ok(());
        }
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show)?;
        self.interactive = false;
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        if self.interactive {
            return Ok(());
        }
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen, Hide)?;
        self.terminal.clear()?;
        self.interactive = true;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.interactive {
            let _ = disable_raw_mode();
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        }
    }
}

fn draw_app(frame: &mut Frame<'_>, app: &mut AppState) {
    if let Mode::Result(result) = &app.mode {
        draw_result_page(frame, app, result);
        return;
    }

    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(frame.area());

    draw_header(frame, areas[0], app);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(44), Constraint::Percentage(56)])
        .split(areas[1]);
    draw_thread_list(frame, body[0], app);
    draw_thread_details(frame, body[1], app);
    draw_footer(frame, areas[2], app);

    match &app.mode {
        Mode::Actions { selected } => draw_actions_overlay(frame, app, *selected),
        Mode::Prompt(prompt) => draw_prompt_overlay(frame, app, prompt),
        Mode::Browsing => {}
        Mode::Result(_) => {}
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let status = match app.language {
        Language::English => format!(
            "codex-session-manager TUI · {} threads · {} providers · {} (F2)",
            app.catalog.threads.len(),
            app.catalog.provider_count,
            app.language.label()
        ),
        Language::Chinese => format!(
            "codex-session-manager TUI · {} 个线程 · {} 个 provider · {}（F2）",
            app.catalog.threads.len(),
            app.catalog.provider_count,
            app.language.label()
        ),
    };
    frame.render_widget(Paragraph::new(status), area);
}

fn draw_thread_list(frame: &mut Frame<'_>, area: Rect, app: &mut AppState) {
    let block = Block::default()
        .title(match app.language {
            Language::English => "Threads by provider",
            Language::Chinese => "按 provider 分组的线程",
        })
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let visible_rows = inner.height as usize;
    app.ensure_scroll(visible_rows);

    if app.catalog.rows.is_empty() {
        let empty = Paragraph::new(match app.language {
            Language::English => "No interactive Codex threads found under the current CODEX_HOME.",
            Language::Chinese => "当前 CODEX_HOME 下没有找到交互式 Codex 线程。",
        })
        .wrap(Wrap { trim: false });
        frame.render_widget(empty, inner);
        return;
    }

    let start = app.scroll.min(app.catalog.rows.len());
    let end = (start + visible_rows).min(app.catalog.rows.len());

    let items = app.catalog.rows[start..end]
        .iter()
        .map(|row| match row {
            CatalogRow::Header { provider, count } => ListItem::new(Line::from(vec![
                provider.clone().cyan().bold(),
                format!(" ({count})").dim(),
            ])),
            CatalogRow::Thread(thread_index) => {
                let thread = &app.catalog.threads[*thread_index];
                let selected = app
                    .catalog
                    .ordered_threads
                    .get(app.selected_thread)
                    .is_some_and(|selected_index| selected_index == thread_index);
                let mut spans = Vec::new();
                if selected {
                    spans.push("› ".green().bold());
                } else {
                    spans.push("  ".into());
                }
                if thread.archived {
                    spans.push(
                        match app.language {
                            Language::English => "[archived] ",
                            Language::Chinese => "[已归档] ",
                        }
                        .yellow(),
                    );
                }
                spans.push(truncate_text(thread.title.as_str(), 48).into());
                if let Some(updated) = thread.updated_label() {
                    spans.push(" · ".dim());
                    spans.push(short_timestamp(updated).dim());
                }
                let line = if selected {
                    Line::from(spans).reversed()
                } else {
                    Line::from(spans)
                };
                ListItem::new(line)
            }
        })
        .collect::<Vec<_>>();

    frame.render_widget(List::new(items), inner);
}

fn draw_thread_details(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let block = Block::default()
        .title(match app.language {
            Language::English => "Thread details",
            Language::Chinese => "线程详情",
        })
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(thread) = app.selected_entry() else {
        frame.render_widget(
            Paragraph::new(match app.language {
                Language::English => "Select a thread to inspect it.",
                Language::Chinese => "选择一个线程以查看详情。",
            }),
            inner,
        );
        return;
    };

    let mut details = vec![
        Line::from(vec![
            field_label(app.language, "Title", "标题"),
            thread.title.clone().into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Index name", "索引标题"),
            thread
                .thread_name
                .clone()
                .unwrap_or_else(|| not_set_text(app.language))
                .into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Provider", "Provider"),
            thread.provider.clone().into(),
        ]),
        Line::from(vec![
            field_label(app.language, "State", "状态"),
            thread.state_label(app.language).into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Thread ID", "线程 ID"),
            thread
                .thread_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| unknown_text(app.language))
                .into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Created", "创建时间"),
            thread
                .created_at
                .clone()
                .unwrap_or_else(|| unknown_text(app.language))
                .into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Updated", "更新时间"),
            thread
                .updated_at
                .clone()
                .or_else(|| thread.created_at.clone())
                .unwrap_or_else(|| unknown_text(app.language))
                .into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Source", "来源"),
            thread
                .source
                .clone()
                .unwrap_or_else(|| unknown_text(app.language))
                .into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Path", "路径"),
            thread.rollout_path.display().to_string().into(),
        ]),
        Line::from(vec![
            field_label(app.language, "Cwd", "工作目录"),
            thread
                .cwd
                .as_ref()
                .map(|cwd| cwd.display().to_string())
                .unwrap_or_else(|| unknown_text(app.language))
                .into(),
        ]),
    ];

    match app.selected_detail() {
        Some(DetailState::Loaded(summary)) => {
            details.extend([
                Line::from(vec![
                    field_label(app.language, "Model", "模型"),
                    summary
                        .latest_model
                        .clone()
                        .unwrap_or_else(|| unknown_text(app.language))
                        .into(),
                ]),
                Line::from(vec![
                    field_label(app.language, "Context window", "上下文窗口"),
                    format_optional_i64(summary.latest_model_context_window, app.language).into(),
                ]),
                Line::from(vec![
                    field_label(app.language, "Context tokens", "上下文 tokens"),
                    format_optional_i64(summary.latest_context_tokens, app.language).into(),
                ]),
                Line::from(vec![
                    field_label(app.language, "Session total tokens", "会话总 tokens"),
                    format_optional_i64(summary.latest_total_tokens, app.language).into(),
                ]),
                Line::from(vec![
                    field_label(app.language, "User turns", "用户轮次"),
                    summary.user_turns.to_string().into(),
                ]),
                Line::from(vec![
                    field_label(app.language, "Memory mode", "记忆模式"),
                    summary
                        .memory_mode
                        .clone()
                        .unwrap_or_else(|| empty_text(app.language))
                        .into(),
                ]),
                Line::from(vec![
                    field_label(app.language, "Forked from", "派生来源"),
                    summary
                        .forked_from_id
                        .clone()
                        .unwrap_or_else(|| empty_text(app.language))
                        .into(),
                ]),
            ]);
        }
        Some(DetailState::Failed(error)) => {
            details.extend([
                Line::from(""),
                Line::from(localized_heading(
                    app.language,
                    "Runtime summary",
                    "运行时摘要",
                )),
                Line::from(match app.language {
                    Language::English => format!("Failed to load detail: {error}"),
                    Language::Chinese => format!("加载详情失败：{error}"),
                }),
            ]);
        }
        None => {
            details.extend([
                Line::from(""),
                Line::from(localized_heading(
                    app.language,
                    "Runtime summary",
                    "运行时摘要",
                )),
                Line::from(match app.language {
                    Language::English => {
                        "Pause on this thread briefly to load runtime summary.".to_string()
                    }
                    Language::Chinese => "在这个线程上停留片刻后会加载运行时摘要。".to_string(),
                }),
            ]);
        }
    }

    details.push(Line::from(""));
    details.push(Line::from(localized_heading(
        app.language,
        "Preview",
        "消息预览",
    )));
    details.push(Line::from(thread.preview.clone().unwrap_or_else(
        || match app.language {
            Language::English => "No first-user-message preview found.".to_string(),
            Language::Chinese => "没有找到首条用户消息预览。".to_string(),
        },
    )));

    frame.render_widget(Paragraph::new(details).wrap(Wrap { trim: false }), inner);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &AppState) {
    let lines = vec![
        Line::from(vec![
            match app.language {
                Language::English => "Keys: ".bold(),
                Language::Chinese => "按键：".bold(),
            },
            "↑/↓".into(),
            footer_text(app.language, " move · ", " 移动 · ").dim(),
            "Enter".into(),
            footer_text(app.language, " actions · ", " 动作 · ").dim(),
            "r".into(),
            footer_text(app.language, " refresh · ", " 刷新 · ").dim(),
            "F2".into(),
            footer_text(app.language, " language · ", " 语言 · ").dim(),
            "q".into(),
            footer_text(app.language, " quit", " 退出").dim(),
        ]),
        status_line(app.status.as_deref().unwrap_or(match app.language {
            Language::English => "Ready",
            Language::Chinese => "就绪",
        })),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_actions_overlay(frame: &mut Frame<'_>, app: &AppState, selected: usize) {
    let popup = centered_rect(82, 92, frame.area());
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(match app.language {
            Language::English => "Thread actions",
            Language::Chinese => "线程动作",
        })
        .borders(Borders::ALL);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let Some(thread) = app.selected_entry() else {
        return;
    };
    let actions = app.available_actions();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(inner);

    let title = vec![
        Line::from(thread.title.clone().bold()),
        Line::from(
            format!(
                "{}{} · {}",
                footer_text(app.language, "Provider: ", "Provider："),
                thread.provider,
                thread.state_label(app.language)
            )
            .dim(),
        ),
    ];
    frame.render_widget(Paragraph::new(title), layout[0]);

    let visible_rows = layout[1].height as usize;
    let (window_start, window_end) = action_window(selected, actions.len(), visible_rows);
    let items = actions[window_start..window_end]
        .iter()
        .enumerate()
        .map(|(visible_index, action)| (window_start + visible_index, action))
        .map(|(index, action)| {
            let prefix = if index == selected {
                "› ".green().bold()
            } else {
                "  ".into()
            };
            let line = if index == selected {
                Line::from(vec![
                    prefix,
                    action.label().to_string().bold(),
                    " · ".dim(),
                    action.description(app.language).into(),
                ])
                .reversed()
            } else {
                Line::from(vec![
                    prefix,
                    action.label().to_string().bold(),
                    " · ".dim(),
                    action.description(app.language).into(),
                ])
            };
            ListItem::new(line)
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), layout[1]);

    let more_before = window_start > 0;
    let more_after = window_end < actions.len();
    let help = Paragraph::new(match app.language {
        Language::English => match (more_before, more_after) {
            (true, true) => "Up/Down scroll more · Enter runs/open prompt · Esc back",
            (true, false) | (false, true) => {
                "Up/Down scroll list · Enter runs/open prompt · Esc back"
            }
            (false, false) => "Enter runs or opens input prompt · Esc goes back",
        },
        Language::Chinese => match (more_before, more_after) {
            (true, true) => "上下继续滚动 · Enter 执行/打开输入框 · Esc 返回",
            (true, false) | (false, true) => "上下滚动列表 · Enter 执行/打开输入框 · Esc 返回",
            (false, false) => "Enter 执行或打开参数输入框 · Esc 返回上一级",
        },
    });
    frame.render_widget(help, layout[2]);
}

fn action_window(selected: usize, total: usize, visible_rows: usize) -> (usize, usize) {
    if total == 0 || visible_rows == 0 || total <= visible_rows {
        return (0, total);
    }

    let half_window = visible_rows / 2;
    let mut start = selected.saturating_sub(half_window);
    let max_start = total.saturating_sub(visible_rows);
    if start > max_start {
        start = max_start;
    }
    let end = (start + visible_rows).min(total);
    (start, end)
}

fn preferred_selection(
    thread_id: Option<&str>,
    rollout_path: Option<&PathBuf>,
) -> Option<ThreadIdentity> {
    thread_id
        .and_then(|id| ThreadId::from_string(id).ok())
        .map(|thread_id| ThreadIdentity {
            thread_id: Some(thread_id),
            rollout_path: rollout_path.cloned().unwrap_or_default(),
        })
        .or_else(|| {
            rollout_path.cloned().map(|rollout_path| ThreadIdentity {
                thread_id: None,
                rollout_path,
            })
        })
}

fn smart_processing_result(language: Language) -> ResultViewState {
    ResultViewState {
        title: localized_heading_text(
            language,
            "Smart Switch In Progress",
            "Smart 切换进行中",
        ),
        lines: vec![match language {
            Language::English => {
                "Executing the selected smart workflow. This may compact, repair, migrate, or distill before the final result page appears.".to_string()
            }
            Language::Chinese => {
                "正在执行已选择的 smart 流程。它可能会进行压缩、修复、迁移或提炼，完成后会停留在结果页。".to_string()
            }
        }],
        scroll: 0,
    }
}

fn distill_processing_result(language: Language) -> ResultViewState {
    ResultViewState {
        title: localized_heading_text(language, "Distillation In Progress", "提炼进行中"),
        lines: vec![match language {
            Language::English => {
                "Building a successor-session handoff. Codex-backed distillation may take longer than deterministic mode.".to_string()
            }
            Language::Chinese => {
                "正在生成继任会话 handoff。Codex 提炼模式通常会比规则提炼更慢。".to_string()
            }
        }],
        scroll: 0,
    }
}

fn error_result_state(
    language: Language,
    english_title: &str,
    chinese_title: &str,
    error: &anyhow::Error,
) -> ResultViewState {
    let mut lines = vec![match language {
        Language::English => "The operation failed. Error chain:".to_string(),
        Language::Chinese => "操作失败。错误链如下：".to_string(),
    }];
    lines.extend(error.chain().enumerate().map(|(index, cause)| {
        if index == 0 {
            format!("1. {cause}")
        } else {
            format!("{}. caused by: {cause}", index + 1)
        }
    }));
    ResultViewState {
        title: localized_heading_text(language, english_title, chinese_title),
        lines,
        scroll: 0,
    }
}

fn localize_known_result_title(language: Language, english_title: &str) -> String {
    match language {
        Language::English => english_title.to_string(),
        Language::Chinese => match english_title {
            "Smart Switch Completed" => "Smart 切换完成".to_string(),
            "Distilled Successor Ready" => "提炼结果已生成".to_string(),
            other => other.to_string(),
        },
    }
}

fn draw_result_page(frame: &mut Frame<'_>, app: &AppState, result: &ResultViewState) {
    frame.render_widget(Clear, frame.area());
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header_block = Block::default()
        .title(match app.language {
            Language::English => "Result",
            Language::Chinese => "结果",
        })
        .borders(Borders::ALL);
    let header_inner = header_block.inner(areas[0]);
    frame.render_widget(header_block, areas[0]);
    frame.render_widget(Paragraph::new(result.title.clone()), header_inner);

    let body_block = Block::default()
        .title(match app.language {
            Language::English => "Output",
            Language::Chinese => "输出",
        })
        .borders(Borders::ALL);
    let body_inner = body_block.inner(areas[1]);
    frame.render_widget(body_block, areas[1]);
    if body_inner.height == 0 {
        return;
    }
    let visible_rows = body_inner.height as usize;
    let start = result
        .scroll
        .min(result.lines.len().saturating_sub(visible_rows));
    let end = (start + visible_rows).min(result.lines.len());
    let lines = result.lines[start..end]
        .iter()
        .cloned()
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_inner);

    let footer_block = Block::default()
        .title(match app.language {
            Language::English => "Keys",
            Language::Chinese => "按键",
        })
        .borders(Borders::ALL);
    let footer_inner = footer_block.inner(areas[2]);
    frame.render_widget(footer_block, areas[2]);
    let footer_lines = vec![
        Line::from(match app.language {
            Language::English => "↑/↓ scroll · Enter/Esc back to list · q quit".to_string(),
            Language::Chinese => "↑/↓ 滚动 · Enter/Esc 返回列表 · q 退出".to_string(),
        }),
        Line::from(match app.language {
            Language::English => {
                if result.lines.is_empty() {
                    "No output lines".to_string()
                } else {
                    format!(
                        "Showing lines {}-{} of {}",
                        start + 1,
                        end,
                        result.lines.len()
                    )
                }
            }
            Language::Chinese => {
                if result.lines.is_empty() {
                    "当前没有输出行".to_string()
                } else {
                    format!(
                        "当前显示第 {}-{} 行，共 {} 行",
                        start + 1,
                        end,
                        result.lines.len()
                    )
                }
            }
        })
        .dim(),
    ];
    frame.render_widget(Paragraph::new(footer_lines), footer_inner);
}

fn draw_prompt_overlay(frame: &mut Frame<'_>, app: &AppState, prompt: &PromptState) {
    let popup = centered_rect(80, 58, frame.area());
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(match app.language {
            Language::English => format!("{} input", prompt.action.label()),
            Language::Chinese => format!("{} 输入", prompt.action.label()),
        })
        .borders(Borders::ALL);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let Some(thread) = app.selected_entry() else {
        return;
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);

    let header = vec![
        Line::from(thread.title.clone().bold()),
        Line::from(prompt.action.description(app.language)),
        Line::from(
            prompt
                .action
                .example(app.language)
                .map(|example| match app.language {
                    Language::English => format!("Example: {example}"),
                    Language::Chinese => format!("示例：{example}"),
                })
                .unwrap_or_else(|| match app.language {
                    Language::English => "Press Enter to confirm or Esc to cancel".to_string(),
                    Language::Chinese => "按 Enter 确认，按 Esc 取消".to_string(),
                })
                .dim(),
        ),
    ];
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), layout[0]);

    let input_block = Block::default()
        .title(match app.language {
            Language::English => "Input",
            Language::Chinese => "输入",
        })
        .borders(Borders::ALL);
    let input_inner = input_block.inner(layout[1]);
    frame.render_widget(input_block, layout[1]);
    let visible_input = tail_text(
        prompt.input.as_str(),
        input_inner.width.saturating_sub(1) as usize,
    );
    frame.render_widget(Paragraph::new(visible_input.clone()), input_inner);
    frame.set_cursor_position((input_inner.x + visible_input.len() as u16, input_inner.y));

    let hint = match prompt.action.input_kind() {
        ActionInputKind::None => match app.language {
            Language::English => "No extra input required".to_string(),
            Language::Chinese => "这个动作不需要额外输入".to_string(),
        },
        ActionInputKind::Text { .. } => match app.language {
            Language::English => "Text mode: the whole line becomes one argument".to_string(),
            Language::Chinese => "文本模式：整行会作为一个参数传入".to_string(),
        },
        ActionInputKind::Raw { required: false } => match app.language {
            Language::English => {
                "Raw mode: append extra CLI args, or leave blank for defaults".to_string()
            }
            Language::Chinese => "原始参数模式：追加 CLI 参数；留空则使用默认值".to_string(),
        },
        ActionInputKind::Raw { required: true } => match app.language {
            Language::English => {
                "Raw mode: enter the same extra args you would pass on the CLI".to_string()
            }
            Language::Chinese => "原始参数模式：输入你在 CLI 里会追加的参数".to_string(),
        },
    };
    frame.render_widget(Paragraph::new(hint).wrap(Wrap { trim: false }), layout[2]);
    frame.render_widget(
        Paragraph::new(match app.language {
            Language::English => "Enter confirm · Esc cancel · Backspace delete",
            Language::Chinese => "Enter 确认 · Esc 取消 · Backspace 删除",
        }),
        layout[3],
    );
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn status_line(message: &str) -> Line<'static> {
    if message.starts_with("Failed")
        || message.starts_with("Input error")
        || message.starts_with("执行失败")
        || message.starts_with("输入错误")
        || message.starts_with("加载详情失败")
    {
        Line::from(message.to_string().red())
    } else {
        Line::from(message.to_string().dim())
    }
}

fn field_label(
    language: Language,
    english: &'static str,
    chinese: &'static str,
) -> ratatui::text::Span<'static> {
    match language {
        Language::English => format!("{english}: ").bold(),
        Language::Chinese => format!("{chinese}：").bold(),
    }
}

fn localized_heading(
    language: Language,
    english: &'static str,
    chinese: &'static str,
) -> ratatui::text::Span<'static> {
    match language {
        Language::English => english.bold(),
        Language::Chinese => chinese.bold(),
    }
}

fn localized_heading_text(language: Language, english: &str, chinese: &str) -> String {
    match language {
        Language::English => english.to_string(),
        Language::Chinese => chinese.to_string(),
    }
}

fn unknown_text(language: Language) -> String {
    match language {
        Language::English => "unknown".to_string(),
        Language::Chinese => "未知".to_string(),
    }
}

fn empty_text(language: Language) -> String {
    match language {
        Language::English => "".to_string(),
        Language::Chinese => "无".to_string(),
    }
}

fn not_set_text(language: Language) -> String {
    match language {
        Language::English => "not set".to_string(),
        Language::Chinese => "未设置".to_string(),
    }
}

fn footer_text(language: Language, english: &'static str, chinese: &'static str) -> &'static str {
    match language {
        Language::English => english,
        Language::Chinese => chinese,
    }
}

fn format_optional_i64(value: Option<i64>, language: Language) -> String {
    value
        .map(|number| number.to_string())
        .unwrap_or_else(|| unknown_text(language))
}

fn derive_thread_title(
    thread_name: Option<&str>,
    preview: Option<&str>,
    thread_id: Option<ThreadId>,
    rollout_path: &Path,
) -> String {
    if let Some(name) = thread_name.map(clean_text).filter(|name| !name.is_empty()) {
        return name;
    }
    if let Some(preview) = preview
        .map(clean_text)
        .filter(|preview| !preview.is_empty())
    {
        return truncate_text(preview.as_str(), 72);
    }
    if let Some(thread_id) = thread_id {
        return thread_id.to_string();
    }
    rollout_path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| rollout_path.display().to_string())
}

fn clean_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars && max_chars >= 1 {
        truncated.pop();
        truncated.push('…');
    }
    truncated
}

fn tail_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    chars[chars.len() - max_chars..].iter().collect()
}

fn short_timestamp(timestamp: &str) -> String {
    let compact = timestamp.get(..16).unwrap_or(timestamp).replace('T', " ");
    compact.trim_end_matches('Z').to_string()
}

fn format_session_source(source: &SessionSource) -> String {
    serde_json::to_string(source)
        .unwrap_or_else(|_| "unknown".to_string())
        .trim_matches('"')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::Action;
    use super::AppState;
    use super::Catalog;
    use super::DetailState;
    use super::Language;
    use super::ThreadEntry;
    use super::action_window;
    use super::build_command_argv;
    use super::derive_thread_title;
    use codex_core::config::ConfigBuilder;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn sample_thread() -> ThreadEntry {
        ThreadEntry {
            thread_id: ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").ok(),
            rollout_path: PathBuf::from("D:/codex/rollout.jsonl"),
            provider: "openai".to_string(),
            archived: false,
            title: "Sample".to_string(),
            thread_name: Some("Sample".to_string()),
            preview: Some("preview".to_string()),
            cwd: None,
            source: Some("cli".to_string()),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn derive_thread_title_prefers_thread_name() {
        let thread_id = ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").ok();
        let title = derive_thread_title(
            Some("  标题  "),
            Some("first user message"),
            thread_id,
            PathBuf::from("D:/codex/rollout.jsonl").as_path(),
        );
        assert_eq!(title, "标题");
    }

    #[test]
    fn build_command_argv_keeps_rename_text_as_single_argument() {
        let argv = build_command_argv(Action::Rename, &sample_thread(), "迁移到 yunyi")
            .expect("build command argv");
        assert_eq!(
            argv,
            vec![
                "codex-session-manager".to_string(),
                "rename".to_string(),
                "019cd66f-f4ea-7022-802b-7007c11cea97".to_string(),
                "迁移到 yunyi".to_string(),
            ]
        );
    }

    #[test]
    fn build_command_argv_splits_raw_flags_with_shell_rules() {
        let argv = build_command_argv(
            Action::Migrate,
            &sample_thread(),
            "--provider yunyi --thread-name \"迁移 256k\"",
        )
        .expect("build command argv");
        assert_eq!(
            argv,
            vec![
                "codex-session-manager".to_string(),
                "migrate".to_string(),
                "019cd66f-f4ea-7022-802b-7007c11cea97".to_string(),
                "--provider".to_string(),
                "yunyi".to_string(),
                "--thread-name".to_string(),
                "迁移 256k".to_string(),
            ]
        );
    }

    #[test]
    fn language_detection_maps_zh_locale_to_chinese() {
        assert_eq!(Language::from_locale_tag("zh-CN"), Language::Chinese);
        assert_eq!(Language::from_locale_tag("zh_Hans"), Language::Chinese);
        assert_eq!(Language::from_locale_tag("en-US"), Language::English);
    }

    #[test]
    fn language_toggle_switches_between_english_and_chinese() {
        assert_eq!(Language::English.toggle(), Language::Chinese);
        assert_eq!(Language::Chinese.toggle(), Language::English);
    }

    #[test]
    fn action_window_shows_all_when_list_fits() {
        assert_eq!(action_window(0, 5, 10), (0, 5));
    }

    #[test]
    fn action_window_scrolls_to_keep_selected_visible() {
        assert_eq!(action_window(14, 15, 10), (5, 15));
        assert_eq!(action_window(9, 15, 10), (4, 14));
    }

    #[tokio::test]
    async fn schedule_selected_detail_refresh_debounces_missing_detail() {
        let temp = tempdir().expect("tempdir");
        let config = ConfigBuilder::default()
            .codex_home(temp.path().to_path_buf())
            .build()
            .await
            .expect("build config");
        let mut app = AppState::new(config);
        app.catalog = Catalog::new(vec![sample_thread()]);

        app.schedule_selected_detail_refresh();

        assert!(app.detail_dirty);
        assert!(app.detail_load_due_at.is_some());
    }

    #[tokio::test]
    async fn schedule_selected_detail_refresh_skips_cached_detail() {
        let temp = tempdir().expect("tempdir");
        let config = ConfigBuilder::default()
            .codex_home(temp.path().to_path_buf())
            .build()
            .await
            .expect("build config");
        let mut app = AppState::new(config);
        let thread = sample_thread();
        app.catalog = Catalog::new(vec![thread.clone()]);
        app.detail_cache.insert(
            thread.rollout_path.clone(),
            DetailState::Failed("cached".to_string()),
        );

        app.schedule_selected_detail_refresh();

        assert!(!app.detail_dirty);
        assert!(app.detail_load_due_at.is_none());
    }
}
