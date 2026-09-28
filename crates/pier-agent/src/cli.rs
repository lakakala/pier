use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// 在服务器上部署并守护应用服务
#[derive(Parser)]
#[command(version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    #[command(name = "__apply-upgrade", hide = true)]
    ApplyUpgrade,
    /// 交互授权并启动 systemd 服务
    Init,
    /// 前台运行 agent（systemd 使用）
    Run {
        /// Agent 配置文件路径
        #[arg(long, value_name = "PATH", default_value = pier_agent::init::DEFAULT_CONFIG)]
        config: PathBuf,
    },
}
