mod app;
mod config;
mod ha_api;
mod ha_auth;
mod ha_ws;
mod http;
mod logging;
mod state_store;
mod storage;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(about = "Milestone 0 control plane for low-power Linux targets")]
struct Args {
    #[arg(long, env = "PI_CTRL_CONFIG", default_value = "config/default.toml")]
    config: std::path::PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init_logging();

    let config = config::AppConfig::load(&args.config).await?;
    app::run(config).await
}
