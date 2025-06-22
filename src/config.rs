use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::info;

use crate::errors::TrainError;
use crate::ui::{get_user_input, print_info};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrainConfig {
    pub editor: EditorConfig,
    pub conflict_resolution: ConflictResolutionConfig,
    pub git: GitConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditorConfig {
    pub default_editor: String,
    pub editor_args: Vec<String>,
    pub wait_for_editor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictResolutionConfig {
    pub auto_resolve_strategy: AutoResolveStrategy,
    pub backup_on_conflict: bool,
    pub max_retry_attempts: u32,
    pub prompt_before_force_push: bool,
    #[serde(default)]
    pub auto_force_push_after_rebase: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    pub default_rebase_strategy: RebaseStrategy,
    pub auto_stash: bool,
    pub verify_signatures: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AutoResolveStrategy {
    /// Never auto-resolve, always prompt user
    Never,
    /// Auto-resolve simple conflicts (e.g., whitespace, line endings)
    Simple,
    /// Auto-resolve when possible, prompt for complex conflicts
    Smart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RebaseStrategy {
    /// Standard rebase
    Standard,
    /// Rebase with merge strategy
    Merge,
    /// Interactive rebase when conflicts occur
    Interactive,
}

impl Default for EditorConfig {
    fn default() -> Self {
        let default_editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| {
                // Try to detect common editors
                if which::which("cursor").is_ok() {
                    "cursor".to_string()
                } else if which::which("code").is_ok() {
                    "code".to_string()
                } else if which::which("vim").is_ok() {
                    "vim".to_string()
                } else {
                    "nano".to_string()
                }
            });

        Self {
            default_editor,
            editor_args: vec!["--wait".to_string()],
            wait_for_editor: true,
        }
    }
}

impl Default for ConflictResolutionConfig {
    fn default() -> Self {
        Self {
            auto_resolve_strategy: AutoResolveStrategy::Smart,
            backup_on_conflict: true,
            max_retry_attempts: 3,
            prompt_before_force_push: true,
            auto_force_push_after_rebase: false,
        }
    }
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            default_rebase_strategy: RebaseStrategy::Standard,
            auto_stash: true,
            verify_signatures: false,
        }
    }
}

pub struct ConfigManager {
    config_path: PathBuf,
    config: TrainConfig,
}

impl ConfigManager {
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| TrainError::IoError {
                message: "Could not determine config directory".to_string(),
            })?
            .join("git-train");

        let config_path = config_dir.join("config.toml");

        // Create config directory if it doesn't exist
        if !config_dir.exists() {
            fs::create_dir_all(&config_dir)?;
            info!("Created config directory: {:?}", config_dir);
        }

        let config = if config_path.exists() {
            Self::load_config(&config_path)?
        } else {
            let default_config = TrainConfig::default();
            Self::save_config(&config_path, &default_config)?;
            print_info(&format!("Created default config at: {:?}", config_path));
            default_config
        };

        Ok(Self {
            config_path,
            config,
        })
    }

    pub fn get_config(&self) -> &TrainConfig {
        &self.config
    }

    pub fn update_config<F>(&mut self, updater: F) -> Result<()>
    where
        F: FnOnce(&mut TrainConfig),
    {
        updater(&mut self.config);
        Self::save_config(&self.config_path, &self.config)?;
        Ok(())
    }

    pub fn set_default_editor(&mut self, editor: &str) -> Result<()> {
        self.update_config(|config| {
            config.editor.default_editor = editor.to_string();

            // Set appropriate args based on editor
            config.editor.editor_args = match editor {
                "cursor" | "code" => vec!["--wait".to_string()],
                "vim" | "nvim" => vec![],
                "nano" => vec![],
                "emacs" => vec!["--no-window-system".to_string()],
                _ => vec!["--wait".to_string()],
            };
        })?;

        print_info(&format!("Set default editor to: {}", editor));
        Ok(())
    }

    pub fn configure_interactive(&mut self) -> Result<()> {
        print_info("Let's configure git-train for your workflow");

        // Configure editor
        let current_editor = &self.config.editor.default_editor;
        let editor_prompt = format!("Default editor (current: {})", current_editor);
        let editor = get_user_input(&editor_prompt, Some(current_editor))?;

        if editor != *current_editor {
            self.set_default_editor(&editor)?;
        }

        // Configure conflict resolution strategy
        let strategies = [
            "Never auto-resolve",
            "Simple auto-resolve",
            "Smart auto-resolve",
        ];
        let current_strategy_idx = match self.config.conflict_resolution.auto_resolve_strategy {
            AutoResolveStrategy::Never => 0,
            AutoResolveStrategy::Simple => 1,
            AutoResolveStrategy::Smart => 2,
        };

        println!("Conflict resolution strategies:");
        for (i, strategy) in strategies.iter().enumerate() {
            let marker = if i == current_strategy_idx {
                "→"
            } else {
                " "
            };
            println!("{} {}: {}", marker, i + 1, strategy);
        }

        let strategy_input = get_user_input(
            "Choose conflict resolution strategy (1-3)",
            Some(&(current_strategy_idx + 1).to_string()),
        )?;

        if let Ok(choice) = strategy_input.parse::<usize>() {
            if (1..=3).contains(&choice) {
                let new_strategy = match choice {
                    1 => AutoResolveStrategy::Never,
                    2 => AutoResolveStrategy::Simple,
                    3 => AutoResolveStrategy::Smart,
                    _ => unreachable!(),
                };

                self.update_config(|config| {
                    config.conflict_resolution.auto_resolve_strategy = new_strategy;
                })?;
            }
        }

        print_info("Configuration updated successfully");
        Ok(())
    }

    fn load_config(path: &PathBuf) -> Result<TrainConfig> {
        let content = fs::read_to_string(path)?;
        let config: TrainConfig =
            toml::from_str(&content).map_err(|e| TrainError::SerializationError {
                message: format!("Failed to parse config: {}", e),
            })?;
        Ok(config)
    }

    fn save_config(path: &PathBuf, config: &TrainConfig) -> Result<()> {
        let content =
            toml::to_string_pretty(config).map_err(|e| TrainError::SerializationError {
                message: format!("Failed to serialize config: {}", e),
            })?;
        fs::write(path, content)?;
        Ok(())
    }
}

// Helper function to check if a command exists
mod which {
    use std::process::Command;

    pub fn which(command: &str) -> Result<(), ()> {
        Command::new("which")
            .arg(command)
            .output()
            .map_err(|_| ())
            .and_then(|output| {
                if output.status.success() {
                    Ok(())
                } else {
                    Err(())
                }
            })
    }
}
