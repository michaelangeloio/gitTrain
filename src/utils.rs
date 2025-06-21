use anyhow::Result;
use console::{style, Term};
use inquire::{Confirm, Select, Text};
use std::process::Command;
use tracing::{error, info};

pub fn confirm_action(message: &str) -> Result<bool> {
    let confirmation = Confirm::new(message).with_default(false).prompt()?;

    Ok(confirmation)
}

pub fn select_from_list<T: ToString + Clone>(items: &[T], prompt: &str) -> Result<usize> {
    let string_items: Vec<String> = items.iter().map(|item| item.to_string()).collect();
    let selection = Select::new(prompt, string_items)
        .with_page_size(15)
        .prompt()?;

    // Find the index of the selected item
    let selected_string = selection;
    for (i, item) in items.iter().enumerate() {
        if item.to_string() == selected_string {
            return Ok(i);
        }
    }

    // This shouldn't happen, but fallback to 0
    Ok(0)
}

pub fn get_user_input(prompt: &str, default: Option<&str>) -> Result<String> {
    let mut input = Text::new(prompt);

    if let Some(default_value) = default {
        input = input.with_default(default_value);
    }

    let result = input.prompt()?;
    Ok(result)
}

#[derive(Debug, Clone)]
pub struct NavigationOption {
    pub display: String,
    pub action: NavigationAction,
}

#[derive(Debug, Clone)]
pub enum NavigationAction {
    SwitchToBranch(String),
    ShowBranchInfo(String),
    CreateMR(String),
    ViewMR(String, u64),
    RefreshStatus,
    Exit,
}

impl std::fmt::Display for NavigationOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display)
    }
}

#[derive(Debug, Clone)]
pub struct MrStatusInfo {
    pub iid: u64,
    pub state: String,
}

pub fn create_navigation_options(
    branches: &[String],
    current_branch: Option<&str>,
    branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>,
) -> Vec<NavigationOption> {
    let mut options = Vec::new();

    // Add section separator
    options.push(NavigationOption {
        display: style("── BRANCHES ────────────────────────────────").dim().to_string(),
        action: NavigationAction::RefreshStatus, // Dummy action
    });

    // Add branch navigation options
    for branch in branches {
        let is_current = current_branch.is_some_and(|current| current == branch);
        let mr_info = if let Some(mr_status) = branch_mr_status.get(branch) {
            let (status_symbol, status_text) = match mr_status.state.as_str() {
                "merged" => ("✔", "MERGED"),
                "closed" => ("✘", "CLOSED"),
                "opened" => ("●", "OPEN"),
                _ => ("?", mr_status.state.as_str()),
            };
            format!(" [MR !{} {} {}]", mr_status.iid, status_symbol, status_text)
        } else {
            String::new()
        };

        let styled_symbol = if is_current {
            style("▶").bold().cyan()
        } else {
            style("│")
        };

        let display = format!(
            "{} {} {}{}",
            styled_symbol,
            style(branch).bold(),
            if is_current { style(" (current)").dim() } else { style("") },
            style(&mr_info).dim()
        );

        options.push(NavigationOption {
            display,
            action: NavigationAction::SwitchToBranch(branch.clone()),
        });
    }

    // Add section separator for actions
    options.push(NavigationOption {
        display: style("── ACTIONS ─────────────────────────────────").dim().to_string(),
        action: NavigationAction::RefreshStatus, // Dummy action
    });

    // Add branch-specific actions grouped by current branch
    if let Some(current) = current_branch {
        if branches.contains(&current.to_string()) {
            // Show info action for current branch
            options.push(NavigationOption {
                display: format!("  {} Show info for {}", 
                    style("ℹ").blue(),
                    style(current).bold()
                ),
                action: NavigationAction::ShowBranchInfo(current.to_string()),
            });

            // MR action for current branch
            if let Some(mr_status) = branch_mr_status.get(current) {
                options.push(NavigationOption {
                    display: format!("  {} View MR !{} for {}", 
                        style("→").green(),
                        mr_status.iid,
                        style(current).bold()
                    ),
                    action: NavigationAction::ViewMR(current.to_string(), mr_status.iid),
                });
            } else {
                options.push(NavigationOption {
                    display: format!("  {} Create MR for {}", 
                        style("+").green(),
                        style(current).bold()
                    ),
                    action: NavigationAction::CreateMR(current.to_string()),
                });
            }
        }
    }

    // Add other branch actions in a submenu style
    for branch in branches {
        if Some(branch.as_str()) == current_branch {
            continue; // Skip current branch as we already handled it above
        }

        options.push(NavigationOption {
            display: format!("  {} Show info for {}", 
                style("ℹ").blue().dim(),
                style(branch).dim()
            ),
            action: NavigationAction::ShowBranchInfo(branch.clone()),
        });

        if let Some(mr_status) = branch_mr_status.get(branch) {
            options.push(NavigationOption {
                display: format!("  {} View MR !{} for {}", 
                    style("→").green().dim(),
                    mr_status.iid,
                    style(branch).dim()
                ),
                action: NavigationAction::ViewMR(branch.clone(), mr_status.iid),
            });
        } else {
            options.push(NavigationOption {
                display: format!("  {} Create MR for {}", 
                    style("+").green().dim(),
                    style(branch).dim()
                ),
                action: NavigationAction::CreateMR(branch.clone()),
            });
        }
    }

    // Add final section separator
    options.push(NavigationOption {
        display: style("── UTILITIES ───────────────────────────────").dim().to_string(),
        action: NavigationAction::RefreshStatus, // Dummy action
    });

    // Add utility options
    options.push(NavigationOption {
        display: format!("  {} Refresh status", style("↻").cyan()),
        action: NavigationAction::RefreshStatus,
    });

    options.push(NavigationOption {
        display: format!("  {} Exit navigation", style("✘").red()),
        action: NavigationAction::Exit,
    });

    options
}

pub fn interactive_stack_navigation(
    options: &[NavigationOption],
    prompt: &str,
) -> Result<NavigationAction> {
    let selection = Select::new(prompt, options.to_vec())
        .with_help_message("↑↓ navigate • type to search • Enter to select • Ctrl+C to exit")
        .with_page_size(20)
        .prompt()?;

    // Return the action from the selected option
    Ok(selection.action.clone())
}

pub fn print_success(message: &str) {
    println!("{} {}", style("✔").bold().green(), message);
}

pub fn print_warning(message: &str) {
    println!("{} {}", style("⚠").bold().yellow(), message);
}

pub fn print_error(message: &str) {
    println!("{} {}", style("✘").bold().red(), message);
}

pub fn print_info(message: &str) {
    println!("{} {}", style("ℹ").bold().blue(), message);
}

pub fn print_train_header(title: &str) {
    let term = Term::stdout();
    let width = term.size().1 as usize;
    let border_width = width.min(80);
    let border = "═".repeat(border_width);

    println!("{}", style(&border).bold().cyan());
    
    // Center the title with train symbols
    let title_content = format!(" ▶ {} ◀ ", title);
    let padding = if border_width > title_content.len() {
        (border_width - title_content.len()) / 2
    } else {
        0
    };
    let left_pad = " ".repeat(padding);
    let right_pad = " ".repeat(border_width.saturating_sub(title_content.len() + padding));
    
    println!(
        "{}{}{}{}{}",
        style("║").bold().cyan(),
        left_pad,
        style(&title_content).bold().white(),
        right_pad,
        style("║").bold().cyan()
    );
    println!("{}", style(&border).bold().cyan());
}

pub fn sanitize_branch_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
            ' ' => '-',
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_lowercase()
}

pub fn run_git_command(args: &[&str]) -> Result<String> {
    info!("Running git command: git {}", args.join(" "));

    let output = Command::new("git").args(args).output()?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error!("Git command failed: {}", stderr);
        Err(anyhow::anyhow!("Git command failed: {}", stderr))
    }
}

pub fn get_current_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

pub fn create_backup_name(prefix: &str) -> String {
    format!("{}_backup_{}", prefix, get_current_timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_branch_name() {
        assert_eq!(sanitize_branch_name("Feature Branch"), "feature-branch");
        assert_eq!(sanitize_branch_name("fix/bug#123"), "fix_bug_123");
        assert_eq!(sanitize_branch_name("--start--"), "start");
    }
}
