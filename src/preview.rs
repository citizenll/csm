use crate::cli::FirstTokenPreviewArgs;
use crate::cli::TargetArgs;
use crate::runtime::load_session_runtime_config;
use crate::runtime::resolve_target;
use crate::summary::build_session_summary;
use crate::types::SessionSummary;
use anyhow::Context;
use anyhow::Result;
use codex_core::RolloutRecorder;
use codex_core::features::Feature;
use codex_core::parse_turn_item;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TurnContextItem;
use serde::Serialize;
use std::path::PathBuf;

pub(crate) async fn run(args: FirstTokenPreviewArgs) -> Result<()> {
    let json = args.json;
    let output = execute(args).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    for line in render_output_lines(&output) {
        println!("{line}");
    }
    Ok(())
}

pub(crate) async fn execute(args: FirstTokenPreviewArgs) -> Result<FirstTokenPreviewOutput> {
    let resolved = resolve_target(&args.target).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let runtime_config =
        load_session_runtime_config(args.target.config_profile.clone(), &summary).await?;
    let history = RolloutRecorder::get_rollout_history(resolved.rollout_path.as_path())
        .await
        .with_context(|| {
            format!(
                "failed to reconstruct rollout history from {}",
                resolved.rollout_path.display()
            )
        })?;
    let rollout_items = history.get_rollout_items();
    let preview = build_prompt_preview_snapshot(
        &summary,
        &runtime_config,
        rollout_items.as_slice(),
        args.input.as_deref(),
    )
    .await?;
    Ok(FirstTokenPreviewOutput::from_snapshot(
        &summary, args.input, preview,
    ))
}

pub(crate) async fn build_prompt_preview_for_distill(
    target: &TargetArgs,
) -> Result<PromptPreviewSnapshot> {
    let resolved = resolve_target(target).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let runtime_config =
        load_session_runtime_config(target.config_profile.clone(), &summary).await?;
    let history = RolloutRecorder::get_rollout_history(resolved.rollout_path.as_path())
        .await
        .with_context(|| {
            format!(
                "failed to reconstruct rollout history from {}",
                resolved.rollout_path.display()
            )
        })?;
    let rollout_items = history.get_rollout_items();
    build_prompt_preview_snapshot(&summary, &runtime_config, rollout_items.as_slice(), None).await
}

#[derive(Debug, Clone)]
pub(crate) struct PromptPreviewSnapshot {
    pub(crate) reconstructed_history: Vec<ResponseItem>,
    pub(crate) history_items_count: usize,
    pub(crate) history_tokens_estimate: usize,
    pub(crate) previous_turn_model: Option<String>,
    pub(crate) reference_context_item: Option<TurnContextItem>,
    pub(crate) context_strategy: NextTurnContextStrategy,
    pub(crate) latest_compaction_summary: Option<String>,
    pub(crate) latest_compaction_replacement_items: Option<usize>,
    pub(crate) memory_prompt_available: bool,
    pub(crate) memory_summary_tokens_estimate: Option<usize>,
    pub(crate) developer_instructions_present: bool,
    pub(crate) user_instructions_present: bool,
    pub(crate) prompt_sections: Vec<PromptSectionPreview>,
    pub(crate) history_tail: Vec<PromptItemPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) enum NextTurnContextStrategy {
    FullInitialContext {
        model_switch_message: bool,
        memory_prompt: bool,
        developer_instructions: bool,
        user_instructions: bool,
    },
    SettingsUpdate {
        model_switch_message: bool,
        reasons: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PromptSectionPreview {
    pub(crate) kind: String,
    pub(crate) description: String,
    pub(crate) estimated_tokens: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PromptItemPreview {
    pub(crate) index: usize,
    pub(crate) role: String,
    pub(crate) kind: String,
    pub(crate) preview: String,
    pub(crate) estimated_tokens: usize,
}

#[derive(Debug, Serialize)]
pub(crate) struct FirstTokenPreviewOutput {
    source_thread_id: String,
    source_thread_name: Option<String>,
    source_rollout_path: PathBuf,
    source_archived: bool,
    source_provider: Option<String>,
    source_model: Option<String>,
    source_context_tokens_estimate: Option<i64>,
    source_context_window: Option<i64>,
    next_user_input: Option<String>,
    reconstructed_history_items: usize,
    reconstructed_history_tokens_estimate: usize,
    previous_turn_model: Option<String>,
    reference_context_item_present: bool,
    next_turn_context_strategy: NextTurnContextStrategy,
    latest_compaction_summary: Option<String>,
    latest_compaction_replacement_items: Option<usize>,
    memory_prompt_available: bool,
    memory_summary_tokens_estimate: Option<usize>,
    developer_instructions_present: bool,
    user_instructions_present: bool,
    prompt_sections: Vec<PromptSectionPreview>,
    history_tail: Vec<PromptItemPreview>,
}

impl FirstTokenPreviewOutput {
    fn from_snapshot(
        summary: &SessionSummary,
        next_user_input: Option<String>,
        snapshot: PromptPreviewSnapshot,
    ) -> Self {
        Self {
            source_thread_id: summary.thread_id.clone(),
            source_thread_name: summary.thread_name.clone(),
            source_rollout_path: summary.rollout_path.clone(),
            source_archived: summary.archived,
            source_provider: summary.session_provider.clone(),
            source_model: summary.latest_model.clone(),
            source_context_tokens_estimate: summary.latest_context_tokens,
            source_context_window: summary.latest_model_context_window,
            next_user_input,
            reconstructed_history_items: snapshot.history_items_count,
            reconstructed_history_tokens_estimate: snapshot.history_tokens_estimate,
            previous_turn_model: snapshot.previous_turn_model,
            reference_context_item_present: snapshot.reference_context_item.is_some(),
            next_turn_context_strategy: snapshot.context_strategy,
            latest_compaction_summary: snapshot.latest_compaction_summary,
            latest_compaction_replacement_items: snapshot.latest_compaction_replacement_items,
            memory_prompt_available: snapshot.memory_prompt_available,
            memory_summary_tokens_estimate: snapshot.memory_summary_tokens_estimate,
            developer_instructions_present: snapshot.developer_instructions_present,
            user_instructions_present: snapshot.user_instructions_present,
            prompt_sections: snapshot.prompt_sections,
            history_tail: snapshot.history_tail,
        }
    }
}

pub(crate) fn render_output_lines(output: &FirstTokenPreviewOutput) -> Vec<String> {
    let mut lines = vec![
        format!("source_thread_id: {}", output.source_thread_id),
        format!(
            "source_thread_name: {}",
            output.source_thread_name.as_deref().unwrap_or("")
        ),
        format!(
            "source_rollout_path: {}",
            output.source_rollout_path.display()
        ),
        format!("source_archived: {}", output.source_archived),
        format!(
            "source_provider: {}",
            output.source_provider.as_deref().unwrap_or("")
        ),
        format!(
            "source_model: {}",
            output.source_model.as_deref().unwrap_or("")
        ),
        format!(
            "source_context_tokens_estimate: {}",
            output
                .source_context_tokens_estimate
                .map_or_else(String::new, |value| value.to_string())
        ),
        format!(
            "source_context_window: {}",
            output
                .source_context_window
                .map_or_else(String::new, |value| value.to_string())
        ),
        format!(
            "reconstructed_history_items: {}",
            output.reconstructed_history_items
        ),
        format!(
            "reconstructed_history_tokens_estimate: {}",
            output.reconstructed_history_tokens_estimate
        ),
    ];
    if let Some(summary) = output.latest_compaction_summary.as_deref() {
        lines.push(String::new());
        lines.push("latest_compaction_summary:".to_string());
        lines.push(summary.to_string());
    }
    match &output.next_turn_context_strategy {
        NextTurnContextStrategy::FullInitialContext {
            model_switch_message,
            memory_prompt,
            developer_instructions,
            user_instructions,
        } => {
            lines.push(String::new());
            lines.push("next_turn_context_strategy: full_initial_context".to_string());
            lines.push(format!("model_switch_message: {model_switch_message}"));
            lines.push(format!("memory_prompt_injected: {memory_prompt}"));
            lines.push(format!(
                "developer_instructions_injected: {developer_instructions}"
            ));
            lines.push(format!("user_instructions_injected: {user_instructions}"));
        }
        NextTurnContextStrategy::SettingsUpdate {
            model_switch_message,
            reasons,
        } => {
            lines.push(String::new());
            lines.push("next_turn_context_strategy: settings_update".to_string());
            lines.push(format!("model_switch_message: {model_switch_message}"));
            lines.push(format!("settings_update_reasons: {}", reasons.join(", ")));
        }
    }
    lines.push(format!(
        "memory_prompt_available: {}",
        output.memory_prompt_available
    ));
    lines.push(format!(
        "memory_summary_tokens_estimate: {}",
        output
            .memory_summary_tokens_estimate
            .map_or_else(String::new, |value| value.to_string())
    ));
    lines.push(format!(
        "developer_instructions_present: {}",
        output.developer_instructions_present
    ));
    lines.push(format!(
        "user_instructions_present: {}",
        output.user_instructions_present
    ));
    lines.push(format!(
        "latest_compaction_replacement_items: {}",
        output
            .latest_compaction_replacement_items
            .map_or_else(String::new, |value| value.to_string())
    ));
    if let Some(next_user_input) = output.next_user_input.as_deref() {
        lines.push(format!("next_user_input: {next_user_input}"));
    }
    if !output.prompt_sections.is_empty() {
        lines.push(String::new());
        lines.push("prompt_sections:".to_string());
        lines.extend(output.prompt_sections.iter().map(|section| {
            format!(
                "- {} | tokens≈{} | {}",
                section.kind, section.estimated_tokens, section.description
            )
        }));
    }
    if !output.history_tail.is_empty() {
        lines.push(String::new());
        lines.push("history_tail:".to_string());
        lines.extend(output.history_tail.iter().map(|item| {
            format!(
                "- #{} {} {} | tokens≈{} | {}",
                item.index, item.role, item.kind, item.estimated_tokens, item.preview
            )
        }));
    }
    lines
}

#[derive(Debug, Clone)]
struct RolloutReconstruction {
    history: Vec<ResponseItem>,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: Option<TurnContextItem>,
    latest_compaction_summary: Option<String>,
    latest_compaction_replacement_items: Option<usize>,
}

#[derive(Debug, Clone)]
struct PreviousTurnSettings {
    model: String,
}

#[derive(Debug, Default)]
enum TurnReferenceContextItem {
    #[default]
    NeverSet,
    Cleared,
    Latest(Box<TurnContextItem>),
}

#[derive(Debug, Default)]
struct ActiveReplaySegment<'a> {
    turn_id: Option<String>,
    counts_as_user_turn: bool,
    previous_turn_settings: Option<PreviousTurnSettings>,
    reference_context_item: TurnReferenceContextItem,
    base_replacement_history: Option<&'a [ResponseItem]>,
}

async fn build_prompt_preview_snapshot(
    summary: &SessionSummary,
    runtime_config: &codex_core::config::Config,
    rollout_items: &[RolloutItem],
    next_user_input: Option<&str>,
) -> Result<PromptPreviewSnapshot> {
    let reconstruction = reconstruct_rollout_history(rollout_items);
    let current_model = runtime_config
        .model
        .clone()
        .or(summary.latest_model.clone())
        .unwrap_or_default();
    let model_switch_message = reconstruction
        .previous_turn_settings
        .as_ref()
        .is_some_and(|settings| settings.model != current_model);
    let developer_instructions_present = runtime_config
        .developer_instructions
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    let user_instructions_present = runtime_config
        .user_instructions
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    let (memory_prompt_available, memory_summary_tokens_estimate) =
        detect_memory_prompt(runtime_config).await?;
    let mut prompt_sections = vec![PromptSectionPreview {
        kind: "reconstructed_history".to_string(),
        description: format!(
            "{} model-visible history item(s) reconstructed from rollout replay",
            reconstruction.history.len()
        ),
        estimated_tokens: summary
            .latest_context_tokens
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or_else(|| estimate_response_items_tokens(reconstruction.history.as_slice())),
    }];
    let context_strategy = if reconstruction.reference_context_item.is_none() {
        prompt_sections.push(PromptSectionPreview {
            kind: "initial_context".to_string(),
            description:
                "next turn will reinject canonical initial context before appending the new user input"
                    .to_string(),
            estimated_tokens: estimate_initial_context_tokens(
                developer_instructions_present,
                user_instructions_present,
                memory_summary_tokens_estimate,
            ),
        });
        NextTurnContextStrategy::FullInitialContext {
            model_switch_message,
            memory_prompt: memory_prompt_available,
            developer_instructions: developer_instructions_present,
            user_instructions: user_instructions_present,
        }
    } else {
        let reasons = diff_reasons(
            reconstruction.reference_context_item.as_ref(),
            runtime_config,
        );
        if model_switch_message || !reasons.is_empty() {
            prompt_sections.push(PromptSectionPreview {
                kind: "settings_update".to_string(),
                description: if reasons.is_empty() {
                    "next turn only adds the model-switch developer message".to_string()
                } else {
                    format!(
                        "next turn sends incremental context updates for: {}",
                        reasons.join(", ")
                    )
                },
                estimated_tokens: 256,
            });
        }
        NextTurnContextStrategy::SettingsUpdate {
            model_switch_message,
            reasons,
        }
    };
    if memory_prompt_available {
        prompt_sections.push(PromptSectionPreview {
            kind: "memory_prompt".to_string(),
            description: "developer instructions will include the local memory quick-pass workflow"
                .to_string(),
            estimated_tokens: memory_summary_tokens_estimate.unwrap_or(0),
        });
    }
    if developer_instructions_present {
        prompt_sections.push(PromptSectionPreview {
            kind: "developer_instructions".to_string(),
            description: "resolved runtime has additional developer instructions".to_string(),
            estimated_tokens: runtime_config
                .developer_instructions
                .as_deref()
                .map_or(0, approx_token_count),
        });
    }
    if user_instructions_present {
        prompt_sections.push(PromptSectionPreview {
            kind: "user_instructions".to_string(),
            description: "resolved runtime has user instructions".to_string(),
            estimated_tokens: runtime_config
                .user_instructions
                .as_deref()
                .map_or(0, approx_token_count),
        });
    }
    if let Some(next_user_input) = next_user_input {
        prompt_sections.push(PromptSectionPreview {
            kind: "pending_user_input".to_string(),
            description: "simulated next user message appended at the end of the prompt"
                .to_string(),
            estimated_tokens: approx_token_count(next_user_input),
        });
    }

    Ok(PromptPreviewSnapshot {
        reconstructed_history: reconstruction.history.clone(),
        history_items_count: reconstruction.history.len(),
        history_tokens_estimate: summary
            .latest_context_tokens
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or_else(|| estimate_response_items_tokens(reconstruction.history.as_slice())),
        previous_turn_model: reconstruction
            .previous_turn_settings
            .map(|settings| settings.model),
        reference_context_item: reconstruction.reference_context_item,
        context_strategy,
        latest_compaction_summary: reconstruction.latest_compaction_summary,
        latest_compaction_replacement_items: reconstruction.latest_compaction_replacement_items,
        memory_prompt_available,
        memory_summary_tokens_estimate,
        developer_instructions_present,
        user_instructions_present,
        prompt_sections,
        history_tail: build_history_tail(reconstruction.history.as_slice(), 12),
    })
}

fn reconstruct_rollout_history(rollout_items: &[RolloutItem]) -> RolloutReconstruction {
    let mut base_replacement_history: Option<&[ResponseItem]> = None;
    let mut previous_turn_settings = None;
    let mut reference_context_item = TurnReferenceContextItem::NeverSet;
    let mut pending_rollback_turns = 0usize;
    let mut rollout_suffix = rollout_items;
    let mut active_segment: Option<ActiveReplaySegment<'_>> = None;
    let mut latest_compaction_summary = None;
    let mut latest_compaction_replacement_items = None;

    for (index, item) in rollout_items.iter().enumerate().rev() {
        match item {
            RolloutItem::Compacted(compacted) => {
                if latest_compaction_summary.is_none() {
                    let text = normalize_message(compacted.message.as_str());
                    if !text.is_empty() {
                        latest_compaction_summary = Some(text);
                    }
                    latest_compaction_replacement_items =
                        compacted.replacement_history.as_ref().map(Vec::len);
                }
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                if matches!(
                    active_segment.reference_context_item,
                    TurnReferenceContextItem::NeverSet
                ) {
                    active_segment.reference_context_item = TurnReferenceContextItem::Cleared;
                }
                if active_segment.base_replacement_history.is_none()
                    && let Some(replacement_history) = &compacted.replacement_history
                {
                    active_segment.base_replacement_history = Some(replacement_history);
                    rollout_suffix = &rollout_items[index + 1..];
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                pending_rollback_turns = pending_rollback_turns
                    .saturating_add(usize::try_from(rollback.num_turns).unwrap_or(usize::MAX));
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                if active_segment.turn_id.is_none() {
                    active_segment.turn_id = Some(event.turn_id.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                if let Some(active_segment) = active_segment.as_mut() {
                    if active_segment.turn_id.is_none()
                        && let Some(turn_id) = &event.turn_id
                    {
                        active_segment.turn_id = Some(turn_id.clone());
                    }
                } else if let Some(turn_id) = &event.turn_id {
                    active_segment = Some(ActiveReplaySegment {
                        turn_id: Some(turn_id.clone()),
                        ..Default::default()
                    });
                }
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                active_segment.counts_as_user_turn = true;
            }
            RolloutItem::TurnContext(ctx) => {
                let active_segment =
                    active_segment.get_or_insert_with(ActiveReplaySegment::default);
                if active_segment.turn_id.is_none() {
                    active_segment.turn_id = ctx.turn_id.clone();
                }
                if turn_ids_are_compatible(
                    active_segment.turn_id.as_deref(),
                    ctx.turn_id.as_deref(),
                ) {
                    active_segment.previous_turn_settings = Some(PreviousTurnSettings {
                        model: ctx.model.clone(),
                    });
                    if matches!(
                        active_segment.reference_context_item,
                        TurnReferenceContextItem::NeverSet
                    ) {
                        active_segment.reference_context_item =
                            TurnReferenceContextItem::Latest(Box::new(ctx.clone()));
                    }
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                if active_segment.as_ref().is_some_and(|active_segment| {
                    turn_ids_are_compatible(
                        active_segment.turn_id.as_deref(),
                        Some(event.turn_id.as_str()),
                    )
                }) && let Some(active_segment) = active_segment.take()
                {
                    finalize_active_segment(
                        active_segment,
                        &mut base_replacement_history,
                        &mut previous_turn_settings,
                        &mut reference_context_item,
                        &mut pending_rollback_turns,
                    );
                }
            }
            RolloutItem::ResponseItem(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::SessionMeta(_) => {}
        }

        if base_replacement_history.is_some()
            && previous_turn_settings.is_some()
            && !matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        {
            break;
        }
    }

    if let Some(active_segment) = active_segment.take() {
        finalize_active_segment(
            active_segment,
            &mut base_replacement_history,
            &mut previous_turn_settings,
            &mut reference_context_item,
            &mut pending_rollback_turns,
        );
    }

    let mut history = Vec::new();
    let mut saw_legacy_compaction_without_replacement_history = false;
    if let Some(base_replacement_history) = base_replacement_history {
        history = filter_prompt_history(base_replacement_history);
    }
    for item in rollout_suffix {
        match item {
            RolloutItem::ResponseItem(response_item) => {
                if is_prompt_history_item(response_item) {
                    history.push(response_item.clone());
                }
            }
            RolloutItem::Compacted(compacted) => {
                if let Some(replacement_history) = &compacted.replacement_history {
                    history = filter_prompt_history(replacement_history);
                } else {
                    saw_legacy_compaction_without_replacement_history = true;
                    let user_messages = collect_user_messages(history.as_slice());
                    history =
                        build_compacted_history(Vec::new(), &user_messages, &compacted.message);
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                drop_last_n_user_turns(&mut history, rollback.num_turns);
            }
            RolloutItem::EventMsg(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::SessionMeta(_) => {}
        }
    }

    history.retain(|item| !matches!(item, ResponseItem::GhostSnapshot { .. }));
    let reference_context_item = match reference_context_item {
        TurnReferenceContextItem::NeverSet | TurnReferenceContextItem::Cleared => None,
        TurnReferenceContextItem::Latest(item) => Some(*item),
    };
    let reference_context_item = if saw_legacy_compaction_without_replacement_history {
        None
    } else {
        reference_context_item
    };

    RolloutReconstruction {
        history,
        previous_turn_settings,
        reference_context_item,
        latest_compaction_summary,
        latest_compaction_replacement_items,
    }
}

fn finalize_active_segment<'a>(
    active_segment: ActiveReplaySegment<'a>,
    base_replacement_history: &mut Option<&'a [ResponseItem]>,
    previous_turn_settings: &mut Option<PreviousTurnSettings>,
    reference_context_item: &mut TurnReferenceContextItem,
    pending_rollback_turns: &mut usize,
) {
    if *pending_rollback_turns > 0 {
        if active_segment.counts_as_user_turn {
            *pending_rollback_turns -= 1;
        }
        return;
    }
    if base_replacement_history.is_none()
        && let Some(segment_base_replacement_history) = active_segment.base_replacement_history
    {
        *base_replacement_history = Some(segment_base_replacement_history);
    }
    if previous_turn_settings.is_none() && active_segment.counts_as_user_turn {
        *previous_turn_settings = active_segment.previous_turn_settings;
    }
    if matches!(reference_context_item, TurnReferenceContextItem::NeverSet)
        && (active_segment.counts_as_user_turn
            || matches!(
                active_segment.reference_context_item,
                TurnReferenceContextItem::Cleared
            ))
    {
        *reference_context_item = active_segment.reference_context_item;
    }
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn filter_prompt_history(items: &[ResponseItem]) -> Vec<ResponseItem> {
    items
        .iter()
        .filter(|item| is_prompt_history_item(item))
        .cloned()
        .collect()
}

fn is_prompt_history_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, .. } => role.as_str() != "system",
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::GhostSnapshot { .. } => true,
        ResponseItem::Other => false,
    }
}

fn build_compacted_history(
    mut initial_context: Vec<ResponseItem>,
    user_messages: &[String],
    summary_text: &str,
) -> Vec<ResponseItem> {
    const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;
    let mut selected_messages = Vec::new();
    let mut remaining = COMPACT_USER_MESSAGE_MAX_TOKENS;
    for message in user_messages.iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = approx_token_count(message);
        if tokens <= remaining {
            selected_messages.push(message.clone());
            remaining = remaining.saturating_sub(tokens);
        } else {
            selected_messages.push(truncate_for_preview(message, remaining));
            break;
        }
    }
    selected_messages.reverse();
    for message in selected_messages {
        initial_context.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: message }],
            end_turn: None,
            phase: None,
        });
    }
    initial_context.push(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: if summary_text.trim().is_empty() {
                "(no summary available)".to_string()
            } else {
                summary_text.to_string()
            },
        }],
        end_turn: None,
        phase: None,
    });
    initial_context
}

fn collect_user_messages(items: &[ResponseItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match parse_turn_item(item) {
            Some(codex_protocol::items::TurnItem::UserMessage(user_message)) => {
                let message = user_message.message();
                if is_summary_message(message.as_str()) {
                    None
                } else {
                    Some(message)
                }
            }
            _ => None,
        })
        .collect()
}

fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{}\n", codex_core::compact::SUMMARY_PREFIX).as_str())
}

fn drop_last_n_user_turns(items: &mut Vec<ResponseItem>, num_turns: u32) {
    if num_turns == 0 {
        return;
    }
    let snapshot = items.clone();
    let user_positions = snapshot
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            ResponseItem::Message { role, .. } if role == "user" => Some(index),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(&first_user_idx) = user_positions.first() else {
        *items = snapshot;
        return;
    };
    let n_from_end = usize::try_from(num_turns).unwrap_or(usize::MAX);
    let cut_idx = if n_from_end >= user_positions.len() {
        first_user_idx
    } else {
        user_positions[user_positions.len() - n_from_end]
    };
    *items = snapshot[..cut_idx].to_vec();
}

fn build_history_tail(items: &[ResponseItem], limit: usize) -> Vec<PromptItemPreview> {
    let start = items.len().saturating_sub(limit);
    items[start..]
        .iter()
        .enumerate()
        .map(|(offset, item)| prompt_item_preview(start + offset + 1, item))
        .collect()
}

fn prompt_item_preview(index: usize, item: &ResponseItem) -> PromptItemPreview {
    match item {
        ResponseItem::Message { role, content, .. } => PromptItemPreview {
            index,
            role: role.clone(),
            kind: "message".to_string(),
            preview: truncate_for_preview(
                &content
                    .iter()
                    .map(|part| match part {
                        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                            text.as_str()
                        }
                        ContentItem::InputImage { .. } => "[image]",
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                160,
            ),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::FunctionCall { name, .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: format!("function_call:{name}"),
            preview: "(tool call)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::CustomToolCall { name, .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: format!("custom_tool_call:{name}"),
            preview: "(custom tool call)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::FunctionCallOutput { .. } => PromptItemPreview {
            index,
            role: "tool".to_string(),
            kind: "function_call_output".to_string(),
            preview: "(tool output)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::CustomToolCallOutput { .. } => PromptItemPreview {
            index,
            role: "tool".to_string(),
            kind: "custom_tool_call_output".to_string(),
            preview: "(custom tool output)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::LocalShellCall { .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: "local_shell_call".to_string(),
            preview: "(local shell call)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::Reasoning { .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: "reasoning".to_string(),
            preview: "(encrypted reasoning content)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::WebSearchCall { .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: "web_search_call".to_string(),
            preview: "(web search call)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::ImageGenerationCall { .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: "image_generation_call".to_string(),
            preview: "(image generation call)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::ToolSearchCall { .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: "tool_search_call".to_string(),
            preview: "(tool search call)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::ToolSearchOutput { .. } => PromptItemPreview {
            index,
            role: "tool".to_string(),
            kind: "tool_search_output".to_string(),
            preview: "(tool search output)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::Compaction { .. } => PromptItemPreview {
            index,
            role: "assistant".to_string(),
            kind: "compaction".to_string(),
            preview: "(encrypted compaction summary)".to_string(),
            estimated_tokens: estimate_response_item_tokens(item),
        },
        ResponseItem::GhostSnapshot { .. } => PromptItemPreview {
            index,
            role: "system".to_string(),
            kind: "ghost_snapshot".to_string(),
            preview: "(ghost snapshot)".to_string(),
            estimated_tokens: 0,
        },
        ResponseItem::Other => PromptItemPreview {
            index,
            role: "other".to_string(),
            kind: "other".to_string(),
            preview: "(other response item)".to_string(),
            estimated_tokens: 0,
        },
    }
}

async fn detect_memory_prompt(
    runtime_config: &codex_core::config::Config,
) -> Result<(bool, Option<usize>)> {
    if !runtime_config.features.enabled(Feature::MemoryTool)
        || !runtime_config.memories.use_memories
    {
        return Ok((false, None));
    }
    let path = runtime_config
        .codex_home
        .join("memories")
        .join("memory_summary.md");
    if !path.exists() {
        return Ok((false, None));
    }
    let memory_summary = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let trimmed = memory_summary.trim();
    if trimmed.is_empty() {
        return Ok((false, None));
    }
    Ok((true, Some(approx_token_count(trimmed))))
}

fn diff_reasons(
    reference_context_item: Option<&TurnContextItem>,
    runtime_config: &codex_core::config::Config,
) -> Vec<String> {
    let Some(reference_context_item) = reference_context_item else {
        return Vec::new();
    };
    let mut reasons = Vec::new();
    if reference_context_item.cwd != runtime_config.cwd {
        reasons.push("environment_context".to_string());
    }
    if reference_context_item.approval_policy != runtime_config.permissions.approval_policy.value()
        || reference_context_item.sandbox_policy != *runtime_config.permissions.sandbox_policy.get()
    {
        reasons.push("permissions".to_string());
    }
    if reference_context_item.personality != runtime_config.personality {
        reasons.push("personality".to_string());
    }
    reasons
}

fn estimate_initial_context_tokens(
    developer_instructions_present: bool,
    user_instructions_present: bool,
    memory_summary_tokens_estimate: Option<usize>,
) -> usize {
    let mut estimate = 256usize;
    if developer_instructions_present {
        estimate += 128;
    }
    if user_instructions_present {
        estimate += 128;
    }
    estimate + memory_summary_tokens_estimate.unwrap_or(0)
}

fn estimate_response_items_tokens(items: &[ResponseItem]) -> usize {
    items.iter().map(estimate_response_item_tokens).sum()
}

fn estimate_response_item_tokens(item: &ResponseItem) -> usize {
    serde_json::to_string(item)
        .map(|serialized| approx_token_count(serialized.as_str()))
        .unwrap_or(0)
}

fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn truncate_for_preview(text: &str, max_chars: usize) -> String {
    let text = normalize_message(text);
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut text = text.chars().take(max_chars).collect::<String>();
    text.push('…');
    text
}

fn normalize_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::build_compacted_history;
    use super::collect_user_messages;
    use super::drop_last_n_user_turns;
    use super::reconstruct_rollout_history;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::CompactedItem;
    use codex_protocol::protocol::RolloutItem;
    use pretty_assertions::assert_eq;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    #[test]
    fn build_compacted_history_keeps_recent_user_messages_and_summary() {
        let history = build_compacted_history(
            Vec::new(),
            &["first".to_string(), "second".to_string()],
            "summary text",
        );
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn drop_last_n_user_turns_removes_newest_user_messages() {
        let mut items = vec![
            user_message("one"),
            user_message("two"),
            user_message("three"),
        ];
        drop_last_n_user_turns(&mut items, 2);
        assert_eq!(items, vec![user_message("one")]);
    }

    #[test]
    fn reconstruct_rollout_history_applies_replacement_history() {
        let rollout_items = vec![
            RolloutItem::ResponseItem(user_message("one")),
            RolloutItem::ResponseItem(user_message("two")),
            RolloutItem::Compacted(CompactedItem {
                message: "summary".to_string(),
                replacement_history: Some(vec![user_message("one"), user_message("two")]),
            }),
        ];

        let reconstruction = reconstruct_rollout_history(&rollout_items);
        assert_eq!(
            collect_user_messages(reconstruction.history.as_slice()),
            vec!["one", "two"]
        );
    }
}
