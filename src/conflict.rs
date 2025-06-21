use anyhow::Result;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::NamedTempFile;
use tracing::{info, warn, error};

use crate::config::{TrainConfig, AutoResolveStrategy, RebaseStrategy};
use crate::errors::TrainError;
use crate::utils::{run_git_command, print_success, print_warning, print_error, print_info, confirm_action};

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub files: Vec<ConflictFile>,
    pub conflict_type: ConflictType,
    pub can_auto_resolve: bool,
}

#[derive(Debug, Clone)]
pub struct ConflictFile {
    pub path: String,
    pub status: ConflictStatus,
    pub conflict_markers: Vec<ConflictMarker>,
}

#[derive(Debug, Clone)]
pub struct ConflictMarker {
    pub line_number: usize,
    pub marker_type: ConflictMarkerType,
    pub content: String,
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
pub enum ConflictMarkerType {
    Ours,      // <<<<<<< HEAD
    Theirs,    // >>>>>>> commit
    Separator, // =======
}

#[derive(Debug, Clone)]
pub enum ConflictType {
    Rebase,
    Merge,
    CherryPick,
    Unknown,
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
            line.starts_with("UU") || line.starts_with("AA") || 
            line.starts_with("DU") || line.starts_with("UD") ||
            line.starts_with("AU") || line.starts_with("UA")
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
        let files_to_remove = [
            "MERGE_HEAD",
            "MERGE_MSG",
            "MERGE_MODE",
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
        
        Ok(())
    }

    /// Clean up stale cherry-pick files
    fn cleanup_stale_cherry_pick_files(&self) -> Result<()> {
        let git_dir = &self.git_dir;
        let files_to_remove = [
            "CHERRY_PICK_HEAD",
            "CHERRY_PICK_MSG",
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
        
        Ok(())
    }

    /// Detect and analyze conflicts in the repository
    pub fn detect_conflicts(&self) -> Result<Option<ConflictInfo>> {
        let git_state = self.get_git_state()?;
        
        match git_state {
            GitState::Clean => Ok(None),
            GitState::Rebasing => self.analyze_rebase_conflicts(),
            GitState::Merging => self.analyze_merge_conflicts(),
            GitState::CherryPicking => self.analyze_cherry_pick_conflicts(),
            GitState::Conflicted => self.analyze_working_directory_conflicts(),
        }
    }

    /// Attempt to resolve conflicts automatically based on configuration
    pub async fn auto_resolve_conflicts(&self, conflict_info: &ConflictInfo) -> Result<bool> {
        match self.config.conflict_resolution.auto_resolve_strategy {
            AutoResolveStrategy::Never => {
                print_info("Auto-resolution disabled, will prompt for manual resolution");
                Ok(false)
            },
            AutoResolveStrategy::Simple => {
                self.attempt_simple_resolution(conflict_info).await
            },
            AutoResolveStrategy::Smart => {
                self.attempt_smart_resolution(conflict_info).await
            },
        }
    }

    /// Handle conflicts with user intervention
    pub async fn resolve_conflicts_interactively(&self, conflict_info: &ConflictInfo) -> Result<()> {
        print_info("Conflicts detected. Opening editor for manual resolution...");
        
        // Show conflict summary
        self.print_conflict_summary(conflict_info);
        
        // Ask user how they want to proceed
        let options = vec![
            "Try automatic resolution",
            "Open editor to resolve conflicts manually",
            "Abort current operation",
        ];
        
        let choice = match crate::utils::select_from_list(&options, "How would you like to resolve the conflicts?") {
            Ok(choice) => choice,
            Err(_) => {
                // User cancelled (Ctrl+C) - provide graceful handling
                print_warning("Operation cancelled by user.");
                print_info("Resolution options:");
                print_info("• Run 'git-train resolve interactive' to try again");
                print_info("• Run 'git-train resolve abort' to cancel the current operation"); 
                print_info("• Resolve conflicts manually and run 'git-train resolve continue'");
                return Err(TrainError::InvalidState {
                    message: "Conflict resolution cancelled by user".to_string(),
                }.into());
            }
        };
        
        match choice {
            0 => {
                if self.auto_resolve_conflicts(conflict_info).await? {
                    print_success("Conflicts resolved automatically");
                    self.verify_conflicts_resolved().await
                } else {
                    print_warning("Automatic resolution failed, falling back to manual resolution");
                    self.open_editor_for_conflicts(conflict_info).await
                }
            },
            1 => self.open_editor_for_conflicts(conflict_info).await,
            2 => {
                self.abort_current_operation()?;
                print_success("Current operation aborted. Repository is now clean.");
                Ok(())
            },
            _ => unreachable!(),
        }
    }

    /// Open the configured editor for manual conflict resolution
    async fn open_editor_for_conflicts(&self, conflict_info: &ConflictInfo) -> Result<()> {
        let editor_config = &self.config.editor;
        
        for conflict_file in &conflict_info.files {
            print_info(&format!("Opening {} in {}", conflict_file.path, editor_config.default_editor));
            
            let mut cmd = Command::new(&editor_config.default_editor);
            cmd.args(&editor_config.editor_args);
            cmd.arg(&conflict_file.path);
            
            match cmd.status() {
                Ok(status) => {
                    if !status.success() {
                        print_warning(&format!("Editor {} exited with non-zero status", editor_config.default_editor));
                        
                        // Check if user wants to continue with other files or abort
                        if conflict_info.files.len() > 1 {
                            let continue_choice = crate::utils::confirm_action("Continue editing other files?")?;
                            if !continue_choice {
                                print_info("Resolution options:");
                                print_info("• Run 'git-train resolve interactive' to resume editing");
                                print_info("• Run 'git-train resolve abort' to cancel the operation");
                                return Err(TrainError::InvalidState {
                                    message: "Manual conflict resolution interrupted".to_string(),
                                }.into());
                            }
                        }
                    }
                }
                Err(e) => {
                    print_error(&format!("Failed to launch editor {}: {}", editor_config.default_editor, e));
                    print_info("You can:");
                    print_info("• Resolve conflicts manually in your preferred editor");
                    print_info("• Run 'git-train resolve continue' when done");
                    print_info("• Run 'git-train resolve abort' to cancel");
                    return Err(TrainError::GitError {
                        message: format!("Could not launch editor: {}", e),
                    }.into());
                }
            }
        }
        
        // Verify conflicts are resolved
        self.verify_conflicts_resolved().await
    }

    /// Verify that all conflicts have been resolved
    pub async fn verify_conflicts_resolved(&self) -> Result<()> {
        let current_conflicts = self.detect_conflicts()?;
        
        if let Some(conflicts) = current_conflicts {
            print_error(&format!("Still have {} unresolved conflicts", conflicts.files.len()));
            
            if confirm_action("Do you want to continue editing?")? {
                return Box::pin(self.open_editor_for_conflicts(&conflicts)).await;
            } else {
                return Err(TrainError::InvalidState {
                    message: "Conflicts not resolved".to_string(),
                }.into());
            }
        }
        
        // Add resolved files and continue
        run_git_command(&["add", "."])?;
        
        match self.get_git_state()? {
            GitState::Rebasing => {
                run_git_command(&["rebase", "--continue"])?;
                print_success("Rebase continued successfully");
            },
            GitState::Merging => {
                run_git_command(&["commit", "--no-edit"])?;
                print_success("Merge completed successfully");
            },
            GitState::CherryPicking => {
                run_git_command(&["cherry-pick", "--continue"])?;
                print_success("Cherry-pick continued successfully");
            },
            _ => {
                print_success("Conflicts resolved");
            }
        }
        
        Ok(())
    }

    /// Attempt simple automatic conflict resolution
    async fn attempt_simple_resolution(&self, conflict_info: &ConflictInfo) -> Result<bool> {
        let mut resolved_count = 0;
        
        for conflict_file in &conflict_info.files {
            if self.can_resolve_simple(&conflict_file)? {
                if self.resolve_simple_conflict(&conflict_file)? {
                    resolved_count += 1;
                    print_success(&format!("Auto-resolved simple conflict in {}", conflict_file.path));
                }
            }
        }
        
        if resolved_count == conflict_info.files.len() {
            run_git_command(&["add", "."])?;
            Ok(true)
        } else {
            print_warning(&format!("Only resolved {} of {} conflicts", resolved_count, conflict_info.files.len()));
            Ok(false)
        }
    }

    /// Attempt smart automatic conflict resolution
    async fn attempt_smart_resolution(&self, conflict_info: &ConflictInfo) -> Result<bool> {
        // First try simple resolution
        if self.attempt_simple_resolution(conflict_info).await? {
            return Ok(true);
        }
        
        // Try more sophisticated resolution strategies
        let mut resolved_count = 0;
        
        for conflict_file in &conflict_info.files {
            if self.can_resolve_smart(&conflict_file)? {
                if self.resolve_smart_conflict(&conflict_file)? {
                    resolved_count += 1;
                    print_success(&format!("Auto-resolved conflict in {}", conflict_file.path));
                }
            }
        }
        
        if resolved_count == conflict_info.files.len() {
            run_git_command(&["add", "."])?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Check if a conflict can be resolved with simple strategies
    fn can_resolve_simple(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        
        // Simple cases we can auto-resolve:
        // 1. Whitespace-only conflicts
        // 2. Line ending conflicts
        // 3. Import/include order conflicts
        
        for marker in &conflict_file.conflict_markers {
            if !self.is_whitespace_conflict(&content, marker)? &&
               !self.is_line_ending_conflict(&content, marker)? &&
               !self.is_import_order_conflict(&content, marker)? {
                return Ok(false);
            }
        }
        
        Ok(true)
    }

    /// Check if a conflict can be resolved with smart strategies
    fn can_resolve_smart(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        
        // Smart resolution strategies:
        // 1. All simple cases
        // 2. Non-overlapping changes
        // 3. Clear preference patterns (e.g., always take newer version)
        
        if self.can_resolve_simple(conflict_file)? {
            return Ok(true);
        }
        
        // Check for non-overlapping changes
        self.has_non_overlapping_changes(&content, conflict_file)
    }

    fn analyze_rebase_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::Rebase)
    }

    fn analyze_merge_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::Merge)
    }

    fn analyze_cherry_pick_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::CherryPick)
    }

    fn analyze_working_directory_conflicts(&self) -> Result<Option<ConflictInfo>> {
        self.analyze_conflicts_by_type(ConflictType::Unknown)
    }

    fn analyze_conflicts_by_type(&self, conflict_type: ConflictType) -> Result<Option<ConflictInfo>> {
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
        
        let can_auto_resolve = conflict_files.iter()
            .all(|f| self.can_resolve_simple(f).unwrap_or(false));
        
        Ok(Some(ConflictInfo {
            files: conflict_files,
            conflict_type,
            can_auto_resolve,
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
        
        let conflict_markers = self.find_conflict_markers(&file_path)?;
        
        Ok(Some(ConflictFile {
            path: file_path,
            status,
            conflict_markers,
        }))
    }

    fn find_conflict_markers(&self, file_path: &str) -> Result<Vec<ConflictMarker>> {
        let content = match fs::read_to_string(file_path) {
            Ok(content) => content,
            Err(_) => return Ok(Vec::new()), // File might be deleted
        };
        
        let mut markers = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        
        for (line_num, line) in lines.iter().enumerate() {
            if line.starts_with("<<<<<<<") {
                markers.push(ConflictMarker {
                    line_number: line_num + 1,
                    marker_type: ConflictMarkerType::Ours,
                    content: line.to_string(),
                });
            } else if line.starts_with("=======") {
                markers.push(ConflictMarker {
                    line_number: line_num + 1,
                    marker_type: ConflictMarkerType::Separator,
                    content: line.to_string(),
                });
            } else if line.starts_with(">>>>>>>") {
                markers.push(ConflictMarker {
                    line_number: line_num + 1,
                    marker_type: ConflictMarkerType::Theirs,
                    content: line.to_string(),
                });
            }
        }
        
        Ok(markers)
    }

    pub fn print_conflict_summary(&self, conflict_info: &ConflictInfo) {
        print_warning(&format!("Found {} conflicted files:", conflict_info.files.len()));
        
        for conflict_file in &conflict_info.files {
            println!("  📄 {} ({:?})", conflict_file.path, conflict_file.status);
            if !conflict_file.conflict_markers.is_empty() {
                println!("     {} conflict markers found", conflict_file.conflict_markers.len());
            }
        }
        
        if conflict_info.can_auto_resolve {
            print_info("These conflicts may be automatically resolvable");
        } else {
            print_warning("Manual resolution may be required");
        }
    }

    pub fn abort_current_operation(&self) -> Result<()> {
        match self.get_git_state()? {
            GitState::Rebasing => {
                run_git_command(&["rebase", "--abort"])?;
                print_info("Rebase aborted");
            },
            GitState::Merging => {
                run_git_command(&["merge", "--abort"])?;
                print_info("Merge aborted");
            },
            GitState::CherryPicking => {
                run_git_command(&["cherry-pick", "--abort"])?;
                print_info("Cherry-pick aborted");
            },
            _ => {
                print_warning("No operation to abort");
            }
        }
        Ok(())
    }

    /// Force cleanup of all stale git state files
    pub fn cleanup_all_stale_files(&self) -> Result<()> {
        print_info("Cleaning up all potentially stale git state files...");
        
        // Clean up stale files even if we think they're active
        // This is a force cleanup for recovery scenarios
        self.cleanup_stale_rebase_files()?;
        self.cleanup_stale_merge_files()?; 
        self.cleanup_stale_cherry_pick_files()?;
        
        print_success("Cleaned up all stale git state files");
        Ok(())
    }

    // Implement actual conflict resolution strategies
    fn resolve_simple_conflict(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        let mut modified = false;
        let mut new_content = content.clone();
        
        // Try to resolve each conflict marker
        for marker in &conflict_file.conflict_markers {
            if self.is_whitespace_conflict(&content, marker)? {
                new_content = self.resolve_whitespace_conflict(&new_content, marker)?;
                modified = true;
            } else if self.is_line_ending_conflict(&content, marker)? {
                new_content = self.resolve_line_ending_conflict(&new_content, marker)?;
                modified = true;
            } else if self.is_import_order_conflict(&content, marker)? {
                new_content = self.resolve_import_order_conflict(&new_content, marker)?;
                modified = true;
            }
        }
        
        if modified {
            fs::write(&conflict_file.path, new_content)?;
            print_success(&format!("Auto-resolved simple conflicts in {}", conflict_file.path));
        }
        
        Ok(modified)
    }

    fn resolve_smart_conflict(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        
        // First try simple resolution
        if self.resolve_simple_conflict(conflict_file)? {
            return Ok(true);
        }
        
        // Try non-overlapping changes resolution
        if self.has_non_overlapping_changes(&content, conflict_file)? {
            return self.resolve_non_overlapping_conflict(conflict_file);
        }
        
        Ok(false)
    }

    fn is_whitespace_conflict(&self, content: &str, marker: &ConflictMarker) -> Result<bool> {
        let lines: Vec<&str> = content.lines().collect();
        
        // Find the conflict block
        let start_line = marker.line_number.saturating_sub(1);
        let mut ours_lines = Vec::new();
        let mut theirs_lines = Vec::new();
        let mut in_ours = false;
        let mut in_theirs = false;
        
        for (i, line) in lines.iter().enumerate() {
            if i >= start_line {
                if line.starts_with("<<<<<<<") {
                    in_ours = true;
                    continue;
                } else if line.starts_with("=======") {
                    in_ours = false;
                    in_theirs = true;
                    continue;
                } else if line.starts_with(">>>>>>>") {
                    break;
                }
                
                if in_ours {
                    ours_lines.push(*line);
                } else if in_theirs {
                    theirs_lines.push(*line);
                }
            }
        }
        
        // Check if the only differences are whitespace
        if ours_lines.len() == theirs_lines.len() {
            for (our_line, their_line) in ours_lines.iter().zip(theirs_lines.iter()) {
                if our_line.trim() != their_line.trim() {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        
        Ok(false)
    }

    fn is_line_ending_conflict(&self, content: &str, _marker: &ConflictMarker) -> Result<bool> {
        // Simple heuristic: if the file contains mixed line endings, it might be a line ending conflict
        let has_crlf = content.contains("\r\n");
        let has_lf = content.contains('\n') && !content.contains("\r\n");
        
        Ok(has_crlf && has_lf)
    }

    fn is_import_order_conflict(&self, content: &str, marker: &ConflictMarker) -> Result<bool> {
        let lines: Vec<&str> = content.lines().collect();
        let start_line = marker.line_number.saturating_sub(1);
        
        // Check if conflict is in import/include section
        for (i, line) in lines.iter().enumerate() {
            if i >= start_line && i < start_line + 10 {  // Look in the vicinity
                let trimmed = line.trim();
                if trimmed.starts_with("import ") || trimmed.starts_with("from ") ||
                   trimmed.starts_with("#include") || trimmed.starts_with("use ") {
                    return Ok(true);
                }
            }
        }
        
        Ok(false)
    }

    fn has_non_overlapping_changes(&self, content: &str, conflict_file: &ConflictFile) -> Result<bool> {
        // Simplified check: if there are multiple conflict blocks that don't seem to interact
        Ok(conflict_file.conflict_markers.len() == 3) // Standard conflict has exactly 3 markers
    }

    fn resolve_whitespace_conflict(&self, content: &str, marker: &ConflictMarker) -> Result<String> {
        let lines: Vec<&str> = content.lines().collect();
        let mut result_lines = Vec::new();
        let start_line = marker.line_number.saturating_sub(1);
        let mut i = 0;
        
        while i < lines.len() {
            if i == start_line && lines[i].starts_with("<<<<<<<") {
                // Found conflict start, collect 'ours' lines (normalized)
                i += 1; // Skip conflict marker
                let mut resolved_lines = Vec::new();
                
                while i < lines.len() && !lines[i].starts_with("=======") {
                    resolved_lines.push(lines[i].trim_end().to_string()); // Normalize whitespace
                    i += 1;
                }
                
                // Skip separator and 'theirs' lines
                i += 1; // Skip =======
                while i < lines.len() && !lines[i].starts_with(">>>>>>>") {
                    i += 1;
                }
                i += 1; // Skip >>>>>>>
                
                // Add resolved lines
                result_lines.extend(resolved_lines);
            } else {
                result_lines.push(lines[i].to_string());
                i += 1;
            }
        }
        
        Ok(result_lines.join("\n"))
    }

    fn resolve_line_ending_conflict(&self, content: &str, _marker: &ConflictMarker) -> Result<String> {
        // Normalize to Unix line endings
        Ok(content.replace("\r\n", "\n"))
    }

    fn resolve_import_order_conflict(&self, content: &str, marker: &ConflictMarker) -> Result<String> {
        let lines: Vec<&str> = content.lines().collect();
        let mut result_lines = Vec::new();
        let start_line = marker.line_number.saturating_sub(1);
        let mut i = 0;
        
        while i < lines.len() {
            if i == start_line && lines[i].starts_with("<<<<<<<") {
                // Collect all import lines from both sides and sort them
                let mut import_lines = std::collections::HashSet::new();
                i += 1; // Skip conflict marker
                
                // Collect 'ours' imports
                while i < lines.len() && !lines[i].starts_with("=======") {
                    let line = lines[i].trim();
                    if !line.is_empty() {
                        import_lines.insert(line.to_string());
                    }
                    i += 1;
                }
                
                i += 1; // Skip =======
                
                // Collect 'theirs' imports
                while i < lines.len() && !lines[i].starts_with(">>>>>>>") {
                    let line = lines[i].trim();
                    if !line.is_empty() {
                        import_lines.insert(line.to_string());
                    }
                    i += 1;
                }
                
                i += 1; // Skip >>>>>>>
                
                // Sort and add import lines
                let mut sorted_imports: Vec<String> = import_lines.into_iter().collect();
                sorted_imports.sort();
                result_lines.extend(sorted_imports);
            } else {
                result_lines.push(lines[i].to_string());
                i += 1;
            }
        }
        
        Ok(result_lines.join("\n"))
    }

    fn resolve_non_overlapping_conflict(&self, conflict_file: &ConflictFile) -> Result<bool> {
        let content = fs::read_to_string(&conflict_file.path)?;
        let lines: Vec<&str> = content.lines().collect();
        let mut result_lines = Vec::new();
        let mut i = 0;
        
        while i < lines.len() {
            if lines[i].starts_with("<<<<<<<") {
                // Take both 'ours' and 'theirs' sections
                i += 1; // Skip conflict marker
                
                // Add 'ours' lines
                while i < lines.len() && !lines[i].starts_with("=======") {
                    result_lines.push(lines[i].to_string());
                    i += 1;
                }
                
                i += 1; // Skip =======
                
                // Add 'theirs' lines
                while i < lines.len() && !lines[i].starts_with(">>>>>>>") {
                    result_lines.push(lines[i].to_string());
                    i += 1;
                }
                
                i += 1; // Skip >>>>>>>
            } else {
                result_lines.push(lines[i].to_string());
                i += 1;
            }
        }
        
        fs::write(&conflict_file.path, result_lines.join("\n"))?;
        Ok(true)
    }
} 