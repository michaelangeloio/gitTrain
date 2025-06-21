use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tracing::{info, warn, error};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::utils::{
    run_git_command, print_success, print_warning, print_error, print_info, 
    print_train_header, sanitize_branch_name, confirm_action, get_user_input,
    create_backup_name
};
use crate::gitlab::{GitLabClient, CreateMergeRequestRequest, GitLabProject};
use crate::errors::TrainError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackBranch {
    pub name: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub commit_hash: String,
    pub mr_iid: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stack {
    pub id: String,
    pub name: String,
    pub base_branch: String,
    pub branches: HashMap<String, StackBranch>,
    pub current_branch: Option<String>,
    pub gitlab_project: Option<GitLabProject>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct StackManager {
    git_dir: PathBuf,
    train_dir: PathBuf,
    current_stack: Option<Stack>,
    gitlab_client: Option<GitLabClient>,
}

impl StackManager {
    pub async fn new() -> Result<Self> {
        let git_dir = Self::find_git_dir()?;
        let train_dir = git_dir.join("train");
        
        // Create train directory if it doesn't exist
        if !train_dir.exists() {
            fs::create_dir_all(&train_dir)?;
            info!("Created train directory: {:?}", train_dir);
        }

        // Try to initialize GitLab client
        let gitlab_client = match GitLabClient::new().await {
            Ok(client) => {
                print_info("GitLab integration initialized");
                Some(client)
            }
            Err(e) => {
                print_warning(&format!("GitLab integration not available: {}", e));
                None
            }
        };

        Ok(Self {
            git_dir,
            train_dir,
            current_stack: None,
            gitlab_client,
        })
    }

    fn find_git_dir() -> Result<PathBuf> {
        let output = run_git_command(&["rev-parse", "--git-dir"])?;
        let git_dir = PathBuf::from(output.trim());
        
        if !git_dir.exists() {
            return Err(TrainError::GitError {
                message: "Not in a git repository".to_string(),
            }.into());
        }
        
        Ok(git_dir.canonicalize()?)
    }

    pub async fn create_stack(&mut self, name: &str) -> Result<()> {
        print_train_header(&format!("Creating Stack: {}", name));

        // Ensure we're on a clean working directory
        self.ensure_clean_working_directory()?;

        let current_branch = self.get_current_branch()?;
        let current_commit = self.get_current_commit_hash()?;
        let base_branch = self.determine_base_branch(&current_branch)?;

        let sanitized_name = sanitize_branch_name(name);
        let stack_id = Uuid::new_v4().to_string();

        // Get GitLab project information if available
        let gitlab_project = if let Some(gitlab_client) = &mut self.gitlab_client {
            print_info("Detecting GitLab project...");
            match gitlab_client.detect_and_cache_project().await {
                Ok(project) => {
                    print_success(&format!("Detected GitLab project: {}/{}", 
                        project.namespace.path, project.path));
                    print_info(&format!("Project URL: {}", project.web_url));
                    Some(project.clone())
                }
                Err(e) => {
                    print_warning(&format!("GitLab project could not be auto-detected: {}", e));
                    None
                }
            }
        } else {
            None
        };

        // Create the stack structure
        let mut stack = Stack {
            id: stack_id.clone(),
            name: sanitized_name.clone(),
            base_branch: base_branch.clone(),
            branches: HashMap::new(),
            current_branch: Some(current_branch.clone()),
            gitlab_project,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // Add the current branch to the stack
        let branch = StackBranch {
            name: current_branch.clone(),
            parent: Some(base_branch.clone()),
            children: vec![],
            commit_hash: current_commit,
            mr_iid: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        stack.branches.insert(current_branch.clone(), branch);

        // Save the stack
        self.save_stack_state(&stack)?;
        self.current_stack = Some(stack);

        print_success(&format!("Created stack '{}' with base branch '{}'", sanitized_name, base_branch));
        print_info(&format!("Current branch '{}' added to stack", current_branch));

        Ok(())
    }

    pub async fn save_changes(&mut self, message: &str) -> Result<()> {
        print_train_header("Saving Changes");

        let stack = self.load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Ensure the current branch is part of the stack
        if !stack.branches.contains_key(&current_branch) {
            return Err(TrainError::StackError {
                message: format!("Branch '{}' is not part of the current stack", current_branch),
            }.into());
        }

        // Check if there are changes to commit
        if !self.has_uncommitted_changes()? {
            print_info("No changes to commit");
            return Ok(());
        }

        // Create a backup before making changes
        let backup_branch = create_backup_name(&current_branch);
        run_git_command(&["branch", &backup_branch])?;
        print_info(&format!("Created backup branch: {}", backup_branch));

        // Commit the changes
        run_git_command(&["add", "."])?;
        run_git_command(&["commit", "-m", message])?;

        let new_commit_hash = self.get_current_commit_hash()?;
        print_success(&format!("Committed changes: {}", &new_commit_hash[..8]));

        // Update the stack state
        let mut updated_stack = stack.clone();
        if let Some(branch) = updated_stack.branches.get_mut(&current_branch) {
            branch.commit_hash = new_commit_hash;
            branch.updated_at = Utc::now();
        }
        updated_stack.updated_at = Utc::now();

        // Propagate changes to dependent branches
        self.propagate_changes(&mut updated_stack, &current_branch).await?;

        // Save the updated stack
        self.save_stack_state(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Changes saved and propagated to dependent branches");

        Ok(())
    }

    pub async fn amend_changes(&mut self, new_message: Option<&str>) -> Result<()> {
        print_train_header("Amending Changes");

        let stack = self.load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Ensure the current branch is part of the stack
        if !stack.branches.contains_key(&current_branch) {
            return Err(TrainError::StackError {
                message: format!("Branch '{}' is not part of the current stack", current_branch),
            }.into());
        }

        // Create a backup before making changes
        let backup_branch = create_backup_name(&current_branch);
        run_git_command(&["branch", &backup_branch])?;
        print_info(&format!("Created backup branch: {}", backup_branch));

        // Amend the current commit
        if let Some(message) = new_message {
            // Amend with new message
            run_git_command(&["commit", "--amend", "-m", message])?;
            print_success(&format!("Amended commit with new message: {}", message));
        } else {
            // Check if there are staged changes to amend
            let staged_output = run_git_command(&["diff", "--cached", "--name-only"])?;
            if staged_output.trim().is_empty() {
                // No staged changes, just amend message
                run_git_command(&["commit", "--amend", "--no-edit"])?;
                print_success("Amended commit (no changes)");
            } else {
                // Stage all changes and amend
                run_git_command(&["add", "."])?;
                run_git_command(&["commit", "--amend", "--no-edit"])?;
                print_success("Amended commit with staged changes");
            }
        }

        let new_commit_hash = self.get_current_commit_hash()?;
        print_success(&format!("New commit hash: {}", &new_commit_hash[..8]));

        // Update the stack state
        let mut updated_stack = stack.clone();
        if let Some(branch) = updated_stack.branches.get_mut(&current_branch) {
            branch.commit_hash = new_commit_hash;
            branch.updated_at = Utc::now();
        }
        updated_stack.updated_at = Utc::now();

        // Propagate changes to dependent branches (resync downstream)
        print_info("Resyncing downstream branches...");
        self.propagate_changes(&mut updated_stack, &current_branch).await?;

        // Save the updated stack
        self.save_stack_state(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Changes amended and downstream branches resynced");

        Ok(())
    }

    pub async fn add_branch_to_stack(&mut self, parent: Option<&str>) -> Result<()> {
        print_train_header("Adding Branch to Stack");

        let mut stack = self.load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Check if branch is already in the stack
        if stack.branches.contains_key(&current_branch) {
            print_warning(&format!("Branch '{}' is already part of the stack", current_branch));
            return Ok(());
        }

        // Ensure we're on a clean working directory
        self.ensure_clean_working_directory()?;

        let current_commit = self.get_current_commit_hash()?;
        
        // Determine the parent branch
        let parent_branch = if let Some(parent) = parent {
            if !stack.branches.contains_key(parent) && parent != stack.base_branch {
                return Err(TrainError::StackError {
                    message: format!("Parent branch '{}' is not part of the stack", parent),
                }.into());
            }
            parent.to_string()
        } else {
            stack.base_branch.clone()
        };

        // Add the branch to the stack
        let branch = StackBranch {
            name: current_branch.clone(),
            parent: Some(parent_branch.clone()),
            children: vec![],
            commit_hash: current_commit,
            mr_iid: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        stack.branches.insert(current_branch.clone(), branch);
        stack.updated_at = Utc::now();

        // Save the updated stack
        self.save_stack_state(&stack)?;
        self.current_stack = Some(stack);

        print_success(&format!("Added branch '{}' to stack with parent '{}'", current_branch, parent_branch));

        Ok(())
    }

    pub async fn list_stacks(&self) -> Result<()> {
        print_train_header("Available Stacks");

        let stack_files = std::fs::read_dir(&self.train_dir)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()? == "json" && path.file_name()? != "current.json" {
                    Some(path)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if stack_files.is_empty() {
            print_info("No stacks found");
            return Ok(());
        }

        let current_stack_id = std::fs::read_to_string(self.train_dir.join("current.json"))
            .unwrap_or_default()
            .trim()
            .to_string();

        for stack_file in stack_files {
            if let Ok(stack_json) = std::fs::read_to_string(&stack_file) {
                if let Ok(stack) = serde_json::from_str::<Stack>(&stack_json) {
                    let is_current = if current_stack_id == stack.id { " (current)" } else { "" };
                    let project_info = if let Some(project) = &stack.gitlab_project {
                        format!(" | Project: {}/{}", project.namespace.path, project.path)
                    } else {
                        String::new()
                    };
                    
                    println!("📋 {} ({}){}", stack.name, &stack.id[..8], is_current);
                    println!("   └─ Base: {} | Branches: {} | Updated: {}{}", 
                        stack.base_branch, 
                        stack.branches.len(),
                        stack.updated_at.format("%Y-%m-%d %H:%M"),
                        project_info
                    );
                }
            }
        }

        Ok(())
    }

    pub async fn switch_stack(&mut self, stack_identifier: &str) -> Result<()> {
        print_train_header(&format!("Switching to Stack: {}", stack_identifier));

        // Find the stack by name or ID
        let stack_files = std::fs::read_dir(&self.train_dir)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()? == "json" && path.file_name()? != "current.json" {
                    Some(path)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut target_stack: Option<Stack> = None;

        for stack_file in stack_files {
            if let Ok(stack_json) = std::fs::read_to_string(&stack_file) {
                if let Ok(stack) = serde_json::from_str::<Stack>(&stack_json) {
                    if stack.name == stack_identifier || stack.id.starts_with(stack_identifier) {
                        target_stack = Some(stack);
                        break;
                    }
                }
            }
        }

        let stack = target_stack.ok_or_else(|| {
            TrainError::StackError {
                message: format!("Stack '{}' not found", stack_identifier),
            }
        })?;

        // Update the current stack pointer
        let current_file = self.train_dir.join("current.json");
        std::fs::write(&current_file, &stack.id)?;

        self.current_stack = Some(stack.clone());

        print_success(&format!("Switched to stack '{}' ({})", stack.name, &stack.id[..8]));
        
        // Show status of the new stack
        self.show_status().await?;

        Ok(())
    }

    pub async fn delete_stack(&mut self, stack_identifier: &str, force: bool) -> Result<()> {
        print_train_header(&format!("Deleting Stack: {}", stack_identifier));

        // Find the stack by name or ID
        let stack_files = std::fs::read_dir(&self.train_dir)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()? == "json" && path.file_name()? != "current.json" {
                    Some(path)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let mut target_stack: Option<(Stack, PathBuf)> = None;

        for stack_file in stack_files {
            if let Ok(stack_json) = std::fs::read_to_string(&stack_file) {
                if let Ok(stack) = serde_json::from_str::<Stack>(&stack_json) {
                    if stack.name == stack_identifier || stack.id.starts_with(stack_identifier) {
                        target_stack = Some((stack, stack_file));
                        break;
                    }
                }
            }
        }

        let (stack, stack_file) = target_stack.ok_or_else(|| {
            TrainError::StackError {
                message: format!("Stack '{}' not found", stack_identifier),
            }
        })?;

        // Check if this is the current stack
        let current_file = self.train_dir.join("current.json");
        let is_current_stack = if let Ok(current_stack_id) = std::fs::read_to_string(&current_file) {
            current_stack_id.trim() == stack.id
        } else {
            false
        };

        // Show what will be deleted
        print_warning(&format!("This will permanently delete stack '{}' ({})", stack.name, &stack.id[..8]));
        print_info(&format!("Stack contains {} branches:", stack.branches.len()));
        for branch_name in stack.branches.keys() {
            println!("  - {}", branch_name);
        }

        if let Some(project) = &stack.gitlab_project {
            print_info(&format!("Associated with GitLab project: {}/{}", 
                project.namespace.path, project.path));
        }

        if is_current_stack {
            print_warning("This is the current active stack");
        }

        // Confirm deletion unless forced
        if !force {
            print_warning("Are you sure you want to delete this stack? This action cannot be undone.");
            let confirmed = get_user_input("Type 'yes' to confirm deletion", None)?;
            if confirmed.to_lowercase() != "yes" {
                print_info("Stack deletion cancelled");
                return Ok(());
            }
        }

        // Delete the stack file
        std::fs::remove_file(&stack_file)?;
        print_success(&format!("Deleted stack file: {:?}", stack_file));

        // If this was the current stack, clear the current stack reference
        if is_current_stack {
            if current_file.exists() {
                std::fs::remove_file(&current_file)?;
            }
            self.current_stack = None;
            print_info("Cleared current stack reference");
        }

        print_success(&format!("Stack '{}' has been deleted", stack.name));
        print_info("Note: Git branches were not deleted. You may want to clean them up manually if needed.");

        Ok(())
    }

    pub async fn show_status(&mut self) -> Result<()> {
        print_train_header("Stack Status");

        let stack = match &self.current_stack {
            Some(stack) => stack,
            None => &{
                // Try to load existing stack
                match self.load_current_stack() {
                    Ok(stack) => {
                        self.current_stack = Some(stack.clone());
                        stack
                    }
                    Err(_) => {
                        print_warning("No active stack found");
                        return Ok(());
                    }
                }
            }
        };

        println!("Stack: {} ({})", stack.name, &stack.id[..8]);
        println!("Base branch: {}", stack.base_branch);
        
        if let Some(project) = &stack.gitlab_project {
            println!("GitLab project: {}/{} (ID: {})", 
                project.namespace.path, project.path, project.id);
            println!("Project URL: {}", project.web_url);
        }
        
        println!("Created: {}", stack.created_at.format("%Y-%m-%d %H:%M:%S UTC"));
        println!("Updated: {}", stack.updated_at.format("%Y-%m-%d %H:%M:%S UTC"));
        println!();

        // Build branch hierarchy
        let hierarchy = self.build_branch_hierarchy(stack);
        self.print_branch_hierarchy(&hierarchy, stack, 0);

        // Show working directory status
        let status_output = run_git_command(&["status", "--porcelain"])?;
        if !status_output.is_empty() {
            println!("\nWorking directory status:");
            println!("{}", status_output);
        }

        Ok(())
    }

    pub async fn push_stack(&mut self) -> Result<()> {
        print_train_header("Pushing Stack");

        let stack = self.load_current_stack()?;

        // Push all branches in the stack
        for (branch_name, branch) in &stack.branches {
            print_info(&format!("Pushing branch: {}", branch_name));
            
            match run_git_command(&["push", "origin", &format!("{}:{}", branch_name, branch_name)]) {
                Ok(_) => print_success(&format!("Pushed {}", branch_name)),
                Err(e) => {
                    print_error(&format!("Failed to push {}: {}", branch_name, e));
                    continue;
                }
            }

                    // Create or update merge request if GitLab client is available
        if self.gitlab_client.is_some() {
            // Ensure project is detected and cached
            if let Some(gitlab_client) = &mut self.gitlab_client {
                if gitlab_client.get_project_details().is_none() {
                    let _ = gitlab_client.detect_and_cache_project().await;
                }
            }
            
            // Now use immutable reference for API calls
            if let Some(gitlab_client) = &self.gitlab_client {
                match self.create_or_update_mr(gitlab_client, branch_name, branch, &stack).await {
                    Ok(_) => print_success(&format!("Updated merge request for {}", branch_name)),
                    Err(e) => print_warning(&format!("Failed to update MR for {}: {}", branch_name, e)),
                }
            }
        }
        }

        print_success("Stack pushed to remote");

        Ok(())
    }

    pub async fn sync_with_remote(&mut self) -> Result<()> {
        print_train_header("Syncing with Remote");

        let stack = self.load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Ensure working directory is clean
        self.ensure_clean_working_directory()?;

        // Update the base branch
        print_info(&format!("Updating base branch: {}", stack.base_branch));
        run_git_command(&["checkout", &stack.base_branch])?;
        run_git_command(&["pull", "origin", &stack.base_branch])?;

        // Rebase all stack branches
        let mut updated_stack = stack.clone();
        let hierarchy = self.build_branch_hierarchy(&stack);
        
        self.rebase_branch_hierarchy(&mut updated_stack, &hierarchy, &stack.base_branch).await?;

        // Switch back to the original branch
        run_git_command(&["checkout", &current_branch])?;

        // Save the updated stack
        self.save_stack_state(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Stack synchronized with remote");

        Ok(())
    }

    fn get_current_branch(&self) -> Result<String> {
        run_git_command(&["branch", "--show-current"])
    }

    fn get_current_commit_hash(&self) -> Result<String> {
        run_git_command(&["rev-parse", "HEAD"])
    }

    fn has_uncommitted_changes(&self) -> Result<bool> {
        let output = run_git_command(&["status", "--porcelain"])?;
        Ok(!output.trim().is_empty())
    }

    fn ensure_clean_working_directory(&self) -> Result<()> {
        if self.has_uncommitted_changes()? {
            return Err(TrainError::StackError {
                message: "Working directory is not clean. Please commit or stash your changes first.".to_string(),
            }.into());
        }
        Ok(())
    }

    fn determine_base_branch(&self, current_branch: &str) -> Result<String> {
        // Try to determine the base branch by checking common base branches
        let potential_bases = ["main", "master", "develop", "dev"];
        
        for base in &potential_bases {
            if let Ok(_) = run_git_command(&["merge-base", current_branch, base]) {
                return Ok(base.to_string());
            }
        }

        // If no common base found, ask the user
        let base = get_user_input("Enter base branch name", Some("main"))?;
        Ok(base)
    }

    async fn propagate_changes(&self, stack: &mut Stack, changed_branch: &str) -> Result<()> {
        let hierarchy = self.build_branch_hierarchy(stack);
        
        if let Some(children) = hierarchy.get(changed_branch) {
            for child_branch in children {
                print_info(&format!("Propagating changes to: {}", child_branch));
                
                // Checkout the child branch
                run_git_command(&["checkout", child_branch])?;
                
                // Rebase onto the changed branch
                match run_git_command(&["rebase", changed_branch]) {
                    Ok(_) => {
                        let new_commit = self.get_current_commit_hash()?;
                        if let Some(branch) = stack.branches.get_mut(child_branch) {
                            branch.commit_hash = new_commit;
                            branch.updated_at = Utc::now();
                        }
                        print_success(&format!("Rebased {} onto {}", child_branch, changed_branch));
                    }
                    Err(e) => {
                        print_error(&format!("Failed to rebase {}: {}", child_branch, e));
                        // Continue with other branches
                    }
                }
                
                // Recursively propagate to grandchildren
                Box::pin(self.propagate_changes(stack, child_branch)).await?;
            }
        }

        Ok(())
    }

    fn build_branch_hierarchy(&self, stack: &Stack) -> HashMap<String, Vec<String>> {
        let mut hierarchy: HashMap<String, Vec<String>> = HashMap::new();
        
        for (branch_name, branch) in &stack.branches {
            if let Some(parent) = &branch.parent {
                hierarchy.entry(parent.clone())
                    .or_insert_with(Vec::new)
                    .push(branch_name.clone());
            }
        }

        hierarchy
    }

    fn print_branch_hierarchy(&self, hierarchy: &HashMap<String, Vec<String>>, stack: &Stack, indent: usize) {
        let indent_str = "  ".repeat(indent);
        
        for (branch_name, branch) in &stack.branches {
            if branch.parent.is_none() || (indent == 0 && branch.parent.as_ref() == Some(&stack.base_branch)) {
                let status = if Some(branch_name) == stack.current_branch.as_ref() { " (current)" } else { "" };
                let mr_info = if let Some(iid) = branch.mr_iid { 
                    format!(" [MR !{}]", iid) 
                } else { 
                    String::new() 
                };
                
                println!("{}📋 {}{}{}", indent_str, branch_name, status, mr_info);
                println!("{}   └─ {}", indent_str, &branch.commit_hash[..8]);
                
                if let Some(children) = hierarchy.get(branch_name) {
                    for child in children {
                        self.print_branch_details(child, stack, indent + 1);
                        self.print_children_recursive(hierarchy, stack, child, indent + 1);
                    }
                }
            }
        }
    }

    fn print_branch_details(&self, branch_name: &str, stack: &Stack, indent: usize) {
        let indent_str = "  ".repeat(indent);
        
        if let Some(branch) = stack.branches.get(branch_name) {
            let status = if Some(branch_name) == stack.current_branch.as_deref() { " (current)" } else { "" };
            let mr_info = if let Some(iid) = branch.mr_iid { 
                format!(" [MR !{}]", iid) 
            } else { 
                String::new() 
            };
            
            println!("{}├─ {}{}{}", indent_str, branch_name, status, mr_info);
            println!("{}│  └─ {}", indent_str, &branch.commit_hash[..8]);
        }
    }

    fn print_children_recursive(&self, hierarchy: &HashMap<String, Vec<String>>, stack: &Stack, branch_name: &str, indent: usize) {
        if let Some(children) = hierarchy.get(branch_name) {
            for child in children {
                self.print_branch_details(child, stack, indent + 1);
                self.print_children_recursive(hierarchy, stack, child, indent + 1);
            }
        }
    }

    async fn rebase_branch_hierarchy(&self, stack: &mut Stack, hierarchy: &HashMap<String, Vec<String>>, base_branch: &str) -> Result<()> {
        // Rebase branches in order of dependency
        if let Some(children) = hierarchy.get(base_branch) {
            for child in children {
                print_info(&format!("Rebasing {} onto {}", child, base_branch));
                
                run_git_command(&["checkout", child])?;
                match run_git_command(&["rebase", base_branch]) {
                    Ok(_) => {
                        let new_commit = self.get_current_commit_hash()?;
                        if let Some(branch) = stack.branches.get_mut(child) {
                            branch.commit_hash = new_commit;
                            branch.updated_at = Utc::now();
                        }
                        print_success(&format!("Rebased {}", child));
                    }
                    Err(e) => {
                        print_error(&format!("Failed to rebase {}: {}", child, e));
                    }
                }
                
                // Recursively rebase children
                Box::pin(self.rebase_branch_hierarchy(stack, hierarchy, child)).await?;
            }
        }

        Ok(())
    }

    async fn create_or_update_mr(&self, gitlab_client: &GitLabClient, branch_name: &str, branch: &StackBranch, stack: &Stack) -> Result<()> {
        let parent_branch = branch.parent.as_deref().unwrap_or(&stack.base_branch);
        
        let title = format!("[Stack: {}] {}", stack.name, branch_name);
        let description = Some(format!(
            "Part of stack: {}\n\nBase branch: {}\nParent branch: {}\n\nStack ID: {}",
            stack.name, stack.base_branch, parent_branch, stack.id
        ));

        if branch.mr_iid.is_none() {
            // Create new MR
            let request = CreateMergeRequestRequest {
                source_branch: branch_name.to_string(),
                target_branch: parent_branch.to_string(),
                title,
                description,
            };

            match gitlab_client.create_merge_request(request).await {
                Ok(mr) => {
                    print_success(&format!("Created MR !{} for branch {}", mr.iid, branch_name));
                }
                Err(e) => {
                    print_warning(&format!("Failed to create MR for {}: {}", branch_name, e));
                }
            }
        } else {
            // Update existing MR
            let iid = branch.mr_iid.unwrap();
            match gitlab_client.update_merge_request(iid, Some(title), description).await {
                Ok(_) => {
                    print_success(&format!("Updated MR !{} for branch {}", iid, branch_name));
                }
                Err(e) => {
                    print_warning(&format!("Failed to update MR !{} for {}: {}", iid, branch_name, e));
                }
            }
        }

        Ok(())
    }

    fn save_stack_state(&self, stack: &Stack) -> Result<()> {
        let stack_file = self.train_dir.join(format!("{}.json", stack.id));
        let stack_json = serde_json::to_string_pretty(stack)?;
        
        fs::write(&stack_file, stack_json)?;
        
        // Also save a "current" symlink/file for easy access
        let current_file = self.train_dir.join("current.json");
        fs::write(&current_file, &stack.id)?;
        
        info!("Saved stack state to: {:?}", stack_file);
        Ok(())
    }

    fn load_current_stack(&self) -> Result<Stack> {
        let current_file = self.train_dir.join("current.json");
        if !current_file.exists() {
            return Err(TrainError::StackError {
                message: "No current stack found".to_string(),
            }.into());
        }

        let stack_id = fs::read_to_string(&current_file)?;
        let stack_file = self.train_dir.join(format!("{}.json", stack_id.trim()));
        
        if !stack_file.exists() {
            return Err(TrainError::StackError {
                message: format!("Stack file not found: {:?}", stack_file),
            }.into());
        }

        let stack_json = fs::read_to_string(&stack_file)?;
        let stack: Stack = serde_json::from_str(&stack_json)?;
        
        Ok(stack)
    }
} 