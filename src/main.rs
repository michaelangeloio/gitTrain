use anyhow::Result;
use clap::Parser;

mod app;
mod cli;
mod config;
mod conflict;
mod errors;
mod git;
mod gitlab;
mod stack;
mod utils;

use app::AppContext;
use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    // Initialize and run the application context
    AppContext::new()?.run(cli).await
}
