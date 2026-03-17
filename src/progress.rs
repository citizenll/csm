use crate::cli::DistillCompressionLevel;
use crate::cli::DistillMode;

pub(crate) type ProgressSender = std::sync::mpsc::Sender<OperationProgressEvent>;

#[derive(Debug, Clone)]
pub(crate) enum OperationProgressEvent {
    Distill(DistillProgressEvent),
    Smart(SmartProgressEvent),
}

#[derive(Debug, Clone)]
pub(crate) enum DistillProgressEvent {
    ResolvingTarget,
    LoadingSessionSummary,
    RebuildingHistory,
    ReadingRolloutLines,
    AnalyzingHistory {
        history_items: usize,
        raw_lines: usize,
    },
    BuildingDeterministicBrief,
    DeterministicBriefReady {
        user_messages: usize,
        assistant_messages: usize,
        durable_guidance: usize,
        estimated_tokens: usize,
    },
    ResolvingRuntimeConfig,
    StartingCodexDistillation,
    CodexEphemeralThreadStarted,
    CodexTurnSubmitted,
    CodexDistillationCompleted {
        estimated_tokens: usize,
    },
    CodexDistillationFallback {
        error: String,
        estimated_tokens: usize,
    },
    BuildingReport,
    PreviewReady,
    WritingProfile {
        profile: String,
    },
    StartingSuccessorThread,
    SuccessorThreadNamed {
        thread_name: String,
    },
    SeedingSuccessorThread,
    SeedTurnCompleted,
    ArchivingSource,
    Completed {
        preview_only: bool,
        successor_thread_id: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum SmartProgressEvent {
    StrategyConfirmed {
        strategy: SmartStrategyKind,
        provider: String,
        model: String,
        target_context_window: Option<i64>,
    },
    StartingDistill {
        mode: DistillMode,
        compression_level: DistillCompressionLevel,
    },
    CheckingContextWindow {
        current_tokens: i64,
        target_window: i64,
    },
    RunningCompaction {
        attempt: u32,
        max_attempts: u32,
    },
    ReloadingSummaryAfterCompaction {
        attempt: u32,
    },
    WritingProfile {
        profile: String,
    },
    RepairingResumeState {
        provider: String,
        model: String,
        context_window: Option<i64>,
    },
    SnapshottingThreadList,
    RunningMigration,
    RefreshingThreadList,
    Completed {
        thread_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SmartStrategyKind {
    SameThreadProfileOnly,
    SameThreadRepair,
    SameThreadRepairAfterCompaction,
    CrossProviderMigrate,
    CrossProviderMigrateAfterCompaction,
    DistillSuccessor,
}

pub(crate) fn emit_progress(progress: Option<&ProgressSender>, event: OperationProgressEvent) {
    if let Some(progress) = progress {
        let _ = progress.send(event);
    }
}
