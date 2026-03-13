use crate::runtime::load_runtime_config;
use crate::runtime::write_profile_from_config;
use crate::summary::build_session_summary;
use crate::summary::is_archived_rollout;
use crate::types::ForkOutcome;
use crate::types::ForkRequest;
use crate::types::OperationKind;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::ARCHIVED_SESSIONS_SUBDIR;
use codex_core::AuthManager;
use codex_core::SESSIONS_SUBDIR;
use codex_core::ThreadManager;
use codex_core::append_thread_name;
use codex_core::config::Config;
use codex_core::features::Feature;
use codex_core::find_thread_path_by_id_str;
use codex_core::models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_core::read_session_meta_line;
use codex_core::rollout_date_parts;
use codex_core::state_db::read_repair_rollout_path;
use codex_core::util::normalize_thread_name;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::time::timeout;

const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

pub(crate) async fn repair_rollout_state(config: &Config, rollout_path: &Path) -> Result<ThreadId> {
    let session_meta = read_session_meta_line(rollout_path).await?;
    let state_db = codex_core::state_db::open_if_present(
        config.codex_home.as_path(),
        config.model_provider_id.as_str(),
    )
    .await;
    read_repair_rollout_path(
        state_db.as_deref(),
        Some(session_meta.meta.id),
        Some(is_archived_rollout(rollout_path)),
        rollout_path,
    )
    .await;
    Ok(session_meta.meta.id)
}

pub(crate) async fn reconcile_rollout_path(
    config: &Config,
    thread_id: ThreadId,
    rollout_path: &Path,
    archived: bool,
) -> Result<()> {
    let state_db = codex_core::state_db::open_if_present(
        config.codex_home.as_path(),
        config.model_provider_id.as_str(),
    )
    .await;
    read_repair_rollout_path(
        state_db.as_deref(),
        Some(thread_id),
        Some(archived),
        rollout_path,
    )
    .await;
    Ok(())
}

pub(crate) async fn archive_rollout_file(
    codex_home: &Path,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> Result<PathBuf> {
    let sessions_dir = codex_home.join(SESSIONS_SUBDIR);
    let canonical_sessions_dir =
        tokio::fs::canonicalize(&sessions_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve sessions directory {}",
                    sessions_dir.display()
                )
            })?;
    let canonical_rollout_path = tokio::fs::canonicalize(rollout_path)
        .await
        .with_context(|| format!("failed to resolve rollout path {}", rollout_path.display()))?;

    if !canonical_rollout_path.starts_with(&canonical_sessions_dir) {
        bail!(
            "rollout path `{}` must be in sessions directory",
            rollout_path.display()
        );
    }

    let file_name = validate_rollout_file_name(thread_id, rollout_path, &canonical_rollout_path)?;
    let archive_dir = codex_home.join(ARCHIVED_SESSIONS_SUBDIR);
    tokio::fs::create_dir_all(&archive_dir).await?;
    let archived_path = archive_dir.join(&file_name);
    tokio::fs::rename(&canonical_rollout_path, &archived_path)
        .await
        .with_context(|| format!("failed to archive {}", rollout_path.display()))?;
    Ok(archived_path)
}

pub(crate) async fn unarchive_rollout_file(
    codex_home: &Path,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> Result<PathBuf> {
    let archived_dir = codex_home.join(ARCHIVED_SESSIONS_SUBDIR);
    let canonical_archived_dir =
        tokio::fs::canonicalize(&archived_dir)
            .await
            .with_context(|| {
                format!(
                    "failed to resolve archived directory {}",
                    archived_dir.display()
                )
            })?;
    let canonical_rollout_path = tokio::fs::canonicalize(rollout_path)
        .await
        .with_context(|| format!("failed to resolve rollout path {}", rollout_path.display()))?;

    if !canonical_rollout_path.starts_with(&canonical_archived_dir) {
        bail!(
            "rollout path `{}` must be in archived directory",
            rollout_path.display()
        );
    }

    let file_name = validate_rollout_file_name(thread_id, rollout_path, &canonical_rollout_path)?;
    let Some((year, month, day)) = rollout_date_parts(file_name.as_os_str()) else {
        bail!(
            "rollout path `{}` missing filename timestamp",
            rollout_path.display()
        );
    };

    let restored_dir = codex_home
        .join(SESSIONS_SUBDIR)
        .join(year)
        .join(month)
        .join(day);
    tokio::fs::create_dir_all(&restored_dir).await?;
    let restored_path = restored_dir.join(&file_name);
    tokio::fs::rename(&canonical_rollout_path, &restored_path)
        .await
        .with_context(|| format!("failed to unarchive {}", rollout_path.display()))?;
    touch_rollout_path(restored_path.as_path()).await?;
    Ok(restored_path)
}

pub(crate) async fn compact_rollout_once(
    config: &Config,
    rollout_path: &Path,
    timeout_secs: u64,
) -> Result<ThreadId> {
    let session_meta = read_session_meta_line(rollout_path).await?;
    let (thread_manager, auth_manager) = build_thread_manager(config, session_meta.meta.source);
    let new_thread = thread_manager
        .resume_thread_from_rollout(
            config.clone(),
            rollout_path.to_path_buf(),
            Arc::clone(&auth_manager),
            None,
        )
        .await
        .with_context(|| format!("failed to resume {}", rollout_path.display()))?;
    let submit_id = new_thread
        .thread
        .submit(Op::Compact)
        .await
        .with_context(|| format!("failed to submit compact for {}", rollout_path.display()))?;
    let wait_result = wait_for_operation_completion(
        &new_thread.thread,
        submit_id.as_str(),
        OperationKind::Compact,
        timeout_secs,
    )
    .await;
    let shutdown_result =
        shutdown_thread(&thread_manager, new_thread.thread_id, &new_thread.thread).await;
    wait_result?;
    shutdown_result?;
    Ok(new_thread.thread_id)
}

pub(crate) async fn rollback_rollout_once(
    config: &Config,
    rollout_path: &Path,
    num_turns: u32,
    timeout_secs: u64,
) -> Result<ThreadId> {
    let session_meta = read_session_meta_line(rollout_path).await?;
    let (thread_manager, auth_manager) = build_thread_manager(config, session_meta.meta.source);
    let new_thread = thread_manager
        .resume_thread_from_rollout(
            config.clone(),
            rollout_path.to_path_buf(),
            Arc::clone(&auth_manager),
            None,
        )
        .await
        .with_context(|| format!("failed to resume {}", rollout_path.display()))?;
    let submit_id = new_thread
        .thread
        .submit(Op::ThreadRollback { num_turns })
        .await
        .with_context(|| format!("failed to submit rollback for {}", rollout_path.display()))?;
    let wait_result = wait_for_operation_completion(
        &new_thread.thread,
        submit_id.as_str(),
        OperationKind::Rollback,
        timeout_secs,
    )
    .await;
    let shutdown_result =
        shutdown_thread(&thread_manager, new_thread.thread_id, &new_thread.thread).await;
    wait_result?;
    shutdown_result?;
    Ok(new_thread.thread_id)
}

pub(crate) async fn fork_rollout_path(request: ForkRequest) -> Result<ForkOutcome> {
    let ForkRequest {
        source_profile,
        source_rollout_path,
        model,
        provider,
        context_window,
        auto_compact_token_limit,
        write_profile,
        thread_name,
        persist_extended_history,
        nth_user_message,
    } = request;

    let base_config =
        load_runtime_config(source_profile.clone(), None, None, None, None, None).await?;
    let source_summary = build_session_summary(&base_config, source_rollout_path.as_path()).await?;
    let source_meta = read_session_meta_line(source_rollout_path.as_path()).await?;
    let target_config = load_runtime_config(
        source_profile,
        Some(source_summary.session_cwd.clone()),
        model.or(source_summary.latest_model.clone()),
        provider.or(source_summary.session_provider.clone()),
        context_window.or(source_summary.latest_model_context_window),
        auto_compact_token_limit,
    )
    .await?;
    let normalized_thread_name = thread_name
        .as_deref()
        .map(|name| normalize_thread_name(name).context("thread name must not be empty"))
        .transpose()?;

    let (thread_manager, _auth_manager) =
        build_thread_manager(&target_config, source_meta.meta.source.clone());
    let new_thread = thread_manager
        .fork_thread(
            nth_user_message.unwrap_or(usize::MAX),
            target_config.clone(),
            source_rollout_path,
            persist_extended_history,
            None,
        )
        .await
        .context("failed to fork rollout")?;

    let new_rollout_path =
        resolve_new_rollout_path(&target_config, &new_thread.thread, new_thread.thread_id).await?;

    let rename_result = if let Some(thread_name) = normalized_thread_name.as_deref() {
        append_thread_name(&target_config.codex_home, new_thread.thread_id, thread_name)
            .await
            .with_context(|| {
                format!(
                    "fork created new thread {}, but writing thread name failed",
                    new_thread.thread_id
                )
            })
    } else {
        Ok(())
    };

    let profile_result = if let Some(profile) = write_profile.as_deref() {
        write_profile_from_config(profile, &target_config)
            .await
            .with_context(|| {
                format!(
                    "fork created new thread {}, but writing profile `{profile}` failed",
                    new_thread.thread_id
                )
            })
    } else {
        Ok(())
    };

    let shutdown_result =
        shutdown_thread(&thread_manager, new_thread.thread_id, &new_thread.thread).await;
    rename_result?;
    profile_result?;
    shutdown_result?;

    Ok(ForkOutcome {
        thread_id: new_thread.thread_id,
        rollout_path: new_rollout_path,
        runtime_profile: write_profile,
    })
}

fn validate_rollout_file_name(
    thread_id: ThreadId,
    rollout_path: &Path,
    canonical_rollout_path: &Path,
) -> Result<OsString> {
    let required_suffix = format!("{thread_id}.jsonl");
    let Some(file_name) = canonical_rollout_path.file_name().map(OsStr::to_owned) else {
        bail!(
            "rollout path `{}` missing file name",
            rollout_path.display()
        );
    };
    if !file_name
        .to_string_lossy()
        .ends_with(required_suffix.as_str())
    {
        bail!(
            "rollout path `{}` does not match thread id {thread_id}",
            rollout_path.display()
        );
    }
    Ok(file_name)
}

async fn touch_rollout_path(path: &Path) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let times = FileTimes::new().set_modified(SystemTime::now());
        OpenOptions::new()
            .append(true)
            .open(&path)?
            .set_times(times)?;
        Ok(())
    })
    .await
    .context("touch task panicked")?
}

pub(crate) fn build_thread_manager(
    config: &Config,
    session_source: SessionSource,
) -> (Arc<ThreadManager>, Arc<AuthManager>) {
    let auth_manager = AuthManager::shared(
        config.codex_home.clone(),
        true,
        config.cli_auth_credentials_store_mode,
    );
    auth_manager.set_forced_chatgpt_workspace_id(config.forced_chatgpt_workspace_id.clone());
    let thread_manager = Arc::new(ThreadManager::new(
        config,
        Arc::clone(&auth_manager),
        session_source,
        CollaborationModesConfig {
            default_mode_request_user_input: config
                .features
                .enabled(Feature::DefaultModeRequestUserInput),
        },
    ));
    (thread_manager, auth_manager)
}

pub(crate) async fn resolve_new_rollout_path(
    config: &Config,
    thread: &Arc<codex_core::CodexThread>,
    thread_id: ThreadId,
) -> Result<PathBuf> {
    if let Some(rollout_path) = thread.rollout_path() {
        return Ok(rollout_path);
    }
    find_thread_path_by_id_str(&config.codex_home, &thread_id.to_string())
        .await?
        .with_context(|| format!("unable to resolve rollout path for new thread {thread_id}"))
}

async fn wait_for_operation_completion(
    thread: &Arc<codex_core::CodexThread>,
    submit_id: &str,
    operation_kind: OperationKind,
    timeout_secs: u64,
) -> Result<()> {
    let wait = async {
        loop {
            let event = thread.next_event().await?;
            if event.id != submit_id {
                continue;
            }
            match (operation_kind, event.msg) {
                (OperationKind::Compact, EventMsg::ContextCompacted(_)) => return Ok(()),
                (OperationKind::Compact, EventMsg::ItemCompleted(completed))
                    if matches!(completed.item, TurnItem::ContextCompaction(_)) =>
                {
                    return Ok(());
                }
                (OperationKind::Rollback, EventMsg::ThreadRolledBack(_)) => return Ok(()),
                (_, EventMsg::Error(error)) => bail!(error.message),
                _ => {}
            }
        }
    };

    timeout(Duration::from_secs(timeout_secs), wait)
        .await
        .with_context(|| format!("timed out waiting for operation `{submit_id}`"))?
}

pub(crate) async fn shutdown_thread(
    thread_manager: &Arc<ThreadManager>,
    thread_id: ThreadId,
    thread: &Arc<codex_core::CodexThread>,
) -> Result<()> {
    let submit_id = thread
        .submit(Op::Shutdown)
        .await
        .context("failed to submit shutdown")?;
    let wait = async {
        loop {
            let event = thread.next_event().await?;
            if event.id == submit_id && matches!(event.msg, EventMsg::ShutdownComplete) {
                return Ok(());
            }
        }
    };

    let wait_result = timeout(Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS), wait)
        .await
        .context("timed out waiting for shutdown")?;
    let _ = thread_manager.remove_thread(&thread_id).await;
    wait_result
}
