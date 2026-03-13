use crate::operations::archive_rollout_file;
use crate::operations::unarchive_rollout_file;
use crate::rollout_edit::MetaPatch;
use crate::rollout_edit::ResumeStatePatch;
use crate::rollout_edit::rewrite_rollout_meta_contents;
use crate::rollout_edit::rewrite_rollout_resume_state_contents;
use crate::runtime::find_thread_id_by_name_in_session_index;
use crate::runtime::render_profiled_resume_command;
use crate::runtime::shell_quote;
use crate::summary::build_session_summary;
use clap::CommandFactory;
use clap::Parser;
use codex_core::SESSIONS_SUBDIR;
use codex_core::append_thread_name;
use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsageInfo;
use std::path::Path;
use std::path::PathBuf;
use tempfile::tempdir;

fn session_meta_line(thread_id: ThreadId, cwd: &Path) -> codex_protocol::protocol::SessionMetaLine {
    codex_protocol::protocol::SessionMetaLine {
        meta: codex_protocol::protocol::SessionMeta {
            id: thread_id,
            forked_from_id: None,
            timestamp: "2026-03-11T00:00:00Z".to_string(),
            cwd: cwd.to_path_buf(),
            originator: "test".to_string(),
            cli_version: "0.0.0".to_string(),
            source: SessionSource::Exec,
            agent_nickname: None,
            agent_role: None,
            model_provider: Some("openai".to_string()),
            base_instructions: None,
            dynamic_tools: None,
            memory_mode: Some("enabled".to_string()),
        },
        git: None,
    }
}

fn rollout_line(timestamp: &str, item: RolloutItem) -> String {
    serde_json::to_string(&RolloutLine {
        timestamp: timestamp.to_string(),
        item,
    })
    .expect("serialize rollout line")
}

fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        end_turn: None,
        phase: None,
    }
}

async fn write_rollout(path: &Path, lines: &[String]) {
    let mut contents = lines.join("\n");
    contents.push('\n');
    tokio::fs::write(path, contents)
        .await
        .expect("write rollout");
}

fn real_fixture_rollout_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("rollout-2026-02-11T16-24-46-019c4bcd-857a-7e50-9229-b30be321c56b.jsonl")
}

#[test]
fn cli_command_graph_debug_asserts() {
    crate::Cli::command().debug_assert();
}

#[test]
fn cli_allows_no_subcommand_for_tui() {
    let cli = crate::Cli::try_parse_from(["codex-session-manager"]).expect("parse cli");
    assert!(cli.command.is_none());
}

#[test]
fn cli_parses_repair_window_alias() {
    let cli = crate::Cli::try_parse_from([
        "codex-session-manager",
        "repair-window",
        "thread-123",
        "--context-window",
        "258400",
    ])
    .expect("parse cli");

    let Some(crate::cli::Command::RepairResumeState(args)) = cli.command else {
        panic!("expected repair-resume-state command");
    };

    assert_eq!(args.target.target, "thread-123");
    assert_eq!(args.context_window, Some(258_400));
}

#[test]
fn cli_parses_smart_command() {
    let cli = crate::Cli::try_parse_from([
        "codex-session-manager",
        "smart",
        "thread-123",
        "--archive-source",
    ])
    .expect("parse cli");

    let Some(crate::cli::Command::Smart(args)) = cli.command else {
        panic!("expected smart command");
    };

    assert_eq!(args.target.target, "thread-123");
    assert!(args.archive_source);
}

#[test]
fn cli_parses_distill_command() {
    let cli = crate::Cli::try_parse_from([
        "codex-session-manager",
        "distill",
        "thread-123",
        "--preview-only",
        "--recent-turns",
        "6",
    ])
    .expect("parse cli");

    let Some(crate::cli::Command::Distill(args)) = cli.command else {
        panic!("expected distill command");
    };

    assert_eq!(args.target.target, "thread-123");
    assert!(args.preview_only);
    assert_eq!(args.recent_turns, 6);
}

#[test]
fn rewrite_rollout_meta_contents_updates_first_session_meta_line() {
    let thread_id =
        ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").expect("thread id");
    let session_meta = session_meta_line(thread_id, Path::new("D:/Dev/self/codex"));
    let original = format!(
        "{}\n{}\n",
        rollout_line(
            "2026-03-11T00:00:00Z",
            RolloutItem::SessionMeta(session_meta.clone())
        ),
        rollout_line(
            "2026-03-11T00:00:01Z",
            RolloutItem::ResponseItem(user_message("hello"))
        )
    );

    let updated = rewrite_rollout_meta_contents(
        &original,
        &MetaPatch {
            provider: Some("openrouter".to_string()),
            cwd: Some(PathBuf::from("D:/Work/project")),
            memory_mode: Some("polluted".to_string()),
            clear_memory_mode: false,
        },
    )
    .expect("rewrite");

    let first_line = updated.lines().next().expect("first line");
    let parsed: RolloutLine = serde_json::from_str(first_line).expect("parse rewritten line");
    let RolloutItem::SessionMeta(updated_meta) = parsed.item else {
        panic!("expected session meta");
    };
    assert_eq!(
        updated_meta.meta.model_provider.as_deref(),
        Some("openrouter")
    );
    assert_eq!(updated_meta.meta.cwd, PathBuf::from("D:/Work/project"));
    assert_eq!(updated_meta.meta.memory_mode.as_deref(), Some("polluted"));
}

#[test]
fn rewrite_rollout_resume_state_contents_updates_window_hints() {
    let thread_id =
        ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").expect("thread id");
    let original = format!(
        "{}\n{}\n{}\n",
        rollout_line(
            "2026-03-11T00:00:00Z",
            RolloutItem::SessionMeta(session_meta_line(thread_id, Path::new("D:/Dev/self/codex")))
        ),
        rollout_line(
            "2026-03-11T00:00:01Z",
            RolloutItem::EventMsg(EventMsg::TurnStarted(
                codex_protocol::protocol::TurnStartedEvent {
                    turn_id: "turn-1".to_string(),
                    model_context_window: Some(950_000),
                    collaboration_mode_kind: Default::default(),
                },
            ))
        ),
        rollout_line(
            "2026-03-11T00:00:02Z",
            RolloutItem::EventMsg(EventMsg::TokenCount(
                codex_protocol::protocol::TokenCountEvent {
                    info: Some(TokenUsageInfo {
                        total_token_usage: codex_protocol::protocol::TokenUsage {
                            input_tokens: 10,
                            cached_input_tokens: 0,
                            output_tokens: 2,
                            reasoning_output_tokens: 0,
                            total_tokens: 12,
                        },
                        last_token_usage: codex_protocol::protocol::TokenUsage {
                            input_tokens: 10,
                            cached_input_tokens: 0,
                            output_tokens: 2,
                            reasoning_output_tokens: 0,
                            total_tokens: 12,
                        },
                        model_context_window: Some(950_000),
                    }),
                    rate_limits: None,
                },
            ))
        ),
    );

    let (updated, stats) = rewrite_rollout_resume_state_contents(
        &original,
        &ResumeStatePatch {
            model_context_window: 258_400,
        },
    )
    .expect("rewrite");

    assert_eq!(stats.token_count_events_updated, 1);
    assert_eq!(stats.turn_started_events_updated, 1);
    assert!(updated.contains("\"model_context_window\":258400"));
    assert!(!updated.contains("\"model_context_window\":950000"));
}

#[tokio::test]
async fn build_session_summary_applies_compaction_and_rollback() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path();
    let cwd = codex_home.join("workspace");
    tokio::fs::create_dir_all(&cwd).await.expect("create cwd");

    let thread_id =
        ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").expect("thread id");
    let rollout_dir = codex_home
        .join(SESSIONS_SUBDIR)
        .join("2026")
        .join("03")
        .join("11");
    tokio::fs::create_dir_all(&rollout_dir)
        .await
        .expect("create rollout dir");
    let rollout_path = rollout_dir.join(format!("rollout-2026-03-11T00-00-00-{thread_id}.jsonl"));

    append_thread_name(codex_home, thread_id, "Test Thread")
        .await
        .expect("append thread name");

    let lines = vec![
        rollout_line(
            "2026-03-11T00:00:00Z",
            RolloutItem::SessionMeta(session_meta_line(thread_id, &cwd)),
        ),
        rollout_line(
            "2026-03-11T00:00:01Z",
            RolloutItem::ResponseItem(user_message("first")),
        ),
        rollout_line(
            "2026-03-11T00:00:02Z",
            RolloutItem::ResponseItem(user_message("second")),
        ),
        rollout_line(
            "2026-03-11T00:00:03Z",
            RolloutItem::Compacted(codex_protocol::protocol::CompactedItem {
                message: "summary".to_string(),
                replacement_history: Some(vec![user_message("first"), user_message("second")]),
            }),
        ),
        rollout_line(
            "2026-03-11T00:00:04Z",
            RolloutItem::EventMsg(EventMsg::TokenCount(
                codex_protocol::protocol::TokenCountEvent {
                    info: Some(TokenUsageInfo {
                        total_token_usage: codex_protocol::protocol::TokenUsage {
                            input_tokens: 0,
                            cached_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: 1200,
                        },
                        last_token_usage: codex_protocol::protocol::TokenUsage {
                            input_tokens: 0,
                            cached_input_tokens: 0,
                            output_tokens: 0,
                            reasoning_output_tokens: 0,
                            total_tokens: 450,
                        },
                        model_context_window: Some(256000),
                    }),
                    rate_limits: None,
                },
            )),
        ),
        rollout_line(
            "2026-03-11T00:00:05Z",
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                codex_protocol::protocol::ThreadRolledBackEvent { num_turns: 1 },
            )),
        ),
    ];
    write_rollout(&rollout_path, &lines).await;

    let config = ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd.clone()),
            ..Default::default()
        })
        .build()
        .await
        .expect("build config");
    let summary = build_session_summary(&config, &rollout_path)
        .await
        .expect("build summary");

    assert_eq!(summary.thread_name.as_deref(), Some("Test Thread"));
    assert_eq!(summary.user_turns, 1);
    assert_eq!(summary.first_user_message.as_deref(), Some("first"));
    assert_eq!(summary.latest_total_tokens, Some(1200));
    assert_eq!(summary.latest_context_tokens, Some(450));
    assert_eq!(summary.latest_model_context_window, Some(256000));
    assert_eq!(summary.memory_mode.as_deref(), Some("enabled"));
}

#[tokio::test]
async fn archive_and_unarchive_roundtrip_preserves_rollout() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path();
    let thread_id =
        ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").expect("thread id");
    let sessions_dir = codex_home
        .join(SESSIONS_SUBDIR)
        .join("2026")
        .join("03")
        .join("11");
    tokio::fs::create_dir_all(&sessions_dir)
        .await
        .expect("create sessions dir");

    let rollout_path = sessions_dir.join(format!("rollout-2026-03-11T00-00-00-{thread_id}.jsonl"));
    write_rollout(
        &rollout_path,
        &[rollout_line(
            "2026-03-11T00:00:00Z",
            RolloutItem::SessionMeta(session_meta_line(thread_id, codex_home)),
        )],
    )
    .await;

    let archived_path = archive_rollout_file(codex_home, thread_id, &rollout_path)
        .await
        .expect("archive");
    assert!(archived_path.exists());
    assert!(!rollout_path.exists());

    let restored_path = unarchive_rollout_file(codex_home, thread_id, &archived_path)
        .await
        .expect("unarchive");
    assert!(restored_path.exists());
    assert!(!archived_path.exists());
    assert_eq!(restored_path, rollout_path);
}

#[tokio::test]
async fn build_session_summary_handles_real_forked_rollout_fixture() {
    let rollout_path = real_fixture_rollout_path();
    assert!(
        rollout_path.exists(),
        "missing test fixture: {}",
        rollout_path.display()
    );

    let codex_home = tempdir().expect("tempdir");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("build config");

    let summary = build_session_summary(&config, &rollout_path)
        .await
        .expect("build summary");

    assert_eq!(summary.thread_id, "019c4bcd-857a-7e50-9229-b30be321c56b");
    assert_eq!(
        summary.forked_from_id.as_deref(),
        Some("019bc4a8-fcda-70a2-ad21-2dd6c5d63c21")
    );
    assert_eq!(summary.source, "cli");
    assert_eq!(summary.session_provider.as_deref(), Some("openai"));
    assert_eq!(summary.session_cwd, PathBuf::from(r"D:\pi_workspace"));
    assert_eq!(summary.session_timestamp, "2026-02-11T08:24:46.202Z");
    assert_eq!(summary.latest_model.as_deref(), Some("gpt-5.2-codex"));
    assert_eq!(summary.latest_total_tokens, Some(43_615_395));
    assert_eq!(summary.latest_context_tokens, Some(271_103));
    assert_eq!(summary.latest_model_context_window, Some(950_000));
    assert_eq!(summary.user_turns, 332);
    assert_eq!(summary.memory_mode, None);
    assert!(
        summary
            .first_user_message
            .as_deref()
            .is_some_and(|message| message.starts_with("然后有个这三个测试")),
        "unexpected first user message: {:?}",
        summary.first_user_message
    );
}

#[tokio::test]
async fn repair_resume_state_rewrites_real_fixture_window_summary() {
    let fixture_path = real_fixture_rollout_path();
    let fixture_contents = tokio::fs::read_to_string(&fixture_path)
        .await
        .expect("read fixture");
    let (updated_contents, stats) = rewrite_rollout_resume_state_contents(
        &fixture_contents,
        &ResumeStatePatch {
            model_context_window: 258_400,
        },
    )
    .expect("rewrite");
    assert!(stats.token_count_events_updated > 0);

    let temp = tempdir().expect("tempdir");
    let rollout_dir = temp
        .path()
        .join(SESSIONS_SUBDIR)
        .join("2026")
        .join("02")
        .join("11");
    tokio::fs::create_dir_all(&rollout_dir)
        .await
        .expect("create rollout dir");
    let rollout_path =
        rollout_dir.join("rollout-2026-02-11T16-24-46-019c4bcd-857a-7e50-9229-b30be321c56b.jsonl");
    tokio::fs::write(&rollout_path, updated_contents)
        .await
        .expect("write rollout");

    let config = ConfigBuilder::default()
        .codex_home(temp.path().to_path_buf())
        .build()
        .await
        .expect("build config");
    let summary = build_session_summary(&config, &rollout_path)
        .await
        .expect("build summary");

    assert_eq!(summary.latest_model_context_window, Some(258_400));
    assert_eq!(summary.latest_context_tokens, Some(271_103));
    assert_eq!(
        summary.forked_from_id.as_deref(),
        Some("019bc4a8-fcda-70a2-ad21-2dd6c5d63c21")
    );
}

#[tokio::test]
async fn run_command_executes_show_via_dedicated_stack_thread() {
    crate::run_command(crate::cli::Command::Show(crate::cli::ShowArgs {
        target: crate::cli::TargetArgs {
            target: real_fixture_rollout_path().display().to_string(),
            config_profile: None,
        },
        json: false,
    }))
    .await
    .expect("run show command");
}

#[test]
fn render_profiled_resume_command_prefixes_profile() {
    let thread_id =
        ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").expect("thread id");
    let command = render_profiled_resume_command(Some("migrate-openrouter"), thread_id);
    assert!(command.starts_with("codex --profile migrate-openrouter resume "));
}

#[test]
fn shell_quote_wraps_unsafe_values() {
    assert_eq!(shell_quote("safe-value"), "safe-value");
    assert_eq!(shell_quote("needs space"), "'needs space'");
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
}

#[test]
fn find_thread_id_by_name_in_session_index_returns_latest_match() {
    let temp = tempdir().expect("tempdir");
    let codex_home = temp.path();
    let first_id = ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea97").expect("first id");
    let second_id =
        ThreadId::from_string("019cd66f-f4ea-7022-802b-7007c11cea98").expect("second id");

    std::fs::write(
        codex_home.join("session_index.jsonl"),
        format!(
            "{{\"id\":\"{first_id}\",\"thread_name\":\"same\",\"updated_at\":\"2026-03-11T00:00:00Z\"}}\n{{\"id\":\"{second_id}\",\"thread_name\":\"same\",\"updated_at\":\"2026-03-11T00:01:00Z\"}}\n"
        ),
    )
    .expect("write session index");

    let found =
        find_thread_id_by_name_in_session_index(codex_home, "same").expect("resolve thread id");
    assert_eq!(found, Some(second_id));
}
