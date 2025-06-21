use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

use crate::config::TrainConfig;
use crate::errors::TrainError;
use crate::git::GitRepository;
use crate::ui;

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub files: Vec<ConflictFile>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;
    use std::path::Path;

    fn init_repo(dir: &Path) {
        std::process::Command::new("git")
            .arg("init")
            .current_dir(dir)
            .output()
            .expect("git init");
        fs::write(dir.join("file.txt"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn test_git_state_clean() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        let repo = GitRepository::new(tmp.path()).unwrap();
        let resolver = ConflictResolver::new(TrainConfig::default(), tmp.path().join(".git"), repo);
        let state = resolver.get_git_state().unwrap();
        assert!(matches!(state, GitState::Clean));
    }

    #[test]
    fn test_git_state_stale_rebase_cleanup() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        fs::write(tmp.path().join(".git/REBASE_HEAD"), "dummy").unwrap();
        let repo = GitRepository::new(tmp.path()).unwrap();
        let resolver = ConflictResolver::new(TrainConfig::default(), tmp.path().join(".git"), repo);
        let state = resolver.get_git_state().unwrap();
        assert!(matches!(state, GitState::StaleRebase));
        resolver.cleanup_stale_state(state).unwrap();
        assert!(!tmp.path().join(".git/REBASE_HEAD").exists());
    }
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
    StaleRebase,
    StaleMerge,
    StaleCherryPick,
}

pub struct ConflictResolver {
    config: TrainConfig,
    git_dir: PathBuf,
    git_repo: GitRepository,
}

impl ConflictResolver {
    pub fn new(config: TrainConfig, git_dir: PathBuf, git_repo: GitRepository) -> Self {
        Self {
            config,
            git_dir,
            git_repo,
        }
    }

    /// Check the current git state for conflicts
    pub fn get_git_state(&self) -> Result<GitState> {
        let git_dir = &self.git_dir;

        // First check what git actually thinks about the repository state
        let status_output = self.git_repo.run(&["status", "--porcelain=v1"])?;
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
            if self.is_rebase_actually_active()? {
                return Ok(GitState::Rebasing);
            } else {
                return Ok(GitState::StaleRebase);
            }
        }

        if git_dir.join("MERGE_HEAD").exists() {
            if self.is_merge_actually_active()? {
                return Ok(GitState::Merging);
            } else {
                return Ok(GitState::StaleMerge);
            }
        }

        if git_dir.join("CHERRY_PICK_HEAD").exists() {
            if self.is_cherry_pick_actually_active()? {
                return Ok(GitState::CherryPicking);
            } else {
                return Ok(GitState::StaleCherryPick);
            }
        }

        Ok(GitState::Clean)
    }

    /// Check if a rebase is actually in progress (not just stale files)
    fn is_rebase_actually_active(&self) -> Result<bool> {
        let git_dir = &self.git_dir;
        Ok(git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists())
    }

    /// Check if a merge is actually in progress
    fn is_merge_actually_active(&self) -> Result<bool> {
        if !self.git_dir.join("MERGE_HEAD").exists() {
            return Ok(false);
        }
        let output = self.git_repo.run(&["ls-files", "-u"])?;
        Ok(!output.trim().is_empty())
    }

    /// Check if a cherry-pick is actually in progress
    fn is_cherry_pick_actually_active(&self) -> Result<bool> {
        if !self.git_dir.join("CHERRY_PICK_HEAD").exists() {
            return Ok(false);
        }
        let output = self.git_repo.run(&["ls-files", "-u"])?;
        Ok(!output.trim().is_empty())
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
                    ui::print_warning(&format!("Could not remove stale file {}: {}", file, e));
                } else {
                    ui::print_info(&format!("Removed stale file: {}", file));
                }
            }
        }

        // Remove any stale rebase directories
        let rebase_dirs = ["rebase-merge", "rebase-apply"];
        for dir in &rebase_dirs {
            let dir_path = git_dir.join(dir);
            if dir_path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&dir_path) {
                    ui::print_warning(&format!("Could not remove stale directory {}: {}", dir, e));
                } else {
                    ui::print_info(&format!("Removed stale directory: {}", dir));
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
                    ui::print_warning(&format!("Could not remove stale file {}: {}", file, e));
                } else {
                    ui::print_info(&format!("Removed stale file: {}", file));
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
                    ui::print_warning(&format!("Could not remove stale file {}: {}", file, e));
                } else {
                    ui::print_info(&format!("Removed stale file: {}", file));
                }
            }
        }

        Ok(())
    }

    /// Cleanup stale state files based on detected git state
    pub fn cleanup_stale_state(&self, state: GitState) -> Result<()> {
        match state {
            GitState::StaleRebase => self.cleanup_stale_rebase_files(),
            GitState::StaleMerge => self.cleanup_stale_merge_files(),
            GitState::StaleCherryPick => self.cleanup_stale_cherry_pick_files(),
            _ => Ok(()),
        }
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
            stale_state => {
                // If state files are stale, clean them up and report no conflicts
                self.cleanup_stale_state(stale_state)?;
                Ok(None)
            }
        }
    }

    /// Attempt to resolve conflicts automatically based on configuration
    pub async fn auto_resolve_conflicts(&self, _conflict_info: &ConflictInfo) -> Result<bool> {
        ui::print_info("Automatic conflict resolution is disabled");
        Ok(false)
    }

    /// Handle conflicts with user intervention
    pub async fn resolve_conflicts_interactively(
        &self,
        conflict_info: &ConflictInfo,
    ) -> Result<()> {
        ui::print_info("Conflicts detected. Manual resolution required.");

        // Show conflict summary
        self.print_conflict_summary(conflict_info);

        // Ask user how they want to proceed
        let options = vec![
            "Open editor to resolve conflicts manually and continue when ready",
            "Abort current operation",
        ];

        let choice =
            match ui::select_from_list(&options, "How would you like to resolve the conflicts?") {
                Ok(choice) => choice,
                Err(_) => {
                    // User cancelled (Ctrl+C) - provide graceful handling
                    ui::print_warning("Operation cancelled by user.");
                    ui::print_info("Resolution options:");
                    ui::print_info("• Re-run 'git-train sync' to try conflict resolution again");
                    ui::print_info("• Resolve conflicts manually and re-run 'git-train sync'");
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
                ui::print_success("Current operation aborted. Repository is now clean.");
                Ok(())
            }
            _ => unreachable!(),
        }
    }

    /// Open the configured editor for manual conflict resolution
    async fn open_editor_for_conflicts(&self, conflict_info: &ConflictInfo) -> Result<()> {
        let editor_config = &self.config.editor;

        ui::print_info("Opening editor(s) to resolve conflicts...");

        for conflict_file in &conflict_info.files {
            ui::print_info(&format!(
                "Opening {} in {}",
                conflict_file.path, editor_config.default_editor
            ));

            let mut cmd = Command::new(&editor_config.default_editor);
            cmd.args(&editor_config.editor_args);
            cmd.arg(&conflict_file.path);

            match cmd.status() {
                Ok(status) => {
                    if !status.success() {
                        ui::print_warning(&format!(
                            "Editor {} exited with non-zero status",
                            editor_config.default_editor
                        ));

                        // Check if user wants to continue with other files or abort
                        if conflict_info.files.len() > 1 {
                            let continue_choice =
                                ui::confirm_action("Continue editing other files?")?;
                            if !continue_choice {
                                ui::print_info("Please resolve conflicts manually and re-run 'git-train sync' when ready.");
                                return Err(TrainError::InvalidState {
                                    message: "Manual conflict resolution interrupted".to_string(),
                                }
                                .into());
                            }
                        }
                    }
                }
                Err(e) => {
                    ui::print_error(&format!(
                        "Failed to launch editor {}: {}",
                        editor_config.default_editor, e
                    ));
                    ui::print_info("You can:");
                    ui::print_info("• Resolve conflicts manually in your preferred editor");
                    ui::print_info("• Re-run 'git-train sync' when done");
                    return Err(TrainError::GitError {
                        message: format!("Could not launch editor: {}", e),
                    }
                    .into());
                }
            }
        }

        // Wait for user confirmation that they've finished resolving conflicts
        ui::print_info("");
        ui::print_success("Editor(s) have been opened for conflict resolution.");
        ui::print_info("After resolving all conflicts in your editor(s), come back here.");

        // Simple confirmation prompt
        ui::print_info("Press Enter when you have finished resolving all conflicts...");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        // Verify conflicts are resolved
        self.verify_conflicts_resolved().await
    }

    /// Verify that all conflicts have been resolved
    pub async fn verify_conflicts_resolved(&self) -> Result<()> {
        let current_conflicts = self.detect_conflicts()?;

        if let Some(conflicts) = current_conflicts {
            ui::print_error(&format!(
                "Still have {} unresolved conflicts",
                conflicts.files.len()
            ));

            if ui::confirm_action("Do you want to continue editing?")? {
                return Box::pin(self.open_editor_for_conflicts(&conflicts)).await;
            } else {
                return Err(TrainError::InvalidState {
                    message: "Conflicts not resolved".to_string(),
                }
                .into());
            }
        }

        // Add resolved files and continue
        self.git_repo.run(&["add", "."])?;

        let state = self.get_git_state()?;
        match state {
            GitState::Rebasing => {
                self.git_repo.run(&["rebase", "--continue"])?;
                ui::print_success("Rebase continued successfully");
            }
            GitState::Merging => {
                self.git_repo.run(&["commit", "--no-edit"])?;
                ui::print_success("Merge completed successfully");
            }
            GitState::CherryPicking => {
                self.git_repo.run(&["cherry-pick", "--continue"])?;
                ui::print_success("Cherry-pick continued successfully");
            }
            GitState::StaleRebase | GitState::StaleMerge | GitState::StaleCherryPick => {
                self.cleanup_stale_state(state)?;
                ui::print_success("Cleaned up stale state");
            }
            _ => {
                ui::print_success("Conflicts resolved");
            }
        }

        Ok(())
    }

    fn analyze_conflicts(&self) -> Result<Option<ConflictInfo>> {
        let status_output = self.git_repo.run(&["status", "--porcelain"])?;
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
        ui::print_warning(&format!(
            "Found {} conflicted files:",
            conflict_info.files.len()
        ));

        for conflict_file in &conflict_info.files {
            ui::print_info(&format!(
                "  📄 {} ({:?})",
                conflict_file.path, conflict_file.status
            ));
        }

        ui::print_warning("Manual resolution required");
    }

    pub fn abort_current_operation(&self) -> Result<()> {
        let state = self.get_git_state()?;
        match state {
            GitState::Rebasing => {
                self.git_repo.run(&["rebase", "--abort"])?;
                ui::print_info("Rebase aborted");
            }
            GitState::Merging => {
                self.git_repo.run(&["merge", "--abort"])?;
                ui::print_info("Merge aborted");
            }
            GitState::CherryPicking => {
                self.git_repo.run(&["cherry-pick", "--abort"])?;
                ui::print_info("Cherry-pick aborted");
            }
            GitState::StaleRebase | GitState::StaleMerge | GitState::StaleCherryPick => {
                self.cleanup_stale_state(state)?;
                ui::print_info("Stale state cleaned up");
            }
            _ => {
                ui::print_warning("No operation to abort");
            }
        }
        Ok(())
    }
}
