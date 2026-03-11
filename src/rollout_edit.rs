use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub(crate) struct MetaPatch {
    pub(crate) provider: Option<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) memory_mode: Option<String>,
    pub(crate) clear_memory_mode: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResumeStatePatch {
    pub(crate) model_context_window: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResumeStateRewriteStats {
    pub(crate) token_count_events_updated: usize,
    pub(crate) turn_started_events_updated: usize,
}

pub(crate) fn rewrite_rollout_meta_contents(
    existing_contents: &str,
    patch: &MetaPatch,
) -> Result<String> {
    let mut rewritten = String::with_capacity(existing_contents.len());
    let mut replaced = false;

    for segment in existing_contents.split_inclusive('\n') {
        if replaced || segment.trim().is_empty() {
            rewritten.push_str(segment);
            continue;
        }

        let (raw_line, line_ending) = split_line_ending(segment);
        let rollout_line: RolloutLine =
            serde_json::from_str(raw_line).context("failed to parse first rollout line")?;
        let RolloutItem::SessionMeta(mut session_meta_line) = rollout_line.item else {
            bail!("first non-empty rollout line is not a SessionMeta record");
        };

        if let Some(provider) = &patch.provider {
            session_meta_line.meta.model_provider = Some(provider.clone());
        }
        if let Some(cwd) = &patch.cwd {
            session_meta_line.meta.cwd = cwd.clone();
        }
        if patch.clear_memory_mode {
            session_meta_line.meta.memory_mode = None;
        } else if let Some(memory_mode) = &patch.memory_mode {
            session_meta_line.meta.memory_mode = Some(memory_mode.clone());
        }

        let updated_rollout_line = RolloutLine {
            timestamp: rollout_line.timestamp,
            item: RolloutItem::SessionMeta(session_meta_line),
        };
        rewritten.push_str(&serde_json::to_string(&updated_rollout_line)?);
        rewritten.push_str(line_ending);
        replaced = true;
    }

    if !replaced {
        bail!("rollout does not contain a SessionMeta record");
    }

    if !existing_contents.ends_with('\n') && rewritten.ends_with('\n') {
        rewritten.pop();
    }

    Ok(rewritten)
}

pub(crate) fn rewrite_rollout_resume_state_contents(
    existing_contents: &str,
    patch: &ResumeStatePatch,
) -> Result<(String, ResumeStateRewriteStats)> {
    let mut rewritten = String::with_capacity(existing_contents.len());
    let mut stats = ResumeStateRewriteStats::default();

    for (line_index, segment) in existing_contents.split_inclusive('\n').enumerate() {
        if segment.trim().is_empty() {
            rewritten.push_str(segment);
            continue;
        }

        let (raw_line, line_ending) = split_line_ending(segment);
        let mut rollout_line: RolloutLine = serde_json::from_str(raw_line)
            .with_context(|| format!("failed to parse rollout line {}", line_index + 1))?;
        let mut changed = false;

        match &mut rollout_line.item {
            RolloutItem::EventMsg(EventMsg::TokenCount(event)) => {
                if let Some(info) = event.info.as_mut()
                    && info.model_context_window != Some(patch.model_context_window)
                {
                    info.model_context_window = Some(patch.model_context_window);
                    stats.token_count_events_updated += 1;
                    changed = true;
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                if event.model_context_window != Some(patch.model_context_window) {
                    event.model_context_window = Some(patch.model_context_window);
                    stats.turn_started_events_updated += 1;
                    changed = true;
                }
            }
            RolloutItem::EventMsg(_)
            | RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_) => {}
        }

        if changed {
            rewritten.push_str(&serde_json::to_string(&rollout_line)?);
            rewritten.push_str(line_ending);
        } else {
            rewritten.push_str(segment);
        }
    }

    if !existing_contents.ends_with('\n') && rewritten.ends_with('\n') {
        rewritten.pop();
    }

    Ok((rewritten, stats))
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(stripped) = line.strip_suffix("\r\n") {
        (stripped, "\r\n")
    } else if let Some(stripped) = line.strip_suffix('\n') {
        (stripped, "\n")
    } else {
        (line, "")
    }
}
