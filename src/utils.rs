use anyhow::Result;
use console::{style, Term};
use dialoguer::{Confirm, Select, Editor, Input};
use std::process::Command;
use tracing::{info, warn, error};

pub fn confirm_action(message: &str) -> Result<bool> {
    let confirmation = Confirm::new()
        .with_prompt(message)
        .default(false)
        .interact()?;
    
    Ok(confirmation)
}

pub fn select_from_list<T: ToString>(items: &[T], prompt: &str) -> Result<usize> {
    let selection = Select::new()
        .with_prompt(prompt)
        .items(items)
        .default(0)
        .interact()?;
    
    Ok(selection)
}

pub fn get_user_input(prompt: &str, default: Option<&str>) -> Result<String> {
    let mut input = Input::new()
        .with_prompt(prompt);
    
    if let Some(default_value) = default {
        input = input.default(default_value.to_string());
    }
    
    let result = input.interact_text()?;
    Ok(result)
}

pub fn open_editor(initial_content: Option<&str>) -> Result<String> {
    let content = Editor::new()
        .edit(initial_content.unwrap_or(""))? 
        .unwrap_or_default();
    
    Ok(content)
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