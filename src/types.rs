use codex_core::config::Config;
use codex_protocol::ThreadId;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub(crate) struct TokenSnapshot {
    pub(crate) context_tokens: Option<i64>,
    pub(crate) session_total_tokens: Option<i64>,
    pub(crate) model_context_window: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionSummary {
    pub(crate) thread_id: String,
    pub(crate) thread_name: Option<String>,
    pub(crate) rollout_path: PathBuf,
    pub(crate) archived: bool,
    pub(crate) source: String,
    pub(crate) session_provider: Option<String>,
    pub(crate) session_cwd: PathBuf,
    pub(crate) session_timestamp: String,
    pub(crate) latest_model: Option<String>,
    pub(crate) latest_total_tokens: Option<i64>,
    pub(crate) latest_context_tokens: Option<i64>,
    pub(crate) latest_model_context_window: Option<i64>,
    pub(crate) user_turns: usize,
    pub(crate) first_user_message: Option<String>,
    pub(crate) forked_from_id: Option<String>,
    pub(crate) memory_mode: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ForkOutcome {
    pub(crate) thread_id: ThreadId,
    pub(crate) rollout_path: PathBuf,
    pub(crate) runtime_profile: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ForkRequest {
    pub(crate) source_profile: Option<String>,
    pub(crate) source_rollout_path: PathBuf,
    pub(crate) model: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) context_window: Option<i64>,
    pub(crate) auto_compact_token_limit: Option<i64>,
    pub(crate) write_profile: Option<String>,
    pub(crate) thread_name: Option<String>,
    pub(crate) persist_extended_history: bool,
    pub(crate) nth_user_message: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct ResolvedTarget {
    pub(crate) config: Config,
    pub(crate) rollout_path: PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) enum OperationKind {
    Compact,
    Rollback,
}
