use anyhow::Result;
use clap::Parser;

mod cli;
mod gitlab;
mod utils;
mod stack;
mod errors;

use cli::{Cli, Commands};
use stack::StackManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    
    let cli = Cli::parse();
    let mut stack_manager = StackManager::new().await?;
    
    match cli.command {
        Commands::Create { name } => {
            stack_manager.create_stack(&name).await?;
        }
        Commands::Save { message } => {
            stack_manager.save_changes(&message).await?;
        }
        Commands::Amend { message } => {
            stack_manager.amend_changes(message.as_deref()).await?;
        }
        Commands::Add { parent } => {
            stack_manager.add_branch_to_stack(parent.as_deref()).await?;
        }
        Commands::Status => {
            stack_manager.show_status().await?;
        }
        Commands::List => {
            stack_manager.list_stacks().await?;
        }
        Commands::Switch { stack } => {
            stack_manager.switch_stack(&stack).await?;
        }
        Commands::Push => {
            stack_manager.push_stack().await?;
        }
        Commands::Sync => {
            stack_manager.sync_with_remote().await?;
        }
    }

    Ok(())
} 