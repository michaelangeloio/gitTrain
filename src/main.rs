use anyhow::Result;
use clap::Parser;

mod cli;
mod config;
mod conflict;
mod gitlab;
mod utils;
mod stack;
mod errors;

use cli::{Cli, Commands, ConfigCommands, ResolveCommands};
use config::ConfigManager;
use stack::StackManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    
    let cli = Cli::parse();
    
    // Initialize configuration first
    let mut config_manager = ConfigManager::new()?;
    
    // Handle config commands first (don't need StackManager)
    match &cli.command {
        Commands::Config(config_cmd) => {
            return handle_config_commands(config_cmd, &mut config_manager).await;
        }
        _ => {}
    }
    
    // For all other commands, initialize StackManager with config
    let mut stack_manager = StackManager::new_with_config(config_manager.get_config().clone()).await?;
    
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
        Commands::Delete { stack, force } => {
            stack_manager.delete_stack(&stack, force).await?;
        }
        Commands::Push => {
            stack_manager.push_stack().await?;
        }
        Commands::Sync => {
            stack_manager.sync_with_remote().await?;
        }
        Commands::Config(_) => {
            // Already handled above
        }
        Commands::Resolve(resolve_cmd) => {
            handle_resolve_commands(&resolve_cmd, &mut stack_manager).await?;
        }
    }

    Ok(())
}

async fn handle_config_commands(cmd: &ConfigCommands, config_manager: &mut ConfigManager) -> Result<()> {
    match cmd {
        ConfigCommands::Show => {
            let config = config_manager.get_config();
            println!("Git-Train Configuration:");
            println!("========================");
            println!("Editor: {}", config.editor.default_editor);
            println!("Editor args: {:?}", config.editor.editor_args);
            println!("Auto-resolve strategy: {:?}", config.conflict_resolution.auto_resolve_strategy);
            println!("Backup on conflict: {}", config.conflict_resolution.backup_on_conflict);
            println!("Auto-stash: {}", config.git.auto_stash);
            println!("Default rebase strategy: {:?}", config.git.default_rebase_strategy);
        }
        ConfigCommands::Setup => {
            config_manager.configure_interactive()?;
        }
        ConfigCommands::SetEditor { editor } => {
            config_manager.set_default_editor(editor)?;
        }
        ConfigCommands::SetStrategy { strategy } => {
            use config::{AutoResolveStrategy, TrainConfig};
            
            let new_strategy = match strategy.to_lowercase().as_str() {
                "never" => AutoResolveStrategy::Never,
                "simple" => AutoResolveStrategy::Simple,
                "smart" => AutoResolveStrategy::Smart,
                _ => {
                    eprintln!("Invalid strategy. Use 'never', 'simple', or 'smart'");
                    return Ok(());
                }
            };
            
            config_manager.update_config(|config| {
                config.conflict_resolution.auto_resolve_strategy = new_strategy;
            })?;
            
            utils::print_success(&format!("Set conflict resolution strategy to: {}", strategy));
        }
    }
    Ok(())
}

async fn handle_resolve_commands(cmd: &ResolveCommands, stack_manager: &mut StackManager) -> Result<()> {
    let conflict_resolver = stack_manager.get_conflict_resolver();
    
    match cmd {
        ResolveCommands::Check => {
            if let Some(conflicts) = conflict_resolver.detect_conflicts()? {
                utils::print_warning(&format!("Found conflicts in {} files", conflicts.files.len()));
                conflict_resolver.print_conflict_summary(&conflicts);
            } else {
                utils::print_success("No conflicts detected");
            }
        }
        ResolveCommands::Interactive => {
            if let Some(conflicts) = conflict_resolver.detect_conflicts()? {
                conflict_resolver.resolve_conflicts_interactively(&conflicts).await?;
            } else {
                utils::print_info("No conflicts to resolve");
            }
        }
        ResolveCommands::Auto => {
            if let Some(conflicts) = conflict_resolver.detect_conflicts()? {
                if conflict_resolver.auto_resolve_conflicts(&conflicts).await? {
                    utils::print_success("Conflicts resolved automatically");
                } else {
                    utils::print_warning("Could not resolve all conflicts automatically");
                }
            } else {
                utils::print_info("No conflicts to resolve");
            }
        }
        ResolveCommands::Abort => {
            conflict_resolver.abort_current_operation()?;
        }
        ResolveCommands::Continue => {
            // This is primarily for resuming after manual resolution
            conflict_resolver.verify_conflicts_resolved().await?;
        }
    }
    Ok(())
} 