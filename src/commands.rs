use crate::cli::Command;
use crate::cli::CompactArgs;
use crate::cli::ForkArgs;
use crate::cli::MigrateArgs;
use crate::cli::RenameArgs;
use crate::cli::RepairResumeStateArgs;
use crate::cli::RewriteMetaArgs;
use crate::cli::RollbackArgs;
use crate::cli::ShowArgs;
use crate::cli::TargetArgs;
use crate::distill;
use crate::operations::archive_rollout_file;
use crate::operations::compact_rollout_once;
use crate::operations::fork_rollout_path;
use crate::operations::reconcile_rollout_path;
use crate::operations::repair_rollout_state;
use crate::operations::rollback_rollout_once;
use crate::operations::unarchive_rollout_file;
use crate::rollout_edit::MetaPatch;
use crate::rollout_edit::ResumeStatePatch;
use crate::rollout_edit::rewrite_rollout_meta_contents;
use crate::rollout_edit::rewrite_rollout_resume_state_contents;
use crate::runtime::copy_or_print;
use crate::runtime::load_session_runtime_config;
use crate::runtime::render_profiled_resume_command;
use crate::runtime::resolve_resume_state_context_window;
use crate::runtime::resolve_target;
use crate::smart;
use crate::summary::build_session_summary;
use crate::summary::is_archived_rollout;
use crate::types::ForkRequest;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core::SESSIONS_SUBDIR;
use codex_core::append_thread_name;
use codex_core::path_utils::write_atomically;
use codex_core::read_session_meta_line;
use codex_core::util::normalize_thread_name;
use codex_core::util::resume_command;

pub(crate) async fn run(command: Command) -> Result<()> {
    match command {
        Command::Show(args) => show_session(args).await,
        Command::Rename(args) => rename_session(args).await,
        Command::Repair(args) => repair_session(args).await,
        Command::RewriteMeta(args) => rewrite_session_meta(args).await,
        Command::RepairResumeState(args) => repair_resume_state(args).await,
        Command::Fork(args) => fork_session(args).await,
        Command::Archive(args) => archive_session(args).await,
        Command::Unarchive(args) => unarchive_session(args).await,
        Command::CopySessionId(args) => copy_session_id(args).await,
        Command::CopyCwd(args) => copy_session_cwd(args).await,
        Command::CopyRolloutPath(args) => copy_rollout_path(args).await,
        Command::CopyDeeplink(args) => copy_resume_command_for_target(args).await,
        Command::Compact(args) => compact_session(args).await,
        Command::Rollback(args) => rollback_session(args).await,
        Command::Migrate(args) => migrate_session(args).await,
        Command::Smart(args) => smart::run(args).await,
        Command::Distill(args) => distill::run(args).await,
    }
}

async fn show_session(args: ShowArgs) -> Result<()> {
    let resolved = resolve_target(&args.target).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    println!("thread_id: {}", summary.thread_id);
    println!(
        "thread_name: {}",
        summary.thread_name.as_deref().unwrap_or("")
    );
    println!("rollout_path: {}", summary.rollout_path.display());
    println!("archived: {}", summary.archived);
    println!("source: {}", summary.source);
    println!(
        "session_provider: {}",
        summary.session_provider.as_deref().unwrap_or("")
    );
    println!("session_cwd: {}", summary.session_cwd.display());
    println!("session_timestamp: {}", summary.session_timestamp);
    println!(
        "latest_model: {}",
        summary.latest_model.as_deref().unwrap_or("")
    );
    println!(
        "latest_total_tokens: {}",
        summary
            .latest_total_tokens
            .map_or_else(String::new, |value| value.to_string())
    );
    println!(
        "latest_context_tokens: {}",
        summary
            .latest_context_tokens
            .map_or_else(String::new, |value| value.to_string())
    );
    println!(
        "latest_model_context_window: {}",
        summary
            .latest_model_context_window
            .map_or_else(String::new, |value| value.to_string())
    );
    println!("user_turns: {}", summary.user_turns);
    println!(
        "first_user_message: {}",
        summary.first_user_message.as_deref().unwrap_or("")
    );
    println!(
        "forked_from_id: {}",
        summary.forked_from_id.as_deref().unwrap_or("")
    );
    println!(
        "memory_mode: {}",
        summary.memory_mode.as_deref().unwrap_or("")
    );
    Ok(())
}

async fn rename_session(args: RenameArgs) -> Result<()> {
    let resolved = resolve_target(&args.target).await?;
    let session_meta = read_session_meta_line(resolved.rollout_path.as_path()).await?;
    let name = normalize_thread_name(&args.name).context("thread name must not be empty")?;
    append_thread_name(&resolved.config.codex_home, session_meta.meta.id, &name).await?;
    println!("renamed thread {} to {}", session_meta.meta.id, name);
    Ok(())
}

async fn repair_session(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    let thread_id = repair_rollout_state(&resolved.config, resolved.rollout_path.as_path()).await?;
    println!(
        "reconciled thread {} from {}",
        thread_id,
        resolved.rollout_path.display()
    );
    Ok(())
}

async fn rewrite_session_meta(args: RewriteMetaArgs) -> Result<()> {
    let patch = MetaPatch {
        provider: args.provider.clone(),
        cwd: args.cwd.clone(),
        memory_mode: args.memory_mode.clone(),
        clear_memory_mode: args.clear_memory_mode,
    };
    if patch.provider.is_none()
        && patch.cwd.is_none()
        && patch.memory_mode.is_none()
        && !patch.clear_memory_mode
    {
        bail!("no metadata changes requested");
    }

    let resolved = resolve_target(&args.target).await?;
    let existing_contents = std::fs::read_to_string(&resolved.rollout_path)
        .with_context(|| format!("failed to read {}", resolved.rollout_path.display()))?;
    let updated_contents = rewrite_rollout_meta_contents(existing_contents.as_str(), &patch)?;
    write_atomically(resolved.rollout_path.as_path(), updated_contents.as_str())
        .with_context(|| format!("failed to rewrite {}", resolved.rollout_path.display()))?;
    let thread_id = repair_rollout_state(&resolved.config, resolved.rollout_path.as_path()).await?;
    println!(
        "updated session metadata for thread {} at {}",
        thread_id,
        resolved.rollout_path.display()
    );
    Ok(())
}

async fn repair_resume_state(args: RepairResumeStateArgs) -> Result<()> {
    let resolved = resolve_target(&args.target).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let context_window = resolve_resume_state_context_window(&args, &summary).await?;
    let existing_contents = std::fs::read_to_string(&resolved.rollout_path)
        .with_context(|| format!("failed to read {}", resolved.rollout_path.display()))?;
    let (updated_contents, stats) = rewrite_rollout_resume_state_contents(
        existing_contents.as_str(),
        &ResumeStatePatch {
            model_context_window: context_window,
        },
    )?;
    write_atomically(resolved.rollout_path.as_path(), updated_contents.as_str())
        .with_context(|| format!("failed to rewrite {}", resolved.rollout_path.display()))?;
    let thread_id = repair_rollout_state(&resolved.config, resolved.rollout_path.as_path()).await?;
    println!("thread_id: {thread_id}");
    println!("rollout_path: {}", resolved.rollout_path.display());
    println!("context_window: {context_window}");
    println!(
        "token_count_events_updated: {}",
        stats.token_count_events_updated
    );
    println!(
        "turn_started_events_updated: {}",
        stats.turn_started_events_updated
    );
    Ok(())
}

async fn fork_session(args: ForkArgs) -> Result<()> {
    let resolved = resolve_target(&args.target).await?;
    let outcome = fork_rollout_path(ForkRequest {
        source_profile: args.target.config_profile,
        source_rollout_path: resolved.rollout_path,
        model: args.model,
        provider: args.provider,
        context_window: args.context_window,
        auto_compact_token_limit: args.auto_compact_token_limit,
        write_profile: args.write_profile,
        thread_name: args.thread_name,
        persist_extended_history: args.persist_extended_history,
        nth_user_message: args.nth_user_message,
    })
    .await?;

    println!("new_thread_id: {}", outcome.thread_id);
    println!("new_rollout_path: {}", outcome.rollout_path.display());
    println!(
        "resume_command: {}",
        render_profiled_resume_command(outcome.runtime_profile.as_deref(), outcome.thread_id)
    );
    Ok(())
}

async fn archive_session(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    if is_archived_rollout(resolved.rollout_path.as_path()) {
        bail!(
            "rollout is already archived: {}",
            resolved.rollout_path.display()
        );
    }

    let session_meta = read_session_meta_line(resolved.rollout_path.as_path()).await?;
    let archived_path = archive_rollout_file(
        resolved.config.codex_home.as_path(),
        session_meta.meta.id,
        &resolved.rollout_path,
    )
    .await?;
    reconcile_rollout_path(
        &resolved.config,
        session_meta.meta.id,
        archived_path.as_path(),
        true,
    )
    .await?;

    println!("archived_thread_id: {}", session_meta.meta.id);
    println!("archived_rollout_path: {}", archived_path.display());
    Ok(())
}

async fn unarchive_session(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    if !is_archived_rollout(resolved.rollout_path.as_path()) {
        bail!(
            "rollout is not archived: {}",
            resolved.rollout_path.display()
        );
    }

    let session_meta = read_session_meta_line(resolved.rollout_path.as_path()).await?;
    let restored_path = unarchive_rollout_file(
        resolved.config.codex_home.as_path(),
        session_meta.meta.id,
        &resolved.rollout_path,
    )
    .await?;
    reconcile_rollout_path(
        &resolved.config,
        session_meta.meta.id,
        restored_path.as_path(),
        false,
    )
    .await?;

    println!("unarchived_thread_id: {}", session_meta.meta.id);
    println!("restored_rollout_path: {}", restored_path.display());
    Ok(())
}

async fn copy_session_id(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    let session_meta = read_session_meta_line(resolved.rollout_path.as_path()).await?;
    let thread_id = session_meta.meta.id.to_string();
    copy_or_print("thread_id", thread_id.as_str());
    Ok(())
}

async fn copy_session_cwd(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    let session_meta = read_session_meta_line(resolved.rollout_path.as_path()).await?;
    let cwd = session_meta.meta.cwd.display().to_string();
    copy_or_print("cwd", cwd.as_str());
    Ok(())
}

async fn copy_rollout_path(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    let path = resolved.rollout_path.display().to_string();
    copy_or_print("rollout_path", path.as_str());
    Ok(())
}

async fn copy_resume_command_for_target(args: TargetArgs) -> Result<()> {
    let resolved = resolve_target(&args).await?;
    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let session_meta = read_session_meta_line(resolved.rollout_path.as_path()).await?;
    let command = resume_command(summary.thread_name.as_deref(), Some(session_meta.meta.id))
        .context("unable to build resume command")?;
    copy_or_print("resume_command", command.as_str());
    Ok(())
}

async fn compact_session(args: CompactArgs) -> Result<()> {
    let resolved = resolve_target(&args.target).await?;
    if is_archived_rollout(resolved.rollout_path.as_path()) {
        bail!("manual compact only supports active rollouts under {SESSIONS_SUBDIR}");
    }

    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let operation_config =
        load_session_runtime_config(args.target.config_profile.clone(), &summary).await?;
    let compacted_thread_id = compact_rollout_once(
        &operation_config,
        resolved.rollout_path.as_path(),
        args.timeout_secs,
    )
    .await?;
    let repaired_thread_id =
        repair_rollout_state(&resolved.config, resolved.rollout_path.as_path()).await?;
    let updated_summary =
        build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;

    println!("compacted_thread_id: {}", compacted_thread_id);
    println!("repaired_thread_id: {}", repaired_thread_id);
    println!(
        "latest_context_tokens: {}",
        updated_summary
            .latest_context_tokens
            .map_or_else(String::new, |value| value.to_string())
    );
    println!(
        "latest_model_context_window: {}",
        updated_summary
            .latest_model_context_window
            .map_or_else(String::new, |value| value.to_string())
    );
    Ok(())
}

async fn rollback_session(args: RollbackArgs) -> Result<()> {
    if args.num_turns == 0 {
        bail!("num_turns must be >= 1");
    }

    let resolved = resolve_target(&args.target).await?;
    if is_archived_rollout(resolved.rollout_path.as_path()) {
        bail!("rollback only supports active rollouts under {SESSIONS_SUBDIR}");
    }

    let summary = build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;
    let operation_config =
        load_session_runtime_config(args.target.config_profile.clone(), &summary).await?;
    let rolled_back_thread_id = rollback_rollout_once(
        &operation_config,
        resolved.rollout_path.as_path(),
        args.num_turns,
        args.timeout_secs,
    )
    .await?;
    let repaired_thread_id =
        repair_rollout_state(&resolved.config, resolved.rollout_path.as_path()).await?;
    let updated_summary =
        build_session_summary(&resolved.config, resolved.rollout_path.as_path()).await?;

    println!("rolled_back_thread_id: {}", rolled_back_thread_id);
    println!("repaired_thread_id: {}", repaired_thread_id);
    println!("remaining_user_turns: {}", updated_summary.user_turns);
    Ok(())
}

async fn migrate_session(args: MigrateArgs) -> Result<()> {
    if args.max_pre_compactions == 0 {
        bail!("max_pre_compactions must be >= 1");
    }

    let resolved = resolve_target(&args.target).await?;
    let source_profile = args.target.config_profile.clone();
    let source_config = resolved.config;
    let source_rollout_path = resolved.rollout_path;
    let source_thread_id = read_session_meta_line(source_rollout_path.as_path())
        .await?
        .meta
        .id;
    let archived_source = is_archived_rollout(source_rollout_path.as_path());
    let mut summary = build_session_summary(&source_config, source_rollout_path.as_path()).await?;
    let mut compactions_run = 0_u32;

    if args.force_compact {
        if archived_source {
            bail!("source rollout is archived; unarchive it before forcing compaction");
        }
        let operation_config =
            load_session_runtime_config(source_profile.clone(), &summary).await?;
        compact_rollout_once(
            &operation_config,
            source_rollout_path.as_path(),
            args.timeout_secs,
        )
        .await?;
        repair_rollout_state(&source_config, source_rollout_path.as_path()).await?;
        compactions_run += 1;
        summary = build_session_summary(&source_config, source_rollout_path.as_path()).await?;
    }

    if let Some(target_window) = args.context_window {
        if summary.latest_context_tokens.is_none() {
            eprintln!(
                "warning: rollout has no persisted context token estimate; migration cannot preflight the target window"
            );
        }

        while let Some(context_tokens) = summary.latest_context_tokens {
            if context_tokens <= target_window {
                break;
            }
            if archived_source {
                bail!(
                    "source rollout is archived and current context {context_tokens} exceeds target window {target_window}; unarchive first so Codex can compact it"
                );
            }
            if compactions_run >= args.max_pre_compactions {
                bail!(
                    "source rollout still exceeds target window after {} compactions (current_context_tokens={context_tokens}, target_window={target_window})",
                    args.max_pre_compactions
                );
            }

            let operation_config =
                load_session_runtime_config(source_profile.clone(), &summary).await?;
            compact_rollout_once(
                &operation_config,
                source_rollout_path.as_path(),
                args.timeout_secs,
            )
            .await?;
            repair_rollout_state(&source_config, source_rollout_path.as_path()).await?;
            compactions_run += 1;
            summary = build_session_summary(&source_config, source_rollout_path.as_path()).await?;
        }

        if let Some(context_tokens) = summary.latest_context_tokens
            && context_tokens > target_window
        {
            bail!(
                "source rollout still exceeds target window after compaction (current_context_tokens={context_tokens}, target_window={target_window})"
            );
        }
    }

    let fork_outcome = fork_rollout_path(ForkRequest {
        source_profile,
        source_rollout_path: source_rollout_path.clone(),
        model: args.model,
        provider: args.provider,
        context_window: args.context_window,
        auto_compact_token_limit: args.auto_compact_token_limit,
        write_profile: args.write_profile,
        thread_name: args.thread_name,
        persist_extended_history: args.persist_extended_history,
        nth_user_message: args.nth_user_message,
    })
    .await?;

    let mut archived_source_path = None;
    if args.archive_source {
        if archived_source {
            archived_source_path = Some(source_rollout_path.clone());
        } else {
            let archived_path = archive_rollout_file(
                source_config.codex_home.as_path(),
                source_thread_id,
                source_rollout_path.as_path(),
            )
            .await
            .with_context(|| {
                format!(
                    "fork succeeded with new thread {}, but archiving source thread {} failed",
                    fork_outcome.thread_id, source_thread_id
                )
            })?;
            reconcile_rollout_path(
                &source_config,
                source_thread_id,
                archived_path.as_path(),
                true,
            )
            .await?;
            archived_source_path = Some(archived_path);
        }
    }

    println!("source_thread_id: {}", source_thread_id);
    println!("new_thread_id: {}", fork_outcome.thread_id);
    println!("new_rollout_path: {}", fork_outcome.rollout_path.display());
    println!("pre_compactions_run: {}", compactions_run);
    println!(
        "resume_command: {}",
        render_profiled_resume_command(
            fork_outcome.runtime_profile.as_deref(),
            fork_outcome.thread_id
        )
    );
    if let Some(archived_source_path) = archived_source_path {
        println!("archived_source_path: {}", archived_source_path.display());
    }
    Ok(())
}
