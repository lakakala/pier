mod cli;

use anyhow::{Context, Result};
use clap::Parser;
use cli::Cli;

#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let path = cli.config.canonicalize()?;
    let mut config: pier_controller::Config =
        serde_yaml_ng::from_slice(&std::fs::read(&path)?).context("invalid controller config")?;
    let base = path.parent().unwrap();
    if !config.repository.url.is_empty()
        && !config.repository.url.contains(':')
        && std::path::Path::new(&config.repository.url).is_relative()
    {
        config.repository.url = base
            .join(&config.repository.url)
            .to_string_lossy()
            .into_owned();
    }
    if config.state_dir.is_relative() {
        config.state_dir = base.join(&config.state_dir);
    }
    pier_controller::run(config).await
}
