//! Library entrypoint for the standalone Codex session manager.
//!
//! The binary is intentionally thin so the command orchestration can be reused
//! from tests or other Rust entrypoints without duplicating CLI wiring.

mod cli;
mod commands;
mod operations;
mod rollout_edit;
mod runtime;
mod smart;
mod summary;
mod tui;
mod types;

use anyhow::Result;
use std::thread;

pub use crate::cli::Cli;
pub(crate) use crate::cli::Command;

const COMMAND_THREAD_STACK_SIZE_BYTES: usize = 16 * 1024 * 1024;

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(command) => run_command(command).await,
        None => tui::run().await,
    }
}

pub(crate) async fn run_command(command: Command) -> Result<()> {
    tokio::task::spawn_blocking(move || run_command_on_dedicated_thread(command)).await?
}

fn run_command_on_dedicated_thread(command: Command) -> Result<()> {
    let handle = thread::Builder::new()
        .name("codex-session-manager-command".to_string())
        .stack_size(COMMAND_THREAD_STACK_SIZE_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(commands::run(command))
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("command thread panicked")),
    }
}

#[cfg(test)]
mod tests;
