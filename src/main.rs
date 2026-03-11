use clap::Parser;
use codex_session_manager::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    codex_session_manager::run(Cli::parse()).await
}
