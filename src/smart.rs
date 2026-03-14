use crate::cli::Command;
use crate::cli::CompactArgs;
use crate::cli::DistillArgs;
use crate::cli::MigrateArgs;
use crate::cli::RepairResumeStateArgs;
use crate::cli::SmartArgs;
use crate::distill;
use crate::progress::OperationProgressEvent;
use crate::progress::ProgressSender;
use crate::progress::SmartProgressEvent;
use crate::progress::SmartStrategyKind;
use crate::progress::emit_progress;
use crate::run_command;
use crate::runtime::load_runtime_config;
use crate::runtime::render_profiled_resume_command;
use crate::runtime::resolve_target;
use crate::runtime::write_profile_from_config;
use crate::summary::build_session_summary;
use crate::summary::is_archived_rollout;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::AuthManager;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_core::RolloutRecorder;
use codex_core::ThreadManager;
use codex_core::ThreadSortKey;
use codex_core::config::CONFIG_TOML_FILE;
use codex_core::config::Config;
use codex_core::config::ConfigToml;
use codex_core::features::Feature;
use codex_core::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_core::models_manager::manager::RefreshStrategy;
use codex_core::read_session_meta_line;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::protocol::SessionSource;
use crossterm::cursor::Hide;
use crossterm::cursor::Show;
use crossterm::event::Event;
use crossterm::event::KeyCode;
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
use ratatui::style::Stylize as _;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::collections::BTreeSet;
use std::io;
use std::io::Stdout;
use std::path::Path;
use std::time::Duration;

const PICKER_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub(crate) async fn run(args: SmartArgs) -> Result<()> {
    let Some(result) = execute(args).await? else {
        return Ok(());
    };
    for line in &result.lines {
        println!("{line}");
    }
    Ok(())
}

pub(crate) async fn execute(args: SmartArgs) -> Result<Option<SmartExecutionOutput>> {
    let prepared = prepare_execution(args).await?;
    let Some(selection) = pick_selection(&prepared).await? else {
        return Ok(None);
    };

    execute_prepared(prepared, selection).await.map(Some)
}

pub(crate) async fn prepare_execution(args: SmartArgs) -> Result<PreparedSmartExecution> {
    if args.max_pre_compactions == 0 {
        bail!("max_pre_compactions must be >= 1");
    }

    let resolved = resolve_target(&args.target).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let current_provider = summary
        .session_provider
        .clone()
        .unwrap_or_else(|| resolved.config.model_provider_id.clone());
    let current_model = summary
        .latest_model
        .clone()
        .or(resolved.config.model.clone())
        .unwrap_or_default();
    let global_config = load_global_config_toml(resolved.config.codex_home.as_path())?;
    Ok(PreparedSmartExecution {
        args,
        source_config: resolved.config,
        summary,
        current_provider: current_provider.clone(),
        current_model: current_model.clone(),
        provider_choices: collect_provider_choices(&global_config, current_provider.as_str()),
    })
}

pub(crate) async fn pick_selection(
    prepared: &PreparedSmartExecution,
) -> Result<Option<SmartSelection>> {
    SmartPicker::run(SmartContext {
        args: &prepared.args,
        summary: &prepared.summary,
        current_provider: prepared.current_provider.clone(),
        current_model: prepared.current_model.clone(),
        provider_choices: prepared.provider_choices.clone(),
    })
    .await
}

pub(crate) async fn execute_prepared(
    prepared: PreparedSmartExecution,
    selection: SmartSelection,
) -> Result<SmartExecutionOutput> {
    execute_prepared_with_progress(prepared, selection, None).await
}

pub(crate) async fn execute_prepared_with_progress(
    prepared: PreparedSmartExecution,
    selection: SmartSelection,
    progress: Option<ProgressSender>,
) -> Result<SmartExecutionOutput> {
    execute_selection(
        prepared.args,
        prepared.source_config,
        prepared.summary,
        selection,
        progress,
    )
    .await
}

async fn execute_selection(
    args: SmartArgs,
    source_config: Config,
    mut summary: crate::types::SessionSummary,
    selection: SmartSelection,
    progress: Option<ProgressSender>,
) -> Result<SmartExecutionOutput> {
    let target_args = args.target.clone();
    let current_provider = summary
        .session_provider
        .clone()
        .unwrap_or_else(|| source_config.model_provider_id.clone());
    let current_model = summary
        .latest_model
        .clone()
        .or(source_config.model.clone())
        .unwrap_or_default();
    let target_profile = args.write_profile.clone().unwrap_or_else(|| {
        default_smart_profile_name(
            selection.provider.as_str(),
            selection.model.as_str(),
            selection.target_context_window,
        )
    });
    let strategy = planned_strategy(
        &summary,
        current_provider.as_str(),
        current_model.as_str(),
        &selection,
    );
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Smart(SmartProgressEvent::StrategyConfirmed {
            strategy,
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            target_context_window: selection.target_context_window,
        }),
    );

    if matches!(
        selection.execution_mode,
        SmartExecutionMode::DistillCodex | SmartExecutionMode::DistillDeterministic
    ) {
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Smart(SmartProgressEvent::StartingDistill {
                mode: match selection.execution_mode {
                    SmartExecutionMode::DistillCodex => crate::cli::DistillMode::Codex,
                    SmartExecutionMode::DistillDeterministic => {
                        crate::cli::DistillMode::Deterministic
                    }
                    SmartExecutionMode::Direct => unreachable!(),
                },
            }),
        );
        let output = distill::execute_with_progress(
            DistillArgs {
                target: args.target,
                provider: Some(selection.provider),
                model: Some(selection.model),
                context_window: selection.target_context_window,
                auto_compact_token_limit: selection.target_auto_compact_token_limit,
                distill_mode: match selection.execution_mode {
                    SmartExecutionMode::DistillCodex => crate::cli::DistillMode::Codex,
                    SmartExecutionMode::DistillDeterministic => {
                        crate::cli::DistillMode::Deterministic
                    }
                    SmartExecutionMode::Direct => unreachable!(),
                },
                reasoning_effort: None,
                thread_name: summary.thread_name.clone(),
                write_profile: Some(target_profile.clone()),
                archive_source: args.archive_source,
                preview_only: false,
                json: false,
                recent_turns: 8,
                timeout_secs: args.timeout_secs,
            },
            progress.clone(),
        )
        .await?;
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Smart(SmartProgressEvent::Completed {
                thread_id: output.successor_thread_id.clone(),
            }),
        );
        return Ok(SmartExecutionOutput {
            title: "Distilled Successor Ready".to_string(),
            lines: distill::render_output_lines(&output),
            preferred_thread_id: output.successor_thread_id.clone(),
            preferred_rollout_path: output.successor_rollout_path.clone(),
        });
    }

    if selection.provider == current_provider {
        let mut compactions_run = 0_u32;
        if let Some(target_window) = selection.target_context_window {
            while let Some(context_tokens) = summary.latest_context_tokens {
                emit_progress(
                    progress.as_ref(),
                    OperationProgressEvent::Smart(SmartProgressEvent::CheckingContextWindow {
                        current_tokens: context_tokens,
                        target_window,
                    }),
                );
                if context_tokens <= target_window {
                    break;
                }
                if is_archived_rollout(summary.rollout_path.as_path()) {
                    bail!(
                        "thread is archived and current context {context_tokens} exceeds target window {target_window}; unarchive it before smart switching"
                    );
                }
                if compactions_run >= args.max_pre_compactions {
                    bail!(
                        "thread still exceeds target window after {} compactions (current_context_tokens={context_tokens}, target_window={target_window})",
                        args.max_pre_compactions
                    );
                }
                emit_progress(
                    progress.as_ref(),
                    OperationProgressEvent::Smart(SmartProgressEvent::RunningCompaction {
                        attempt: compactions_run + 1,
                        max_attempts: args.max_pre_compactions,
                    }),
                );
                run_command(Command::Compact(CompactArgs {
                    target: args.target.clone(),
                    timeout_secs: args.timeout_secs,
                }))
                .await?;
                compactions_run += 1;
                emit_progress(
                    progress.as_ref(),
                    OperationProgressEvent::Smart(
                        SmartProgressEvent::ReloadingSummaryAfterCompaction {
                            attempt: compactions_run,
                        },
                    ),
                );
                summary =
                    build_session_summary(&source_config, summary.rollout_path.as_path()).await?;
            }
        }

        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Smart(SmartProgressEvent::WritingProfile {
                profile: target_profile.clone(),
            }),
        );
        write_profile_from_config(target_profile.as_str(), &selection.target_config).await?;
        if selection.should_repair_resume_state(&current_model, summary.latest_model_context_window)
            && let Some(target_window) = selection.target_context_window
        {
            emit_progress(
                progress.as_ref(),
                OperationProgressEvent::Smart(SmartProgressEvent::RepairingResumeState {
                    provider: selection.provider.clone(),
                    model: selection.model.clone(),
                    context_window: Some(target_window),
                }),
            );
            run_command(Command::RepairResumeState(RepairResumeStateArgs {
                target: args.target.clone(),
                context_window: Some(target_window),
                model: Some(selection.model.clone()),
                provider: Some(selection.provider.clone()),
            }))
            .await?;
        }

        let thread_id = read_session_meta_line(summary.rollout_path.as_path())
            .await?
            .meta
            .id;
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Smart(SmartProgressEvent::Completed {
                thread_id: Some(thread_id.to_string()),
            }),
        );
        return Ok(SmartExecutionOutput {
            title: "Smart Switch Completed".to_string(),
            lines: vec![
                "smart_strategy: same-thread".to_string(),
                format!("target_provider: {}", selection.provider),
                format!("target_model: {}", selection.model),
                format!(
                    "target_context_window: {}",
                    selection
                        .target_context_window
                        .map_or_else(String::new, |value| value.to_string())
                ),
                format!("profile: {}", target_profile),
                format!(
                    "resume_command: {}",
                    render_profiled_resume_command(Some(target_profile.as_str()), thread_id)
                ),
            ],
            preferred_thread_id: Some(thread_id.to_string()),
            preferred_rollout_path: Some(summary.rollout_path.clone()),
        });
    }
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Smart(SmartProgressEvent::SnapshottingThreadList),
    );
    let before_thread_ids = {
        let config = source_config.clone();
        let items = RolloutRecorder::list_threads(
            &config,
            500,
            None,
            ThreadSortKey::UpdatedAt,
            INTERACTIVE_SESSION_SOURCES,
            None,
            config.model_provider_id.as_str(),
            None,
        )
        .await?;
        items
            .items
            .into_iter()
            .filter_map(|item| item.thread_id.map(|id| id.to_string()))
            .collect::<std::collections::HashSet<_>>()
    };
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Smart(SmartProgressEvent::RunningMigration),
    );
    run_command(Command::Migrate(MigrateArgs {
        target: target_args.clone(),
        model: Some(selection.model.clone()),
        provider: Some(selection.provider.clone()),
        context_window: selection.target_context_window,
        auto_compact_token_limit: selection.target_auto_compact_token_limit,
        write_profile: Some(target_profile.clone()),
        thread_name: summary.thread_name.clone(),
        persist_extended_history: false,
        nth_user_message: None,
        force_compact: false,
        max_pre_compactions: args.max_pre_compactions,
        archive_source: args.archive_source,
        timeout_secs: args.timeout_secs,
    }))
    .await?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Smart(SmartProgressEvent::RefreshingThreadList),
    );
    let config =
        load_runtime_config(target_args.config_profile, None, None, None, None, None).await?;
    let items = RolloutRecorder::list_threads(
        &config,
        500,
        None,
        ThreadSortKey::UpdatedAt,
        INTERACTIVE_SESSION_SOURCES,
        None,
        config.model_provider_id.as_str(),
        None,
    )
    .await?;
    let successor = items.items.into_iter().find(|item| {
        item.thread_id
            .as_ref()
            .is_some_and(|id| !before_thread_ids.contains(&id.to_string()))
    });
    let preferred_thread_id = successor
        .as_ref()
        .and_then(|item| item.thread_id.as_ref().map(ToString::to_string));
    let preferred_rollout_path = successor.map(|item| item.path);
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Smart(SmartProgressEvent::Completed {
            thread_id: preferred_thread_id.clone(),
        }),
    );
    Ok(SmartExecutionOutput {
        title: "Smart Switch Completed".to_string(),
        lines: vec![
            "smart_strategy: cross-provider-migrate".to_string(),
            format!("target_provider: {}", selection.provider),
            format!("target_model: {}", selection.model),
            format!(
                "target_context_window: {}",
                selection
                    .target_context_window
                    .map_or_else(String::new, |value| value.to_string())
            ),
            format!("profile: {}", target_profile),
            format!("archive_source: {}", args.archive_source),
        ],
        preferred_thread_id,
        preferred_rollout_path,
    })
}

pub(crate) struct SmartExecutionOutput {
    pub(crate) title: String,
    pub(crate) lines: Vec<String>,
    pub(crate) preferred_thread_id: Option<String>,
    pub(crate) preferred_rollout_path: Option<std::path::PathBuf>,
}

pub(crate) struct PreparedSmartExecution {
    args: SmartArgs,
    source_config: Config,
    summary: crate::types::SessionSummary,
    current_provider: String,
    current_model: String,
    provider_choices: Vec<String>,
}

struct SmartContext<'a> {
    args: &'a SmartArgs,
    summary: &'a crate::types::SessionSummary,
    current_provider: String,
    current_model: String,
    provider_choices: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SmartSelection {
    provider: String,
    model: String,
    execution_mode: SmartExecutionMode,
    target_config: Config,
    target_context_window: Option<i64>,
    target_auto_compact_token_limit: Option<i64>,
}

impl SmartSelection {
    fn should_repair_resume_state(&self, current_model: &str, current_window: Option<i64>) -> bool {
        self.model != current_model
            || (self.target_context_window.is_some()
                && self.target_context_window != current_window)
    }
}

fn planned_strategy(
    summary: &crate::types::SessionSummary,
    current_provider: &str,
    current_model: &str,
    selection: &SmartSelection,
) -> SmartStrategyKind {
    let current_context_tokens = summary.latest_context_tokens;
    let current_context_window = summary.latest_model_context_window;
    let needs_compaction = selection
        .target_context_window
        .zip(current_context_tokens)
        .is_some_and(|(target_window, context_tokens)| context_tokens > target_window);
    let needs_repair_resume_state = selection
        .should_repair_resume_state(current_model, current_context_window)
        && selection.target_context_window.is_some();
    let same_provider = selection.provider == current_provider;
    match selection.execution_mode {
        SmartExecutionMode::DistillCodex | SmartExecutionMode::DistillDeterministic => {
            SmartStrategyKind::DistillSuccessor
        }
        SmartExecutionMode::Direct => {
            match (same_provider, needs_compaction, needs_repair_resume_state) {
                (true, false, false) => SmartStrategyKind::SameThreadProfileOnly,
                (true, false, true) => SmartStrategyKind::SameThreadRepair,
                (true, true, _) => SmartStrategyKind::SameThreadRepairAfterCompaction,
                (false, false, _) => SmartStrategyKind::CrossProviderMigrate,
                (false, true, _) => SmartStrategyKind::CrossProviderMigrateAfterCompaction,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmartStrategy {
    SameThreadProfileOnly,
    SameThreadRepair,
    SameThreadRepairAfterCompaction,
    CrossProviderMigrate,
    CrossProviderMigrateAfterCompaction,
    DistillSuccessor,
}

#[derive(Debug, Clone)]
struct SmartPreview {
    selection: SmartSelection,
    target_profile: String,
    strategy: SmartStrategy,
    current_context_tokens: Option<i64>,
    current_context_window: Option<i64>,
    needs_compaction: bool,
    needs_repair_resume_state: bool,
    will_archive_source: bool,
    blocked_reason: Option<String>,
    max_pre_compactions: u32,
}

struct SmartPicker {
    language: PickerLanguage,
    current_provider: String,
    current_model: String,
    current_window: Option<i64>,
    summary_path: String,
    providers: Vec<String>,
    provider_index: usize,
    models: Vec<ModelPreset>,
    model_index: usize,
    execution_mode_index: usize,
    step: SmartStep,
    preview: Option<SmartPreview>,
}

#[derive(Clone, Copy)]
enum SmartStep {
    Provider,
    Model,
    Mode,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmartExecutionMode {
    Direct,
    DistillCodex,
    DistillDeterministic,
}

impl SmartExecutionMode {
    fn all() -> [SmartExecutionMode; 3] {
        [
            SmartExecutionMode::Direct,
            SmartExecutionMode::DistillCodex,
            SmartExecutionMode::DistillDeterministic,
        ]
    }
}

#[derive(Clone, Copy)]
enum PickerLanguage {
    English,
    Chinese,
}

impl PickerLanguage {
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
}

impl SmartPicker {
    async fn run(context: SmartContext<'_>) -> Result<Option<SmartSelection>> {
        let language = PickerLanguage::detect();
        let provider_index = context
            .provider_choices
            .iter()
            .position(|provider| provider == &context.current_provider)
            .unwrap_or(0);
        let mut picker = Self {
            language,
            current_provider: context.current_provider.clone(),
            current_model: context.current_model.clone(),
            current_window: context.summary.latest_model_context_window,
            summary_path: context.summary.rollout_path.display().to_string(),
            providers: context.provider_choices.clone(),
            provider_index,
            models: Vec::new(),
            model_index: 0,
            execution_mode_index: 0,
            step: SmartStep::Provider,
            preview: None,
        };

        let mut terminal = SmartTerminal::enter()?;
        loop {
            terminal.draw(&picker)?;
            if !poll(PICKER_POLL_INTERVAL)? {
                continue;
            }
            let Event::Key(key) = read()? else {
                continue;
            };
            if matches!(key.kind, KeyEventKind::Release) {
                continue;
            }
            match (picker.step, key.code) {
                (_, KeyCode::Char('q')) => {
                    terminal.leave()?;
                    return Ok(None);
                }
                (SmartStep::Provider, KeyCode::Esc) => {
                    terminal.leave()?;
                    return Ok(None);
                }
                (SmartStep::Provider, KeyCode::Up) => {
                    picker.provider_index = picker.provider_index.saturating_sub(1);
                }
                (SmartStep::Provider, KeyCode::Down) => {
                    picker.provider_index =
                        (picker.provider_index + 1).min(picker.providers.len().saturating_sub(1));
                }
                (SmartStep::Provider, KeyCode::Enter) => {
                    let provider = picker.selected_provider().to_string();
                    picker.models =
                        available_models_for_provider(&context, provider.as_str()).await?;
                    if picker.models.is_empty() {
                        bail!("selected provider `{provider}` has no picker-visible models");
                    }
                    picker.preview = None;
                    picker.model_index = picker
                        .models
                        .iter()
                        .position(|model| model.model == context.current_model)
                        .unwrap_or(0);
                    picker.step = SmartStep::Model;
                }
                (SmartStep::Model, KeyCode::Esc) => {
                    picker.preview = None;
                    picker.step = SmartStep::Provider;
                }
                (SmartStep::Model, KeyCode::Up) => {
                    picker.preview = None;
                    picker.model_index = picker.model_index.saturating_sub(1);
                }
                (SmartStep::Model, KeyCode::Down) => {
                    picker.preview = None;
                    picker.model_index =
                        (picker.model_index + 1).min(picker.models.len().saturating_sub(1));
                }
                (SmartStep::Model, KeyCode::Enter) => {
                    picker.execution_mode_index = 0;
                    picker.preview = None;
                    picker.step = SmartStep::Mode;
                }
                (SmartStep::Mode, KeyCode::Esc) => {
                    picker.step = SmartStep::Model;
                }
                (SmartStep::Mode, KeyCode::Up) => {
                    picker.preview = None;
                    picker.execution_mode_index = picker.execution_mode_index.saturating_sub(1);
                }
                (SmartStep::Mode, KeyCode::Down) => {
                    picker.preview = None;
                    picker.execution_mode_index = (picker.execution_mode_index + 1)
                        .min(SmartExecutionMode::all().len().saturating_sub(1));
                }
                (SmartStep::Mode, KeyCode::Enter) => {
                    let provider = picker.selected_provider().to_string();
                    let model = picker.selected_model()?.model.clone();
                    let execution_mode = picker.selected_execution_mode();
                    picker.preview =
                        Some(build_preview(&context, provider, model, execution_mode).await?);
                    picker.step = SmartStep::Confirm;
                }
                (SmartStep::Confirm, KeyCode::Esc) => {
                    picker.step = SmartStep::Mode;
                }
                (SmartStep::Confirm, KeyCode::Enter) => {
                    let preview = picker.preview.clone().context("missing smart preview")?;
                    terminal.leave()?;
                    return Ok(Some(preview.selection));
                }
                _ => {}
            }
        }
    }

    fn selected_provider(&self) -> &str {
        self.providers
            .get(self.provider_index)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn selected_model(&self) -> Result<&ModelPreset> {
        self.models
            .get(self.model_index)
            .context("no models available for selected provider")
    }

    fn selected_execution_mode(&self) -> SmartExecutionMode {
        SmartExecutionMode::all()
            .get(self.execution_mode_index)
            .copied()
            .unwrap_or(SmartExecutionMode::Direct)
    }
}

fn load_global_config_toml(codex_home: &Path) -> Result<ConfigToml> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    if !config_path.exists() {
        return Ok(ConfigToml::default());
    }
    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    toml::from_str::<ConfigToml>(&contents)
        .with_context(|| format!("failed to parse {}", config_path.display()))
}

fn collect_provider_choices(config: &ConfigToml, current_provider: &str) -> Vec<String> {
    let mut providers = BTreeSet::new();
    providers.insert(current_provider.to_string());
    if let Some(provider) = config.model_provider.clone() {
        providers.insert(provider);
    }
    for provider in config.model_providers.keys() {
        providers.insert(provider.clone());
    }
    for profile in config.profiles.values() {
        if let Some(provider) = profile.model_provider.clone() {
            providers.insert(provider);
        }
    }

    let mut values = providers.into_iter().collect::<Vec<_>>();
    values.sort();
    if let Some(index) = values
        .iter()
        .position(|provider| provider == current_provider)
    {
        let current = values.remove(index);
        values.insert(0, current);
    }
    values
}

async fn available_models_for_provider(
    context: &SmartContext<'_>,
    provider: &str,
) -> Result<Vec<ModelPreset>> {
    let runtime_config = load_runtime_config(
        None,
        Some(context.summary.session_cwd.clone()),
        None,
        Some(provider.to_string()),
        None,
        None,
    )
    .await?;
    list_available_models(&runtime_config).await
}

async fn list_available_models(config: &Config) -> Result<Vec<ModelPreset>> {
    let auth_manager = AuthManager::shared(
        config.codex_home.clone(),
        true,
        config.cli_auth_credentials_store_mode,
    );
    auth_manager.set_forced_chatgpt_workspace_id(config.forced_chatgpt_workspace_id.clone());
    let thread_manager = ThreadManager::new(
        config,
        auth_manager,
        SessionSource::Exec,
        CollaborationModesConfig {
            default_mode_request_user_input: config
                .features
                .enabled(Feature::DefaultModeRequestUserInput),
        },
    );
    let models = thread_manager.list_models(RefreshStrategy::Offline).await;
    Ok(models
        .into_iter()
        .filter(|model| model.show_in_picker && model.supported_in_api)
        .collect())
}

async fn build_preview(
    context: &SmartContext<'_>,
    provider: String,
    model: String,
    execution_mode: SmartExecutionMode,
) -> Result<SmartPreview> {
    let target_config = load_runtime_config(
        None,
        Some(context.summary.session_cwd.clone()),
        Some(model.clone()),
        Some(provider.clone()),
        None,
        None,
    )
    .await?;
    let selection = SmartSelection {
        provider,
        model,
        execution_mode,
        target_context_window: target_config.model_context_window,
        target_auto_compact_token_limit: target_config.model_auto_compact_token_limit,
        target_config,
    };

    Ok(plan_preview(
        context.summary,
        context.current_provider.as_str(),
        context.current_model.as_str(),
        context.args,
        selection,
    ))
}

fn plan_preview(
    summary: &crate::types::SessionSummary,
    current_provider: &str,
    current_model: &str,
    args: &SmartArgs,
    selection: SmartSelection,
) -> SmartPreview {
    let target_profile = args.write_profile.clone().unwrap_or_else(|| {
        default_smart_profile_name(
            selection.provider.as_str(),
            selection.model.as_str(),
            selection.target_context_window,
        )
    });
    let current_context_tokens = summary.latest_context_tokens;
    let current_context_window = summary.latest_model_context_window;
    let execution_mode = selection.execution_mode;
    let needs_compaction = selection
        .target_context_window
        .zip(current_context_tokens)
        .is_some_and(|(target_window, context_tokens)| context_tokens > target_window);
    let needs_repair_resume_state = selection
        .should_repair_resume_state(current_model, current_context_window)
        && selection.target_context_window.is_some();
    let same_provider = selection.provider == current_provider;
    let strategy = match execution_mode {
        SmartExecutionMode::DistillCodex | SmartExecutionMode::DistillDeterministic => {
            SmartStrategy::DistillSuccessor
        }
        SmartExecutionMode::Direct => {
            match (same_provider, needs_compaction, needs_repair_resume_state) {
                (true, false, false) => SmartStrategy::SameThreadProfileOnly,
                (true, false, true) => SmartStrategy::SameThreadRepair,
                (true, true, _) => SmartStrategy::SameThreadRepairAfterCompaction,
                (false, false, _) => SmartStrategy::CrossProviderMigrate,
                (false, true, _) => SmartStrategy::CrossProviderMigrateAfterCompaction,
            }
        }
    };
    let blocked_reason = if summary.archived
        && needs_compaction
        && execution_mode == SmartExecutionMode::Direct
    {
        Some(match same_provider {
            true => match selection.target_context_window {
                Some(target_window) => format!(
                    "thread is archived and current context {} exceeds target window {target_window}",
                    current_context_tokens.unwrap_or_default()
                ),
                None => {
                    "thread is archived and the selected runtime still needs compaction".to_string()
                }
            },
            false => match selection.target_context_window {
                Some(target_window) => format!(
                    "source thread is archived and migration would need compaction because current context {} exceeds target window {target_window}",
                    current_context_tokens.unwrap_or_default()
                ),
                None => "source thread is archived and the selected runtime still needs compaction"
                    .to_string(),
            },
        })
    } else {
        None
    };

    SmartPreview {
        selection,
        target_profile,
        strategy,
        current_context_tokens,
        current_context_window,
        needs_compaction,
        needs_repair_resume_state,
        will_archive_source: !same_provider && args.archive_source,
        blocked_reason,
        max_pre_compactions: args.max_pre_compactions,
    }
}

fn default_smart_profile_name(provider: &str, model: &str, context_window: Option<i64>) -> String {
    let provider = sanitize_profile_component(provider);
    let model = sanitize_profile_component(model);
    match context_window {
        Some(context_window) => format!("smart-{provider}-{model}-{context_window}"),
        None => format!("smart-{provider}-{model}"),
    }
}

fn sanitize_profile_component(value: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            out.push(character);
            previous_dash = false;
        } else if !previous_dash {
            out.push('-');
            previous_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn strategy_label(strategy: SmartStrategy) -> &'static str {
    match strategy {
        SmartStrategy::SameThreadProfileOnly => "same-thread profile update",
        SmartStrategy::SameThreadRepair => "same-thread repair",
        SmartStrategy::SameThreadRepairAfterCompaction => "same-thread compact then repair",
        SmartStrategy::CrossProviderMigrate => "cross-provider migrate",
        SmartStrategy::CrossProviderMigrateAfterCompaction => "cross-provider compact then migrate",
        SmartStrategy::DistillSuccessor => "distill successor session",
    }
}

fn strategy_label_zh(strategy: SmartStrategy) -> &'static str {
    match strategy {
        SmartStrategy::SameThreadProfileOnly => "同线程仅更新 profile",
        SmartStrategy::SameThreadRepair => "同线程修复",
        SmartStrategy::SameThreadRepairAfterCompaction => "同线程先压缩再修复",
        SmartStrategy::CrossProviderMigrate => "跨 provider 迁移",
        SmartStrategy::CrossProviderMigrateAfterCompaction => "跨 provider 先压缩再迁移",
        SmartStrategy::DistillSuccessor => "提炼轻量继任会话",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn yes_no_zh(value: bool) -> &'static str {
    if value { "是" } else { "否" }
}

struct SmartTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    interactive: bool,
}

impl SmartTerminal {
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

    fn draw(&mut self, picker: &SmartPicker) -> Result<()> {
        self.terminal.draw(|frame| draw_picker(frame, picker))?;
        Ok(())
    }

    fn leave(&mut self) -> Result<()> {
        if !self.interactive {
            return Ok(());
        }
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show)?;
        self.interactive = false;
        Ok(())
    }
}

impl Drop for SmartTerminal {
    fn drop(&mut self) {
        if self.interactive {
            let _ = disable_raw_mode();
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        }
    }
}

fn draw_picker(frame: &mut Frame<'_>, picker: &SmartPicker) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(4),
        ])
        .split(frame.area());

    frame.render_widget(
        Paragraph::new(match picker.language {
            PickerLanguage::English => "Smart switch · select provider, then model, then confirm",
            PickerLanguage::Chinese => "Smart 切换 · 先选 provider，再选 model，最后确认",
        }),
        areas[0],
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(areas[1]);

    let list_block = Block::default()
        .title(match picker.step {
            SmartStep::Provider => match picker.language {
                PickerLanguage::English => "Select provider",
                PickerLanguage::Chinese => "选择 provider",
            },
            SmartStep::Model => match picker.language {
                PickerLanguage::English => "Select model",
                PickerLanguage::Chinese => "选择 model",
            },
            SmartStep::Mode => match picker.language {
                PickerLanguage::English => "Select execution mode",
                PickerLanguage::Chinese => "选择执行模式",
            },
            SmartStep::Confirm => match picker.language {
                PickerLanguage::English => "Confirm",
                PickerLanguage::Chinese => "确认执行",
            },
        })
        .borders(Borders::ALL);
    let list_inner = list_block.inner(body[0]);
    frame.render_widget(list_block, body[0]);

    match picker.step {
        SmartStep::Provider => {
            let items = picker
                .providers
                .iter()
                .enumerate()
                .map(|(index, provider)| {
                    let line = if index == picker.provider_index {
                        Line::from(vec![
                            "› ".green().bold(),
                            provider.clone().bold(),
                            if provider == &picker.current_provider {
                                match picker.language {
                                    PickerLanguage::English => " · current".dim(),
                                    PickerLanguage::Chinese => " · 当前".dim(),
                                }
                            } else {
                                "".into()
                            },
                        ])
                        .reversed()
                    } else {
                        Line::from(vec![
                            "  ".into(),
                            provider.clone().into(),
                            if provider == &picker.current_provider {
                                match picker.language {
                                    PickerLanguage::English => " · current".dim(),
                                    PickerLanguage::Chinese => " · 当前".dim(),
                                }
                            } else {
                                "".into()
                            },
                        ])
                    };
                    ListItem::new(line)
                })
                .collect::<Vec<_>>();
            frame.render_widget(List::new(items), list_inner);
        }
        SmartStep::Model => {
            let items = picker
                .models
                .iter()
                .enumerate()
                .map(|(index, model)| {
                    let title = if model.display_name == model.model {
                        model.model.clone()
                    } else {
                        format!("{} ({})", model.display_name, model.model)
                    };
                    let line = if index == picker.model_index {
                        Line::from(vec![
                            "› ".green().bold(),
                            title.bold(),
                            if model.model == picker.current_model {
                                match picker.language {
                                    PickerLanguage::English => " · current".dim(),
                                    PickerLanguage::Chinese => " · 当前".dim(),
                                }
                            } else {
                                "".into()
                            },
                        ])
                        .reversed()
                    } else {
                        Line::from(vec![
                            "  ".into(),
                            title.into(),
                            if model.model == picker.current_model {
                                match picker.language {
                                    PickerLanguage::English => " · current".dim(),
                                    PickerLanguage::Chinese => " · 当前".dim(),
                                }
                            } else {
                                "".into()
                            },
                        ])
                    };
                    ListItem::new(line)
                })
                .collect::<Vec<_>>();
            frame.render_widget(List::new(items), list_inner);
        }
        SmartStep::Mode => {
            let items = SmartExecutionMode::all()
                .iter()
                .enumerate()
                .map(|(index, mode)| {
                    let (title, description) = match (picker.language, mode) {
                        (PickerLanguage::English, SmartExecutionMode::Direct) => (
                            "Direct switch",
                            "Repair or migrate the selected thread directly",
                        ),
                        (PickerLanguage::English, SmartExecutionMode::DistillCodex) => (
                            "Codex distill",
                            "Use Codex to generate a lighter successor session",
                        ),
                        (PickerLanguage::English, SmartExecutionMode::DistillDeterministic) => (
                            "Deterministic distill",
                            "Build a lighter successor session from rule-based extraction",
                        ),
                        (PickerLanguage::Chinese, SmartExecutionMode::Direct) => {
                            ("直接切换", "直接在当前线程上修复或迁移")
                        }
                        (PickerLanguage::Chinese, SmartExecutionMode::DistillCodex) => {
                            ("Codex 提炼", "用 Codex 生成一个更轻的新会话")
                        }
                        (PickerLanguage::Chinese, SmartExecutionMode::DistillDeterministic) => {
                            ("规则提炼", "按规则提取有效历史，生成更轻的新会话")
                        }
                    };
                    let line = if index == picker.execution_mode_index {
                        Line::from(vec![
                            "› ".green().bold(),
                            title.bold(),
                            " · ".dim(),
                            description.into(),
                        ])
                        .reversed()
                    } else {
                        Line::from(vec![
                            "  ".into(),
                            title.into(),
                            " · ".dim(),
                            description.into(),
                        ])
                    };
                    ListItem::new(line)
                })
                .collect::<Vec<_>>();
            frame.render_widget(List::new(items), list_inner);
        }
        SmartStep::Confirm => {
            let preview = picker.preview.as_ref();
            let lines = vec![
                Line::from(match picker.language {
                    PickerLanguage::English => {
                        "Press Enter to execute the smart switch.".to_string()
                    }
                    PickerLanguage::Chinese => "按 Enter 执行 smart 切换。".to_string(),
                }),
                Line::from(""),
                Line::from(match picker.language {
                    PickerLanguage::English => {
                        format!("From provider: {}", picker.current_provider)
                    }
                    PickerLanguage::Chinese => {
                        format!("当前 provider：{}", picker.current_provider)
                    }
                }),
                Line::from(match picker.language {
                    PickerLanguage::English => format!("From model: {}", picker.current_model),
                    PickerLanguage::Chinese => format!("当前 model：{}", picker.current_model),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => format!(
                        "From context window: {}",
                        preview
                            .current_context_window
                            .map_or_else(String::new, |value| value.to_string())
                    ),
                    (PickerLanguage::Chinese, Some(preview)) => format!(
                        "当前上下文窗口：{}",
                        preview
                            .current_context_window
                            .map_or_else(String::new, |value| value.to_string())
                    ),
                    (PickerLanguage::English, None) => "From context window: ".to_string(),
                    (PickerLanguage::Chinese, None) => "当前上下文窗口：".to_string(),
                }),
                Line::from(match picker.language {
                    PickerLanguage::English => {
                        format!("To provider: {}", picker.selected_provider())
                    }
                    PickerLanguage::Chinese => {
                        format!("目标 provider：{}", picker.selected_provider())
                    }
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => {
                        format!("To model: {}", preview.selection.model)
                    }
                    (PickerLanguage::Chinese, Some(preview)) => {
                        format!("目标 model：{}", preview.selection.model)
                    }
                    (PickerLanguage::English, None) => "To model: ".to_string(),
                    (PickerLanguage::Chinese, None) => "目标 model：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => format!(
                        "Target context window: {}",
                        preview
                            .selection
                            .target_context_window
                            .map_or_else(String::new, |value| value.to_string())
                    ),
                    (PickerLanguage::Chinese, Some(preview)) => format!(
                        "目标上下文窗口：{}",
                        preview
                            .selection
                            .target_context_window
                            .map_or_else(String::new, |value| value.to_string())
                    ),
                    (PickerLanguage::English, None) => "Target context window: ".to_string(),
                    (PickerLanguage::Chinese, None) => "目标上下文窗口：".to_string(),
                }),
                Line::from(""),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => format!(
                        "Execution mode: {}",
                        match preview.selection.execution_mode {
                            SmartExecutionMode::Direct => "direct switch",
                            SmartExecutionMode::DistillCodex => "codex distill",
                            SmartExecutionMode::DistillDeterministic => "deterministic distill",
                        }
                    ),
                    (PickerLanguage::Chinese, Some(preview)) => format!(
                        "执行模式：{}",
                        match preview.selection.execution_mode {
                            SmartExecutionMode::Direct => "直接切换",
                            SmartExecutionMode::DistillCodex => "Codex 提炼",
                            SmartExecutionMode::DistillDeterministic => "规则提炼",
                        }
                    ),
                    (PickerLanguage::English, None) => "Execution mode: ".to_string(),
                    (PickerLanguage::Chinese, None) => "执行模式：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => {
                        format!("Strategy: {}", strategy_label(preview.strategy))
                    }
                    (PickerLanguage::Chinese, Some(preview)) => {
                        format!("策略：{}", strategy_label_zh(preview.strategy))
                    }
                    (PickerLanguage::English, None) => "Strategy: ".to_string(),
                    (PickerLanguage::Chinese, None) => "策略：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => {
                        format!("Write profile: {}", preview.target_profile)
                    }
                    (PickerLanguage::Chinese, Some(preview)) => {
                        format!("写入 profile：{}", preview.target_profile)
                    }
                    (PickerLanguage::English, None) => "Write profile: ".to_string(),
                    (PickerLanguage::Chinese, None) => "写入 profile：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => format!(
                        "Repair resume state: {}",
                        yes_no(preview.needs_repair_resume_state)
                    ),
                    (PickerLanguage::Chinese, Some(preview)) => format!(
                        "修复 resume-state：{}",
                        yes_no_zh(preview.needs_repair_resume_state)
                    ),
                    (PickerLanguage::English, None) => "Repair resume state: ".to_string(),
                    (PickerLanguage::Chinese, None) => "修复 resume-state：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => {
                        if preview.needs_compaction {
                            format!(
                                "Pre-compaction: yes (current_context_tokens={} > target_window={}, max_runs={})",
                                preview
                                    .current_context_tokens
                                    .map_or_else(String::new, |value| value.to_string()),
                                preview
                                    .selection
                                    .target_context_window
                                    .map_or_else(String::new, |value| value.to_string()),
                                preview.max_pre_compactions
                            )
                        } else {
                            "Pre-compaction: no".to_string()
                        }
                    }
                    (PickerLanguage::Chinese, Some(preview)) => {
                        if preview.needs_compaction {
                            format!(
                                "预压缩：需要（current_context_tokens={} > target_window={}，最多 {} 次）",
                                preview
                                    .current_context_tokens
                                    .map_or_else(String::new, |value| value.to_string()),
                                preview
                                    .selection
                                    .target_context_window
                                    .map_or_else(String::new, |value| value.to_string()),
                                preview.max_pre_compactions
                            )
                        } else {
                            "预压缩：不需要".to_string()
                        }
                    }
                    (PickerLanguage::English, None) => "Pre-compaction: ".to_string(),
                    (PickerLanguage::Chinese, None) => "预压缩：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => format!(
                        "Archive source after migrate: {}",
                        yes_no(preview.will_archive_source)
                    ),
                    (PickerLanguage::Chinese, Some(preview)) => format!(
                        "迁移后归档源线程：{}",
                        yes_no_zh(preview.will_archive_source)
                    ),
                    (PickerLanguage::English, None) => "Archive source after migrate: ".to_string(),
                    (PickerLanguage::Chinese, None) => "迁移后归档源线程：".to_string(),
                }),
                Line::from(match (picker.language, preview) {
                    (PickerLanguage::English, Some(preview)) => preview
                        .blocked_reason
                        .as_ref()
                        .map(|reason| format!("Warning: {reason}"))
                        .unwrap_or_else(String::new),
                    (PickerLanguage::Chinese, Some(preview)) => preview
                        .blocked_reason
                        .as_ref()
                        .map(|reason| format!("警告：{reason}"))
                        .unwrap_or_else(String::new),
                    (PickerLanguage::English, None) => String::new(),
                    (PickerLanguage::Chinese, None) => String::new(),
                }),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list_inner);
        }
    }

    let detail_block = Block::default()
        .title(match picker.language {
            PickerLanguage::English => "Current thread",
            PickerLanguage::Chinese => "当前线程",
        })
        .borders(Borders::ALL);
    let detail_inner = detail_block.inner(body[1]);
    frame.render_widget(detail_block, body[1]);
    let details = vec![
        Line::from(match picker.language {
            PickerLanguage::English => format!("Provider: {}", picker.current_provider),
            PickerLanguage::Chinese => format!("Provider：{}", picker.current_provider),
        }),
        Line::from(match picker.language {
            PickerLanguage::English => format!("Model: {}", picker.current_model),
            PickerLanguage::Chinese => format!("Model：{}", picker.current_model),
        }),
        Line::from(match picker.language {
            PickerLanguage::English => format!(
                "Current context window: {}",
                picker
                    .current_window
                    .map_or_else(String::new, |value| value.to_string())
            ),
            PickerLanguage::Chinese => format!(
                "当前上下文窗口：{}",
                picker
                    .current_window
                    .map_or_else(String::new, |value| value.to_string())
            ),
        }),
        Line::from(""),
        Line::from(match picker.language {
            PickerLanguage::English => format!("Rollout: {}", picker.summary_path),
            PickerLanguage::Chinese => format!("Rollout：{}", picker.summary_path),
        }),
    ];
    frame.render_widget(
        Paragraph::new(details).wrap(Wrap { trim: false }),
        detail_inner,
    );

    frame.render_widget(
        Paragraph::new(match picker.language {
            PickerLanguage::English => "Up/Down move · Enter confirm · Esc back · q cancel",
            PickerLanguage::Chinese => "上下移动 · Enter 确认 · Esc 返回 · q 取消",
        }),
        areas[2],
    );
}

#[cfg(test)]
mod tests {
    use super::SmartExecutionMode;
    use super::SmartSelection;
    use super::SmartStrategy;
    use super::collect_provider_choices;
    use super::default_smart_profile_name;
    use super::plan_preview;
    use crate::cli::SmartArgs;
    use crate::cli::TargetArgs;
    use crate::types::SessionSummary;
    use codex_core::ModelProviderInfo;
    use codex_core::config::ConfigBuilder;
    use codex_core::config::ConfigToml;
    use codex_core::config::profile::ConfigProfile;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    async fn sample_config() -> codex_core::config::Config {
        let temp = tempdir().expect("tempdir");
        ConfigBuilder::default()
            .codex_home(temp.path().to_path_buf())
            .build()
            .await
            .expect("build config")
    }

    #[test]
    fn collect_provider_choices_prefers_current_provider_first() {
        let mut config = ConfigToml {
            model_provider: Some("openai".to_string()),
            ..Default::default()
        };
        config.model_providers.insert(
            "yunyi".to_string(),
            ModelProviderInfo::create_openai_provider(),
        );
        config.profiles.insert(
            "team".to_string(),
            ConfigProfile {
                model_provider: Some("openrouter".to_string()),
                ..Default::default()
            },
        );

        let providers = collect_provider_choices(&config, "openai");
        assert_eq!(providers[0], "openai");
        assert!(providers.contains(&"yunyi".to_string()));
        assert!(providers.contains(&"openrouter".to_string()));
    }

    #[test]
    fn default_smart_profile_name_sanitizes_components() {
        assert_eq!(
            default_smart_profile_name("yunyi", "gpt-5.2-codex", Some(258400)),
            "smart-yunyi-gpt-5-2-codex-258400"
        );
        assert_eq!(
            default_smart_profile_name("model_providers.yunyi", "gpt/5.4", None),
            "smart-model-providers-yunyi-gpt-5-4"
        );
    }

    #[tokio::test]
    async fn plan_preview_marks_same_provider_compaction_repair() {
        let config = sample_config().await;
        let preview = plan_preview(
            &SessionSummary {
                thread_id: "thread-1".to_string(),
                thread_name: None,
                rollout_path: "D:/tmp/rollout.jsonl".into(),
                archived: false,
                source: "cli".to_string(),
                session_provider: Some("openai".to_string()),
                session_cwd: "D:/tmp".into(),
                session_timestamp: "2026-03-11T00:00:00Z".to_string(),
                latest_model: Some("gpt-5.4".to_string()),
                latest_total_tokens: Some(1000),
                latest_context_tokens: Some(400000),
                latest_model_context_window: Some(950000),
                user_turns: 1,
                first_user_message: None,
                forked_from_id: None,
                memory_mode: None,
            },
            "openai",
            "gpt-5.4",
            &SmartArgs {
                target: TargetArgs {
                    target: "thread-1".to_string(),
                    config_profile: None,
                },
                write_profile: None,
                archive_source: false,
                max_pre_compactions: 3,
                timeout_secs: 300,
            },
            SmartSelection {
                provider: "openai".to_string(),
                model: "gpt-5.2".to_string(),
                execution_mode: SmartExecutionMode::Direct,
                target_config: config,
                target_context_window: Some(258400),
                target_auto_compact_token_limit: Some(232560),
            },
        );

        assert_eq!(
            preview.strategy,
            SmartStrategy::SameThreadRepairAfterCompaction
        );
        assert!(preview.needs_compaction);
        assert!(preview.needs_repair_resume_state);
        assert_eq!(preview.blocked_reason, None);
    }

    #[tokio::test]
    async fn plan_preview_marks_archived_cross_provider_compaction_as_blocked() {
        let config = sample_config().await;
        let preview = plan_preview(
            &SessionSummary {
                thread_id: "thread-1".to_string(),
                thread_name: None,
                rollout_path: "D:/tmp/rollout.jsonl".into(),
                archived: true,
                source: "cli".to_string(),
                session_provider: Some("openai".to_string()),
                session_cwd: "D:/tmp".into(),
                session_timestamp: "2026-03-11T00:00:00Z".to_string(),
                latest_model: Some("gpt-5.4".to_string()),
                latest_total_tokens: Some(1000),
                latest_context_tokens: Some(400000),
                latest_model_context_window: Some(950000),
                user_turns: 1,
                first_user_message: None,
                forked_from_id: None,
                memory_mode: None,
            },
            "openai",
            "gpt-5.4",
            &SmartArgs {
                target: TargetArgs {
                    target: "thread-1".to_string(),
                    config_profile: None,
                },
                write_profile: Some("yunyi-256k".to_string()),
                archive_source: true,
                max_pre_compactions: 3,
                timeout_secs: 300,
            },
            SmartSelection {
                provider: "yunyi".to_string(),
                model: "gpt-5.2".to_string(),
                execution_mode: SmartExecutionMode::Direct,
                target_config: config,
                target_context_window: Some(258400),
                target_auto_compact_token_limit: Some(232560),
            },
        );

        assert_eq!(
            preview.strategy,
            SmartStrategy::CrossProviderMigrateAfterCompaction
        );
        assert!(preview.will_archive_source);
        assert!(preview.blocked_reason.is_some());
    }

    #[tokio::test]
    async fn plan_preview_uses_distill_strategy_when_selected() {
        let config = sample_config().await;
        let preview = plan_preview(
            &SessionSummary {
                thread_id: "thread-1".to_string(),
                thread_name: None,
                rollout_path: "D:/tmp/rollout.jsonl".into(),
                archived: true,
                source: "cli".to_string(),
                session_provider: Some("openai".to_string()),
                session_cwd: "D:/tmp".into(),
                session_timestamp: "2026-03-11T00:00:00Z".to_string(),
                latest_model: Some("gpt-5.4".to_string()),
                latest_total_tokens: Some(1000),
                latest_context_tokens: Some(400000),
                latest_model_context_window: Some(950000),
                user_turns: 1,
                first_user_message: None,
                forked_from_id: None,
                memory_mode: None,
            },
            "openai",
            "gpt-5.4",
            &SmartArgs {
                target: TargetArgs {
                    target: "thread-1".to_string(),
                    config_profile: None,
                },
                write_profile: Some("distilled".to_string()),
                archive_source: true,
                max_pre_compactions: 3,
                timeout_secs: 300,
            },
            SmartSelection {
                provider: "yunyi".to_string(),
                model: "gpt-5.2".to_string(),
                execution_mode: SmartExecutionMode::DistillCodex,
                target_config: config,
                target_context_window: Some(258400),
                target_auto_compact_token_limit: Some(232560),
            },
        );

        assert_eq!(preview.strategy, SmartStrategy::DistillSuccessor);
        assert_eq!(preview.blocked_reason, None);
    }

    #[tokio::test]
    async fn plan_preview_uses_distill_strategy_for_deterministic_mode() {
        let config = sample_config().await;
        let preview = plan_preview(
            &SessionSummary {
                thread_id: "thread-1".to_string(),
                thread_name: None,
                rollout_path: "D:/tmp/rollout.jsonl".into(),
                archived: false,
                source: "cli".to_string(),
                session_provider: Some("openai".to_string()),
                session_cwd: "D:/tmp".into(),
                session_timestamp: "2026-03-11T00:00:00Z".to_string(),
                latest_model: Some("gpt-5.4".to_string()),
                latest_total_tokens: Some(1000),
                latest_context_tokens: Some(400000),
                latest_model_context_window: Some(950000),
                user_turns: 1,
                first_user_message: None,
                forked_from_id: None,
                memory_mode: None,
            },
            "openai",
            "gpt-5.4",
            &SmartArgs {
                target: TargetArgs {
                    target: "thread-1".to_string(),
                    config_profile: None,
                },
                write_profile: Some("distilled".to_string()),
                archive_source: false,
                max_pre_compactions: 3,
                timeout_secs: 300,
            },
            SmartSelection {
                provider: "openai".to_string(),
                model: "gpt-5.2".to_string(),
                execution_mode: SmartExecutionMode::DistillDeterministic,
                target_config: config,
                target_context_window: Some(258400),
                target_auto_compact_token_limit: Some(232560),
            },
        );

        assert_eq!(preview.strategy, SmartStrategy::DistillSuccessor);
        assert_eq!(preview.blocked_reason, None);
    }
}
