use clap::Parser;
use std::path::PathBuf;

/// 管理应用定义、服务器和部署任务
#[derive(Parser)]
#[command(version)]
pub(crate) struct Cli {
    /// Controller 启动配置文件路径
    #[arg(long, value_name = "PATH")]
    pub config: PathBuf,
}
