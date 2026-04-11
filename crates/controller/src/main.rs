use anyhow::Result;
use clap::Parser;
use home_edge::{app, config, logging};

#[derive(Debug, Parser)]
#[command(about = "Milestone 0 edge runtime for low-power Linux targets")]
struct Args {
    #[arg(long, env = "HOME_EDGE_CONFIG", default_value = "config/default.toml")]
    config: std::path::PathBuf,

    /// Wipe all persisted state before starting: login credentials, OAuth tokens,
    /// registered devices, and onboarding progress. The server will start in
    /// first-run mode and require going through onboarding again.
    #[arg(long)]
    reset: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging::init_logging();

    let config = config::AppConfig::load(&args.config).await?;
    app::run(config, args.reset).await
}
