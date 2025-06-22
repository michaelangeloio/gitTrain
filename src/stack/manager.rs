use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::fs;
use tracing::info;
use uuid::Uuid;

use crate::config::TrainConfig;
use crate::conflict::{ConflictResolver, GitState};
use crate::errors::TrainError;
use crate::git::GitRepository;
use crate::gitlab::{CreateMergeRequestRequest, GitLabClient};
use crate::stack::state::StackState;
use crate::stack::types::{Stack, StackBranch};
use crate::ui::{
    self, confirm_action, get_user_input, print_error, print_info, print_success,
    print_train_header, print_warning, MrStatusInfo,
};
use crate::utils::{create_backup_name, sanitize_branch_name};

pub struct StackManager {
    stack_state: StackState,
    current_stack: Option<Stack>,
    gitlab_client: Option<GitLabClient>,
    config: TrainConfig,
    conflict_resolver: ConflictResolver,
    git_repo: GitRepository,
}

impl StackManager {
    pub async fn new_with_config(config: TrainConfig) -> Result<Self> {
        let git_repo = GitRepository::new_from_current_dir()?;
        let git_dir = git_repo.run(&["rev-parse", "--git-dir"])?;
        let git_dir_path = std::path::PathBuf::from(git_dir.trim()).canonicalize()?;

        let train_dir = git_dir_path.join("train");

        // Create train directory if it doesn't exist
        if !train_dir.exists() {
            fs::create_dir_all(&train_dir)?;
            info!("Created train directory: {:?}", train_dir);
        }

        // Try to initialize GitLab client
        let gitlab_client = match GitLabClient::new(git_repo.clone()).await {
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
        let conflict_resolver =
            ConflictResolver::new(config.clone(), git_dir_path.clone(), git_repo.clone());
        let stack_state = StackState::new(train_dir)?;

        Ok(Self {
            stack_state,
            current_stack: None,
            gitlab_client,
            config,
            conflict_resolver,
            git_repo,
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
                message: format!("Cannot rebase: git is in state {:?}. Please run 'git-train sync' to handle conflicts.", git_state),
            }.into());
        }

        // Create backup if configured
        if self.config.conflict_resolution.backup_on_conflict {
            let backup_branch = create_backup_name(branch);
            self.git_repo.run(&["branch", &backup_branch])?;
            print_info(&format!("Created backup branch: {}", backup_branch));
        }

        // Attempt the rebase
        match self.git_repo.run(&["rebase", onto]) {
            Ok(_) => {
                print_success(&format!("Rebased {} onto {} successfully", branch, onto));
                Ok(())
            }
            Err(_) => {
                // Check if we have conflicts
                if let Some(conflicts) = self.conflict_resolver.detect_conflicts()? {
                    print_info(&format!(
                        "Conflicts detected during rebase of {} onto {}",
                        branch, onto
                    ));

                    // Try automatic resolution first
                    if self
                        .conflict_resolver
                        .auto_resolve_conflicts(&conflicts)
                        .await?
                    {
                        // Continue the rebase
                        self.git_repo.run(&["rebase", "--continue"])?;
                        print_success("Auto-resolved conflicts and completed rebase");
                        Ok(())
                    } else {
                        // Fall back to interactive resolution
                        match self.config.conflict_resolution.auto_resolve_strategy {
                            crate::config::AutoResolveStrategy::Never => {
                                print_warning(
                                    "Auto-resolution disabled. Please resolve conflicts manually:",
                                );
                                print_info(
                                    "Re-run 'git-train sync' to continue with manual conflict resolution",
                                );
                                Err(TrainError::InvalidState {
                                    message: format!("Manual conflict resolution required for rebase of {} onto {}", branch, onto),
                                }.into())
                            }
                            _ => {
                                // Offer interactive resolution with better error handling
                                match self
                                    .conflict_resolver
                                    .resolve_conflicts_interactively(&conflicts)
                                    .await
                                {
                                    Ok(_) => Ok(()),
                                    Err(e) => {
                                        print_error(&format!(
                                            "Interactive conflict resolution failed: {}",
                                            e
                                        ));
                                        print_info("Resolution options:");
                                        print_info(
                                            "• Re-run 'git-train sync' to try conflict resolution again",
                                        );
                                        print_info("• Resolve conflicts manually and re-run 'git-train sync'");
                                        Err(TrainError::InvalidState {
                                            message: format!(
                                                "Rebase of {} onto {} requires manual intervention",
                                                branch, onto
                                            ),
                                        }
                                        .into())
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Rebase failed for other reasons
                    Err(TrainError::GitError {
                        message: format!("Rebase of {} onto {} failed", branch, onto),
                    }
                    .into())
                }
            }
        }
    }

    pub async fn create_stack(&mut self, name: &str) -> Result<()> {
        print_train_header(&format!("Creating Stack: {}", name));

        // Ensure we're on a clean working directory
        self.ensure_clean_working_directory()?;

        let current_branch = self.git_repo.get_current_branch()?;
        let current_commit = self.git_repo.get_current_commit_hash()?;
        let base_branch = self.determine_base_branch(&current_branch)?;

        let sanitized_name = sanitize_branch_name(name);
        let stack_id = Uuid::new_v4().to_string();

        // Get GitLab project information if available
        let gitlab_project = if let Some(gitlab_client) = &mut self.gitlab_client {
            print_info("Detecting GitLab project...");
            match gitlab_client.detect_and_cache_project().await {
                Ok(project) => {
                    print_success(&format!(
                        "Detected GitLab project: {}/{}",
                        project.namespace.path, project.path
                    ));
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
        self.stack_state.save_stack(&stack)?;
        self.current_stack = Some(stack);

        print_success(&format!(
            "Created stack '{}' with base branch '{}'",
            sanitized_name, base_branch
        ));
        print_info(&format!(
            "Current branch '{}' added to stack",
            current_branch
        ));

        Ok(())
    }

    pub async fn save_changes(&mut self, message: &str) -> Result<()> {
        print_train_header("Saving Changes");

        let stack = self.get_or_load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Ensure the current branch is part of the stack
        if !stack.branches.contains_key(&current_branch) {
            return Err(TrainError::StackError {
                message: format!(
                    "Branch '{}' is not part of the current stack",
                    current_branch
                ),
            }
            .into());
        }

        // Check if there are changes to commit
        if !self.has_uncommitted_changes()? {
            print_info("No changes to commit");
            return Ok(());
        }

        // Create a backup before making changes
        let backup_branch = create_backup_name(&current_branch);
        self.git_repo.run(&["branch", &backup_branch])?;
        print_info(&format!("Created backup branch: {}", backup_branch));

        // Commit the changes
        self.git_repo.run(&["add", "."])?;
        self.git_repo.run(&["commit", "-m", message])?;

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
        self.propagate_changes(&mut updated_stack, &current_branch)
            .await?;

        // Save the updated stack
        self.stack_state.save_stack(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Changes saved and propagated to dependent branches");

        Ok(())
    }

    pub async fn amend_changes(&mut self, new_message: Option<&str>) -> Result<()> {
        print_train_header("Amending Changes");

        let stack = self.get_or_load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Ensure the current branch is part of the stack
        if !stack.branches.contains_key(&current_branch) {
            return Err(TrainError::StackError {
                message: format!(
                    "Branch '{}' is not part of the current stack",
                    current_branch
                ),
            }
            .into());
        }

        // Create a backup before making changes
        let backup_branch = create_backup_name(&current_branch);
        self.git_repo.run(&["branch", &backup_branch])?;
        print_info(&format!("Created backup branch: {}", backup_branch));

        // Amend the current commit
        if let Some(message) = new_message {
            // Amend with new message
            self.git_repo.run(&["commit", "--amend", "-m", message])?;
            print_success(&format!("Amended commit with new message: {}", message));
        } else {
            // Check if there are staged changes to amend
            let staged_output = self.git_repo.run(&["diff", "--cached", "--name-only"])?;
            if staged_output.trim().is_empty() {
                // No staged changes, just amend message
                self.git_repo.run(&["commit", "--amend", "--no-edit"])?;
                print_success("Amended commit (no changes)");
            } else {
                // Stage all changes and amend
                self.git_repo.run(&["add", "."])?;
                self.git_repo.run(&["commit", "--amend", "--no-edit"])?;
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
        self.propagate_changes(&mut updated_stack, &current_branch)
            .await?;

        // Save the updated stack
        self.stack_state.save_stack(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Changes amended and downstream branches resynced");

        Ok(())
    }

    /// Intelligently detect the best parent branch by analyzing git history
    async fn detect_smart_parent(&self, current_branch: &str, stack: &Stack) -> Result<String> {
        // Get the commits in the current branch that are not in the base branch
        let commits_output = self.git_repo.run(&[
            "rev-list",
            &format!("{}..{}", stack.base_branch, current_branch),
            "--reverse",
        ])?;

        let commits: Vec<&str> = commits_output.trim().lines().collect();

        if commits.is_empty() {
            // No commits beyond base branch, parent should be base branch
            return Ok(stack.base_branch.clone());
        }

        // Check each stack branch to see which one contains the most commits from our branch
        let mut best_parent = stack.base_branch.clone();
        let mut max_shared_commits = 0;

        for branch_name in stack.branches.keys() {
            // Get commits in this stack branch
            let branch_commits_output = self.git_repo.run(&[
                "rev-list",
                &format!("{}..{}", stack.base_branch, branch_name),
            ])?;

            let branch_commits: std::collections::HashSet<&str> =
                branch_commits_output.trim().lines().collect();

            // Count how many of our commits are in this branch
            let shared_commits = commits
                .iter()
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
            print_info(&format!(
                "Detected '{}' as parent (shares {} commits)",
                best_parent, max_shared_commits
            ));
            Ok(best_parent)
        } else {
            // No shared commits with any stack branch, use base branch
            print_info(&format!(
                "No shared commits with stack branches, using base branch '{}'",
                stack.base_branch
            ));
            Ok(stack.base_branch.clone())
        }
    }

    pub async fn add_branch_to_stack(&mut self, parent: Option<&str>) -> Result<()> {
        print_train_header("Adding Branch to Stack");

        let mut stack = self.get_or_load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Check if branch is already in the stack
        if stack.branches.contains_key(&current_branch) {
            print_warning(&format!(
                "Branch '{}' is already part of the stack",
                current_branch
            ));
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
                }
                .into());
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
        self.stack_state.save_stack(&stack)?;
        self.current_stack = Some(stack);

        print_success(&format!(
            "Added branch '{}' to stack with parent '{}'",
            current_branch, parent_branch
        ));

        Ok(())
    }

    pub async fn list_stacks(&self) -> Result<()> {
        print_train_header("Available Stacks");

        let stacks = self.stack_state.list()?;

        if stacks.is_empty() {
            print_info("No stacks found");
            return Ok(());
        }

        let current_stack_id = self.stack_state.get_current_stack_id().unwrap_or_default();

        for stack in stacks {
            let is_current = if current_stack_id == stack.id {
                " (current)"
            } else {
                ""
            };
            let project_info = if let Some(project) = &stack.gitlab_project {
                format!(" | Project: {}/{}", project.namespace.path, project.path)
            } else {
                String::new()
            };

            ui::print_info(&format!(
                "▶ {} ({}){}",
                stack.name,
                &stack.id[..8],
                is_current
            ));
            ui::print_info(&format!(
                "   └─ Base: {} | Branches: {} | Updated: {}{}",
                stack.base_branch,
                stack.branches.len(),
                stack.updated_at.format("%Y-%m-%d %H:%M"),
                project_info
            ));
        }

        Ok(())
    }

    pub async fn switch_stack(&mut self, stack_identifier: &str) -> Result<()> {
        print_train_header(&format!("Switching to Stack: {}", stack_identifier));

        let stack = self.stack_state.find_by_identifier(stack_identifier)?;

        // Update the current stack pointer
        self.stack_state.set_current(&stack)?;

        self.current_stack = Some(stack.clone());

        print_success(&format!(
            "Switched to stack '{}' ({})",
            stack.name,
            &stack.id[..8]
        ));

        // Show status of the new stack
        self.show_status().await?;

        Ok(())
    }

    pub async fn delete_stack(&mut self, stack_identifier: &str, force: bool) -> Result<()> {
        print_train_header(&format!("Deleting Stack: {}", stack_identifier));

        let stack = self.stack_state.find_by_identifier(stack_identifier)?;

        // Check if this is the current stack
        let is_current_stack = if let Ok(current_stack_id) = self.stack_state.get_current_stack_id()
        {
            current_stack_id.trim() == stack.id
        } else {
            false
        };

        // Show what will be deleted
        print_warning(&format!(
            "This will permanently delete stack '{}' ({})",
            stack.name,
            &stack.id[..8]
        ));
        print_info(&format!(
            "Stack contains {} branches:",
            stack.branches.len()
        ));
        for branch_name in stack.branches.keys() {
            ui::print_info(&format!("  - {}", branch_name));
        }

        if let Some(project) = &stack.gitlab_project {
            print_info(&format!(
                "Associated with GitLab project: {}/{}",
                project.namespace.path, project.path
            ));
        }

        if is_current_stack {
            print_warning("This is the current active stack");
        }

        // Confirm deletion unless forced
        if !force {
            print_warning(
                "Are you sure you want to delete this stack? This action cannot be undone.",
            );
            let confirmed = get_user_input("Type 'yes' to confirm deletion", None)?;
            if confirmed.to_lowercase() != "yes" {
                print_info("Stack deletion cancelled");
                return Ok(());
            }
        }

        // Delete the stack file
        self.stack_state.delete(&stack)?;
        print_success(&format!("Deleted stack config for: {}", stack.name));

        // If this was the current stack, clear the current stack reference
        if is_current_stack {
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

        ui::print_info(&format!("Stack: {} ({})", stack.name, &stack.id[..8]));
        ui::print_info(&format!("Base branch: {}", stack.base_branch));

        if let Some(project) = &stack.gitlab_project {
            ui::print_info(&format!(
                "GitLab project: {}/{} (ID: {})",
                project.namespace.path, project.path, project.id
            ));
            ui::print_info(&format!("Project URL: {}", project.web_url));
        }

        ui::print_info(&format!(
            "Created: {}",
            stack.created_at.format("%Y-%m-%d %H:%M:%S UTC")
        ));
        ui::print_info(&format!(
            "Updated: {}",
            stack.updated_at.format("%Y-%m-%d %H:%M:%S UTC")
        ));
        ui::print_info("");

        // Build branch hierarchy and collect MR status
        let hierarchy = self.build_branch_hierarchy(&stack);
        let branch_mr_status = self.collect_mr_status_info(&stack).await;
        self.print_branch_hierarchy_with_status(&hierarchy, &stack, &branch_mr_status, 0);

        // Show working directory status
        let status_output = self.git_repo.run(&["status", "--porcelain"])?;
        if !status_output.is_empty() {
            ui::print_info("\nWorking directory status:");
            ui::print_info(&status_output);
        }

        Ok(())
    }

    pub async fn navigate_stack_interactively(&mut self) -> Result<()> {
        loop {
            // Load current stack state
            let stack = match self.get_or_load_current_stack() {
                Ok(stack) => {
                    self.current_stack = Some(stack.clone());
                    stack
                }
                Err(_) => {
                    print_warning(
                        "No active stack found. Please create or switch to a stack first.",
                    );
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
            let options = ui::create_navigation_options(
                &branches,
                current_git_branch.as_deref(),
                &branch_mr_status,
            );

            // Show interactive menu
            match ui::interactive_stack_navigation(&options, "Select an action:") {
                Ok(action) => {
                    match action {
                        ui::NavigationAction::SwitchToBranch(branch_name) => {
                            if let Err(e) = self.switch_to_branch(&branch_name).await {
                                print_error(&format!(
                                    "Failed to switch to branch {}: {}",
                                    branch_name, e
                                ));
                            }
                        }
                        ui::NavigationAction::ShowBranchInfo(branch_name) => {
                            self.show_branch_info(&branch_name, &stack).await;
                        }
                        ui::NavigationAction::CreateMR(branch_name) => {
                            if let Err(e) = self.create_mr_for_branch(&branch_name, &stack).await {
                                print_error(&format!(
                                    "Failed to create MR for {}: {}",
                                    branch_name, e
                                ));
                            }
                        }
                        ui::NavigationAction::ViewMR(branch_name, mr_iid) => {
                            self.view_mr_info(&branch_name, mr_iid, &stack).await;
                        }
                        ui::NavigationAction::RefreshStatus => {
                            // Just continue the loop to refresh
                            continue;
                        }
                        ui::NavigationAction::Exit => {
                            print_info("Exiting navigation");
                            break;
                        }
                    }
                }
                Err(_) => {
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
        if self.ensure_clean_working_directory().is_err() {
            print_warning("Working directory is not clean. Stashing changes...");
            self.git_repo
                .run(&["stash", "push", "-m", "git-train navigation stash"])?;
        }

        // Switch to the branch
        self.git_repo.run(&["checkout", branch_name])?;
        print_success(&format!("Switched to branch: {}", branch_name));

        Ok(())
    }

    async fn show_branch_info(&self, branch_name: &str, stack: &Stack) {
        print_train_header(&format!("Branch Info: {}", branch_name));

        if let Some(branch) = stack.branches.get(branch_name) {
            ui::print_info(&format!("Branch: {}", branch.name));
            ui::print_info(&format!(
                "Parent: {}",
                branch.parent.as_deref().unwrap_or(&stack.base_branch)
            ));
            ui::print_info(&format!("Commit: {}", &branch.commit_hash[..8]));
            ui::print_info(&format!(
                "Created: {}",
                branch.created_at.format("%Y-%m-%d %H:%M:%S UTC")
            ));
            ui::print_info(&format!(
                "Updated: {}",
                branch.updated_at.format("%Y-%m-%d %H:%M:%S UTC")
            ));

            if let Some(mr_iid) = branch.mr_iid {
                ui::print_info(&format!("Merge Request: !{}", mr_iid));
                if let Some(project) = &stack.gitlab_project {
                    ui::print_info(&format!(
                        "MR URL: {}/merge_requests/{}",
                        project.web_url, mr_iid
                    ));
                }
            } else {
                ui::print_info("Merge Request: Not created");
            }

            // Show children if any
            let hierarchy = self.build_branch_hierarchy(stack);
            if let Some(children) = hierarchy.get(branch_name) {
                if !children.is_empty() {
                    ui::print_info(&format!("Children: {}", children.join(", ")));
                }
            }

            // Show commit info
            if let Ok(commit_info) =
                self.git_repo
                    .run(&["show", "--oneline", "-s", &branch.commit_hash])
            {
                ui::print_info(&format!("Commit info: {}", commit_info));
            }
        } else {
            print_error(&format!("Branch '{}' not found in stack", branch_name));
        }

        ui::print_info("\nPress Enter to continue...");
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
                    &mut stack_mut,
                )
                .await?;

                // Save the updated stack
                self.stack_state.save_stack(&stack_mut)?;
                self.current_stack = Some(stack_mut);

                print_success(&format!(
                    "MR creation initiated for branch: {}",
                    branch_name
                ));
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
                    ui::print_info(&format!("Title: {}", mr.title));
                    ui::print_info(&format!("State: {}", mr.state));
                    ui::print_info(&format!("Source: {}", mr.source_branch));
                    ui::print_info(&format!("Target: {}", mr.target_branch));
                    ui::print_info(&format!("ID: {}", mr.id));
                    ui::print_info(&format!("IID: {}", mr.iid));

                    if let Some(project) = &stack.gitlab_project {
                        ui::print_info(&format!(
                            "URL: {}/merge_requests/{}",
                            project.web_url, mr.iid
                        ));
                    }

                    if let Some(description) = &mr.description {
                        if !description.is_empty() {
                            ui::print_info("\nDescription:");
                            ui::print_info(description);
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

        ui::print_info("\nPress Enter to continue...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }

    pub async fn push_stack(&mut self) -> Result<()> {
        print_train_header("Pushing Stack");

        let mut stack = self.get_or_load_current_stack()?;
        let mut push_failures = Vec::new();
        let mut successful_pushes = Vec::new();

        // Push all branches in the stack
        for branch_name in stack.branches.keys() {
            print_info(&format!("Pushing branch: {}", branch_name));

            // First try a normal push
            match self.git_repo.run(&[
                "push",
                "origin",
                &format!("{}:{}", branch_name, branch_name),
            ]) {
                Ok(_) => {
                    print_success(&format!("Pushed {}", branch_name));
                    successful_pushes.push(branch_name.clone());
                }
                Err(e) => {
                    // Check if this is a non-fast-forward error (common after rebase)
                    let error_msg = format!("{}", e);
                    if error_msg.contains("non-fast-forward") || error_msg.contains("rejected") {
                        print_warning(&format!(
                            "Branch {} was rejected (non-fast-forward)",
                            branch_name
                        ));
                        print_info(
                            "This is common after rebasing. Checking if force-push is safe...",
                        );

                        // Check if we should force push safely
                        if self.should_force_push_branch(branch_name, &stack).await? {
                            match self.git_repo.run(&[
                                "push",
                                "--force-with-lease",
                                "origin",
                                &format!("{}:{}", branch_name, branch_name),
                            ]) {
                                Ok(_) => {
                                    print_success(&format!("Force-pushed {} safely", branch_name));
                                    successful_pushes.push(branch_name.clone());
                                }
                                Err(force_err) => {
                                    print_error(&format!(
                                        "Force-push failed for {}: {}",
                                        branch_name, force_err
                                    ));
                                    print_warning("This might mean someone else pushed changes. Manual intervention required.");
                                    push_failures
                                        .push((branch_name.clone(), format!("{}", force_err)));
                                }
                            }
                        } else {
                            print_warning(&format!(
                                "Skipping force-push for {} (safety check failed)",
                                branch_name
                            ));
                            push_failures.push((
                                branch_name.clone(),
                                "Force-push deemed unsafe".to_string(),
                            ));
                        }
                    } else {
                        print_error(&format!("Failed to push {}: {}", branch_name, e));
                        push_failures.push((branch_name.clone(), format!("{}", e)));
                    }
                }
            }
        }

        // Report results
        if !successful_pushes.is_empty() {
            print_success(&format!(
                "Successfully pushed {} branches: {}",
                successful_pushes.len(),
                successful_pushes.join(", ")
            ));
        }

        if !push_failures.is_empty() {
            print_warning(&format!("Failed to push {} branches:", push_failures.len()));
            for (branch, error) in &push_failures {
                ui::print_error(&format!("  ✘ {}: {}", branch, error));
            }
            print_info("You can:");
            print_info("• Run 'git-train sync' to ensure branches are up to date");
            print_info("• Force-push manually with 'git push --force-with-lease' if you're sure");
            print_info("• Check for conflicts with remote changes");
        }

        // Create or update merge requests with intelligent target branch selection
        self.process_all_branches_for_mrs(&mut stack, "Updated merge request for")
            .await;

        // Save the updated stack with MR IIDs
        self.stack_state.save_stack(&stack)?;
        self.current_stack = Some(stack);

        if push_failures.is_empty() {
            print_success("Stack pushed to remote successfully");
        } else {
            print_warning("Stack partially pushed to remote (some branches failed)");
        }

        Ok(())
    }

    /// Determine if it's safe to force-push a branch
    async fn should_force_push_branch(&self, branch_name: &str, stack: &Stack) -> Result<bool> {
        // Safety checks for force-push

        // 1. Check if the branch exists remotely
        let remote_exists = self
            .git_repo
            .run(&["ls-remote", "--heads", "origin", branch_name])
            .map(|output| !output.trim().is_empty())
            .unwrap_or(false);

        if !remote_exists {
            // New branch, safe to push
            print_info(&format!(
                "Branch {} doesn't exist remotely, safe to push",
                branch_name
            ));
            return Ok(true);
        }

        // 2. Check if this branch is part of our stack and we control it
        if !stack.branches.contains_key(branch_name) {
            print_warning(&format!(
                "Branch {} is not part of our stack, unsafe to force-push",
                branch_name
            ));
            return Ok(false);
        }

        // 3. Check configuration for automatic force-push behavior
        if self.config.conflict_resolution.auto_force_push_after_rebase {
            print_info(&format!(
                "Auto force-push enabled, proceeding with {} (--force-with-lease)",
                branch_name
            ));
        } else if self.config.conflict_resolution.prompt_before_force_push {
            print_warning(&format!(
                "Branch {} requires force-push after rebase",
                branch_name
            ));
            print_info("This will overwrite the remote branch with your rebased version.");

            let proceed = confirm_action(&format!("Force-push {} safely?", branch_name))?;
            if !proceed {
                print_info("Skipping force-push. You can push manually later if needed.");
                return Ok(false);
            }
        } else {
            // Neither auto nor prompt enabled, skip force-push
            print_info(&format!(
                "Force-push not configured for automatic mode, skipping {}",
                branch_name
            ));
            return Ok(false);
        }

        // 4. Additional safety: ensure we're not too far ahead (sanity check)
        match self.git_repo.run(&[
            "rev-list",
            "--count",
            &format!("origin/{}..{}", branch_name, branch_name),
        ]) {
            Ok(output) => {
                if let Ok(ahead_count) = output.trim().parse::<u32>() {
                    if ahead_count > 20 {
                        print_warning(&format!(
                            "Branch {} is {} commits ahead of remote, this seems unusual",
                            branch_name, ahead_count
                        ));
                        print_warning("This might indicate a problem. Manual review recommended.");
                        return Ok(false);
                    }
                }
            }
            Err(_) => {
                // Can't determine, err on the side of caution
                print_info("Could not determine commit difference, proceeding with caution");
            }
        }

        Ok(true)
    }

    /// Check for and attempt to recover from invalid git states
    pub async fn check_and_recover_git_state(&self) -> Result<()> {
        let git_state = self.conflict_resolver.get_git_state()?;

        match git_state {
            GitState::Clean => Ok(()),
            GitState::Rebasing | GitState::Merging | GitState::CherryPicking => {
                print_warning(&format!(
                    "Git is in state {:?}. Recovery options:",
                    git_state
                ));

                if let Some(conflicts) = self.conflict_resolver.detect_conflicts()? {
                    print_info(&format!(
                        "Found {} conflicted files that need resolution",
                        conflicts.files.len()
                    ));
                    self.conflict_resolver.print_conflict_summary(&conflicts);

                    let options = vec![
                        "Try to resolve conflicts automatically",
                        "Resolve conflicts interactively",
                        "Abort the current operation",
                        "Continue with manual resolution later",
                    ];

                    let choice = ui::select_from_list(&options, "How would you like to proceed?")?;

                    match choice {
                        0 => {
                            if self
                                .conflict_resolver
                                .auto_resolve_conflicts(&conflicts)
                                .await?
                            {
                                let state = self.conflict_resolver.get_git_state()?;
                                self
                                    .conflict_resolver
                                    .verify_conflicts_resolved(&conflicts, state)
                                    .await?;
                                print_success(
                                    "Automatically resolved conflicts and completed operation",
                                );
                                Ok(())
                            } else {
                                print_warning("Automatic resolution failed. Please resolve manually or abort.");
                                Err(TrainError::InvalidState {
                                    message: "Automatic conflict resolution failed".to_string(),
                                }
                                .into())
                            }
                        }
                        1 => {
                            self.conflict_resolver
                                .resolve_conflicts_interactively(&conflicts)
                                .await
                        }
                        2 => {
                            self.conflict_resolver.abort_current_operation()?;
                            print_success("Aborted current operation. Repository is now clean.");
                            Ok(())
                        }
                        3 => {
                            print_info("Resolution deferred. Re-run 'git-train sync' when ready.");
                            Err(TrainError::InvalidState {
                                message: "Manual conflict resolution deferred".to_string(),
                            }
                            .into())
                        }
                        _ => unreachable!(),
                    }
                } else {
                    // No conflicts detected - this could be stale state files
                    print_info("No conflicts detected. This might be stale git state files.");
                    print_info("Attempting to clean up stale state and continue...");

                    // The get_git_state() call should have already cleaned up stale files,
                    // so let's check the state again after cleanup
                    let new_git_state = self.conflict_resolver.get_git_state()?;

                    if matches!(new_git_state, GitState::Clean) {
                        print_success("Successfully cleaned up stale git state files. Repository is now clean.");
                        Ok(())
                    } else {
                        // Still in problematic state, try to continue the operation
                        print_info(
                            "Repository still shows active operation. Attempting to continue...",
                        );
                        match git_state {
                            GitState::Rebasing => {
                                match self.git_repo.run(&["rebase", "--continue"]) {
                                    Ok(_) => {
                                        print_success("Successfully continued rebase");
                                        Ok(())
                                    }
                                    Err(_) => {
                                        print_warning(
                                            "Could not continue rebase. Offering to abort...",
                                        );
                                        if confirm_action("Abort the rebase?")? {
                                            self.git_repo.run(&["rebase", "--abort"])?;
                                            print_success(
                                                "Rebase aborted. Repository is now clean.",
                                            );
                                            Ok(())
                                        } else {
                                            Err(TrainError::InvalidState {
                                                message: "Could not continue interrupted rebase"
                                                    .to_string(),
                                            }
                                            .into())
                                        }
                                    }
                                }
                            }
                            GitState::Merging => {
                                match self.git_repo.run(&["commit", "--no-edit"]) {
                                    Ok(_) => {
                                        print_success("Successfully completed merge");
                                        Ok(())
                                    }
                                    Err(_) => {
                                        print_warning(
                                            "Could not complete merge. Offering to abort...",
                                        );
                                        if confirm_action("Abort the merge?")? {
                                            self.git_repo.run(&["merge", "--abort"])?;
                                            print_success(
                                                "Merge aborted. Repository is now clean.",
                                            );
                                            Ok(())
                                        } else {
                                            Err(TrainError::InvalidState {
                                                message: "Could not complete interrupted merge"
                                                    .to_string(),
                                            }
                                            .into())
                                        }
                                    }
                                }
                            }
                            GitState::CherryPicking => {
                                match self.git_repo.run(&["cherry-pick", "--continue"]) {
                                    Ok(_) => {
                                        print_success("Successfully continued cherry-pick");
                                        Ok(())
                                    }
                                    Err(_) => {
                                        print_warning(
                                            "Could not continue cherry-pick. Offering to abort...",
                                        );
                                        if confirm_action("Abort the cherry-pick?")? {
                                            self.git_repo.run(&["cherry-pick", "--abort"])?;
                                            print_success(
                                                "Cherry-pick aborted. Repository is now clean.",
                                            );
                                            Ok(())
                                        } else {
                                            Err(TrainError::InvalidState {
                                                message:
                                                    "Could not continue interrupted cherry-pick"
                                                        .to_string(),
                                            }
                                            .into())
                                        }
                                    }
                                }
                            }
                            _ => Ok(()),
                        }
                    }
                }
            }
            GitState::Conflicted => {
                print_warning("Repository has unresolved conflicts.");
                if let Some(conflicts) = self.conflict_resolver.detect_conflicts()? {
                    self.conflict_resolver
                        .resolve_conflicts_interactively(&conflicts)
                        .await
                } else {
                    Err(TrainError::InvalidState {
                        message: "Repository appears to have conflicts but none were detected"
                            .to_string(),
                    }
                    .into())
                }
            }
        }
    }

    pub async fn sync_with_remote(&mut self) -> Result<()> {
        print_train_header("Syncing with Remote");

        // First check and attempt to recover from any invalid git state
        if let Err(e) = self.check_and_recover_git_state().await {
            print_error(&format!("Cannot sync: {}", e));
            print_info("Please resolve the git state issue first:");
            print_info("• Conflicts will be handled during sync");
            return Err(e);
        }

        let stack = self.get_or_load_current_stack()?;
        let current_branch = self.get_current_branch()?;

        // Ensure working directory is clean
        self.ensure_clean_working_directory()?;

        // Update the base branch
        print_info(&format!("Updating base branch: {}", stack.base_branch));
        self.git_repo.run(&["checkout", &stack.base_branch])?;
        self.git_repo.run(&["pull", "origin", &stack.base_branch])?;

        // Rebase all stack branches with better error handling
        let mut updated_stack = stack.clone();
        let hierarchy = self.build_branch_hierarchy(&stack);

        match self
            .rebase_branch_hierarchy(&mut updated_stack, &hierarchy, &stack.base_branch)
            .await
        {
            Ok(_) => {
                print_success("Successfully rebased all branches");
            }
            Err(e) => {
                print_error(&format!("Some branches failed to rebase: {}", e));
                print_info("You can:");
                print_info("• Re-run 'git-train sync' to handle conflicts");
                print_info("• Run 'git-train sync' again after resolving issues");

                // Try to return to a safe state
                if self.git_repo.run(&["checkout", &current_branch]).is_err() {
                    print_warning(&format!("Could not return to original branch '{}'. You may need to checkout manually.", current_branch));
                }

                return Err(e);
            }
        }

        // Update merge request targets if GitLab client is available
        if self.gitlab_client.is_some() {
            print_info("Updating merge request targets after sync...");
            self.process_branches_with_mrs_for_updates(
                &mut updated_stack,
                "Updated MR targets for",
            )
            .await;
        }

        // Switch back to the original branch
        self.git_repo.run(&["checkout", &current_branch])?;

        // Save the updated stack
        self.stack_state.save_stack(&updated_stack)?;
        self.current_stack = Some(updated_stack);

        print_success("Stack synchronized with remote and MR targets updated");

        Ok(())
    }

    pub fn get_current_branch(&self) -> Result<String> {
        self.git_repo.get_current_branch()
    }

    fn get_current_commit_hash(&self) -> Result<String> {
        self.git_repo.get_current_commit_hash()
    }

    pub fn has_uncommitted_changes(&self) -> Result<bool> {
        self.git_repo.has_uncommitted_changes()
    }

    fn ensure_clean_working_directory(&self) -> Result<()> {
        if self.has_uncommitted_changes()? {
            return Err(TrainError::StackError {
                message:
                    "Working directory is not clean. Please commit or stash your changes first."
                        .to_string(),
            }
            .into());
        }
        Ok(())
    }

    /// Gets the current stack, loading it if not already cached
    pub fn get_or_load_current_stack(&mut self) -> Result<Stack> {
        match &self.current_stack {
            Some(stack) => Ok(stack.clone()),
            None => {
                let stack = self.stack_state.load_current()?;
                self.current_stack = Some(stack.clone());
                Ok(stack)
            }
        }
    }

    /// Collects MR status information for all branches in the stack
    async fn collect_mr_status_info(
        &self,
        stack: &Stack,
    ) -> std::collections::HashMap<String, MrStatusInfo> {
        let mut branch_mr_status = std::collections::HashMap::new();

        if let Some(gitlab_client) = &self.gitlab_client {
            for (branch_name, branch) in &stack.branches {
                if let Some(mr_iid) = branch.mr_iid {
                    // Fetch current MR status from GitLab
                    match gitlab_client.get_merge_request(mr_iid).await {
                        Ok(mr) => {
                            branch_mr_status.insert(
                                branch_name.clone(),
                                MrStatusInfo {
                                    iid: mr_iid,
                                    state: mr.state,
                                },
                            );
                        }
                        Err(_) => {
                            // If we can't fetch MR status, show as unknown
                            branch_mr_status.insert(
                                branch_name.clone(),
                                MrStatusInfo {
                                    iid: mr_iid,
                                    state: "unknown".to_string(),
                                },
                            );
                        }
                    }
                }
            }
        } else {
            // No GitLab client, just use the stored MR IIDs without status
            for (branch_name, branch) in &stack.branches {
                if let Some(mr_iid) = branch.mr_iid {
                    branch_mr_status.insert(
                        branch_name.clone(),
                        MrStatusInfo {
                            iid: mr_iid,
                            state: "unknown".to_string(),
                        },
                    );
                }
            }
        }

        branch_mr_status
    }

    /// Process all branches in the stack for MR creation/updates
    async fn process_all_branches_for_mrs(&self, stack: &mut Stack, success_message_prefix: &str) {
        if let Some(gitlab_client) = &self.gitlab_client {
            let branches_to_process: Vec<(String, StackBranch)> =
                stack.branches.clone().into_iter().collect();
            for (branch_name, branch) in branches_to_process {
                match self
                    .create_or_update_mr_with_smart_targeting_and_store(
                        gitlab_client,
                        &branch_name,
                        &branch,
                        stack,
                    )
                    .await
                {
                    Ok(_) => print_success(&format!("{} {}", success_message_prefix, branch_name)),
                    Err(e) => {
                        print_warning(&format!("Failed to update MR for {}: {}", branch_name, e))
                    }
                }
            }
        }
    }

    /// Process only branches that already have MRs for updates
    async fn process_branches_with_mrs_for_updates(
        &self,
        stack: &mut Stack,
        success_message_prefix: &str,
    ) {
        if let Some(gitlab_client) = &self.gitlab_client {
            let branches_to_process: Vec<(String, StackBranch)> =
                stack.branches.clone().into_iter().collect();
            for (branch_name, branch) in branches_to_process {
                if branch.mr_iid.is_some() {
                    match self
                        .create_or_update_mr_with_smart_targeting_and_store(
                            gitlab_client,
                            &branch_name,
                            &branch,
                            stack,
                        )
                        .await
                    {
                        Ok(_) => {
                            print_success(&format!("{} {}", success_message_prefix, branch_name))
                        }
                        Err(e) => print_warning(&format!(
                            "Failed to update MR for {}: {}",
                            branch_name, e
                        )),
                    }
                }
            }
        }
    }

    fn print_branch_hierarchy_with_status(
        &self,
        hierarchy: &HashMap<String, Vec<String>>,
        stack: &Stack,
        branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>,
        indent: usize,
    ) {
        let indent_str = "  ".repeat(indent);

        for (branch_name, branch) in &stack.branches {
            if branch.parent.is_none()
                || (indent == 0 && branch.parent.as_ref() == Some(&stack.base_branch))
            {
                let status = if Some(branch_name) == stack.current_branch.as_ref() {
                    " (current)"
                } else {
                    ""
                };
                let mr_info = format_mr_info_with_status(branch_name, branch_mr_status);

                ui::print_info(&format!(
                    "{}▶ {}{}{}",
                    indent_str, branch_name, status, mr_info
                ));
                ui::print_info(&format!("{}   └─ {}", indent_str, &branch.commit_hash[..8]));

                if let Some(children) = hierarchy.get(branch_name) {
                    for child in children {
                        self.print_branch_details_with_status(
                            child,
                            stack,
                            branch_mr_status,
                            indent + 1,
                        );
                        self.print_children_recursive_with_status(
                            hierarchy,
                            stack,
                            branch_mr_status,
                            child,
                            indent + 1,
                        );
                    }
                }
            }
        }
    }

    fn print_branch_details_with_status(
        &self,
        branch_name: &str,
        stack: &Stack,
        branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>,
        indent: usize,
    ) {
        let indent_str = "  ".repeat(indent);

        if let Some(branch) = stack.branches.get(branch_name) {
            let status = if Some(branch_name) == stack.current_branch.as_deref() {
                " (current)"
            } else {
                ""
            };
            let mr_info = format_mr_info_with_status(branch_name, branch_mr_status);

            ui::print_info(&format!(
                "{}├─ {}{}{}",
                indent_str, branch_name, status, mr_info
            ));
            ui::print_info(&format!("{}│  └─ {}", indent_str, &branch.commit_hash[..8]));
        }
    }

    fn print_children_recursive_with_status(
        &self,
        hierarchy: &HashMap<String, Vec<String>>,
        stack: &Stack,
        branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>,
        branch_name: &str,
        indent: usize,
    ) {
        if let Some(children) = hierarchy.get(branch_name) {
            for child in children {
                self.print_branch_details_with_status(child, stack, branch_mr_status, indent + 1);
                self.print_children_recursive_with_status(
                    hierarchy,
                    stack,
                    branch_mr_status,
                    child,
                    indent + 1,
                );
            }
        }
    }

    fn determine_base_branch(&self, _current_branch: &str) -> Result<String> {
        // Simple strategy: check for 'main' or 'master'
        for branch in ["main", "master"] {
            if self
                .git_repo
                .run(&["rev-parse", "--verify", branch])
                .is_ok()
            {
                return Ok(branch.to_string());
            }
        }
        // Fallback to a warning and user input if needed
        print_warning("Could not determine a default base branch ('main' or 'master' not found)");
        get_user_input("Please enter the base branch name:", None)}

    async fn propagate_changes(&self, stack: &mut Stack, changed_branch: &str) -> Result<()> {
        let hierarchy = self.build_branch_hierarchy(stack);
        if let Some(children) = hierarchy.get(changed_branch) {
            for child_branch_name in children {
                print_info(&format!(
                    "Propagating changes to child branch: {}",
                    child_branch_name
                ));

                self.git_repo.run(&["checkout", child_branch_name])?;
                self.smart_rebase(child_branch_name, changed_branch).await?;

                // Update commit hash
                if let Some(branch) = stack.branches.get_mut(child_branch_name) {
                    branch.commit_hash = self.get_current_commit_hash()?;
                    branch.updated_at = Utc::now();
                }
            }
        }
        Ok(())
    }

    fn build_branch_hierarchy(&self, stack: &Stack) -> HashMap<String, Vec<String>> {
        let mut hierarchy: HashMap<String, Vec<String>> = HashMap::new();

        for (branch_name, branch) in &stack.branches {
            if let Some(parent) = &branch.parent {
                hierarchy
                    .entry(parent.clone())
                    .or_default()
                    .push(branch_name.clone());
            }
        }

        hierarchy
    }

    async fn rebase_branch_hierarchy(
        &self,
        stack: &mut Stack,
        hierarchy: &HashMap<String, Vec<String>>,
        base_branch: &str,
    ) -> Result<()> {
        let mut rebased_branches = std::collections::HashSet::new();

        // Start with branches that have the base branch as a parent
        let mut branches_to_rebase: Vec<String> = hierarchy
            .iter()
            .filter(|(child, _)| {
                stack.branches.get(*child).and_then(|b| b.parent.as_ref())
                    == Some(&base_branch.to_string())
            })
            .map(|(child, _)| child.clone())
            .collect();

        // Sort for consistent order
        branches_to_rebase.sort();

        let mut all_rebased_ok = true;

        while let Some(branch_name) = branches_to_rebase.pop() {
            if rebased_branches.contains(&branch_name) {
                continue;
            }

            let parent_branch_name = stack
                .branches
                .get(&branch_name)
                .and_then(|b| b.parent.clone())
                .unwrap_or_else(|| base_branch.to_string());

            print_info(&format!(
                "Rebasing '{}' onto '{}'",
                branch_name, parent_branch_name
            ));
            self.git_repo.run(&["checkout", &branch_name])?;

            match self.smart_rebase(&branch_name, &parent_branch_name).await {
                Ok(_) => {
                    // Update commit hash
                    if let Some(branch) = stack.branches.get_mut(&branch_name) {
                        branch.commit_hash = self.get_current_commit_hash()?;
                        branch.updated_at = Utc::now();
                    }
                    rebased_branches.insert(branch_name.clone());

                    // Add children of this branch to the queue
                    if let Some(children) = hierarchy.get(&branch_name) {
                        for child in children {
                            branches_to_rebase.push(child.clone());
                        }
                    }
                }
                Err(e) => {
                    print_error(&format!("Failed to rebase branch '{}': {}", branch_name, e));
                    all_rebased_ok = false;
                    // Don't proceed with children if parent fails
                }
            }
        }

        if all_rebased_ok {
            Ok(())
        } else {
            Err(TrainError::GitError {
                message: "One or more branches failed to rebase".to_string(),
            }
            .into())
        }
    }

    /// Intelligently determine the optimal target branch for a given branch in the stack
    async fn determine_optimal_target_branch(
        &self,
        branch_name: &str,
        stack: &Stack,
        gitlab_client: &GitLabClient,
    ) -> Result<String> {
        let branch = stack
            .branches
            .get(branch_name)
            .ok_or_else(|| TrainError::StackError {
                message: format!("Branch '{}' not found in stack", branch_name),
            })?;

        let mut current_parent = branch
            .parent
            .as_deref()
            .unwrap_or(&stack.base_branch)
            .to_string();

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
                                print_info(&format!(
                                    "Parent branch '{}' is merged, retargeting to '{}'",
                                    current_parent, parent_mr.target_branch
                                ));
                                current_parent = parent_mr.target_branch;
                                continue;
                            } else if parent_mr.state == "closed" {
                                // Parent MR is closed, move up the hierarchy
                                print_info(&format!(
                                    "Parent branch '{}' MR is closed, moving up hierarchy",
                                    current_parent
                                ));
                                current_parent = parent_branch
                                    .parent
                                    .as_deref()
                                    .unwrap_or(&stack.base_branch)
                                    .to_string();
                                continue;
                            }
                            // Parent MR is open, this is a valid target
                            break;
                        }
                        Err(_) => {
                            // Can't get MR status, assume branch is still valid
                            print_warning(&format!(
                                "Unable to check MR status for parent '{}', assuming valid",
                                current_parent
                            ));
                            break;
                        }
                    }
                } else {
                    // Parent branch exists but has no MR, check if the branch itself still exists
                    match self.git_repo.run(&[
                        "rev-parse",
                        "--verify",
                        &format!("origin/{}", current_parent),
                    ]) {
                        Ok(_) => break, // Branch exists remotely
                        Err(_) => {
                            // Branch doesn't exist remotely, move up the hierarchy
                            print_info(&format!(
                                "Parent branch '{}' not found remotely, moving up hierarchy",
                                current_parent
                            ));
                            current_parent = parent_branch
                                .parent
                                .as_deref()
                                .unwrap_or(&stack.base_branch)
                                .to_string();
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
    async fn create_or_update_mr_with_smart_targeting_and_store(
        &self,
        gitlab_client: &GitLabClient,
        branch_name: &str,
        branch: &StackBranch,
        stack: &mut Stack,
    ) -> Result<()> {
        // Determine the optimal target branch
        let optimal_target = self
            .determine_optimal_target_branch(branch_name, stack, gitlab_client)
            .await?;
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
                    print_success(&format!(
                        "Created MR !{} for branch {} targeting {}",
                        mr.iid, branch_name, optimal_target
                    ));
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
                print_info(&format!(
                    "Retargeting MR !{} for {} from '{}' to '{}'",
                    iid, branch_name, current_mr.target_branch, optimal_target
                ));

                match gitlab_client
                    .update_merge_request_with_target(
                        iid,
                        Some(title),
                        description,
                        Some(optimal_target.clone()),
                    )
                    .await
                {
                    Ok(_) => {
                        print_success(&format!(
                            "Retargeted MR !{} for branch {} to {}",
                            iid, branch_name, optimal_target
                        ));
                        // Update the stack to reflect the change
                        if let Some(stack_branch) = stack.branches.get_mut(branch_name) {
                            stack_branch.updated_at = Utc::now();
                        }
                        stack.updated_at = Utc::now();
                    }
                    Err(e) => {
                        print_warning(&format!(
                            "Failed to retarget MR !{} for {}: {}",
                            iid, branch_name, e
                        ));
                    }
                }
            } else {
                // Just update title and description
                match gitlab_client
                    .update_merge_request(iid, Some(title), description)
                    .await
                {
                    Ok(_) => {
                        print_success(&format!("Updated MR !{} for branch {}", iid, branch_name));
                        // Update the stack to reflect the change
                        if let Some(stack_branch) = stack.branches.get_mut(branch_name) {
                            stack_branch.updated_at = Utc::now();
                        }
                        stack.updated_at = Utc::now();
                    }
                    Err(e) => {
                        print_warning(&format!(
                            "Failed to update MR !{} for {}: {}",
                            iid, branch_name, e
                        ));
                    }
                }
            }
        }

        Ok(())
    }
}

/// Format MR info with status for enhanced display
fn format_mr_info_with_status(
    branch_name: &str,
    branch_mr_status: &std::collections::HashMap<String, MrStatusInfo>,
) -> String {
    if let Some(mr_status) = branch_mr_status.get(branch_name) {
        let (status_icon, status_text) = match mr_status.state.as_str() {
            "merged" => ("✔", "MERGED".to_string()),
            "closed" => ("✘", "CLOSED".to_string()),
            "opened" => ("●", "OPEN".to_string()),
            _ => ("?", mr_status.state.to_uppercase()),
        };
        format!(" [MR !{} {} {}]", mr_status.iid, status_icon, status_text)
    } else {
        String::new()
    }
}
