mod cli;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use std::path::PathBuf;

fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match cli.command {
        Command::ApplyUpgrade => pier_agent::upgrade::apply(),
        Command::Init => pier_agent::init::wizard(),
        Command::Run { config } => run(config),
    }
}

fn run(path: PathBuf) -> Result<()> {
    let config = pier_agent::init::load_config(&path)?;
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()?
        .block_on(pier_agent::run(config))
}
