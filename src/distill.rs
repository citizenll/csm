use crate::cli::DistillArgs;
use crate::cli::DistillCompressionLevel;
use crate::cli::DistillMode;
use crate::operations::archive_rollout_file;
use crate::operations::build_thread_manager;
use crate::operations::reconcile_rollout_path;
use crate::operations::resolve_new_rollout_path;
use crate::operations::shutdown_thread;
use crate::preview;
use crate::profile_cleanup::cleanup_generated_profiles;
use crate::progress::DistillProgressEvent;
use crate::progress::OperationProgressEvent;
use crate::progress::ProgressSender;
use crate::progress::emit_progress;
use crate::runtime::load_runtime_config;
use crate::runtime::render_profiled_resume_command;
use crate::runtime::resolve_target;
use crate::runtime::write_profile_from_config;
use crate::summary::build_session_summary;
use crate::summary::read_rollout_lines;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::append_thread_name;
use codex_core::parse_turn_item;
use codex_core::read_session_meta_line;
use codex_core::util::normalize_thread_name;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use serde::Serialize;
use std::time::Duration;

pub(crate) async fn run(args: DistillArgs) -> Result<()> {
    let json = args.json;
    let output = execute(args).await?;
    print_output(&output, json)
}

pub(crate) async fn execute(args: DistillArgs) -> Result<DistillOutput> {
    execute_with_progress(args, None).await
}

pub(crate) async fn execute_with_progress(
    args: DistillArgs,
    progress: Option<ProgressSender>,
) -> Result<DistillOutput> {
    if args.recent_turns == 0 {
        bail!("recent_turns must be >= 1");
    }

    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::ResolvingTarget),
    );
    let resolved = resolve_target(&args.target).await?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::LoadingSessionSummary),
    );
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::RebuildingHistory),
    );
    let initial_history =
        codex_core::RolloutRecorder::get_rollout_history(resolved.rollout_path.as_path())
            .await
            .with_context(|| {
                format!(
                    "failed to reconstruct history from {}",
                    resolved.rollout_path.display()
                )
            })?;
    let reconstructed_items = initial_history.get_rollout_items();
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::ReadingRolloutLines),
    );
    let raw_rollout_lines = read_rollout_lines(resolved.rollout_path.as_path()).await?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::AnalyzingHistory {
            history_items: reconstructed_items.len(),
            raw_lines: raw_rollout_lines.len(),
        }),
    );
    let prompt_preview = preview::build_prompt_preview_for_distill(&args.target).await?;
    let compression_policy = DistillCompressionPolicy::for_level(args.compression_level);
    let successor_thread_name =
        default_distilled_thread_name(&summary, args.thread_name.as_deref())?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::ResolvingRuntimeConfig),
    );
    let target_runtime_config = load_distill_runtime_config(&args, &summary).await?;
    let mut analysis = analyze_rollout(
        prompt_preview.reconstructed_history.as_slice(),
        raw_rollout_lines.as_slice(),
        &prompt_preview,
        args.recent_turns,
        compression_policy,
    );
    analysis.pinned_facts = collect_pinned_facts(
        &summary,
        &analysis,
        &prompt_preview,
        &target_runtime_config,
        compression_policy,
    );
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::BuildingDeterministicBrief),
    );
    let deterministic_brief = build_distilled_brief(&summary, &analysis, compression_policy);
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::DeterministicBriefReady {
            user_messages: analysis.recent_user_messages.len(),
            assistant_messages: analysis.recent_assistant_messages.len(),
            durable_guidance: analysis.durable_guidance.len(),
            estimated_tokens: approx_token_count(deterministic_brief.as_str()),
        }),
    );
    let session_source = read_session_meta_line(summary.rollout_path.as_path())
        .await?
        .meta
        .source;
    let (brief, effective_distill_mode, distill_note) = match args.distill_mode {
        DistillMode::Deterministic => (
            deterministic_brief.clone(),
            DistillMode::Deterministic,
            None,
        ),
        DistillMode::Codex => match run_codex_distillation(
            &target_runtime_config,
            session_source,
            deterministic_brief.as_str(),
            compression_policy,
            parse_reasoning_effort(args.reasoning_effort.as_deref())?,
            args.timeout_secs,
            progress.as_ref(),
        )
        .await
        {
            Ok(brief) => (brief, DistillMode::Codex, None),
            Err(error) => {
                emit_progress(
                    progress.as_ref(),
                    OperationProgressEvent::Distill(
                        DistillProgressEvent::CodexDistillationFallback {
                            error: error.to_string(),
                            estimated_tokens: approx_token_count(deterministic_brief.as_str()),
                        },
                    ),
                );
                (
                    deterministic_brief.clone(),
                    DistillMode::Deterministic,
                    Some(format!(
                        "codex distillation failed, fell back to deterministic: {error}"
                    )),
                )
            }
        },
    };
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::BuildingReport),
    );
    let report = build_report(
        &summary,
        &analysis,
        brief.as_str(),
        successor_thread_name.clone(),
        &target_runtime_config,
        DistillReportOptions {
            distill_mode: effective_distill_mode,
            compression_level: args.compression_level,
            distill_note,
        },
    );

    if args.preview_only {
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Distill(DistillProgressEvent::PreviewReady),
        );
        let output = DistillOutput {
            successor_thread_id: None,
            successor_rollout_path: None,
            resume_command: None,
            source_archived: summary.archived,
            report,
            brief,
        };
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Distill(DistillProgressEvent::Completed {
                preview_only: true,
                successor_thread_id: None,
            }),
        );
        return Ok(output);
    }

    let runtime_config = target_runtime_config;
    if let Some(profile) = args.write_profile.as_deref() {
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Distill(DistillProgressEvent::WritingProfile {
                profile: profile.to_string(),
            }),
        );
        write_profile_from_config(profile, &runtime_config).await?;
    }

    let session_meta = read_session_meta_line(summary.rollout_path.as_path()).await?;
    let (thread_manager, _auth_manager) =
        build_thread_manager(&runtime_config, session_meta.meta.source.clone());
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::StartingSuccessorThread),
    );
    let new_thread = thread_manager
        .start_thread(runtime_config.clone())
        .await
        .context("failed to start distilled successor thread")?;
    let successor_rollout_path =
        resolve_new_rollout_path(&runtime_config, &new_thread.thread, new_thread.thread_id).await?;
    let successor_name = successor_thread_name;
    append_thread_name(
        &runtime_config.codex_home,
        new_thread.thread_id,
        successor_name.as_str(),
    )
    .await
    .with_context(|| format!("failed to assign thread name `{successor_name}`"))?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::SuccessorThreadNamed {
            thread_name: successor_name.clone(),
        }),
    );

    let model = runtime_config
        .model
        .clone()
        .or(summary.latest_model.clone())
        .context("could not resolve model slug for distilled successor")?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::SeedingSuccessorThread),
    );
    let submit_id = new_thread
        .thread
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: handoff_seed_prompt(brief.as_str()),
                text_elements: Vec::new(),
            }],
            cwd: summary.session_cwd.clone(),
            approval_policy: runtime_config.permissions.approval_policy.value(),
            sandbox_policy: runtime_config.permissions.sandbox_policy.get().clone(),
            model,
            effort: None,
            summary: None,
            service_tier: None,
            final_output_json_schema: None,
            collaboration_mode: None,
            personality: runtime_config.personality,
        })
        .await
        .context("failed to submit distilled handoff turn")?;

    let seed_turn_result =
        wait_for_seed_turn_completion(&new_thread.thread, submit_id.as_str(), args.timeout_secs)
            .await;
    let shutdown_result =
        shutdown_thread(&thread_manager, new_thread.thread_id, &new_thread.thread).await;
    seed_turn_result?;
    shutdown_result?;
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::SeedTurnCompleted),
    );

    if args.archive_source {
        emit_progress(
            progress.as_ref(),
            OperationProgressEvent::Distill(DistillProgressEvent::ArchivingSource),
        );
        let archived_path = archive_rollout_file(
            runtime_config.codex_home.as_path(),
            session_meta.meta.id,
            summary.rollout_path.as_path(),
        )
        .await?;
        reconcile_rollout_path(
            &runtime_config,
            session_meta.meta.id,
            archived_path.as_path(),
            true,
        )
        .await?;
    }

    let _ = cleanup_generated_profiles(
        runtime_config.codex_home.as_path(),
        &[
            args.target.config_profile.as_deref(),
            args.write_profile.as_deref(),
        ],
    )
    .await;

    let output = DistillOutput {
        successor_thread_id: Some(new_thread.thread_id.to_string()),
        successor_rollout_path: Some(successor_rollout_path),
        resume_command: Some(render_profiled_resume_command(
            args.write_profile.as_deref(),
            new_thread.thread_id,
        )),
        source_archived: args.archive_source,
        report,
        brief,
    };
    emit_progress(
        progress.as_ref(),
        OperationProgressEvent::Distill(DistillProgressEvent::Completed {
            preview_only: false,
            successor_thread_id: output.successor_thread_id.clone(),
        }),
    );
    Ok(output)
}

#[derive(Debug, Clone)]
struct DistillAnalysis {
    latest_compaction_summary: Option<String>,
    recent_user_messages: Vec<String>,
    recent_assistant_messages: Vec<String>,
    durable_guidance: Vec<String>,
    pinned_facts: Vec<String>,
    prompt_reconstruction_notes: Vec<String>,
    recent_warnings: Vec<String>,
    recent_errors: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct DistillCompressionPolicy {
    level: DistillCompressionLevel,
    minimum_recent_turns: usize,
    minimum_durable_guidance: usize,
    pinned_guidance_items: usize,
    pinned_tool_names: usize,
    compaction_summary_chars: usize,
    request_chars: usize,
    outcome_chars: usize,
    guidance_chars: usize,
    warning_chars: usize,
}

impl DistillCompressionPolicy {
    fn for_level(level: DistillCompressionLevel) -> Self {
        match level {
            DistillCompressionLevel::Lossless => Self {
                level,
                minimum_recent_turns: 24,
                minimum_durable_guidance: 32,
                pinned_guidance_items: 10,
                pinned_tool_names: 8,
                compaction_summary_chars: 3_600,
                request_chars: 640,
                outcome_chars: 640,
                guidance_chars: 900,
                warning_chars: 480,
            },
            DistillCompressionLevel::Balanced => Self {
                level,
                minimum_recent_turns: 12,
                minimum_durable_guidance: 20,
                pinned_guidance_items: 6,
                pinned_tool_names: 5,
                compaction_summary_chars: 2_400,
                request_chars: 480,
                outcome_chars: 480,
                guidance_chars: 540,
                warning_chars: 320,
            },
            DistillCompressionLevel::Aggressive => Self {
                level,
                minimum_recent_turns: 8,
                minimum_durable_guidance: 12,
                pinned_guidance_items: 4,
                pinned_tool_names: 3,
                compaction_summary_chars: 1_800,
                request_chars: 320,
                outcome_chars: 320,
                guidance_chars: 360,
                warning_chars: 240,
            },
        }
    }

    fn effective_recent_turns(self, requested_recent_turns: usize) -> usize {
        requested_recent_turns.max(self.minimum_recent_turns)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DistillReport {
    pub(crate) source_thread_id: String,
    pub(crate) source_thread_name: Option<String>,
    pub(crate) source_rollout_path: std::path::PathBuf,
    pub(crate) source_provider: Option<String>,
    pub(crate) source_model: Option<String>,
    pub(crate) source_context_tokens_estimate: Option<i64>,
    pub(crate) source_context_window: Option<i64>,
    pub(crate) source_user_turns: usize,
    pub(crate) successor_provider: String,
    pub(crate) successor_model: String,
    pub(crate) successor_context_window: Option<i64>,
    pub(crate) distill_mode: String,
    pub(crate) compression_level: String,
    pub(crate) distill_note: Option<String>,
    pub(crate) successor_thread_name: String,
    pub(crate) successor_seed_tokens_estimate: usize,
    pub(crate) compression_ratio: Option<f64>,
    pub(crate) recent_user_messages_kept: usize,
    pub(crate) recent_assistant_messages_kept: usize,
    pub(crate) warnings_kept: usize,
    pub(crate) errors_kept: usize,
    pub(crate) had_compaction_summary: bool,
}

#[derive(Debug, Clone)]
struct DistillReportOptions {
    distill_mode: DistillMode,
    compression_level: DistillCompressionLevel,
    distill_note: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DistillOutput {
    pub(crate) successor_thread_id: Option<String>,
    pub(crate) successor_rollout_path: Option<std::path::PathBuf>,
    pub(crate) resume_command: Option<String>,
    pub(crate) source_archived: bool,
    pub(crate) report: DistillReport,
    pub(crate) brief: String,
}

fn analyze_rollout(
    reconstructed_items: &[codex_protocol::models::ResponseItem],
    raw_rollout_lines: &[RolloutLine],
    prompt_preview: &preview::PromptPreviewSnapshot,
    recent_turns: usize,
    compression_policy: DistillCompressionPolicy,
) -> DistillAnalysis {
    let effective_recent_turns = compression_policy.effective_recent_turns(recent_turns);
    let mut user_messages = Vec::new();
    let mut assistant_messages = Vec::new();
    for item in reconstructed_items {
        match parse_turn_item(item) {
            Some(TurnItem::UserMessage(user_message)) => {
                let text = normalize_message(user_message.message().as_str());
                if !text.is_empty() {
                    user_messages.push(text);
                }
            }
            Some(TurnItem::AgentMessage(agent_message)) => {
                let text = agent_message_text(&agent_message);
                if !text.is_empty() {
                    assistant_messages.push(text);
                }
            }
            Some(TurnItem::Plan(_))
            | Some(TurnItem::Reasoning(_))
            | Some(TurnItem::WebSearch(_))
            | Some(TurnItem::ImageGeneration(_))
            | Some(TurnItem::ContextCompaction(_))
            | None => {}
        }
    }

    let latest_compaction_summary = raw_rollout_lines.iter().rev().find_map(|line| {
        let RolloutItem::Compacted(compacted) = &line.item else {
            return None;
        };
        let text = normalize_message(compacted.message.as_str());
        (!text.is_empty()).then_some(text)
    });

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    for line in raw_rollout_lines {
        let RolloutItem::EventMsg(event_msg) = &line.item else {
            continue;
        };
        match event_msg {
            EventMsg::Warning(warning) => {
                let text = normalize_message(warning.message.as_str());
                if !text.is_empty() {
                    warnings.push(text);
                }
            }
            EventMsg::Error(error) => {
                let text = normalize_message(error.message.as_str());
                if !text.is_empty() {
                    errors.push(text);
                }
            }
            EventMsg::TurnStarted(_)
            | EventMsg::TurnComplete(_)
            | EventMsg::TokenCount(_)
            | EventMsg::ContextCompacted(_)
            | EventMsg::ThreadRolledBack(_)
            | EventMsg::UserMessage(_)
            | EventMsg::AgentMessage(_)
            | EventMsg::AgentMessageDelta(_)
            | EventMsg::AgentReasoning(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::AgentReasoningRawContent(_)
            | EventMsg::AgentReasoningRawContentDelta(_)
            | EventMsg::AgentReasoningSectionBreak(_)
            | EventMsg::SessionConfigured(_)
            | EventMsg::ThreadNameUpdated(_)
            | EventMsg::ModelReroute(_)
            | EventMsg::DeprecationNotice(_)
            | EventMsg::BackgroundEvent(_)
            | EventMsg::UndoStarted(_)
            | EventMsg::UndoCompleted(_)
            | EventMsg::StreamError(_)
            | EventMsg::PatchApplyBegin(_)
            | EventMsg::PatchApplyEnd(_)
            | EventMsg::TurnDiff(_)
            | EventMsg::GetHistoryEntryResponse(_)
            | EventMsg::McpListToolsResponse(_)
            | EventMsg::ListCustomPromptsResponse(_)
            | EventMsg::ListSkillsResponse(_)
            | EventMsg::ListRemoteSkillsResponse(_)
            | EventMsg::RemoteSkillDownloaded(_)
            | EventMsg::SkillsUpdateAvailable
            | EventMsg::PlanUpdate(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::TurnAborted(_)
            | EventMsg::ShutdownComplete
            | EventMsg::EnteredReviewMode(_)
            | EventMsg::ExitedReviewMode(_)
            | EventMsg::RawResponseItem(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::ItemCompleted(_)
            | EventMsg::HookStarted(_)
            | EventMsg::HookCompleted(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::CollabAgentSpawnBegin(_)
            | EventMsg::CollabAgentSpawnEnd(_)
            | EventMsg::CollabAgentInteractionBegin(_)
            | EventMsg::CollabAgentInteractionEnd(_)
            | EventMsg::ViewImageToolCall(_)
            | EventMsg::ImageGenerationBegin(_)
            | EventMsg::ImageGenerationEnd(_)
            | EventMsg::McpStartupUpdate(_)
            | EventMsg::McpStartupComplete(_)
            | EventMsg::McpToolCallBegin(_)
            | EventMsg::McpToolCallEnd(_)
            | EventMsg::WebSearchBegin(_)
            | EventMsg::WebSearchEnd(_)
            | EventMsg::ExecCommandBegin(_)
            | EventMsg::ExecCommandOutputDelta(_)
            | EventMsg::TerminalInteraction(_)
            | EventMsg::ExecCommandEnd(_)
            | EventMsg::ExecApprovalRequest(_)
            | EventMsg::RequestPermissions(_)
            | EventMsg::RequestUserInput(_)
            | EventMsg::DynamicToolCallRequest(_)
            | EventMsg::DynamicToolCallResponse(_)
            | EventMsg::ElicitationRequest(_)
            | EventMsg::ApplyPatchApprovalRequest(_)
            | EventMsg::RealtimeConversationStarted(_)
            | EventMsg::RealtimeConversationRealtime(_)
            | EventMsg::RealtimeConversationClosed(_) => {}
            _ => {}
        }
    }

    DistillAnalysis {
        latest_compaction_summary: prompt_preview
            .latest_compaction_summary
            .clone()
            .or(latest_compaction_summary),
        recent_user_messages: take_tail_dedup(user_messages, effective_recent_turns),
        recent_assistant_messages: take_tail_dedup(assistant_messages, effective_recent_turns),
        durable_guidance: collect_durable_guidance_from_response_items(
            reconstructed_items,
            effective_recent_turns.max(compression_policy.minimum_durable_guidance),
        ),
        pinned_facts: Vec::new(),
        prompt_reconstruction_notes: prompt_reconstruction_notes(prompt_preview),
        recent_warnings: take_tail_dedup(warnings, 6),
        recent_errors: take_tail_dedup(errors, 6),
    }
}

fn build_distilled_brief(
    summary: &crate::types::SessionSummary,
    analysis: &DistillAnalysis,
    compression_policy: DistillCompressionPolicy,
) -> String {
    let thread_title = summary
        .thread_name
        .clone()
        .or(summary.first_user_message.clone())
        .unwrap_or_else(|| summary.thread_id.clone());
    let provider = summary
        .session_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let model = summary
        .latest_model
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let context_window = summary
        .latest_model_context_window
        .map_or_else(String::new, |value| value.to_string());
    let context_tokens = summary
        .latest_context_tokens
        .map_or_else(String::new, |value| value.to_string());

    let mut sections = vec![
        "You are continuing work from a distilled Codex session.".to_string(),
        "Treat the following handoff brief as the authoritative carry-over context for the successor thread.".to_string(),
        "Prefer this brief over assumptions from omitted older history.".to_string(),
        String::new(),
        "# Source Thread".to_string(),
        format!("- Thread title: {thread_title}"),
        format!("- Source thread id: {}", summary.thread_id),
        format!("- Workspace: {}", summary.session_cwd.display()),
        format!("- Provider: {provider}"),
        format!("- Model: {model}"),
        format!("- Context window: {context_window}"),
        format!("- Context tokens estimate: {context_tokens}"),
        format!("- User turns in source thread: {}", summary.user_turns),
    ];

    if !analysis.pinned_facts.is_empty() {
        sections.push(String::new());
        sections.push("# Pinned Facts".to_string());
        sections.extend(analysis.pinned_facts.iter().cloned());
    }

    if let Some(compaction_summary) = analysis.latest_compaction_summary.as_deref() {
        sections.push(String::new());
        sections.push("# Existing Compaction Summary".to_string());
        sections.push(truncate_for_brief(
            compaction_summary,
            compression_policy.compaction_summary_chars,
        ));
    }

    if !analysis.prompt_reconstruction_notes.is_empty() {
        sections.push(String::new());
        sections.push("# Next-Turn Prompt Reconstruction".to_string());
        sections.extend(analysis.prompt_reconstruction_notes.iter().cloned());
    }

    if !analysis.recent_user_messages.is_empty() {
        sections.push(String::new());
        sections.push("# Recent User Requests".to_string());
        sections.extend(analysis.recent_user_messages.iter().enumerate().map(
            |(index, message)| {
                format!(
                    "{}. {}",
                    index + 1,
                    truncate_for_brief(message, compression_policy.request_chars)
                )
            },
        ));
    }

    if !analysis.recent_assistant_messages.is_empty() {
        sections.push(String::new());
        sections.push("# Recent Assistant Outcomes".to_string());
        sections.extend(analysis.recent_assistant_messages.iter().enumerate().map(
            |(index, message)| {
                format!(
                    "{}. {}",
                    index + 1,
                    truncate_for_brief(message, compression_policy.outcome_chars)
                )
            },
        ));
    }

    if !analysis.durable_guidance.is_empty() {
        sections.push(String::new());
        sections.push("# Durable Conventions And Corrections".to_string());
        sections.extend(
            analysis
                .durable_guidance
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    format!(
                        "{}. {}",
                        index + 1,
                        truncate_for_brief(message, compression_policy.guidance_chars)
                    )
                }),
        );
    }

    if !analysis.recent_warnings.is_empty() || !analysis.recent_errors.is_empty() {
        sections.push(String::new());
        sections.push("# Recent Warnings And Errors".to_string());
        sections.extend(analysis.recent_warnings.iter().map(|message| {
            format!(
                "- warning: {}",
                truncate_for_brief(message, compression_policy.warning_chars)
            )
        }));
        sections.extend(analysis.recent_errors.iter().map(|message| {
            format!(
                "- error: {}",
                truncate_for_brief(message, compression_policy.warning_chars)
            )
        }));
    }

    sections.push(String::new());
    sections.push("# Successor Instructions".to_string());
    sections.push("- Continue from this project state without assuming the full source-thread history is still loaded.".to_string());
    sections.push(
        "- Treat the source thread as archival context for audit/detail lookups only.".to_string(),
    );
    sections.push("- When the next real user request arrives, continue from the current workspace and runtime constraints above.".to_string());
    sections.push(
        "- Reply with READY and one short sentence confirming the current project focus."
            .to_string(),
    );

    sections.join("\n")
}

fn build_report(
    summary: &crate::types::SessionSummary,
    analysis: &DistillAnalysis,
    brief: &str,
    successor_thread_name: String,
    target_runtime_config: &codex_core::config::Config,
    options: DistillReportOptions,
) -> DistillReport {
    let successor_seed_tokens_estimate = approx_token_count(brief);
    let compression_ratio = summary.latest_context_tokens.and_then(|source_tokens| {
        (source_tokens > 0).then_some(successor_seed_tokens_estimate as f64 / source_tokens as f64)
    });

    DistillReport {
        source_thread_id: summary.thread_id.clone(),
        source_thread_name: summary.thread_name.clone(),
        source_rollout_path: summary.rollout_path.clone(),
        source_provider: summary.session_provider.clone(),
        source_model: summary.latest_model.clone(),
        source_context_tokens_estimate: summary.latest_context_tokens,
        source_context_window: summary.latest_model_context_window,
        source_user_turns: summary.user_turns,
        successor_provider: target_runtime_config.model_provider_id.clone(),
        successor_model: target_runtime_config.model.clone().unwrap_or_default(),
        successor_context_window: target_runtime_config.model_context_window,
        distill_mode: match options.distill_mode {
            DistillMode::Codex => "codex".to_string(),
            DistillMode::Deterministic => "deterministic".to_string(),
        },
        compression_level: match options.compression_level {
            DistillCompressionLevel::Lossless => "lossless".to_string(),
            DistillCompressionLevel::Balanced => "balanced".to_string(),
            DistillCompressionLevel::Aggressive => "aggressive".to_string(),
        },
        distill_note: options.distill_note,
        successor_thread_name,
        successor_seed_tokens_estimate,
        compression_ratio,
        recent_user_messages_kept: analysis.recent_user_messages.len(),
        recent_assistant_messages_kept: analysis.recent_assistant_messages.len(),
        warnings_kept: analysis.recent_warnings.len(),
        errors_kept: analysis.recent_errors.len(),
        had_compaction_summary: analysis.latest_compaction_summary.is_some(),
    }
}

async fn load_distill_runtime_config(
    args: &DistillArgs,
    summary: &crate::types::SessionSummary,
) -> Result<codex_core::config::Config> {
    load_runtime_config(
        args.target.config_profile.clone(),
        Some(summary.session_cwd.clone()),
        args.model.clone().or(summary.latest_model.clone()),
        args.provider.clone().or(summary.session_provider.clone()),
        args.context_window.or(summary.latest_model_context_window),
        args.auto_compact_token_limit,
    )
    .await
}

async fn run_codex_distillation(
    runtime_config: &codex_core::config::Config,
    session_source: codex_protocol::protocol::SessionSource,
    deterministic_brief: &str,
    compression_policy: DistillCompressionPolicy,
    reasoning_effort: Option<ReasoningEffort>,
    timeout_secs: u64,
    progress: Option<&ProgressSender>,
) -> Result<String> {
    let mut ephemeral_config = runtime_config.clone();
    ephemeral_config.ephemeral = true;
    emit_progress(
        progress,
        OperationProgressEvent::Distill(DistillProgressEvent::StartingCodexDistillation),
    );
    let (thread_manager, _auth_manager) = build_thread_manager(&ephemeral_config, session_source);
    let new_thread = thread_manager
        .start_thread(ephemeral_config.clone())
        .await
        .context("failed to start ephemeral codex distillation thread")?;
    emit_progress(
        progress,
        OperationProgressEvent::Distill(DistillProgressEvent::CodexEphemeralThreadStarted),
    );
    let model = ephemeral_config
        .model
        .clone()
        .context("could not resolve model slug for codex distillation")?;
    let submit_id = new_thread
        .thread
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: codex_distillation_prompt(deterministic_brief, compression_policy),
                text_elements: Vec::new(),
            }],
            cwd: ephemeral_config.cwd.clone(),
            approval_policy: ephemeral_config.permissions.approval_policy.value(),
            sandbox_policy: ephemeral_config.permissions.sandbox_policy.get().clone(),
            model,
            effort: reasoning_effort,
            summary: Some(ReasoningSummaryConfig::None),
            service_tier: None,
            final_output_json_schema: None,
            collaboration_mode: None,
            personality: ephemeral_config.personality,
        })
        .await
        .context("failed to submit codex distillation turn")?;
    emit_progress(
        progress,
        OperationProgressEvent::Distill(DistillProgressEvent::CodexTurnSubmitted),
    );

    let result =
        wait_for_turn_completion_last_message(&new_thread.thread, submit_id.as_str(), timeout_secs)
            .await;
    let shutdown_result =
        shutdown_thread(&thread_manager, new_thread.thread_id, &new_thread.thread).await;
    let last_message = result?;
    shutdown_result?;
    let brief = last_message.context("codex distillation returned no assistant message")?;
    emit_progress(
        progress,
        OperationProgressEvent::Distill(DistillProgressEvent::CodexDistillationCompleted {
            estimated_tokens: approx_token_count(brief.as_str()),
        }),
    );
    Ok(brief)
}

fn codex_distillation_prompt(
    deterministic_brief: &str,
    compression_policy: DistillCompressionPolicy,
) -> String {
    let requirements = match compression_policy.level {
        DistillCompressionLevel::Lossless => {
            "- Preserve nearly all actionable project context, recurring corrections, unresolved work, active constraints, and current technical facts.\n- Do not aggressively shorten sections just to save tokens.\n- Prefer complete but well-organized carry-over context over brevity.\n- Remove only greetings, obvious repetition, and fully resolved dead ends."
        }
        DistillCompressionLevel::Balanced => {
            "- Keep durable project context, recurring corrections, unresolved work, active constraints, and current technical facts.\n- Remove greetings, repeated explanations, and clearly resolved dead ends.\n- Prefer concise but still implementation-safe carry-over context."
        }
        DistillCompressionLevel::Aggressive => {
            "- Keep only durable project context, current goals, unresolved work, active constraints, and risks.\n- Drop greetings, repeated explanations, solved dead ends, and verbose prose.\n- Be aggressively concise while preserving only the highest-signal facts."
        }
    };
    format!(
        "You are distilling a Codex project session into a lightweight successor handoff.\n\nRewrite the following deterministic handoff into a shorter, higher-signal brief.\n\nRequirements:\n{requirements}\n- Preserve concrete technical facts when they still matter.\n- Preserve the entire `# Pinned Facts` section exactly, including every bullet and value.\n- Preserve the substance of `# Durable Conventions And Corrections`; do not collapse it into vague wording.\n- Keep the `# Successor Instructions` section intact in meaning.\n- Output plain markdown with clear section headers.\n\nSource brief:\n\n{deterministic_brief}"
    )
}

fn parse_reasoning_effort(value: Option<&str>) -> Result<Option<ReasoningEffort>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let parsed = match value.trim().to_ascii_lowercase().as_str() {
        "none" => ReasoningEffort::None,
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" | "x-high" => ReasoningEffort::XHigh,
        other => bail!(
            "unsupported reasoning effort `{other}`; expected one of: none, minimal, low, medium, high, xhigh"
        ),
    };
    Ok(Some(parsed))
}

pub(crate) fn render_output_lines(output: &DistillOutput) -> Vec<String> {
    let mut lines = vec![
        format!("source_thread_id: {}", output.report.source_thread_id),
        format!(
            "source_thread_name: {}",
            output.report.source_thread_name.as_deref().unwrap_or("")
        ),
        format!(
            "source_rollout_path: {}",
            output.report.source_rollout_path.display()
        ),
        format!(
            "source_provider: {}",
            output.report.source_provider.as_deref().unwrap_or("")
        ),
        format!(
            "source_model: {}",
            output.report.source_model.as_deref().unwrap_or("")
        ),
        format!(
            "source_context_tokens_estimate: {}",
            output
                .report
                .source_context_tokens_estimate
                .map_or_else(String::new, |value| value.to_string())
        ),
        format!("successor_provider: {}", output.report.successor_provider),
        format!("successor_model: {}", output.report.successor_model),
        format!(
            "successor_context_window: {}",
            output
                .report
                .successor_context_window
                .map_or_else(String::new, |value| value.to_string())
        ),
        format!("distill_mode: {}", output.report.distill_mode),
        format!("compression_level: {}", output.report.compression_level),
        format!(
            "distill_note: {}",
            output.report.distill_note.as_deref().unwrap_or("")
        ),
        format!(
            "successor_seed_tokens_estimate: {}",
            output.report.successor_seed_tokens_estimate
        ),
        format!(
            "compression_ratio: {}",
            output
                .report
                .compression_ratio
                .map_or_else(String::new, |value| format!("{value:.4}"))
        ),
        format!(
            "successor_thread_name: {}",
            output.report.successor_thread_name
        ),
        format!(
            "had_compaction_summary: {}",
            output.report.had_compaction_summary
        ),
        format!(
            "recent_user_messages_kept: {}",
            output.report.recent_user_messages_kept
        ),
        format!(
            "recent_assistant_messages_kept: {}",
            output.report.recent_assistant_messages_kept
        ),
        format!("warnings_kept: {}", output.report.warnings_kept),
        format!("errors_kept: {}", output.report.errors_kept),
        format!(
            "successor_thread_id: {}",
            output.successor_thread_id.as_deref().unwrap_or("")
        ),
        format!(
            "successor_rollout_path: {}",
            output
                .successor_rollout_path
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string())
        ),
        format!(
            "resume_command: {}",
            output.resume_command.as_deref().unwrap_or("")
        ),
        format!("source_archived: {}", output.source_archived),
    ];
    if !output.brief.is_empty() {
        lines.push(String::new());
        lines.push("brief:".to_string());
        lines.extend(output.brief.lines().map(ToOwned::to_owned));
    }
    lines
}

fn print_output(output: &DistillOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    for line in render_output_lines(output) {
        println!("{line}");
    }
    Ok(())
}

async fn wait_for_seed_turn_completion(
    thread: &std::sync::Arc<codex_core::CodexThread>,
    submit_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    wait_for_turn_completion_last_message(thread, submit_id, timeout_secs)
        .await
        .map(|_| ())
}

async fn wait_for_turn_completion_last_message(
    thread: &std::sync::Arc<codex_core::CodexThread>,
    submit_id: &str,
    timeout_secs: u64,
) -> Result<Option<String>> {
    let wait = async {
        loop {
            let event = thread.next_event().await?;
            if event.id != submit_id {
                continue;
            }
            match event.msg {
                EventMsg::TurnComplete(completed) => return Ok(completed.last_agent_message),
                EventMsg::Error(error) => bail!(error.message),
                _ => {}
            }
        }
    };

    tokio::time::timeout(Duration::from_secs(timeout_secs), wait)
        .await
        .with_context(|| format!("timed out waiting for distilled seed turn `{submit_id}`"))?
}

fn default_distilled_thread_name(
    summary: &crate::types::SessionSummary,
    override_name: Option<&str>,
) -> Result<String> {
    if let Some(override_name) = override_name {
        return normalize_thread_name(override_name)
            .context("thread name must not be empty for distilled successor");
    }

    let base = summary
        .thread_name
        .clone()
        .or(summary.first_user_message.clone())
        .unwrap_or_else(|| summary.thread_id.clone());
    let base = truncate_for_brief(base.as_str(), 72);
    normalize_thread_name(format!("Distilled · {base}").as_str())
        .context("failed to derive distilled successor thread name")
}

fn handoff_seed_prompt(brief: &str) -> String {
    format!(
        "Read and internalize the following distilled handoff brief for this project successor session.\n\n{brief}\n\nReply with READY and one short sentence confirming the current project focus."
    )
}

fn take_tail_dedup(items: Vec<String>, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    for item in items.into_iter().rev() {
        if out.iter().any(|existing| existing == &item) {
            continue;
        }
        out.push(item);
        if out.len() == limit {
            break;
        }
    }
    out.reverse();
    out
}

fn agent_message_text(item: &codex_protocol::items::AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|content| match content {
            AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn normalize_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_durable_guidance_from_response_items(
    items: &[codex_protocol::models::ResponseItem],
    limit: usize,
) -> Vec<String> {
    let mut candidates = Vec::new();
    for item in items {
        let text = match parse_turn_item(item) {
            Some(TurnItem::UserMessage(user_message)) => {
                normalize_message(user_message.message().as_str())
            }
            Some(TurnItem::AgentMessage(agent_message)) => agent_message_text(&agent_message),
            Some(TurnItem::Plan(_))
            | Some(TurnItem::Reasoning(_))
            | Some(TurnItem::WebSearch(_))
            | Some(TurnItem::ImageGeneration(_))
            | Some(TurnItem::ContextCompaction(_))
            | None => continue,
        };
        if is_durable_guidance(text.as_str()) {
            candidates.push(text);
        }
    }
    take_tail_dedup(candidates, limit)
}

fn prompt_reconstruction_notes(prompt_preview: &preview::PromptPreviewSnapshot) -> Vec<String> {
    let mut notes = vec![
        format!(
            "- Reconstructed model-visible history items: {}",
            prompt_preview.history_items_count
        ),
        format!(
            "- Reconstructed history tokens estimate: {}",
            prompt_preview.history_tokens_estimate
        ),
        format!(
            "- Built-in and dynamic tool schema tokens estimate: {}",
            prompt_preview.tool_schema_tokens_estimate
        ),
    ];
    if let Some(previous_turn_model) = prompt_preview.previous_turn_model.as_deref() {
        notes.push(format!(
            "- Previous surviving turn model: {previous_turn_model}"
        ));
    }
    match &prompt_preview.context_strategy {
        preview::NextTurnContextStrategy::FullInitialContext {
            model_switch_message,
            memory_prompt,
            developer_instructions,
            user_instructions,
        } => {
            notes.push(
                "- Next real user turn will reinject full canonical initial context.".to_string(),
            );
            notes.push(format!(
                "- Model-switch developer message will be injected: {model_switch_message}"
            ));
            notes.push(format!("- Memory prompt available: {memory_prompt}"));
            notes.push(format!(
                "- Runtime developer instructions present: {developer_instructions}"
            ));
            notes.push(format!(
                "- Runtime user instructions present: {user_instructions}"
            ));
        }
        preview::NextTurnContextStrategy::SettingsUpdate {
            model_switch_message,
            reasons,
        } => {
            notes.push(
                "- Next real user turn will reuse the current baseline and append only context diffs."
                    .to_string(),
            );
            notes.push(format!(
                "- Model-switch developer message will be injected: {model_switch_message}"
            ));
            if !reasons.is_empty() {
                notes.push(format!(
                    "- Expected settings update reasons: {}",
                    reasons.join(", ")
                ));
            }
        }
    }
    notes.push(format!(
        "- Memory prompt available: {}",
        prompt_preview.memory_prompt_available
    ));
    notes.push(format!(
        "- Built-in and dynamic tool count: {}",
        prompt_preview.tool_count
    ));
    notes
}

fn collect_pinned_facts(
    summary: &crate::types::SessionSummary,
    analysis: &DistillAnalysis,
    prompt_preview: &preview::PromptPreviewSnapshot,
    target_runtime_config: &codex_core::config::Config,
    compression_policy: DistillCompressionPolicy,
) -> Vec<String> {
    let mut facts = Vec::new();
    let mut push_unique = |fact: String| {
        if !facts.contains(&fact) {
            facts.push(fact);
        }
    };

    for guidance in analysis
        .durable_guidance
        .iter()
        .filter(|message| is_high_priority_guidance(message))
        .take(compression_policy.pinned_guidance_items)
    {
        push_unique(format!(
            "- Critical guidance: {}",
            truncate_for_brief(guidance, compression_policy.guidance_chars)
        ));
    }

    push_unique(format!(
        "- Reconstructed resume baseline: {} history items, about {} tokens before base instructions and tool schema.",
        prompt_preview.history_items_count, prompt_preview.history_tokens_estimate
    ));
    push_unique(format!(
        "- Successor runtime target: provider `{}`, model `{}`, context window `{}`, auto-compact threshold `{}`.",
        target_runtime_config.model_provider_id,
        target_runtime_config.model.clone().unwrap_or_default(),
        target_runtime_config
            .model_context_window
            .map_or_else(|| "unset".to_string(), |value| value.to_string()),
        target_runtime_config
            .model_auto_compact_token_limit
            .map_or_else(|| "unset".to_string(), |value| value.to_string())
    ));

    if let Some(previous_turn_model) = prompt_preview.previous_turn_model.as_deref() {
        push_unique(format!(
            "- Previous surviving turn model: `{previous_turn_model}`."
        ));
    }
    if let Some(forked_from_id) = summary.forked_from_id.as_deref() {
        push_unique(format!(
            "- Source thread was forked from `{forked_from_id}`."
        ));
    }
    if let Some(memory_mode) = summary.memory_mode.as_deref() {
        push_unique(format!("- Source thread memory mode: `{memory_mode}`."));
    }

    match &prompt_preview.context_strategy {
        preview::NextTurnContextStrategy::FullInitialContext {
            model_switch_message,
            memory_prompt,
            developer_instructions,
            user_instructions,
        } => push_unique(format!(
            "- Next real resume uses `full_initial_context` (model_switch_message={model_switch_message}, memory_prompt={memory_prompt}, developer_instructions={developer_instructions}, user_instructions={user_instructions})."
        )),
        preview::NextTurnContextStrategy::SettingsUpdate {
            model_switch_message,
            reasons,
        } => {
            let reasons = if reasons.is_empty() {
                "none".to_string()
            } else {
                reasons.join(", ")
            };
            push_unique(format!(
                "- Next real resume uses `settings_update` (model_switch_message={model_switch_message}, reasons={reasons})."
            ));
        }
    }

    push_unique(format!(
        "- Memory prompt available on resume: {} (summary tokens `{}`).",
        prompt_preview.memory_prompt_available,
        prompt_preview
            .memory_summary_tokens_estimate
            .map_or_else(|| "unknown".to_string(), |value| value.to_string())
    ));
    push_unique(format!(
        "- Runtime developer instructions present: {}; runtime user instructions present: {}.",
        prompt_preview.developer_instructions_present, prompt_preview.user_instructions_present
    ));

    if prompt_preview.tool_count > 0 {
        push_unique(format!(
            "- Next resume injects {} built-in/dynamic tools with about {} schema tokens.",
            prompt_preview.tool_count, prompt_preview.tool_schema_tokens_estimate
        ));
        let tool_names = prompt_preview
            .tool_names
            .iter()
            .take(compression_policy.pinned_tool_names)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        if !tool_names.is_empty() {
            push_unique(format!("- Representative tool names: {tool_names}."));
        }
    }

    if let Some(replacement_items) = prompt_preview.latest_compaction_replacement_items {
        push_unique(format!(
            "- Latest compaction replacement history currently contributes {replacement_items} item(s)."
        ));
    }
    if prompt_preview.reference_context_item.is_some() {
        push_unique(
            "- A reference context item survives in the reconstructed resume state.".to_string(),
        );
    }

    facts
}

fn is_durable_guidance(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "must",
        "should",
        "prefer",
        "always",
        "never",
        "style",
        "convention",
        "guideline",
        "source of truth",
        "remember this",
        "do not",
        "don't",
        "不要",
        "必须",
        "应该",
        "优先",
        "一律",
        "规范",
        "风格",
        "记住这个",
        "注意",
        "按照",
        "统一",
        "尽量",
        "避免",
        "不要用",
        "应该用",
        "保持",
        "source-backed",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_high_priority_guidance(text: &str) -> bool {
    let normalized = text.to_ascii_lowercase();
    [
        "must",
        "always",
        "never",
        "do not",
        "don't",
        "source of truth",
        "remember this",
        "必须",
        "一律",
        "不要",
        "记住这个",
        "规范",
        "保持",
        "不要用",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn truncate_for_brief(message: &str, max_chars: usize) -> String {
    let mut text = normalize_message(message);
    if text.chars().count() <= max_chars {
        return text;
    }
    text = text.chars().take(max_chars).collect::<String>();
    text.push('…');
    text
}

fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::analyze_rollout;
    use super::approx_token_count;
    use super::build_distilled_brief;
    use super::build_report;
    use super::codex_distillation_prompt;
    use super::collect_pinned_facts;
    use super::parse_reasoning_effort;
    use crate::cli::DistillCompressionLevel;
    use crate::cli::DistillMode;
    use crate::preview::NextTurnContextStrategy;
    use crate::preview::PromptPreviewSnapshot;
    use crate::types::SessionSummary;
    use codex_core::config::ConfigBuilder;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::openai_models::ReasoningEffort;
    use codex_protocol::protocol::CompactedItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn response_message(role: &str, text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![if role == "assistant" {
                ContentItem::OutputText {
                    text: text.to_string(),
                }
            } else {
                ContentItem::InputText {
                    text: text.to_string(),
                }
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn summary() -> SessionSummary {
        SessionSummary {
            thread_id: "thread-1".to_string(),
            thread_name: Some("Session Alpha".to_string()),
            rollout_path: PathBuf::from("D:/tmp/rollout.jsonl"),
            archived: false,
            source: "cli".to_string(),
            session_provider: Some("openai".to_string()),
            session_cwd: PathBuf::from("D:/tmp"),
            session_timestamp: "2026-03-13T00:00:00Z".to_string(),
            latest_model: Some("gpt-5.4".to_string()),
            latest_total_tokens: Some(12345),
            latest_context_tokens: Some(400000),
            latest_model_context_window: Some(950000),
            user_turns: 12,
            first_user_message: Some("bootstrap".to_string()),
            forked_from_id: None,
            memory_mode: None,
        }
    }

    async fn target_runtime_config() -> codex_core::config::Config {
        let temp = tempdir().expect("tempdir");
        ConfigBuilder::default()
            .codex_home(temp.path().to_path_buf())
            .build()
            .await
            .expect("build config")
    }

    #[test]
    fn analyze_rollout_collects_recent_signals() {
        let reconstructed_items = vec![
            response_message("user", "first request"),
            response_message("assistant", "first result"),
            response_message("user", "second request"),
            response_message("assistant", "second result"),
        ];
        let raw_rollout_lines = vec![
            RolloutLine {
                timestamp: "2026-03-13T00:00:00Z".to_string(),
                item: RolloutItem::Compacted(CompactedItem {
                    message: "summary of older work".to_string(),
                    replacement_history: None,
                }),
            },
            RolloutLine {
                timestamp: "2026-03-13T00:00:01Z".to_string(),
                item: RolloutItem::EventMsg(EventMsg::Warning(
                    codex_protocol::protocol::WarningEvent {
                        message: "warning one".to_string(),
                    },
                )),
            },
        ];
        let prompt_preview = PromptPreviewSnapshot {
            reconstructed_history: reconstructed_items.clone(),
            history_items_count: reconstructed_items.len(),
            history_tokens_estimate: 100,
            base_instructions_tokens_estimate: 0,
            tool_schema_tokens_estimate: 0,
            tool_count: 0,
            tool_names: Vec::new(),
            previous_turn_model: Some("gpt-5.4".to_string()),
            reference_context_item: None,
            context_strategy: NextTurnContextStrategy::FullInitialContext {
                model_switch_message: false,
                memory_prompt: false,
                developer_instructions: false,
                user_instructions: false,
            },
            latest_compaction_summary: None,
            latest_compaction_replacement_items: None,
            memory_prompt_available: false,
            memory_summary_tokens_estimate: None,
            developer_instructions_present: false,
            user_instructions_present: false,
            prompt_sections: Vec::new(),
            history_tail: Vec::new(),
        };

        let analysis = analyze_rollout(
            &reconstructed_items,
            &raw_rollout_lines,
            &prompt_preview,
            2,
            super::DistillCompressionPolicy::for_level(DistillCompressionLevel::Balanced),
        );
        assert_eq!(
            analysis.recent_user_messages,
            vec!["first request".to_string(), "second request".to_string()]
        );
        assert_eq!(
            analysis.recent_assistant_messages,
            vec!["first result".to_string(), "second result".to_string()]
        );
        assert_eq!(
            analysis.latest_compaction_summary,
            Some("summary of older work".to_string())
        );
        assert_eq!(analysis.recent_warnings, vec!["warning one".to_string()]);
    }

    #[test]
    fn build_distilled_brief_includes_key_sections() {
        let analysis = super::DistillAnalysis {
            latest_compaction_summary: Some("older summary".to_string()),
            recent_user_messages: vec!["do task".to_string()],
            recent_assistant_messages: vec!["task done".to_string()],
            durable_guidance: vec!["always prefer source-backed fixes".to_string()],
            pinned_facts: vec![
                "- Critical guidance: always prefer source-backed fixes".to_string(),
            ],
            prompt_reconstruction_notes: vec![
                "- Reconstructed model-visible history items: 12".to_string(),
            ],
            recent_warnings: vec![],
            recent_errors: vec!["failure".to_string()],
        };

        let brief = build_distilled_brief(
            &summary(),
            &analysis,
            super::DistillCompressionPolicy::for_level(DistillCompressionLevel::Balanced),
        );
        assert!(brief.contains("# Source Thread"));
        assert!(brief.contains("# Pinned Facts"));
        assert!(brief.contains("# Existing Compaction Summary"));
        assert!(brief.contains("# Next-Turn Prompt Reconstruction"));
        assert!(brief.contains("# Recent User Requests"));
        assert!(brief.contains("# Recent Assistant Outcomes"));
        assert!(brief.contains("# Recent Warnings And Errors"));
    }

    #[test]
    fn build_report_estimates_compression_ratio() {
        let analysis = super::DistillAnalysis {
            latest_compaction_summary: None,
            recent_user_messages: vec!["one".to_string()],
            recent_assistant_messages: vec!["two".to_string()],
            durable_guidance: vec![],
            pinned_facts: vec![],
            prompt_reconstruction_notes: vec![],
            recent_warnings: vec![],
            recent_errors: vec![],
        };
        let brief = "abcd".repeat(400);
        let runtime = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(target_runtime_config());
        let report = build_report(
            &summary(),
            &analysis,
            brief.as_str(),
            "Distilled".to_string(),
            &runtime,
            super::DistillReportOptions {
                distill_mode: DistillMode::Deterministic,
                compression_level: DistillCompressionLevel::Balanced,
                distill_note: None,
            },
        );
        assert_eq!(
            report.successor_seed_tokens_estimate,
            approx_token_count(brief.as_str())
        );
        assert!(report.compression_ratio.is_some());
        assert_eq!(report.successor_thread_name, "Distilled");
        assert_eq!(report.successor_provider, "openai");
        assert_eq!(report.distill_mode, "deterministic");
        assert_eq!(report.compression_level, "balanced");
    }

    #[test]
    fn parse_reasoning_effort_accepts_known_values() {
        assert_eq!(
            parse_reasoning_effort(Some("minimal")).expect("parse"),
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(
            parse_reasoning_effort(Some("xhigh")).expect("parse"),
            Some(ReasoningEffort::XHigh)
        );
        assert!(parse_reasoning_effort(Some("unknown")).is_err());
    }

    #[test]
    fn collect_pinned_facts_keeps_critical_guidance_and_runtime_shape() {
        let prompt_preview = PromptPreviewSnapshot {
            reconstructed_history: Vec::new(),
            history_items_count: 48,
            history_tokens_estimate: 12_345,
            base_instructions_tokens_estimate: 120,
            tool_schema_tokens_estimate: 640,
            tool_count: 3,
            tool_names: vec![
                "exec_command".to_string(),
                "apply_patch".to_string(),
                "web_search".to_string(),
            ],
            previous_turn_model: Some("gpt-5.4".to_string()),
            reference_context_item: None,
            context_strategy: NextTurnContextStrategy::SettingsUpdate {
                model_switch_message: true,
                reasons: vec!["model changed".to_string(), "provider changed".to_string()],
            },
            latest_compaction_summary: None,
            latest_compaction_replacement_items: Some(6),
            memory_prompt_available: true,
            memory_summary_tokens_estimate: Some(512),
            developer_instructions_present: true,
            user_instructions_present: false,
            prompt_sections: Vec::new(),
            history_tail: Vec::new(),
        };
        let analysis = super::DistillAnalysis {
            latest_compaction_summary: None,
            recent_user_messages: vec![],
            recent_assistant_messages: vec![],
            durable_guidance: vec![
                "must keep source-backed fixes".to_string(),
                "prefer smaller diffs".to_string(),
            ],
            pinned_facts: Vec::new(),
            prompt_reconstruction_notes: Vec::new(),
            recent_warnings: Vec::new(),
            recent_errors: Vec::new(),
        };
        let runtime = tokio::runtime::Runtime::new()
            .expect("rt")
            .block_on(target_runtime_config());

        let facts = collect_pinned_facts(
            &summary(),
            &analysis,
            &prompt_preview,
            &runtime,
            super::DistillCompressionPolicy::for_level(DistillCompressionLevel::Balanced),
        );

        assert!(facts.iter().any(|fact| fact.contains("Critical guidance")));
        assert!(
            facts
                .iter()
                .any(|fact| fact.contains("Successor runtime target"))
        );
        assert!(
            facts
                .iter()
                .any(|fact| fact.contains("Representative tool names"))
        );
    }

    #[test]
    fn codex_prompt_preserves_pinned_facts_requirement() {
        let prompt = codex_distillation_prompt(
            "# Pinned Facts\n- must keep source-backed fixes",
            super::DistillCompressionPolicy::for_level(DistillCompressionLevel::Balanced),
        );

        assert!(prompt.contains("Preserve the entire `# Pinned Facts` section exactly"));
        assert!(
            prompt.contains("Preserve the substance of `# Durable Conventions And Corrections`")
        );
    }
}
