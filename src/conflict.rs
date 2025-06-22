use anyhow::Result;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use crate::config::TrainConfig;
use crate::errors::TrainError;
use crate::git::GitRepository;
use crate::ui;

pub trait EditorLauncher: Send + Sync {
    fn launch(&self, editor: &str, args: &[String], file: &str) -> Result<ExitStatus>;
}

pub struct DefaultEditorLauncher;

impl EditorLauncher for DefaultEditorLauncher {
    fn launch(&self, editor: &str, args: &[String], file: &str) -> Result<ExitStatus> {
        let mut cmd = Command::new(editor);
        cmd.args(args);
        cmd.arg(file);
        let status = cmd.status()?;
        Ok(status)
    }
}

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
    git_repo: GitRepository,
    editor_launcher: Box<dyn EditorLauncher>,
}

impl ConflictResolver {
    pub fn new(config: TrainConfig, git_dir: PathBuf, git_repo: GitRepository) -> Self {
        Self {
            config,
            git_dir,
            git_repo,
            editor_launcher: Box::new(DefaultEditorLauncher),
        }
    }

    /// Check the current git state for conflicts
    pub fn get_git_state(&self) -> Result<GitState> {
        let git_dir = &self.git_dir;

        // First check for ongoing operations, then conflicts within those operations
        if git_dir.join("REBASE_HEAD").exists() && self.is_rebase_actually_active()? {
            return Ok(GitState::Rebasing);
        }

        if git_dir.join("MERGE_HEAD").exists() && self.is_merge_actually_active()? {
            return Ok(GitState::Merging);
        }

        if git_dir.join("CHERRY_PICK_HEAD").exists() && self.is_cherry_pick_actually_active()? {
            return Ok(GitState::CherryPicking);
        }

        // If no ongoing operations, check for conflicts in working directory
        let status_output = self.git_repo.run(&["status", "--porcelain=v1"])?;
        let status_lines: Vec<&str> = status_output.lines().collect();

        // Check for actual conflicts in working directory
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

        Ok(GitState::Clean)
    }

    /// Check if a rebase is actually in progress (not just stale files)
    fn is_rebase_actually_active(&self) -> Result<bool> {
        // Try to get rebase info - this will fail if no rebase is actually in progress
        match self.git_repo.run(&["rebase", "--show-current-patch"]) {
            Ok(_output) => Ok(true),
            Err(_e) => {
                // Also check for rebase directories that would exist during an active rebase
                let git_dir = &self.git_dir;
                let rebase_merge_exists = git_dir.join("rebase-merge").exists();
                let rebase_apply_exists = git_dir.join("rebase-apply").exists();
                Ok(rebase_merge_exists || rebase_apply_exists)
            }
        }
    }

    /// Check if a merge is actually in progress
    fn is_merge_actually_active(&self) -> Result<bool> {
        let status = self.git_repo.run(&["status", "--porcelain"])?;
        let has_unmerged = status.lines().any(|l| l.starts_with('U'));
        Ok(has_unmerged)
    }

    /// Check if a cherry-pick is actually in progress
    fn is_cherry_pick_actually_active(&self) -> Result<bool> {
        let status = self.git_repo.run(&["status", "--porcelain"])?;
        let has_unmerged = status.lines().any(|l| l.starts_with('U'));
        Ok(has_unmerged)
    }

    /// Clean up stale rebase state using `git rebase --abort`
    #[cfg(test)]
    pub fn cleanup_stale_rebase_files(&self) -> Result<()> {
        match self.git_repo.run(&["rebase", "--abort"]) {
            Ok(_) => ui::print_info("Ran 'git rebase --abort' to clean stale state"),
            Err(e) => ui::print_warning(&format!(
                "Could not run 'git rebase --abort': {} (state may already be clean)",
                e
            )),
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

        // In test environment, don't try to prompt the user - just return an error
        let is_cfg_test = cfg!(test);
        let has_test_env = std::env::var("RUST_TEST_TIME_UNIT").is_ok();
        let has_cargo_test = std::env::var("CARGO_PKG_NAME").is_ok()
            && (std::thread::current().name().unwrap_or("").contains("test")
                || std::env::args().any(|arg| arg.contains("test")));

        if is_cfg_test || has_test_env || has_cargo_test {
            ui::print_warning("Running in test environment - cannot prompt for user input");
            return Err(TrainError::InvalidState {
                message:
                    "Manual conflict resolution required but running in non-interactive environment"
                        .to_string(),
            }
            .into());
        }

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
            0 => {
                let state = self.get_git_state()?;
                self.open_editor_for_conflicts(conflict_info).await?;
                self.verify_conflicts_resolved(conflict_info, state).await
            }
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

            match self.editor_launcher.launch(
                &editor_config.default_editor,
                &editor_config.editor_args,
                &conflict_file.path,
            ) {
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

        Ok(())
    }

    /// Verify that all conflicts have been resolved
    pub async fn verify_conflicts_resolved(
        &self,
        original: &ConflictInfo,
        initial_state: GitState,
    ) -> Result<()> {
        loop {
            let current_conflicts = self.detect_conflicts()?;
            if let Some(conflicts) = current_conflicts {
                ui::print_error(&format!(
                    "Still have {} unresolved conflicts",
                    conflicts.files.len()
                ));

                if ui::confirm_action("Do you want to continue editing?")? {
                    self.open_editor_for_conflicts(&conflicts).await?;
                    continue;
                } else {
                    return Err(TrainError::InvalidState {
                        message: "Conflicts not resolved".to_string(),
                    }
                    .into());
                }
            }
            break;
        }

        // Add only the files that were conflicted
        for f in &original.files {
            self.git_repo.run(&["add", &f.path])?;
        }

        match initial_state {
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
            ('A', 'A') => ConflictStatus::BothModified,
            ('A', 'U') => ConflictStatus::AddedByUs,
            ('U', 'A') => ConflictStatus::AddedByThem,
            ('D', 'U') => ConflictStatus::DeletedByUs,
            ('U', 'D') => ConflictStatus::DeletedByThem,
            ('D', 'D') => ConflictStatus::BothModified,
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
        match self.get_git_state()? {
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
            _ => {
                ui::print_warning("No operation to abort");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo() -> Result<(tempfile::TempDir, GitRepository, PathBuf)> {
        let tmp = tempfile::tempdir()?;
        Command::new("git")
            .arg("init")
            .current_dir(tmp.path())
            .output()?;
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(tmp.path())
            .output()?;
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(tmp.path())
            .output()?;
        let repo = GitRepository::new(tmp.path())?;
        let git_dir = tmp.path().join(".git");
        Ok((tmp, repo, git_dir))
    }

    #[test]
    fn cleanup_rebase_preserves_orig_head() -> Result<()> {
        let (_tmp, repo, git_dir) = init_repo()?;
        std::fs::write(git_dir.join("ORIG_HEAD"), "dummy")?;
        let resolver = ConflictResolver::new(TrainConfig::default(), git_dir.clone(), repo);

        resolver.cleanup_stale_rebase_files()?;

        assert!(git_dir.join("ORIG_HEAD").exists());
        Ok(())
    }

    #[tokio::test]
    async fn verify_conflicts_adds_only_specified_files() -> Result<()> {
        let (_tmp, repo, git_dir) = init_repo()?;

        std::fs::write(git_dir.parent().unwrap().join("file1.txt"), "a")?;
        std::fs::write(git_dir.parent().unwrap().join("other.txt"), "b")?;
        Command::new("git")
            .args(["add", "."])
            .current_dir(git_dir.parent().unwrap())
            .output()?;
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(git_dir.parent().unwrap())
            .output()?;

        // Modify both files
        std::fs::write(git_dir.parent().unwrap().join("file1.txt"), "changed")?;
        std::fs::write(git_dir.parent().unwrap().join("other.txt"), "changed2")?;

        let resolver = ConflictResolver::new(TrainConfig::default(), git_dir.clone(), repo);
        let info = ConflictInfo {
            files: vec![ConflictFile {
                path: "file1.txt".to_string(),
                status: ConflictStatus::BothModified,
            }],
        };

        resolver
            .verify_conflicts_resolved(&info, GitState::Clean)
            .await?;

        let out = Command::new("git")
            .args(["diff", "--name-only", "--cached"])
            .current_dir(git_dir.parent().unwrap())
            .output()?;
        let staged = String::from_utf8(out.stdout)?;
        assert_eq!(staged.trim(), "file1.txt");

        Ok(())
    }
}
