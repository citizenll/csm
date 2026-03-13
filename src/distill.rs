use crate::cli::DistillArgs;
use crate::operations::archive_rollout_file;
use crate::operations::build_thread_manager;
use crate::operations::reconcile_rollout_path;
use crate::operations::resolve_new_rollout_path;
use crate::operations::shutdown_thread;
use crate::runtime::load_session_runtime_config;
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
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use serde::Serialize;
use std::time::Duration;

pub(crate) async fn run(args: DistillArgs) -> Result<()> {
    if args.recent_turns == 0 {
        bail!("recent_turns must be >= 1");
    }

    let resolved = resolve_target(&args.target).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
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
    let raw_rollout_lines = read_rollout_lines(resolved.rollout_path.as_path()).await?;
    let analysis = analyze_rollout(
        reconstructed_items.as_slice(),
        raw_rollout_lines.as_slice(),
        args.recent_turns,
    );
    let brief = build_distilled_brief(&summary, &analysis);
    let report = build_report(
        &summary,
        &analysis,
        brief.as_str(),
        default_distilled_thread_name(&summary, args.thread_name.as_deref())?,
    );

    if args.preview_only {
        return print_output(
            DistillOutput {
                successor_thread_id: None,
                successor_rollout_path: None,
                resume_command: None,
                source_archived: summary.archived,
                report,
                brief,
            },
            args.json,
        );
    }

    let runtime_config =
        load_session_runtime_config(args.target.config_profile.clone(), &summary).await?;
    if let Some(profile) = args.write_profile.as_deref() {
        write_profile_from_config(profile, &runtime_config).await?;
    }

    let session_meta = read_session_meta_line(summary.rollout_path.as_path()).await?;
    let (thread_manager, _auth_manager) =
        build_thread_manager(&runtime_config, session_meta.meta.source.clone());
    let new_thread = thread_manager
        .start_thread(runtime_config.clone())
        .await
        .context("failed to start distilled successor thread")?;
    let successor_rollout_path =
        resolve_new_rollout_path(&runtime_config, &new_thread.thread, new_thread.thread_id).await?;
    let successor_name = report.successor_thread_name.clone();
    append_thread_name(
        &runtime_config.codex_home,
        new_thread.thread_id,
        successor_name.as_str(),
    )
    .await
    .with_context(|| format!("failed to assign thread name `{successor_name}`"))?;

    let model = runtime_config
        .model
        .clone()
        .or(summary.latest_model.clone())
        .context("could not resolve model slug for distilled successor")?;
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

    if args.archive_source {
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
    print_output(output, args.json)
}

#[derive(Debug, Clone)]
struct DistillAnalysis {
    latest_compaction_summary: Option<String>,
    recent_user_messages: Vec<String>,
    recent_assistant_messages: Vec<String>,
    recent_warnings: Vec<String>,
    recent_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DistillReport {
    source_thread_id: String,
    source_thread_name: Option<String>,
    source_rollout_path: std::path::PathBuf,
    source_provider: Option<String>,
    source_model: Option<String>,
    source_context_tokens_estimate: Option<i64>,
    source_context_window: Option<i64>,
    source_user_turns: usize,
    successor_thread_name: String,
    successor_seed_tokens_estimate: usize,
    compression_ratio: Option<f64>,
    recent_user_messages_kept: usize,
    recent_assistant_messages_kept: usize,
    warnings_kept: usize,
    errors_kept: usize,
    had_compaction_summary: bool,
}

#[derive(Debug, Serialize)]
struct DistillOutput {
    successor_thread_id: Option<String>,
    successor_rollout_path: Option<std::path::PathBuf>,
    resume_command: Option<String>,
    source_archived: bool,
    report: DistillReport,
    brief: String,
}

fn analyze_rollout(
    reconstructed_items: &[RolloutItem],
    raw_rollout_lines: &[RolloutLine],
    recent_turns: usize,
) -> DistillAnalysis {
    let mut user_messages = Vec::new();
    let mut assistant_messages = Vec::new();
    for item in reconstructed_items {
        let RolloutItem::ResponseItem(response_item) = item else {
            continue;
        };
        match parse_turn_item(response_item) {
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
        latest_compaction_summary,
        recent_user_messages: take_tail_dedup(user_messages, recent_turns),
        recent_assistant_messages: take_tail_dedup(assistant_messages, recent_turns),
        recent_warnings: take_tail_dedup(warnings, 6),
        recent_errors: take_tail_dedup(errors, 6),
    }
}

fn build_distilled_brief(
    summary: &crate::types::SessionSummary,
    analysis: &DistillAnalysis,
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

    if let Some(compaction_summary) = analysis.latest_compaction_summary.as_deref() {
        sections.push(String::new());
        sections.push("# Existing Compaction Summary".to_string());
        sections.push(truncate_for_brief(compaction_summary, 1800));
    }

    if !analysis.recent_user_messages.is_empty() {
        sections.push(String::new());
        sections.push("# Recent User Requests".to_string());
        sections.extend(analysis.recent_user_messages.iter().enumerate().map(
            |(index, message)| format!("{}. {}", index + 1, truncate_for_brief(message, 320)),
        ));
    }

    if !analysis.recent_assistant_messages.is_empty() {
        sections.push(String::new());
        sections.push("# Recent Assistant Outcomes".to_string());
        sections.extend(analysis.recent_assistant_messages.iter().enumerate().map(
            |(index, message)| format!("{}. {}", index + 1, truncate_for_brief(message, 320)),
        ));
    }

    if !analysis.recent_warnings.is_empty() || !analysis.recent_errors.is_empty() {
        sections.push(String::new());
        sections.push("# Recent Warnings And Errors".to_string());
        sections.extend(
            analysis
                .recent_warnings
                .iter()
                .map(|message| format!("- warning: {}", truncate_for_brief(message, 240))),
        );
        sections.extend(
            analysis
                .recent_errors
                .iter()
                .map(|message| format!("- error: {}", truncate_for_brief(message, 240))),
        );
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

fn print_output(output: DistillOutput, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("source_thread_id: {}", output.report.source_thread_id);
    println!(
        "source_thread_name: {}",
        output.report.source_thread_name.as_deref().unwrap_or("")
    );
    println!(
        "source_rollout_path: {}",
        output.report.source_rollout_path.display()
    );
    println!(
        "source_provider: {}",
        output.report.source_provider.as_deref().unwrap_or("")
    );
    println!(
        "source_model: {}",
        output.report.source_model.as_deref().unwrap_or("")
    );
    println!(
        "source_context_tokens_estimate: {}",
        output
            .report
            .source_context_tokens_estimate
            .map_or_else(String::new, |value| value.to_string())
    );
    println!(
        "successor_seed_tokens_estimate: {}",
        output.report.successor_seed_tokens_estimate
    );
    println!(
        "compression_ratio: {}",
        output
            .report
            .compression_ratio
            .map_or_else(String::new, |value| format!("{value:.4}"))
    );
    println!(
        "successor_thread_name: {}",
        output.report.successor_thread_name
    );
    println!(
        "had_compaction_summary: {}",
        output.report.had_compaction_summary
    );
    println!(
        "recent_user_messages_kept: {}",
        output.report.recent_user_messages_kept
    );
    println!(
        "recent_assistant_messages_kept: {}",
        output.report.recent_assistant_messages_kept
    );
    println!("warnings_kept: {}", output.report.warnings_kept);
    println!("errors_kept: {}", output.report.errors_kept);
    println!(
        "successor_thread_id: {}",
        output.successor_thread_id.as_deref().unwrap_or("")
    );
    println!(
        "successor_rollout_path: {}",
        output
            .successor_rollout_path
            .as_ref()
            .map_or_else(String::new, |path| path.display().to_string())
    );
    println!(
        "resume_command: {}",
        output.resume_command.as_deref().unwrap_or("")
    );
    println!("source_archived: {}", output.source_archived);
    println!();
    if !output.brief.is_empty() {
        println!("brief:");
        println!("{}", output.brief);
    }
    Ok(())
}

async fn wait_for_seed_turn_completion(
    thread: &std::sync::Arc<codex_core::CodexThread>,
    submit_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    let wait = async {
        loop {
            let event = thread.next_event().await?;
            if event.id != submit_id {
                continue;
            }
            match event.msg {
                EventMsg::TurnComplete(_) => return Ok(()),
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
    use crate::types::SessionSummary;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::CompactedItem;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

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

    #[test]
    fn analyze_rollout_collects_recent_signals() {
        let reconstructed_items = vec![
            RolloutItem::ResponseItem(response_message("user", "first request")),
            RolloutItem::ResponseItem(response_message("assistant", "first result")),
            RolloutItem::ResponseItem(response_message("user", "second request")),
            RolloutItem::ResponseItem(response_message("assistant", "second result")),
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

        let analysis = analyze_rollout(&reconstructed_items, &raw_rollout_lines, 2);
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
            recent_warnings: vec![],
            recent_errors: vec!["failure".to_string()],
        };

        let brief = build_distilled_brief(&summary(), &analysis);
        assert!(brief.contains("# Source Thread"));
        assert!(brief.contains("# Existing Compaction Summary"));
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
            recent_warnings: vec![],
            recent_errors: vec![],
        };
        let brief = "abcd".repeat(400);
        let report = build_report(
            &summary(),
            &analysis,
            brief.as_str(),
            "Distilled".to_string(),
        );
        assert_eq!(
            report.successor_seed_tokens_estimate,
            approx_token_count(brief.as_str())
        );
        assert!(report.compression_ratio.is_some());
        assert_eq!(report.successor_thread_name, "Distilled");
    }
}
