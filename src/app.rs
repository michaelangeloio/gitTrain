use anyhow::Result;

use crate::{
    cli::{Cli, Commands, ConfigCommands},
    config::{ConfigManager, TrainConfig},
    stack::StackManager,
    ui,
};

/// The main application context.
///
/// This struct holds the state and managers for the application.
/// It is responsible for dispatching commands and managing the application lifecycle.
pub struct AppContext {
    config: TrainConfig,
    config_manager: ConfigManager,
}

impl AppContext {
    /// Create a new `AppContext`.
    pub fn new() -> Result<Self> {
        let config_manager = ConfigManager::new()?;
        let config = config_manager.get_config().clone();
        Ok(Self {
            config,
            config_manager,
        })
    }

    /// Run the application with the given CLI arguments.
    pub async fn run(&mut self, cli: Cli) -> Result<()> {
        match cli.command {
            Commands::Config(config_cmd) => self.handle_config_commands(&config_cmd).await,
            Commands::Health => {
                let mut stack_manager = self.get_stack_manager().await?;
                Self::handle_health_command(&mut stack_manager).await
            }
            _ => {
                let mut stack_manager = self.get_stack_manager().await?;
                self.handle_stack_commands(cli.command, &mut stack_manager)
                    .await
            }
        }
    }

    /// Get a `StackManager` instance.
    async fn get_stack_manager(&self) -> Result<StackManager> {
        StackManager::new_with_config(self.config.clone()).await
    }

    /// Handle stack-related commands.
    async fn handle_stack_commands(
        &self,
        command: Commands,
        stack_manager: &mut StackManager,
    ) -> Result<()> {
        match command {
            Commands::Create { name } => stack_manager.create_stack(&name).await,
            Commands::Save { message } => stack_manager.save_changes(&message).await,
            Commands::Amend { message } => stack_manager.amend_changes(message.as_deref()).await,
            Commands::Add { parent } => stack_manager.add_branch_to_stack(parent.as_deref()).await,
            Commands::Status => stack_manager.show_status().await,
            Commands::List => stack_manager.list_stacks().await,
            Commands::Switch { stack } => stack_manager.switch_stack(&stack).await,
            Commands::Navigate => stack_manager.navigate_stack_interactively().await,
            Commands::Delete { stack, force } => stack_manager.delete_stack(&stack, force).await,
            Commands::Push => stack_manager.push_stack().await,
            Commands::Sync => stack_manager.sync_with_remote().await,
            // These are handled in run()
            Commands::Config(_) | Commands::Health => Ok(()),
        }
    }

    /// Handle configuration-related commands.
    async fn handle_config_commands(&mut self, cmd: &ConfigCommands) -> Result<()> {
        match cmd {
            ConfigCommands::Show => {
                let config = self.config_manager.get_config();
                ui::print_train_header("Git-Train Configuration");
                ui::print_config_item("Editor", &config.editor.default_editor);
                ui::print_config_item("Editor args", &format!("{:?}", config.editor.editor_args));
                ui::print_config_item(
                    "Auto-resolve strategy",
                    &format!("{:?}", config.conflict_resolution.auto_resolve_strategy),
                );
                ui::print_config_item(
                    "Backup on conflict",
                    &config.conflict_resolution.backup_on_conflict.to_string(),
                );
                ui::print_config_item(
                    "Prompt before force-push",
                    &config
                        .conflict_resolution
                        .prompt_before_force_push
                        .to_string(),
                );
                ui::print_config_item(
                    "Auto force-push after rebase",
                    &config
                        .conflict_resolution
                        .auto_force_push_after_rebase
                        .to_string(),
                );
                ui::print_config_item("Auto-stash", &config.git.auto_stash.to_string());
                ui::print_config_item(
                    "Default rebase strategy",
                    &format!("{:?}", config.git.default_rebase_strategy),
                );
            }
            ConfigCommands::Setup => {
                self.config_manager.configure_interactive()?;
            }
            ConfigCommands::SetEditor { editor } => {
                self.config_manager.set_default_editor(editor)?;
            }
            ConfigCommands::SetStrategy { strategy } => {
                use crate::config::AutoResolveStrategy;

                let new_strategy = match strategy.to_lowercase().as_str() {
                    "never" => AutoResolveStrategy::Never,
                    "simple" => AutoResolveStrategy::Simple,
                    "smart" => AutoResolveStrategy::Smart,
                    _ => {
                        ui::print_error("Invalid strategy. Use 'never', 'simple', or 'smart'");
                        return Ok(());
                    }
                };

                self.config_manager.update_config(|config| {
                    config.conflict_resolution.auto_resolve_strategy = new_strategy;
                })?;

                ui::print_success(&format!(
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
                        ui::print_error("Invalid mode. Use 'auto', 'prompt', or 'never'");
                        return Ok(());
                    }
                };

                self.config_manager.update_config(|config| {
                    config.conflict_resolution.auto_force_push_after_rebase = auto_force;
                    config.conflict_resolution.prompt_before_force_push = prompt_before;
                })?;

                ui::print_success(&format!("Set force-push behavior to: {}", mode));

                match mode.as_str() {
                    "auto" => ui::print_info(
                        "Branches will be automatically force-pushed after rebase (with --force-with-lease for safety)",
                    ),
                    "prompt" => ui::print_info(
                        "You will be prompted before force-pushing branches after rebase",
                    ),
                    "never" => ui::print_info(
                        "Force-push will be skipped, manual intervention required after rebase",
                    ),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Handle the health check command.
    async fn handle_health_command(stack_manager: &mut StackManager) -> Result<()> {
        ui::print_train_header("Repository Health Check");

        {
            let conflict_resolver = stack_manager.get_conflict_resolver();
            let git_state = conflict_resolver.get_git_state()?;

            // Check git state
            match git_state {
                crate::conflict::GitState::Clean => {
                    ui::print_success("✅ Git repository is in a clean state");
                }
                state => {
                    ui::print_warning(&format!("⚠️ Git repository is in state: {:?}", state));

                    // Check for conflicts
                    if let Some(conflicts) = conflict_resolver.detect_conflicts()? {
                        ui::print_error(&format!(
                            "❌ Found {} conflicted files:",
                            conflicts.files.len()
                        ));
                        conflict_resolver.print_conflict_summary(&conflicts);

                        ui::print_info("Recovery options:");
                        ui::print_info(
                            "• Run 'git-train sync' to continue with integrated conflict resolution",
                        );
                        ui::print_info("• Run 'git-train health' to check current state");
                    } else {
                        ui::print_info("No conflicts detected, but repository needs attention");
                        ui::print_info("Try running: git-train sync");
                    }
                }
            }
        }

        // Check for stack
        match stack_manager.get_or_load_current_stack() {
            Ok(stack) => {
                ui::print_success(&format!("✅ Stack '{}' is available", stack.name));

                // Check working directory
                let has_changes = stack_manager.has_uncommitted_changes().unwrap_or(false);
                if has_changes {
                    ui::print_info("📝 Working directory has uncommitted changes");
                } else {
                    ui::print_success("✅ Working directory is clean");
                }

                // Check current branch
                if let Ok(current_branch) = stack_manager.get_current_branch() {
                    if stack.branches.contains_key(&current_branch) {
                        ui::print_success(&format!(
                            "✅ Current branch '{}' is part of the stack",
                            current_branch
                        ));
                    } else {
                        ui::print_warning(&format!(
                            "⚠️ Current branch '{}' is not part of the stack",
                            current_branch
                        ));
                        ui::print_info("You can add it with: git-train add");
                    }
                }
            }
            Err(_) => {
                ui::print_warning("⚠️ No active stack found");
                ui::print_info("Create a new stack with: git-train create <name>");
            }
        }

        // Overall health summary
        println!();
        let git_state_check = stack_manager.get_conflict_resolver().get_git_state()?;
        match git_state_check {
            crate::conflict::GitState::Clean => {
                ui::print_success("🎉 Repository is healthy and ready for git-train operations");
            }
            _ => {
                ui::print_warning(
                    "🔧 Repository needs attention before git-train operations can proceed safely",
                );
            }
        }

        Ok(())
    }
}
