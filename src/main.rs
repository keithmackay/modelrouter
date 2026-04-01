use clap::Parser;
use modelrouter::cli::commands::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    modelrouter::cli::run(cli).await
}
