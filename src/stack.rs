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
    create_backup_name, NavigationAction, create_navigation_options, interactive_stack_navigation, MrStatusInfo
};
use crate::gitlab::{GitLabClient, CreateMergeRequestRequest, GitLabProject};
use crate::errors::TrainError;
use crate::config::TrainConfig;
use crate::conflict::{ConflictResolver, GitState};

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
    config: TrainConfig,
    conflict_resolver: ConflictResolver,
}

impl StackManager {
    pub async fn new() -> Result<Self> {
        let config = TrainConfig::default();
        Self::new_with_config(config).await
    }

    pub async fn new_with_config(config: TrainConfig) -> Result<Self> {
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

        // Initialize conflict resolver
        let conflict_resolver = ConflictResolver::new(config.clone(), git_dir.clone());

        Ok(Self {
            git_dir,
            train_dir,
            current_stack: None,
            gitlab_client,
            config,
            conflict_resolver,
        })
    }

    pub fn get_conflict_resolver(&self) -> &ConflictResolver {
        &self.conflict_resolver
    }

    /// Smart rebase that handles conflicts automatically when possible
    async fn smart_rebase(&self, branch: &str, onto: &str) -> Result<()> {
        // First check if we're already in a conflict state
        let git_state = self.conflict_resolver.get_git_state()?;
        if !matches!(git_state, GitState::Clean) {
            return Err(TrainError::InvalidState {
                message: format!("Cannot rebase: git is in state {:?}", git_state),
            }.into());
        }

        // Create backup if configured
        if self.config.conflict_resolution.backup_on_conflict {
            let backup_branch = create_backup_name(branch);
            run_git_command(&["branch", &backup_branch])?;
            print_info(&format!("Created backup branch: {}", backup_branch));
        }

        // Attempt the rebase
        match run_git_command(&["rebase", onto]) {
            Ok(_) => {
                print_success(&format!("Rebased {} onto {} successfully", branch, onto));
                Ok(())
            }
            Err(_) => {
                // Check if we have conflicts
                if let Some(conflicts) = self.conflict_resolver.detect_conflicts()? {
                    print_info(&format!("Conflicts detected during rebase of {} onto {}", branch, onto));
                    
                    // Try automatic resolution first
                    if self.conflict_resolver.auto_resolve_conflicts(&conflicts).await? {
                        // Continue the rebase
                        run_git_command(&["rebase", "--continue"])?;
                        print_success(&format!("Auto-resolved conflicts and completed rebase"));
                        Ok(())
                    } else {
                        // Fall back to interactive resolution
                        match self.config.conflict_resolution.auto_resolve_strategy {
                            crate::config::AutoResolveStrategy::Never => {
                                print_warning("Auto-resolution disabled. Please resolve conflicts manually:");
                                print_info(&format!("Run 'git-train resolve interactive' to resolve conflicts"));
                                print_info(&format!("Then run 'git-train resolve continue' to complete the rebase"));
                                Err(TrainError::InvalidState {
                                    message: "Manual conflict resolution required".to_string(),
                                }.into())
                            }
                            _ => {
                                // Offer interactive resolution
                                self.conflict_resolver.resolve_conflicts_interactively(&conflicts).await?;
                                Ok(())
                            }
                        }
                    }
                } else {
                    // Rebase failed for other reasons
                    Err(TrainError::GitError {
                        message: format!("Rebase of {} onto {} failed", branch, onto),
                    }.into())
                }
            }
        }
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

    /// Intelligently detect the best parent branch by analyzing git history
    async fn detect_smart_parent(&self, current_branch: &str, stack: &Stack) -> Result<String> {
        // Get the commits in the current branch that are not in the base branch
        let commits_output = run_git_command(&[
            "rev-list", 
            &format!("{}..{}", stack.base_branch, current_branch),
            "--reverse"
        ])?;
        
        let commits: Vec<&str> = commits_output.trim().lines().collect();
        
        if commits.is_empty() {
            // No commits beyond base branch, parent should be base branch
            return Ok(stack.base_branch.clone());
        }
        
        // Check each stack branch to see which one contains the most commits from our branch
        let mut best_parent = stack.base_branch.clone();
        let mut max_shared_commits = 0;
        
        for (branch_name, branch) in &stack.branches {
            // Get commits in this stack branch
            let branch_commits_output = run_git_command(&[
                "rev-list",
                &format!("{}..{}", stack.base_branch, branch_name)
            ])?;
            
            let branch_commits: std::collections::HashSet<&str> = branch_commits_output
                .trim()
                .lines()
                .collect();
            
            // Count how many of our commits are in this branch
            let shared_commits = commits.iter()
                .filter(|commit| branch_commits.contains(*commit))
                .count();
            
            // If this branch contains more of our commits, it's a better parent candidate
            if shared_commits > max_shared_commits {
                max_shared_commits = shared_commits;
                best_parent = branch_name.clone();
            }
        }
        
        // If we found a stack branch that shares commits, use it
        if max_shared_commits > 0 {
            print_info(&format!("Detected '{}' as parent (shares {} commits)", best_parent, max_shared_commits));
            Ok(best_parent)
        } else {
            // No shared commits with any stack branch, use base branch
            print_info(&format!("No shared commits with stack branches, using base branch '{}'", stack.base_branch));
            Ok(stack.base_branch.clone())
        }
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
            // Smart parent detection based on git history
            self.detect_smart_parent(&current_branch, &stack).await?
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

        let stack = self.find_stack_by_identifier(stack_identifier)?;

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

        let stack = self.find_stack_by_identifier(stack_identifier)?;
        let stack_file = self.train_dir.join(format!("{}.json", stack.id));

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
        
        let stack = self.get_or_load_current_stack()?;

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

        // Build branch hierarchy and collect MR status
        let hierarchy = self.build_branch_hierarchy(&stack);
        let branch_mr_status = self.collect_mr_status_info(&stack).await;
        self.print_branch_hierarchy_with_status(&hierarchy, &stack, &branch_mr_status, 0);

        // Show working directory status
        let status_output = run_git_command(&["status", "--porcelain"])?;
        if !status_output.is_empty() {
            println!("\nWorking directory status:");
            println!("{}", status_output);
        }

        Ok(())
    }

    pub async fn navigate_stack_interactively(&mut self) -> Result<()> {
        use crate::utils::{NavigationAction, create_navigation_options, interactive_stack_navigation, MrStatusInfo};
        
        loop {
            // Load current stack state
            let stack = match self.load_current_stack() {
                Ok(stack) => {
                    self.current_stack = Some(stack.clone());
                    stack
                }
                Err(_) => {
                    print_warning("No active stack found. Please create or switch to a stack first.");
                    return Ok(());
                }
            };

            print_train_header(&format!("Navigate Stack: {}", stack.name));

            // Get current git branch
            let current_git_branch = self.get_current_branch().ok();
            
            // Collect all branches in the stack
            let mut branches: Vec<String> = stack.branches.keys().cloned().collect();
            branches.sort();

            // Collect MR status information (including merge status)
            let branch_mr_status = self.collect_mr_status_info(&stack).await;

            // Create navigation options
            let options = create_navigation_options(
                &branches,
                current_git_branch.as_deref(),
                &branch_mr_status
            );

            // Show interactive menu
            match interactive_stack_navigation(&options, "Select an action:") {
                Ok(action) => {
                    match action {
                        NavigationAction::SwitchToBranch(branch_name) => {
                            if let Err(e) = self.switch_to_branch(&branch_name).await {
                                print_error(&format!("Failed to switch to branch {}: {}", branch_name, e));
                            }
                        }
                        NavigationAction::ShowBranchInfo(branch_name) => {
                            self.show_branch_info(&branch_name, &stack).await;
                        }
                        NavigationAction::CreateMR(branch_name) => {
                            if let Err(e) = self.create_mr_for_branch(&branch_name, &stack).await {
                                print_error(&format!("Failed to create MR for {}: {}", branch_name, e));
                            }
                        }
                        NavigationAction::ViewMR(branch_name, mr_iid) => {
                            self.view_mr_info(&branch_name, mr_iid, &stack).await;
                        }
                        NavigationAction::RefreshStatus => {
                            // Just continue the loop to refresh
                            continue;
                        }
                        NavigationAction::Exit => {
                            print_info("Exiting navigation");
                            break;
                        }
                    }
                }
                Err(e) => {
                    // User cancelled (Ctrl+C or ESC)
                    print_info("Navigation cancelled");
                    break;
                }
            }

            // Add a small pause for better UX
            println!();
        }

        Ok(())
    }

    async fn switch_to_branch(&self, branch_name: &str) -> Result<()> {
        // Ensure working directory is clean
        if let Err(_) = self.ensure_clean_working_directory() {
            print_warning("Working directory is not clean. Stashing changes...");
            run_git_command(&["stash", "push", "-m", "git-train navigation stash"])?;
        }

        // Switch to the branch
        run_git_command(&["checkout", branch_name])?;
        print_success(&format!("Switched to branch: {}", branch_name));
        
        Ok(())
    }

    async fn show_branch_info(&self, branch_name: &str, stack: &Stack) {
        print_train_header(&format!("Branch Info: {}", branch_name));
        
        if let Some(branch) = stack.branches.get(branch_name) {
            println!("Branch: {}", branch.name);
            println!("Parent: {}", branch.parent.as_deref().unwrap_or(&stack.base_branch));
            println!("Commit: {}", &branch.commit_hash[..8]);
            println!("Created: {}", branch.created_at.format("%Y-%m-%d %H:%M:%S UTC"));
            println!("Updated: {}", branch.updated_at.format("%Y-%m-%d %H:%M:%S UTC"));
            
            if let Some(mr_iid) = branch.mr_iid {
                println!("Merge Request: !{}", mr_iid);
                if let Some(project) = &stack.gitlab_project {
                    println!("MR URL: {}/merge_requests/{}", project.web_url, mr_iid);
                }
            } else {
                println!("Merge Request: Not created");
            }

            // Show children if any
            let hierarchy = self.build_branch_hierarchy(stack);
            if let Some(children) = hierarchy.get(branch_name) {
                if !children.is_empty() {
                    println!("Children: {}", children.join(", "));
                }
            }

            // Show commit info
            if let Ok(commit_info) = run_git_command(&["show", "--oneline", "-s", &branch.commit_hash]) {
                println!("Commit info: {}", commit_info);
            }
        } else {
            print_error(&format!("Branch '{}' not found in stack", branch_name));
        }
        
        println!("\nPress Enter to continue...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }

    async fn create_mr_for_branch(&mut self, branch_name: &str, stack: &Stack) -> Result<()> {
        if let Some(gitlab_client) = &self.gitlab_client {
            if let Some(branch) = stack.branches.get(branch_name) {
                let mut stack_mut = stack.clone();
                self.create_or_update_mr_with_smart_targeting_and_store(
                    gitlab_client,
                    branch_name,
                    branch,
                    &mut stack_mut
                ).await?;
                
                // Save the updated stack
                self.save_stack_state(&stack_mut)?;
                self.current_stack = Some(stack_mut);
                
                print_success(&format!("MR creation initiated for branch: {}", branch_name));
            } else {
                print_error(&format!("Branch '{}' not found in stack", branch_name));
            }
        } else {
            print_error("GitLab client not available. Configure GitLab integration first.");
        }
        Ok(())
    }

    async fn view_mr_info(&self, branch_name: &str, mr_iid: u64, stack: &Stack) {
        print_train_header(&format!("MR Info: !{} ({})", mr_iid, branch_name));
        
        if let Some(gitlab_client) = &self.gitlab_client {
            match gitlab_client.get_merge_request(mr_iid).await {
                Ok(mr) => {
                    println!("Title: {}", mr.title);
                    println!("State: {}", mr.state);
                    println!("Source: {}", mr.source_branch);
                    println!("Target: {}", mr.target_branch);
                    println!("ID: {}", mr.id);
                    println!("IID: {}", mr.iid);
                    
                    if let Some(project) = &stack.gitlab_project {
                        println!("URL: {}/merge_requests/{}", project.web_url, mr.iid);
                    }
                    
                    if let Some(description) = &mr.description {
                        if !description.is_empty() {
                            println!("\nDescription:");
                            println!("{}", description);
                        }
                    }
                }
                Err(e) => {
                    print_error(&format!("Failed to fetch MR info: {}", e));
                }
            }
        } else {
            print_error("GitLab client not available");
        }
        
        println!("\nPress Enter to continue...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }

    pub async fn push_stack(&mut self) -> Result<()> {
        print_train_header("Pushing Stack");

        let mut stack = self.load_current_stack()?;

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
        }

        // Create or update merge requests with intelligent target branch selection
        self.process_all_branches_for_mrs(&mut stack, "Updated merge request for").await;

        // Save the updated stack with MR IIDs
        self.save_stack_state(&stack)?;
        self.current_stack = Some(stack);

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

        // Update merge request targets if GitLab client is available
        if self.gitlab_client.is_some() {
            print_info("Updating merge request targets after sync...");
            self.process_branches_with_mrs_for_updates(&mut updated_stack, "Updated MR targets for").await;
        }

        // Switch back to the original branch
        run_git_command(&["checkout", &current_branch])?;

        // Save the updated stack
        self.save_stack_state(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Stack synchronized with remote and MR targets updated");

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

    /// Gets the current stack, loading it if not already cached
    fn get_or_load_current_stack(&mut self) -> Result<Stack> {
        match &self.current_stack {
            Some(stack) => Ok(stack.clone()),
            None => {
                let stack = self.load_current_stack()?;
                self.current_stack = Some(stack.clone());
                Ok(stack)
            }
        }
    }

    /// Collects MR status information for all branches in the stack
    async fn collect_mr_status_info(&self, stack: &Stack) -> std::collections::HashMap<String, MrStatusInfo> {
        let mut branch_mr_status = std::collections::HashMap::new();
        
        if let Some(gitlab_client) = &self.gitlab_client {
            for (branch_name, branch) in &stack.branches {
                if let Some(mr_iid) = branch.mr_iid {
                    // Fetch current MR status from GitLab
                    match gitlab_client.get_merge_request(mr_iid).await {
                        Ok(mr) => {
                            branch_mr_status.insert(branch_name.clone(), MrStatusInfo {
                                iid: mr_iid,
                                state: mr.state,
                            });
                        }
                        Err(_) => {
                            // If we can't fetch MR status, show as unknown
                            branch_mr_status.insert(branch_name.clone(), MrStatusInfo {
                                iid: mr_iid,
                                state: "unknown".to_string(),
                            });
                        }
                    }
                }
            }
        } else {
            // No GitLab client, just use the stored MR IIDs without status
            for (branch_name, branch) in &stack.branches {
                if let Some(mr_iid) = branch.mr_iid {
                    branch_mr_status.insert(branch_name.clone(), MrStatusInfo {
                        iid: mr_iid,
                        state: "unknown".to_string(),
                    });
                }
            }
        }
        
        branch_mr_status
    }

    /// Process all branches in the stack for MR creation/updates
    async fn process_all_branches_for_mrs(&self, stack: &mut Stack, success_message_prefix: &str) {
        if let Some(gitlab_client) = &self.gitlab_client {
            let branches_to_process: Vec<(String, StackBranch)> = stack.branches.clone().into_iter().collect();
            for (branch_name, branch) in branches_to_process {
                match self.create_or_update_mr_with_smart_targeting_and_store(gitlab_client, &branch_name, &branch, stack).await {
                    Ok(_) => print_success(&format!("{} {}", success_message_prefix, branch_name)),
                    Err(e) => print_warning(&format!("Failed to update MR for {}: {}", branch_name, e)),
                }
            }
        }
    }

    /// Process only branches that already have MRs for updates
    async fn process_branches_with_mrs_for_updates(&self, stack: &mut Stack, success_message_prefix: &str) {
        if let Some(gitlab_client) = &self.gitlab_client {
            let branches_to_process: Vec<(String, StackBranch)> = stack.branches.clone().into_iter().collect();
            for (branch_name, branch) in branches_to_process {
                if branch.mr_iid.is_some() {
                    match self.create_or_update_mr_with_smart_targeting_and_store(gitlab_client, &branch_name, &branch, stack).await {
                        Ok(_) => print_success(&format!("{} {}", success_message_prefix, branch_name)),
                        Err(e) => print_warning(&format!("Failed to update MR for {}: {}", branch_name, e)),
                    }
                }
            }
        }
    }

    /// Find a stack by name or ID prefix
    fn find_stack_by_identifier(&self, stack_identifier: &str) -> Result<Stack> {
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

        for stack_file in stack_files {
            if let Ok(stack_json) = std::fs::read_to_string(&stack_file) {
                if let Ok(stack) = serde_json::from_str::<Stack>(&stack_json) {
                    if stack.name == stack_identifier || stack.id.starts_with(stack_identifier) {
                        return Ok(stack);
                    }
                }
            }
        }

        Err(TrainError::StackError {
            message: format!("Stack '{}' not found", stack_identifier),
        }.into())
    }

    /// Format MR info for display in hierarchy
    fn format_mr_info_for_display(&self, mr_iid: Option<u64>) -> String {
        if let Some(iid) = mr_iid {
            format!(" [MR !{}]", iid)
        } else {
            String::new()
        }
    }

    /// Format MR info with status for enhanced display
    fn format_mr_info_with_status(&self, branch_name: &str, branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>) -> String {
        if let Some(mr_status) = branch_mr_status.get(branch_name) {
            let (status_icon, status_text) = match mr_status.state.as_str() {
                "merged" => ("✅", "MERGED".to_string()),
                "closed" => ("❌", "CLOSED".to_string()),
                "opened" => ("🔄", "OPEN".to_string()),
                _ => ("❓", mr_status.state.to_uppercase()),
            };
            format!(" [MR !{} {} {}]", mr_status.iid, status_icon, status_text)
        } else {
            String::new()
        }
    }

    fn print_branch_hierarchy_with_status(&self, hierarchy: &HashMap<String, Vec<String>>, stack: &Stack, branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>, indent: usize) {
        let indent_str = "  ".repeat(indent);
        
        for (branch_name, branch) in &stack.branches {
            if branch.parent.is_none() || (indent == 0 && branch.parent.as_ref() == Some(&stack.base_branch)) {
                let status = if Some(branch_name) == stack.current_branch.as_ref() { " (current)" } else { "" };
                let mr_info = self.format_mr_info_with_status(branch_name, branch_mr_status);
                
                println!("{}📋 {}{}{}", indent_str, branch_name, status, mr_info);
                println!("{}   └─ {}", indent_str, &branch.commit_hash[..8]);
                
                if let Some(children) = hierarchy.get(branch_name) {
                    for child in children {
                        self.print_branch_details_with_status(child, stack, branch_mr_status, indent + 1);
                        self.print_children_recursive_with_status(hierarchy, stack, branch_mr_status, child, indent + 1);
                    }
                }
            }
        }
    }

    fn print_branch_details_with_status(&self, branch_name: &str, stack: &Stack, branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>, indent: usize) {
        let indent_str = "  ".repeat(indent);
        
        if let Some(branch) = stack.branches.get(branch_name) {
            let status = if Some(branch_name) == stack.current_branch.as_deref() { " (current)" } else { "" };
            let mr_info = self.format_mr_info_with_status(branch_name, branch_mr_status);
            
            println!("{}├─ {}{}{}", indent_str, branch_name, status, mr_info);
            println!("{}│  └─ {}", indent_str, &branch.commit_hash[..8]);
        }
    }

    fn print_children_recursive_with_status(&self, hierarchy: &HashMap<String, Vec<String>>, stack: &Stack, branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>, branch_name: &str, indent: usize) {
        if let Some(children) = hierarchy.get(branch_name) {
            for child in children {
                self.print_branch_details_with_status(child, stack, branch_mr_status, indent + 1);
                self.print_children_recursive_with_status(hierarchy, stack, branch_mr_status, child, indent + 1);
            }
        }
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
                
                // Attempt smart rebase with conflict resolution
                if let Err(e) = self.smart_rebase(child_branch, changed_branch).await {
                    print_error(&format!("Failed to rebase {}: {}", child_branch, e));
                    // Continue with other branches
                    continue;
                }
                
                // Update stack state on successful rebase
                let new_commit = self.get_current_commit_hash()?;
                if let Some(branch) = stack.branches.get_mut(child_branch) {
                    branch.commit_hash = new_commit;
                    branch.updated_at = Utc::now();
                }
                print_success(&format!("Rebased {} onto {}", child_branch, changed_branch));
                
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
                let mr_info = self.format_mr_info_for_display(branch.mr_iid);
                
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
            let mr_info = self.format_mr_info_for_display(branch.mr_iid);
            
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
                
                // Use smart rebase with conflict resolution
                if let Err(e) = self.smart_rebase(child, base_branch).await {
                    print_error(&format!("Failed to rebase {}: {}", child, e));
                    // Continue with other branches
                    continue;
                }
                
                // Update stack state on successful rebase
                let new_commit = self.get_current_commit_hash()?;
                if let Some(branch) = stack.branches.get_mut(child) {
                    branch.commit_hash = new_commit;
                    branch.updated_at = Utc::now();
                }
                print_success(&format!("Rebased {}", child));
                
                // Recursively rebase children
                Box::pin(self.rebase_branch_hierarchy(stack, hierarchy, child)).await?;
            }
        }

        Ok(())
    }



    /// Intelligently determine the optimal target branch for a given branch in the stack
    async fn determine_optimal_target_branch(&self, branch_name: &str, stack: &Stack, gitlab_client: &GitLabClient) -> Result<String> {
        let branch = stack.branches.get(branch_name).ok_or_else(|| {
            TrainError::StackError {
                message: format!("Branch '{}' not found in stack", branch_name),
            }
        })?;

        let mut current_parent = branch.parent.as_deref().unwrap_or(&stack.base_branch).to_string();

        // Walk up the stack hierarchy to find the best available target
        loop {
            // Check if the current parent branch still exists and is available
            if current_parent == stack.base_branch {
                // Base branch is always a valid target
                break;
            }

            if let Some(parent_branch) = stack.branches.get(&current_parent) {
                // Check if parent branch has an open MR - if merged, we should target its target
                if let Some(parent_mr_iid) = parent_branch.mr_iid {
                    match gitlab_client.get_merge_request(parent_mr_iid).await {
                        Ok(parent_mr) => {
                            if parent_mr.state == "merged" {
                                // Parent is merged, target its target branch
                                print_info(&format!("Parent branch '{}' is merged, retargeting to '{}'", 
                                    current_parent, parent_mr.target_branch));
                                current_parent = parent_mr.target_branch;
                                continue;
                            } else if parent_mr.state == "closed" {
                                // Parent MR is closed, move up the hierarchy
                                print_info(&format!("Parent branch '{}' MR is closed, moving up hierarchy", current_parent));
                                current_parent = parent_branch.parent.as_deref().unwrap_or(&stack.base_branch).to_string();
                                continue;
                            }
                            // Parent MR is open, this is a valid target
                            break;
                        }
                        Err(_) => {
                            // Can't get MR status, assume branch is still valid
                            print_warning(&format!("Unable to check MR status for parent '{}', assuming valid", current_parent));
                            break;
                        }
                    }
                } else {
                    // Parent branch exists but has no MR, check if the branch itself still exists
                    match run_git_command(&["rev-parse", "--verify", &format!("origin/{}", current_parent)]) {
                        Ok(_) => break, // Branch exists remotely
                        Err(_) => {
                            // Branch doesn't exist remotely, move up the hierarchy
                            print_info(&format!("Parent branch '{}' not found remotely, moving up hierarchy", current_parent));
                            current_parent = parent_branch.parent.as_deref().unwrap_or(&stack.base_branch).to_string();
                            continue;
                        }
                    }
                }
            } else {
                // Parent not in stack, assume it's valid (might be base branch or external branch)
                break;
            }
        }

        Ok(current_parent)
    }

    /// Create or update merge request with intelligent target branch selection and store MR IID
    async fn create_or_update_mr_with_smart_targeting_and_store(&self, gitlab_client: &GitLabClient, branch_name: &str, branch: &StackBranch, stack: &mut Stack) -> Result<()> {
        // Determine the optimal target branch
        let optimal_target = self.determine_optimal_target_branch(branch_name, stack, gitlab_client).await?;
        let original_parent = branch.parent.as_deref().unwrap_or(&stack.base_branch);
        
        let title = format!("[Stack: {}] {}", stack.name, branch_name);
        let description = Some(format!(
            "Part of stack: {}\n\nBase branch: {}\nOriginal parent: {}\nCurrent target: {}\n\nStack ID: {}",
            stack.name, stack.base_branch, original_parent, optimal_target, stack.id
        ));

        if branch.mr_iid.is_none() {
            // Create new MR with optimal target
            let request = CreateMergeRequestRequest {
                source_branch: branch_name.to_string(),
                target_branch: optimal_target.clone(),
                title,
                description,
            };

            match gitlab_client.create_merge_request(request).await {
                Ok(mr) => {
                    print_success(&format!("Created MR !{} for branch {} targeting {}", mr.iid, branch_name, optimal_target));
                    // Update the stack to store the MR IID
                    if let Some(stack_branch) = stack.branches.get_mut(branch_name) {
                        stack_branch.mr_iid = Some(mr.iid);
                        stack_branch.updated_at = Utc::now();
                    }
                    stack.updated_at = Utc::now();
                }
                Err(e) => {
                    print_warning(&format!("Failed to create MR for {}: {}", branch_name, e));
                }
            }
        } else {
            // Update existing MR, potentially changing the target
            let iid = branch.mr_iid.unwrap();
            
            // First check current MR state to see if target needs updating
            let current_mr = gitlab_client.get_merge_request(iid).await?;
            let needs_retarget = current_mr.target_branch != optimal_target;
            
            if needs_retarget {
                print_info(&format!("Retargeting MR !{} for {} from '{}' to '{}'", 
                    iid, branch_name, current_mr.target_branch, optimal_target));
                
                match gitlab_client.update_merge_request_with_target(iid, Some(title), description, Some(optimal_target.clone())).await {
                    Ok(_) => {
                        print_success(&format!("Retargeted MR !{} for branch {} to {}", iid, branch_name, optimal_target));
                        // Update the stack to reflect the change
                        if let Some(stack_branch) = stack.branches.get_mut(branch_name) {
                            stack_branch.updated_at = Utc::now();
                        }
                        stack.updated_at = Utc::now();
                    }
                    Err(e) => {
                        print_warning(&format!("Failed to retarget MR !{} for {}: {}", iid, branch_name, e));
                    }
                }
            } else {
                // Just update title and description
                match gitlab_client.update_merge_request(iid, Some(title), description).await {
                    Ok(_) => {
                        print_success(&format!("Updated MR !{} for branch {}", iid, branch_name));
                        // Update the stack to reflect the change
                        if let Some(stack_branch) = stack.branches.get_mut(branch_name) {
                            stack_branch.updated_at = Utc::now();
                        }
                        stack.updated_at = Utc::now();
                    }
                    Err(e) => {
                        print_warning(&format!("Failed to update MR !{} for {}: {}", iid, branch_name, e));
                    }
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