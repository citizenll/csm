use crate::cli::RepairResumeStateArgs;
use crate::cli::TargetArgs;
use crate::types::ResolvedTarget;
use crate::types::SessionSummary;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use arboard::Clipboard;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::find_codex_home;
use codex_core::find_archived_thread_path_by_id_str;
use codex_core::find_thread_path_by_id_str;
use codex_core::find_thread_path_by_name_str;
use codex_core::util::resume_command;
use codex_protocol::ThreadId;
use std::path::Path;
use std::path::PathBuf;
use toml::Value as TomlValue;
use toml_edit::value;

pub(crate) async fn resolve_target(args: &TargetArgs) -> Result<ResolvedTarget> {
    let config = Box::pin(load_runtime_config(
        args.config_profile.clone(),
        None,
        None,
        None,
        None,
        None,
    ))
    .await?;
    let rollout_path = Box::pin(resolve_rollout_path(&config, args.target.as_str())).await?;
    Ok(ResolvedTarget {
        config,
        rollout_path,
    })
}

pub(crate) async fn load_runtime_config(
    config_profile: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
    provider: Option<String>,
    context_window: Option<i64>,
    auto_compact_token_limit: Option<i64>,
) -> Result<Config> {
    let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
    let mut cli_overrides = Vec::new();
    if let Some(context_window) = context_window {
        cli_overrides.push((
            "model_context_window".to_string(),
            TomlValue::Integer(context_window),
        ));
    }
    if let Some(auto_compact_token_limit) = auto_compact_token_limit {
        cli_overrides.push((
            "model_auto_compact_token_limit".to_string(),
            TomlValue::Integer(auto_compact_token_limit),
        ));
    }

    let harness_overrides = ConfigOverrides {
        model,
        cwd,
        model_provider: provider,
        config_profile,
        ..Default::default()
    };

    let config = Box::pin(
        ConfigBuilder::default()
            .codex_home(codex_home)
            .cli_overrides(cli_overrides)
            .harness_overrides(harness_overrides)
            .build(),
    )
    .await
    .context("failed to load Codex config")?;
    Ok(config)
}

pub(crate) async fn load_session_runtime_config(
    config_profile: Option<String>,
    summary: &SessionSummary,
) -> Result<Config> {
    Box::pin(load_runtime_config(
        config_profile,
        Some(summary.session_cwd.clone()),
        summary.latest_model.clone(),
        summary.session_provider.clone(),
        summary.latest_model_context_window,
        None,
    ))
    .await
}

pub(crate) async fn resolve_resume_state_context_window(
    args: &RepairResumeStateArgs,
    summary: &SessionSummary,
) -> Result<i64> {
    if let Some(context_window) = args.context_window {
        if context_window <= 0 {
            bail!("context window must be positive");
        }
        return Ok(context_window);
    }

    let config = Box::pin(load_runtime_config(
        args.target.config_profile.clone(),
        Some(summary.session_cwd.clone()),
        args.model.clone().or(summary.latest_model.clone()),
        args.provider.clone().or(summary.session_provider.clone()),
        None,
        None,
    ))
    .await?;

    config.model_context_window.context(
        "could not resolve context window from the selected runtime; pass --context-window explicitly",
    )
}

pub(crate) async fn write_profile_from_config(profile: &str, config: &Config) -> Result<()> {
    let mut edits = Vec::new();
    edits.push(ConfigEdit::SetPath {
        segments: scoped_config_segments(Some(profile), "model_provider"),
        value: value(config.model_provider_id.clone()),
    });
    push_optional_i64_edit(
        &mut edits,
        Some(profile),
        "model_context_window",
        config.model_context_window,
    );
    push_optional_i64_edit(
        &mut edits,
        Some(profile),
        "model_auto_compact_token_limit",
        config.model_auto_compact_token_limit,
    );

    ConfigEditsBuilder::new(&config.codex_home)
        .with_profile(Some(profile))
        .set_model(config.model.as_deref(), None)
        .with_edits(edits)
        .apply()
        .await
        .with_context(|| format!("failed to write profile `{profile}`"))
}

pub(crate) async fn resolve_rollout_path(config: &Config, target: &str) -> Result<PathBuf> {
    let target_path = PathBuf::from(target);
    if target_path.exists() {
        return std::fs::canonicalize(&target_path)
            .with_context(|| format!("failed to canonicalize {}", target_path.display()));
    }

    if let Some(path) = find_thread_path_by_id_str(&config.codex_home, target).await? {
        return Ok(path);
    }
    if let Some(path) = find_archived_thread_path_by_id_str(&config.codex_home, target).await? {
        return Ok(path);
    }
    if let Some(path) = find_thread_path_by_name_str(&config.codex_home, target).await? {
        return Ok(path);
    }
    if let Some(thread_id) = find_thread_id_by_name_in_session_index(&config.codex_home, target)?
        && let Some(path) =
            find_archived_thread_path_by_id_str(&config.codex_home, &thread_id.to_string()).await?
    {
        return Ok(path);
    }

    bail!("unable to resolve target `{target}` as rollout path, thread id, or thread name");
}

pub(crate) fn copy_or_print(_label: &str, value: &str) {
    if let Ok(mut clipboard) = Clipboard::new() {
        let _ = clipboard.set_text(value.to_string());
    }
    println!("{value}");
}

pub(crate) fn render_profiled_resume_command(
    runtime_profile: Option<&str>,
    thread_id: ThreadId,
) -> String {
    let resume = resume_command(None, Some(thread_id)).unwrap_or_else(|| {
        let target = thread_id.to_string();
        format!("codex resume {}", shell_quote(target.as_str()))
    });
    match runtime_profile {
        Some(profile) => {
            let suffix = resume.strip_prefix("codex ").unwrap_or(resume.as_str());
            format!("codex --profile {} {suffix}", shell_quote(profile))
        }
        None => resume,
    }
}

pub(crate) fn find_thread_id_by_name_in_session_index(
    codex_home: &Path,
    name: &str,
) -> Result<Option<ThreadId>> {
    let session_index_path = codex_home.join("session_index.jsonl");
    if !session_index_path.exists() || name.trim().is_empty() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&session_index_path)
        .with_context(|| format!("failed to read {}", session_index_path.display()))?;
    for line in contents.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(thread_name) = value.get("thread_name").and_then(|value| value.as_str()) else {
            continue;
        };
        if thread_name != name {
            continue;
        }
        if let Some(id_str) = value.get("id").and_then(|value| value.as_str()) {
            return ThreadId::from_string(id_str).map(Some).map_err(Into::into);
        }
    }
    Ok(None)
}

fn scoped_config_segments(profile: Option<&str>, key: &str) -> Vec<String> {
    match profile {
        Some(profile) => vec!["profiles".to_string(), profile.to_string(), key.to_string()],
        None => vec![key.to_string()],
    }
}

fn push_optional_i64_edit(
    edits: &mut Vec<ConfigEdit>,
    profile: Option<&str>,
    key: &str,
    value_or_none: Option<i64>,
) {
    let segments = scoped_config_segments(profile, key);
    if let Some(value_or_none) = value_or_none {
        edits.push(ConfigEdit::SetPath {
            segments,
            value: value(value_or_none),
        });
    } else {
        edits.push(ConfigEdit::ClearPath { segments });
    }
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(is_shell_safe_byte) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn is_shell_safe_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'_'
            | b'-'
            | b'.'
            | b'/'
            | b':'
    )
}
