use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

use crate::config::TrainConfig;
use crate::errors::TrainError;
use crate::utils::{
    confirm_action, print_error, print_info, print_success, print_warning, run_git_command,
};

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub files: Vec<ConflictFile>,
}

#[derive(Debug, Clone)]
pub struct ConflictFile {
    pub path: String,
    pub status: ConflictStatus,
}

#[derive(Debug, Clone)]
pub enum ConflictStatus {
    BothModified,
    AddedByUs,
    AddedByThem,
    DeletedByUs,
    DeletedByThem,
}

#[derive(Debug, Clone)]
pub enum GitState {
    Clean,
    Rebasing,
    Merging,
    CherryPicking,
    Conflicted,
}

pub struct ConflictResolver {
    config: TrainConfig,
    git_dir: PathBuf,
}

impl ConflictResolver {
    pub fn new(config: TrainConfig, git_dir: PathBuf) -> Self {
        Self { config, git_dir }
    }

    /// Check the current git state for conflicts
    pub fn get_git_state(&self) -> Result<GitState> {
        let git_dir = &self.git_dir;

        // First check what git actually thinks about the repository state
        let status_output = run_git_command(&["status", "--porcelain=v1"])?;
        let status_lines: Vec<&str> = status_output.lines().collect();

        // Check for actual conflicts in working directory first
        let has_conflicts = status_lines.iter().any(|line| {
            line.starts_with("UU")
                || line.starts_with("AA")
                || line.starts_with("DU")
                || line.starts_with("UD")
                || line.starts_with("AU")
                || line.starts_with("UA")
        });

        if has_conflicts {
            return Ok(GitState::Conflicted);
        }

        // Now check for ongoing operations, but verify they're actually active
        if git_dir.join("REBASE_HEAD").exists() {
            // Double-check if rebase is actually in progress
            if self.is_rebase_actually_active()? {
                return Ok(GitState::Rebasing);
            } else {
                // Clean up stale rebase files
                print_info("Detected stale rebase state files, cleaning up...");
                self.cleanup_stale_rebase_files()?;
            }
        }

        if git_dir.join("MERGE_HEAD").exists() {
            // Double-check if merge is actually in progress
            if self.is_merge_actually_active()? {
                return Ok(GitState::Merging);
            } else {
                print_info("Detected stale merge state files, cleaning up...");
                self.cleanup_stale_merge_files()?;
            }
        }

        if git_dir.join("CHERRY_PICK_HEAD").exists() {
            // Double-check if cherry-pick is actually in progress
            if self.is_cherry_pick_actually_active()? {
                return Ok(GitState::CherryPicking);
            } else {
                print_info("Detected stale cherry-pick state files, cleaning up...");
                self.cleanup_stale_cherry_pick_files()?;
            }
        }

        Ok(GitState::Clean)
    }

    /// Check if a rebase is actually in progress (not just stale files)
    fn is_rebase_actually_active(&self) -> Result<bool> {
        // Try to get rebase info - this will fail if no rebase is actually in progress
        match run_git_command(&["rebase", "--show-current-patch"]) {
            Ok(_) => Ok(true),
            Err(_) => {
                // Also check for rebase directories that would exist during an active rebase
                let git_dir = &self.git_dir;
                Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
            }
        }
    }

    /// Check if a merge is actually in progress
    fn is_merge_actually_active(&self) -> Result<bool> {
        // Check if MERGE_MSG exists and is recent, and if there are actual merge conflicts
        let git_dir = &self.git_dir;
        Ok(git_dir.join("MERGE_MSG").exists() && git_dir.join("MERGE_HEAD").exists())
    }

    /// Check if a cherry-pick is actually in progress
    fn is_cherry_pick_actually_active(&self) -> Result<bool> {
        // Try to continue cherry-pick to see if it's actually in progress
        match run_git_command(&["cherry-pick", "--continue", "--dry-run"]) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    /// Clean up stale rebase files
    fn cleanup_stale_rebase_files(&self) -> Result<()> {
        let git_dir = &self.git_dir;
        let files_to_remove = [
            "REBASE_HEAD",
            "ORIG_HEAD", // Only if it's from a rebase
        ];

        for file in &files_to_remove {
            let file_path = git_dir.join(file);
            if file_path.exists() {
                if let Err(e) = std::fs::remove_file(&file_path) {
                    print_warning(&format!("Could not remove stale file {}: {}", file, e));
                } else {
                    print_info(&format!("Removed stale file: {}", file));
                }
            }
        }

        // Remove any stale rebase directories
        let rebase_dirs = ["rebase-merge", "rebase-apply"];
        for dir in &rebase_dirs {
            let dir_path = git_dir.join(dir);
            if dir_path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&dir_path) {
                    print_warning(&format!("Could not remove stale directory {}: {}", dir, e));
                } else {
                    print_info(&format!("Removed stale directory: {}", dir));
                }
            }
        }

        Ok(())
    }

    /// Clean up stale merge files
    fn cleanup_stale_merge_files(&self) -> Result<()> {
        let git_dir = &self.git_dir;
        let files_to_remove = ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"];

        for file in &files_to_remove {
            let file_path = git_dir.join(file);
            if file_path.exists() {
                if let Err(e) = std::fs::remove_file(&file_path) {
                    print_warning(&format!("Could not remove stale file {}: {}", file, e));
                } else {
                    print_info(&format!("Removed stale file: {}", file));
                }
            }
        }

        Ok(())
    }

    /// Clean up stale cherry-pick files
    fn cleanup_stale_cherry_pick_files(&self) -> Result<()> {
        let git_dir = &self.git_dir;
        let files_to_remove = ["CHERRY_PICK_HEAD", "CHERRY_PICK_MSG"];

        for file in &files_to_remove {
            let file_path = git_dir.join(file);
            if file_path.exists() {
                if let Err(e) = std::fs::remove_file(&file_path) {
                    print_warning(&format!("Could not remove stale file {}: {}", file, e));
                } else {
                    print_info(&format!("Removed stale file: {}", file));
                }
            }
        }

        Ok(())
    }

    /// Detect and analyze conflicts in the repository
    pub fn detect_conflicts(&self) -> Result<Option<ConflictInfo>> {
        let git_state = self.get_git_state()?;

        match git_state {
            GitState::Clean => Ok(None),
            GitState::Rebasing
            | GitState::Merging
            | GitState::CherryPicking
            | GitState::Conflicted => self.analyze_conflicts(),
        }
    }

    /// Attempt to resolve conflicts automatically based on configuration
    pub async fn auto_resolve_conflicts(&self, _conflict_info: &ConflictInfo) -> Result<bool> {
        print_info("Automatic conflict resolution is disabled");
        Ok(false)
    }

    /// Handle conflicts with user intervention
    pub async fn resolve_conflicts_interactively(
        &self,
        conflict_info: &ConflictInfo,
    ) -> Result<()> {
        print_info("Conflicts detected. Manual resolution required.");

        // Show conflict summary
        self.print_conflict_summary(conflict_info);

        // Ask user how they want to proceed
        let options = vec![
            "Open editor to resolve conflicts manually and continue when ready",
            "Abort current operation",
        ];

        let choice = match crate::utils::select_from_list(
            &options,
            "How would you like to resolve the conflicts?",
        ) {
            Ok(choice) => choice,
            Err(_) => {
                // User cancelled (Ctrl+C) - provide graceful handling
                print_warning("Operation cancelled by user.");
                print_info("Resolution options:");
                print_info("• Re-run 'git-train sync' to try conflict resolution again");
                print_info("• Resolve conflicts manually and re-run 'git-train sync'");
                return Err(TrainError::InvalidState {
                    message: "Conflict resolution cancelled by user".to_string(),
                }
                .into());
            }
        };

        match choice {
            0 => self.open_editor_for_conflicts(conflict_info).await,
            1 => {
                self.abort_current_operation()?;
                print_success("Current operation aborted. Repository is now clean.");
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    /// Open the configured editor for manual conflict resolution
    async fn open_editor_for_conflicts(&self, conflict_info: &ConflictInfo) -> Result<()> {
        let editor_config = &self.config.editor;

        print_info("Opening editor(s) to resolve conflicts...");

        for conflict_file in &conflict_info.files {
            print_info(&format!(
                "Opening {} in {}",
                conflict_file.path, editor_config.default_editor
            ));

            let mut cmd = Command::new(&editor_config.default_editor);
            cmd.args(&editor_config.editor_args);
            cmd.arg(&conflict_file.path);

            match cmd.status() {
                Ok(status) => {
                    if !status.success() {
                        print_warning(&format!(
                            "Editor {} exited with non-zero status",
                            editor_config.default_editor
                        ));

                        // Check if user wants to continue with other files or abort
                        if conflict_info.files.len() > 1 {
                            let continue_choice =
                                crate::utils::confirm_action("Continue editing other files?")?;
                            if !continue_choice {
                                print_info("Please resolve conflicts manually and re-run 'git-train sync' when ready.");
                                return Err(TrainError::InvalidState {
                                    message: "Manual conflict resolution interrupted".to_string(),
                                }
                                .into());
                            }
                        }
                    }
                }
                Err(e) => {
                    print_error(&format!(
                        "Failed to launch editor {}: {}",
                        editor_config.default_editor, e
                    ));
                    print_info("You can:");
                    print_info("• Resolve conflicts manually in your preferred editor");
                    print_info("• Re-run 'git-train sync' when done");
                    return Err(TrainError::GitError {
                        message: format!("Could not launch editor: {}", e),
                    }
                    .into());
                }
            }
        }

        // Wait for user confirmation that they've finished resolving conflicts
        print_info("");
        print_success("Editor(s) have been opened for conflict resolution.");
        print_info("After resolving all conflicts in your editor(s), come back here.");

        // Simple confirmation prompt
        println!("Press Enter when you have finished resolving all conflicts...");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        // Verify conflicts are resolved
        self.verify_conflicts_resolved().await
    }

    /// Verify that all conflicts have been resolved
    pub async fn verify_conflicts_resolved(&self) -> Result<()> {
        let current_conflicts = self.detect_conflicts()?;

        if let Some(conflicts) = current_conflicts {
            print_error(&format!(
                "Still have {} unresolved conflicts",
                conflicts.files.len()
            ));

            if confirm_action("Do you want to continue editing?")? {
                return Box::pin(self.open_editor_for_conflicts(&conflicts)).await;
            } else {
                return Err(TrainError::InvalidState {
                    message: "Conflicts not resolved".to_string(),
                }
                .into());
            }
        }

        // Add resolved files and continue
        run_git_command(&["add", "."])?;

        match self.get_git_state()? {
            GitState::Rebasing => {
                run_git_command(&["rebase", "--continue"])?;
                print_success("Rebase continued successfully");
            }
            GitState::Merging => {
                run_git_command(&["commit", "--no-edit"])?;
                print_success("Merge completed successfully");
            }
            GitState::CherryPicking => {
                run_git_command(&["cherry-pick", "--continue"])?;
                print_success("Cherry-pick continued successfully");
            }
            _ => {
                print_success("Conflicts resolved");
            }
        }

        Ok(())
    }

    fn analyze_conflicts(&self) -> Result<Option<ConflictInfo>> {
        let status_output = run_git_command(&["status", "--porcelain"])?;
        let mut conflict_files = Vec::new();

        for line in status_output.lines() {
            if let Some(conflict_file) = self.parse_conflict_line(line)? {
                conflict_files.push(conflict_file);
            }
        }

        if conflict_files.is_empty() {
            return Ok(None);
        }

        Ok(Some(ConflictInfo {
            files: conflict_files,
        }))
    }

    fn parse_conflict_line(&self, line: &str) -> Result<Option<ConflictFile>> {
        if line.len() < 3 {
            return Ok(None);
        }

        let status_chars: Vec<char> = line.chars().take(2).collect();
        let file_path = line[3..].to_string();

        let status = match (status_chars[0], status_chars[1]) {
            ('U', 'U') => ConflictStatus::BothModified,
            ('A', 'U') => ConflictStatus::AddedByUs,
            ('U', 'A') => ConflictStatus::AddedByThem,
            ('D', 'U') => ConflictStatus::DeletedByUs,
            ('U', 'D') => ConflictStatus::DeletedByThem,
            _ => return Ok(None),
        };

        Ok(Some(ConflictFile {
            path: file_path,
            status,
        }))
    }

    pub fn print_conflict_summary(&self, conflict_info: &ConflictInfo) {
        print_warning(&format!(
            "Found {} conflicted files:",
            conflict_info.files.len()
        ));

        for conflict_file in &conflict_info.files {
            println!("  📄 {} ({:?})", conflict_file.path, conflict_file.status);
        }

        print_warning("Manual resolution required");
    }

    pub fn abort_current_operation(&self) -> Result<()> {
        match self.get_git_state()? {
            GitState::Rebasing => {
                run_git_command(&["rebase", "--abort"])?;
                print_info("Rebase aborted");
            }
            GitState::Merging => {
                run_git_command(&["merge", "--abort"])?;
                print_info("Merge aborted");
            }
            GitState::CherryPicking => {
                run_git_command(&["cherry-pick", "--abort"])?;
                print_info("Cherry-pick aborted");
            }
            _ => {
                print_warning("No operation to abort");
            }
        }
        Ok(())
    }
}
