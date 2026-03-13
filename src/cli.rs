use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use std::path::PathBuf;

pub(crate) const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 300;
pub(crate) const DEFAULT_ROLLBACK_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum DistillMode {
    Codex,
    Deterministic,
}

#[derive(Parser, Debug)]
#[command(name = "codex-session-manager")]
#[command(about = "Low-level Codex session and rollout manager")]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Command {
    /// Inspect derived metadata for a session rollout.
    Show(ShowArgs),
    /// Rename a thread by appending a new session index entry.
    Rename(RenameArgs),
    /// Reconcile SQLite metadata from a rollout file.
    Repair(TargetArgs),
    /// Rewrite only the first SessionMeta record in a rollout.
    RewriteMeta(RewriteMetaArgs),
    /// Rewrite persisted resume/fork window hints in rollout events.
    #[command(name = "repair-resume-state", alias = "repair-window")]
    RepairResumeState(RepairResumeStateArgs),
    /// Fork a session using Codex's native thread manager.
    Fork(ForkArgs),
    /// Move an active rollout into archived storage.
    Archive(TargetArgs),
    #[command(alias = "restore")]
    /// Restore an archived rollout back into active storage.
    Unarchive(TargetArgs),
    #[command(name = "copy-session-id", alias = "copy-thread-id", alias = "copy-id")]
    /// Copy the resolved thread id.
    CopySessionId(TargetArgs),
    #[command(name = "copy-cwd")]
    /// Copy the session cwd recorded in SessionMeta.
    CopyCwd(TargetArgs),
    #[command(name = "copy-rollout-path", alias = "copy-path")]
    /// Copy the resolved rollout path.
    CopyRolloutPath(TargetArgs),
    #[command(name = "copy-deeplink", alias = "copy-resume-command")]
    /// Copy the canonical `codex resume ...` command.
    CopyDeeplink(TargetArgs),
    /// Trigger native Codex compaction for an active session.
    Compact(CompactArgs),
    /// Drop the last N user turns using native Codex rollback.
    Rollback(RollbackArgs),
    /// Compact if needed, then fork into a new provider/model/runtime shape.
    Migrate(MigrateArgs),
    /// Interactive smart provider/model switch workflow.
    Smart(SmartArgs),
    /// Distill a heavy session into a lighter successor session.
    Distill(DistillArgs),
}

#[derive(Args, Debug, Clone)]
pub(crate) struct TargetArgs {
    /// Rollout path, thread id, or thread name.
    #[arg(value_name = "TARGET")]
    pub(crate) target: String,

    /// Config profile to use while resolving provider/model defaults.
    #[arg(long = "profile", short = 'p')]
    pub(crate) config_profile: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ShowArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Emit structured JSON instead of key/value lines.
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct RenameArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// New thread name to append into `session_index.jsonl`.
    #[arg(value_name = "NAME")]
    pub(crate) name: String,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct RewriteMetaArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Override `SessionMeta.model_provider`.
    #[arg(long)]
    pub(crate) provider: Option<String>,

    /// Override `SessionMeta.cwd`.
    #[arg(long)]
    pub(crate) cwd: Option<PathBuf>,

    /// Override `SessionMeta.memory_mode`.
    #[arg(long, conflicts_with = "clear_memory_mode")]
    pub(crate) memory_mode: Option<String>,

    /// Clear `SessionMeta.memory_mode`.
    #[arg(long, default_value_t = false)]
    pub(crate) clear_memory_mode: bool,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct RepairResumeStateArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Override the persisted model context window recorded in rollout events.
    #[arg(long)]
    pub(crate) context_window: Option<i64>,

    /// Runtime model used when resolving a context window from config.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Runtime provider used when resolving a context window from config.
    #[arg(long)]
    pub(crate) provider: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct ForkArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Override target model for the forked thread.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Override target provider for the forked thread.
    #[arg(long)]
    pub(crate) provider: Option<String>,

    /// Override target model context window.
    #[arg(long)]
    pub(crate) context_window: Option<i64>,

    /// Override target auto-compact token threshold.
    #[arg(long)]
    pub(crate) auto_compact_token_limit: Option<i64>,

    /// Persist the resolved target runtime into a config profile.
    #[arg(long)]
    pub(crate) write_profile: Option<String>,

    /// Optional thread name for the newly forked thread.
    #[arg(long)]
    pub(crate) thread_name: Option<String>,

    /// Preserve extended history when forking.
    #[arg(long, default_value_t = false)]
    pub(crate) persist_extended_history: bool,

    /// Keep only history up to the Nth user message.
    #[arg(long)]
    pub(crate) nth_user_message: Option<usize>,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct CompactArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Maximum seconds to wait for compaction to complete.
    #[arg(long, default_value_t = DEFAULT_OPERATION_TIMEOUT_SECS)]
    pub(crate) timeout_secs: u64,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct RollbackArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Number of user turns to drop from the end of the thread.
    #[arg(value_name = "NUM_TURNS")]
    pub(crate) num_turns: u32,

    /// Maximum seconds to wait for rollback to complete.
    #[arg(long, default_value_t = DEFAULT_ROLLBACK_TIMEOUT_SECS)]
    pub(crate) timeout_secs: u64,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct MigrateArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Override target model for the forked thread.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Override target provider for the forked thread.
    #[arg(long)]
    pub(crate) provider: Option<String>,

    /// Target context window used for migration preflight.
    #[arg(long)]
    pub(crate) context_window: Option<i64>,

    /// Override target auto-compact token threshold.
    #[arg(long)]
    pub(crate) auto_compact_token_limit: Option<i64>,

    /// Persist the resolved target runtime into a config profile.
    #[arg(long)]
    pub(crate) write_profile: Option<String>,

    /// Optional thread name for the forked thread.
    #[arg(long)]
    pub(crate) thread_name: Option<String>,

    /// Preserve extended history when forking.
    #[arg(long, default_value_t = false)]
    pub(crate) persist_extended_history: bool,

    /// Keep only history up to the Nth user message.
    #[arg(long)]
    pub(crate) nth_user_message: Option<usize>,

    /// Force one compaction before migration preflight.
    #[arg(long, default_value_t = false)]
    pub(crate) force_compact: bool,

    /// Maximum number of pre-migration compactions to run.
    #[arg(long, default_value_t = 3)]
    pub(crate) max_pre_compactions: u32,

    /// Archive the source rollout after a successful fork.
    #[arg(long, default_value_t = false)]
    pub(crate) archive_source: bool,

    /// Maximum seconds to wait for each compaction step.
    #[arg(long, default_value_t = DEFAULT_OPERATION_TIMEOUT_SECS)]
    pub(crate) timeout_secs: u64,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct SmartArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Persist the resolved target runtime into this config profile.
    #[arg(long)]
    pub(crate) write_profile: Option<String>,

    /// Archive the source rollout after a successful cross-provider migration.
    #[arg(long, default_value_t = false)]
    pub(crate) archive_source: bool,

    /// Maximum number of pre-switch compactions when shrinking context.
    #[arg(long, default_value_t = 3)]
    pub(crate) max_pre_compactions: u32,

    /// Maximum seconds to wait for each compaction step.
    #[arg(long, default_value_t = DEFAULT_OPERATION_TIMEOUT_SECS)]
    pub(crate) timeout_secs: u64,
}

#[derive(Args, Debug, Clone)]
pub(crate) struct DistillArgs {
    #[command(flatten)]
    pub(crate) target: TargetArgs,

    /// Override target provider for the distilled successor session.
    #[arg(long)]
    pub(crate) provider: Option<String>,

    /// Override target model for the distilled successor session.
    #[arg(long)]
    pub(crate) model: Option<String>,

    /// Override target model context window for the distilled successor session.
    #[arg(long)]
    pub(crate) context_window: Option<i64>,

    /// Override target auto-compact token threshold for the distilled successor session.
    #[arg(long)]
    pub(crate) auto_compact_token_limit: Option<i64>,

    /// Distillation backend to use for generating the successor handoff brief.
    #[arg(long, value_enum, default_value_t = DistillMode::Codex)]
    pub(crate) distill_mode: DistillMode,

    /// Optional reasoning effort for Codex-backed distillation.
    #[arg(long)]
    pub(crate) reasoning_effort: Option<String>,

    /// Optional thread name for the distilled successor session.
    #[arg(long)]
    pub(crate) thread_name: Option<String>,

    /// Persist the successor runtime into this config profile.
    #[arg(long)]
    pub(crate) write_profile: Option<String>,

    /// Archive the source rollout after a successful distillation.
    #[arg(long, default_value_t = false)]
    pub(crate) archive_source: bool,

    /// Show the distillation report without creating a successor thread.
    #[arg(long, default_value_t = false)]
    pub(crate) preview_only: bool,

    /// Emit structured JSON instead of plain text output.
    #[arg(long, default_value_t = false)]
    pub(crate) json: bool,

    /// Number of recent user turns to preserve in the deterministic brief.
    #[arg(long, default_value_t = 8)]
    pub(crate) recent_turns: usize,

    /// Maximum seconds to wait for the seed handoff turn to complete.
    #[arg(long, default_value_t = DEFAULT_OPERATION_TIMEOUT_SECS)]
    pub(crate) timeout_secs: u64,
}
