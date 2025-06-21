use anyhow::Result;
use clap::Parser;

mod cli;
mod config;
mod conflict;
mod errors;
mod gitlab;
mod stack;
mod utils;

use cli::{Cli, Commands, ConfigCommands};
use config::ConfigManager;
use stack::StackManager;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    // Initialize configuration first
    let mut config_manager = ConfigManager::new()?;

    // Handle config commands first (don't need StackManager)
    if let Commands::Config(config_cmd) = &cli.command {
        return handle_config_commands(config_cmd, &mut config_manager).await;
    }

    // For all other commands, initialize StackManager with config
    let mut stack_manager =
        StackManager::new_with_config(config_manager.get_config().clone()).await?;

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
        Commands::Navigate => {
            stack_manager.navigate_stack_interactively().await?;
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
        Commands::Config(cmd) => {
            handle_config_commands(&cmd, &mut config_manager).await?;
        }
        Commands::Health => {
            handle_health_command(&mut stack_manager).await?;
        }
    }

    Ok(())
}

async fn handle_config_commands(
    cmd: &ConfigCommands,
    config_manager: &mut ConfigManager,
) -> Result<()> {
    match cmd {
        ConfigCommands::Show => {
            let config = config_manager.get_config();
            println!("Git-Train Configuration:");
            println!("========================");
            println!("Editor: {}", config.editor.default_editor);
            println!("Editor args: {:?}", config.editor.editor_args);
            println!(
                "Auto-resolve strategy: {:?}",
                config.conflict_resolution.auto_resolve_strategy
            );
            println!(
                "Backup on conflict: {}",
                config.conflict_resolution.backup_on_conflict
            );
            println!(
                "Prompt before force-push: {}",
                config.conflict_resolution.prompt_before_force_push
            );
            println!(
                "Auto force-push after rebase: {}",
                config.conflict_resolution.auto_force_push_after_rebase
            );
            println!("Auto-stash: {}", config.git.auto_stash);
            println!(
                "Default rebase strategy: {:?}",
                config.git.default_rebase_strategy
            );
        }
        ConfigCommands::Setup => {
            config_manager.configure_interactive()?;
        }
        ConfigCommands::SetEditor { editor } => {
            config_manager.set_default_editor(editor)?;
        }
        ConfigCommands::SetStrategy { strategy } => {
            use config::AutoResolveStrategy;

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

            utils::print_success(&format!(
                "Set conflict resolution strategy to: {}",
                strategy
            ));
        }
        ConfigCommands::SetForcePush { mode } => {
            let (auto_force, prompt_before) = match mode.to_lowercase().as_str() {
                "auto" => (true, false),
                "prompt" => (false, true),
                "never" => (false, false),
                _ => {
                    eprintln!("Invalid mode. Use 'auto', 'prompt', or 'never'");
                    return Ok(());
                }
            };

            config_manager.update_config(|config| {
                config.conflict_resolution.auto_force_push_after_rebase = auto_force;
                config.conflict_resolution.prompt_before_force_push = prompt_before;
            })?;

            utils::print_success(&format!("Set force-push behavior to: {}", mode));

            match mode.as_str() {
                "auto" => utils::print_info("Branches will be automatically force-pushed after rebase (with --force-with-lease for safety)"),
                "prompt" => utils::print_info("You will be prompted before force-pushing branches after rebase"),
                "never" => utils::print_info("Force-push will be skipped, manual intervention required after rebase"),
                _ => {}
            }
        }
    }
    Ok(())
}

async fn handle_health_command(stack_manager: &mut StackManager) -> Result<()> {
    utils::print_train_header("Repository Health Check");

    let conflict_resolver = stack_manager.get_conflict_resolver();
    let git_state = conflict_resolver.get_git_state()?;

    // Check git state
    match git_state {
        crate::conflict::GitState::Clean => {
            utils::print_success("✅ Git repository is in a clean state");
        }
        state => {
            utils::print_warning(&format!("⚠️ Git repository is in state: {:?}", state));

            // Check for conflicts
            if let Some(conflicts) = conflict_resolver.detect_conflicts()? {
                utils::print_error(&format!(
                    "❌ Found {} conflicted files:",
                    conflicts.files.len()
                ));
                conflict_resolver.print_conflict_summary(&conflicts);

                utils::print_info("Recovery options:");
                utils::print_info(
                    "• Run 'git-train sync' to continue with integrated conflict resolution",
                );
                utils::print_info("• Run 'git-train health' to check current state");
            } else {
                utils::print_info("No conflicts detected, but repository needs attention");
                utils::print_info("Try running: git-train sync");
            }
        }
    }

    // Check for stack
    match stack_manager.load_current_stack() {
        Ok(stack) => {
            utils::print_success(&format!("✅ Stack '{}' is available", stack.name));

            // Check working directory
            let has_changes = stack_manager.has_uncommitted_changes().unwrap_or(false);
            if has_changes {
                utils::print_info("📝 Working directory has uncommitted changes");
            } else {
                utils::print_success("✅ Working directory is clean");
            }

            // Check current branch
            if let Ok(current_branch) = stack_manager.get_current_branch() {
                if stack.branches.contains_key(&current_branch) {
                    utils::print_success(&format!(
                        "✅ Current branch '{}' is part of the stack",
                        current_branch
                    ));
                } else {
                    utils::print_warning(&format!(
                        "⚠️ Current branch '{}' is not part of the stack",
                        current_branch
                    ));
                    utils::print_info("You can add it with: git-train add");
                }
            }
        }
        Err(_) => {
            utils::print_warning("⚠️ No active stack found");
            utils::print_info("Create a new stack with: git-train create <name>");
        }
    }

    // Overall health summary
    println!();
    let git_state_check = conflict_resolver.get_git_state()?;
    match git_state_check {
        crate::conflict::GitState::Clean => {
            utils::print_success("🎉 Repository is healthy and ready for git-train operations");
        }
        _ => {
            utils::print_warning(
                "🔧 Repository needs attention before git-train operations can proceed safely",
            );
        }
    }

    Ok(())
}
