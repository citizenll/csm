use crate::types::SessionSummary;
use crate::types::TokenSnapshot;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::ARCHIVED_SESSIONS_SUBDIR;
use codex_core::config::Config;
use codex_core::find_thread_name_by_id;
use codex_core::parse_turn_item;
use codex_core::read_session_meta_line;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::USER_MESSAGE_BEGIN;
use std::ffi::OsStr;
use std::path::Path;

pub(crate) async fn build_session_summary(
    config: &Config,
    rollout_path: &Path,
) -> Result<SessionSummary> {
    let session_meta = read_session_meta_line(rollout_path).await?;
    let thread_name = find_thread_name_by_id(&config.codex_home, &session_meta.meta.id).await?;
    let rollout_lines = read_rollout_lines(rollout_path).await?;

    let mut current_user_messages = Vec::new();
    let mut latest_model = None;
    let mut latest_token_snapshot = TokenSnapshot::default();
    let forked_from_id = session_meta.meta.forked_from_id.map(|id| id.to_string());
    let memory_mode = session_meta.meta.memory_mode.clone();

    for rollout_line in rollout_lines {
        match rollout_line.item {
            RolloutItem::SessionMeta(_) => {}
            RolloutItem::ResponseItem(item) => {
                if let Some(message) = user_message_text_from_response_item(&item) {
                    current_user_messages.push(message);
                }
            }
            RolloutItem::Compacted(compacted) => {
                current_user_messages = user_messages_from_replacement_history(
                    compacted.replacement_history.as_deref(),
                );
            }
            RolloutItem::TurnContext(context) => {
                latest_model = Some(context.model);
            }
            RolloutItem::EventMsg(event_msg) => match event_msg {
                EventMsg::TokenCount(event) => {
                    latest_token_snapshot = token_snapshot_from_usage(event.info.as_ref());
                }
                EventMsg::ThreadRolledBack(rollback) => {
                    drop_last_user_messages(&mut current_user_messages, rollback.num_turns);
                }
                _ => {}
            },
        }
    }

    Ok(SessionSummary {
        thread_id: session_meta.meta.id.to_string(),
        thread_name,
        rollout_path: rollout_path.to_path_buf(),
        archived: is_archived_rollout(rollout_path),
        source: format_session_source(&session_meta.meta.source),
        session_provider: session_meta.meta.model_provider,
        session_cwd: session_meta.meta.cwd,
        session_timestamp: session_meta.meta.timestamp,
        latest_model,
        latest_total_tokens: latest_token_snapshot.session_total_tokens,
        latest_context_tokens: latest_token_snapshot.context_tokens,
        latest_model_context_window: latest_token_snapshot.model_context_window,
        user_turns: current_user_messages.len(),
        first_user_message: current_user_messages.first().cloned(),
        forked_from_id,
        memory_mode,
    })
}

pub(crate) fn token_snapshot_from_usage(info: Option<&TokenUsageInfo>) -> TokenSnapshot {
    TokenSnapshot {
        context_tokens: info.map(|usage| usage.last_token_usage.total_tokens),
        session_total_tokens: info.map(|usage| usage.total_token_usage.total_tokens),
        model_context_window: info.and_then(|usage| usage.model_context_window),
    }
}

pub(crate) fn is_archived_rollout(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == OsStr::new(ARCHIVED_SESSIONS_SUBDIR))
}

pub(crate) fn format_session_source(source: &SessionSource) -> String {
    serde_json::to_string(source)
        .unwrap_or_else(|_| "unknown".to_string())
        .trim_matches('"')
        .to_string()
}

pub(crate) async fn read_rollout_lines(path: &Path) -> Result<Vec<RolloutLine>> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut rollout_lines = Vec::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(trimmed) {
            rollout_lines.push(rollout_line);
        }
    }

    if rollout_lines.is_empty() {
        bail!(
            "rollout at {} contains no parseable records",
            path.display()
        );
    }

    Ok(rollout_lines)
}

pub(crate) fn user_message_text_from_response_item(item: &ResponseItem) -> Option<String> {
    match parse_turn_item(item) {
        Some(TurnItem::UserMessage(message)) => {
            let text = strip_user_message_prefix(message.message());
            (!text.is_empty()).then_some(text)
        }
        Some(TurnItem::AgentMessage(_))
        | Some(TurnItem::Plan(_))
        | Some(TurnItem::Reasoning(_))
        | Some(TurnItem::WebSearch(_))
        | Some(TurnItem::ImageGeneration(_))
        | Some(TurnItem::ContextCompaction(_))
        | None => None,
    }
}

pub(crate) fn user_messages_from_replacement_history(
    replacement_history: Option<&[ResponseItem]>,
) -> Vec<String> {
    replacement_history
        .unwrap_or_default()
        .iter()
        .filter_map(user_message_text_from_response_item)
        .collect()
}

pub(crate) fn strip_user_message_prefix(message: String) -> String {
    match message.find(USER_MESSAGE_BEGIN) {
        Some(index) => message[index + USER_MESSAGE_BEGIN.len()..]
            .trim()
            .to_string(),
        None => message.trim().to_string(),
    }
}

pub(crate) fn drop_last_user_messages(messages: &mut Vec<String>, num_turns: u32) {
    let remaining = messages.len().saturating_sub(num_turns as usize);
    messages.truncate(remaining);
}
