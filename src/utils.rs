use anyhow::Result;
use console::{style, Term};
use inquire::{Confirm, Select, Text, Editor};
use std::process::Command;
use tracing::{info, warn, error};

pub fn confirm_action(message: &str) -> Result<bool> {
    let confirmation = Confirm::new(message)
        .with_default(false)
        .prompt()?;
    
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

pub fn fuzzy_select_from_list<T: ToString + Clone>(items: &[T], prompt: &str) -> Result<usize> {
    // The regular Select prompt already has fuzzy filtering enabled by default
    select_from_list(items, prompt)
}

pub fn get_user_input(prompt: &str, default: Option<&str>) -> Result<String> {
    let mut input = Text::new(prompt);
    
    if let Some(default_value) = default {
        input = input.with_default(default_value);
    }
    
    let result = input.prompt()?;
    Ok(result)
}

pub fn open_editor(initial_content: Option<&str>) -> Result<String> {
    let content = Editor::new("Enter your text:")
        .with_predefined_text(initial_content.unwrap_or(""))
        .prompt()?;
    
    Ok(content)
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
    branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>
) -> Vec<NavigationOption> {
    let mut options = Vec::new();
    
    // Add branch navigation options
    for branch in branches {
        let is_current = current_branch.map_or(false, |current| current == branch);
        let mr_info = if let Some(mr_status) = branch_mr_status.get(branch) {
            let (status_icon, status_text, priority_indicator) = match mr_status.state.as_str() {
                "merged" => ("✅", "MERGED".to_string(), " 🎉"),
                "closed" => ("❌", "CLOSED".to_string(), ""),
                "opened" => ("🔄", "OPEN".to_string(), " 🚀"),
                _ => ("❓", mr_status.state.to_uppercase(), ""),
            };
            format!(" [MR !{} {} {}{}]", mr_status.iid, status_icon, status_text, priority_indicator)
        } else {
            String::new()
        };
        
        let display = if is_current {
            format!("🔸 {} (current){}", branch, mr_info)
        } else {
            format!("📋 {}{}", branch, mr_info)
        };
        
        options.push(NavigationOption {
            display: display.clone(),
            action: NavigationAction::SwitchToBranch(branch.clone()),
        });
        
        // Add additional actions for each branch
        options.push(NavigationOption {
            display: format!("  ℹ️  Show info for {}", branch),
            action: NavigationAction::ShowBranchInfo(branch.clone()),
        });
        
        if let Some(mr_status) = branch_mr_status.get(branch) {
            options.push(NavigationOption {
                display: format!("  🔗 View MR !{} for {}", mr_status.iid, branch),
                action: NavigationAction::ViewMR(branch.clone(), mr_status.iid),
            });
        } else {
            options.push(NavigationOption {
                display: format!("  ➕ Create MR for {}", branch),
                action: NavigationAction::CreateMR(branch.clone()),
            });
        }
    }
    
    // Add utility options
    options.push(NavigationOption {
        display: "🔄 Refresh status".to_string(),
        action: NavigationAction::RefreshStatus,
    });
    
    options.push(NavigationOption {
        display: "❌ Exit navigation".to_string(),
        action: NavigationAction::Exit,
    });
    
    options
}

pub fn interactive_stack_navigation(
    options: &[NavigationOption],
    prompt: &str
) -> Result<NavigationAction> {
    let selection = Select::new(prompt, options.to_vec())
        .with_help_message("Use arrows to navigate, type to search, Enter to select")
        .with_page_size(15)
        .prompt()?;
    
    // Return the action from the selected option
    Ok(selection.action.clone())
}

pub fn print_success(message: &str) {
    println!("{} {}", style("✅").bold().green(), message);
}

pub fn print_warning(message: &str) {
    println!("{} {}", style("⚠️ ").bold().yellow(), message);
}

pub fn print_error(message: &str) {
    println!("{} {}", style("❌").bold().red(), message);
}

pub fn print_info(message: &str) {
    println!("{} {}", style("ℹ️ ").bold().blue(), message);
}

pub fn print_train_header(title: &str) {
    let term = Term::stdout();
    let width = term.size().1 as usize;
    let border = "═".repeat(width.min(80));
    
    println!("{}", style(&border).bold().cyan());
    println!("{} 🚂 {} 🚂 {}", 
        style("║").bold().cyan(), 
        style(title).bold().white(),
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

pub fn is_valid_git_ref(name: &str) -> bool {
    // Basic Git ref validation
    !name.is_empty() &&
    !name.starts_with('/') &&
    !name.ends_with('/') &&
    !name.contains("..") &&
    !name.contains(' ') &&
    !name.contains('\t') &&
    !name.contains('\n') &&
    !name.contains('\r') &&
    !name.contains('~') &&
    !name.contains('^') &&
    !name.contains(':') &&
    !name.contains('?') &&
    !name.contains('*') &&
    !name.contains('[')
}

pub fn run_git_command(args: &[&str]) -> Result<String> {
    info!("Running git command: git {}", args.join(" "));
    
    let output = Command::new("git")
        .args(args)
        .output()?;
    
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

    #[test]
    fn test_is_valid_git_ref() {
        assert!(is_valid_git_ref("feature-branch"));
        assert!(is_valid_git_ref("fix_bug_123"));
        assert!(!is_valid_git_ref(""));
        assert!(!is_valid_git_ref("/invalid"));
        assert!(!is_valid_git_ref("invalid/"));
        assert!(!is_valid_git_ref("invalid..ref"));
        assert!(!is_valid_git_ref("invalid ref"));
    }
} 